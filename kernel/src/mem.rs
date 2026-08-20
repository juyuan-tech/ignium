//! 物理内存管理:伙伴(buddy)分配器。
//!
//! 管理 `[对齐后的 _alloc_start, RAM_END)` 物理页,页大小 4KB。
//! 阶(order)0..12,块大小 = 4KB × 2^order(最大 16MB)。
//!
//! # 设计
//! - **空闲链表**:每个 order 一条 intrusive 链表,空闲块的**首字**
//!   存储 next 页索引(块空闲时其内容可复用)。
//! - **元数据**:每页 2 字节 `{order, used}`(静态数组,与物理页一一
//!   对应),分配/合并 O(1) 判定 buddy 状态,与块内容解耦。
//! - **合并**:释放时若 buddy 同阶且空闲,摘除 buddy 并向上合并。
//!
//! # 并发约束(当前)
//! 所有访问经 `with_allocator`:
//! - **IRQ 安全 SpinLock**(MED-3,审计 17 轮):加锁保存/恢复 SIE,
//!   持锁临界区不被抢占,主上下文与 ISR 均不死锁(D3 已实现)。
//! - ISR 仍**零分配**(ISR 零分配约定,D11 容量预留兜底);分配
//!   器锁为 IRQ 安全变体,唤醒不会在中断恢复后再做(无锁唤醒)。

use crate::fdt;
use crate::sync::SpinLock;

/// 物理页大小(4KB)。
pub const PAGE_SIZE: usize = 4096;

/// 最大阶:块大小上限 = 4KB × 2^12 = 16MB。
pub const MAX_ORDER: usize = 12;

/// 空闲链表空指针标记。
const FREE_NONE: usize = usize::MAX;

/// 页元数据数组容量,基于 QEMU virt 默认 128MB RAM 确定。
/// 实际可用页数由 buddy init 时的 `real_count` 决定。
pub const MAX_PAGES: usize = 128 * 1024 * 1024 / PAGE_SIZE;

/// 每页元数据:`order` = 所属块阶,`used` = 已分配。
#[derive(Clone, Copy)]
#[repr(C)]
struct PageMeta {
    order: u8,
    used: bool,
}

/// 页元数据静态数组(64KB,启动时清零)。
/// C1(审计 15 轮):页元数据用 `static mut`(Rust 提供的可变静态
/// 机制)而非"不可变 static 经裸指针写"(后者是形式 UB)。
/// 访问纪律:一律经 `&raw const/&raw mut` 裸指针,不形成引用
/// (编译器不会在此引入别名假设);init 期单线程写,之后只读。
#[allow(static_mut_refs)]
static mut PAGE_META: [PageMeta; MAX_PAGES] = [PageMeta {
    order: 0,
    used: false,
}; MAX_PAGES];

/// 伙伴分配器单例,SpinLock 包装(HIGH-1):所有访问经 `with_allocator`
/// 互斥;static_mut + 裸指针模式移除。
static ALLOCATOR: SpinLock<BuddyAllocator> = SpinLock::new(BuddyAllocator {
    base: 0,
    real_count: 0,
    page_count: 0,
    meta: core::ptr::null_mut(),
    free_lists: [FREE_NONE; MAX_ORDER + 1],
});

/// 伙伴分配器。
pub struct BuddyAllocator {
    /// 可分配区物理基址(页对齐,且对齐到最大块)。
    base: usize,
    /// 真实可分配页数(不含补齐页)。
    real_count: usize,
    /// 补齐后的总页数(含永久占用的补齐页)。
    page_count: usize,
    /// 页元数据数组指针。
    meta: *mut PageMeta,
    /// 各阶空闲链表头(块 = 页索引)。
    free_lists: [usize; MAX_ORDER + 1],
}

// 含裸指针:单上下文 + 关中断互斥下,指针仅内核自身维护且生命周期
// 为内核全程 —— 跨上下文传递安全(供 SpinLock<T: Send> 的 Sync 约束)。
unsafe impl Send for BuddyAllocator {}

/// 分配器单例访问:IRQ 安全 SpinLock(MED-3/审计 17 轮:自旋锁
/// 内部保存/恢复 SIE,持锁临界区不被抢占,消除 convoy)。
/// 每次操作恰好一个 `&mut`;ISR 零分配(容量预留),无 ISR 竞争路径。
fn with_allocator<T>(f: impl FnOnce(&mut BuddyAllocator) -> T) -> T {
    f(&mut ALLOCATOR.lock())
}

impl BuddyAllocator {
    /// 读取某页元数据。
    fn meta(&mut self, idx: usize) -> &mut PageMeta {
        debug_assert!(idx < self.page_count);
        unsafe { &mut *self.meta.add(idx) }
    }

    /// 页索引 → 物理地址。
    #[inline]
    fn block_addr(&self, idx: usize) -> usize {
        self.base + idx * PAGE_SIZE
    }

    /// 物理地址 → 页索引。
    #[inline]
    fn block_index(&self, addr: usize) -> usize {
        (addr - self.base) / PAGE_SIZE
    }

    /// 把块(页索引)压入对应阶空闲链表。
    /// 空闲块的首字写入 next,元数据记录阶并标记空闲。
    fn push(&mut self, idx: usize, order: usize) {
        debug_assert!(order <= MAX_ORDER);
        debug_assert!(idx + (1usize << order) <= self.page_count);
        let next = self.free_lists[order];
        unsafe { (self.block_addr(idx) as *mut usize).write(next) };
        self.free_lists[order] = idx;
        let m = self.meta(idx);
        m.order = order as u8;
        m.used = false;
    }

    /// 从对应阶空闲链表弹出块(页索引),标记已分配。
    fn pop(&mut self, order: usize) -> Option<usize> {
        let head = self.free_lists[order];
        if head == FREE_NONE {
            return None;
        }
        // release 模式也校验(MED#4):链表损坏=内核完整性故障,
        // 宁可 panic 也不返回越界页索引。
        assert!(head < self.page_count, "buddy: corrupt free-list head");
        let next = unsafe { (self.block_addr(head) as *const usize).read() };
        self.free_lists[order] = next;
        Some(head)
    }

    /// 初始化:记录区域,并在建链时**刻蚀出保留区**(如 FDT)。
    ///
    /// 保留区处理(CRITICAL#1):只改元数据是不够的 —— 含保留区的
    /// 空闲块必须被**拆分/刻蚀**,否则整块仍可被分配、覆盖保留区。
    /// 做法:自顶向下 carve,与所有保留区均无交叠的块整块入链,完全在
    /// 任一保留区内的块标记永久占用,部分交叠的递归拆分。
    ///
    /// M8(审计 18 轮外部):支持多个不相交保留区间,避免过度保留。
    ///
    /// # Safety
    /// 只允许调用一次;`base` 必须页对齐;元数据数组容量足够。
    unsafe fn init(&mut self, base: usize, count: usize, reserved: &[(usize, usize)]) {
        debug_assert!(count <= MAX_PAGES);
        // 向上补齐到 2^MAX_ORDER 的倍数:补齐页标记为**永久占用**,
        // 使所有块都是完整的 order-12 —— 尾部不再产生小阶块,
        // 分配/合并行为完全确定(实测抓到的根因:尾部 order-11 块
        // 使两次同阶分配不互为 buddy)。
        let order_size = 1usize << MAX_ORDER;
        let padded = count.div_ceil(order_size) * order_size;
        debug_assert!(padded <= MAX_PAGES);

        self.base = base;
        self.real_count = count;
        self.page_count = padded;
        // C1:裸指针取址(static mut,不形成引用)。
        self.meta = (&raw const PAGE_META).cast::<PageMeta>().cast_mut();
        for i in 0..padded {
            *self.meta(i) = PageMeta {
                order: 0,
                used: false,
            };
        }
        for i in count..padded {
            let m = self.meta(i);
            m.order = MAX_ORDER as u8;
            m.used = true; // 永久占用,永不出链
        }
        if reserved.is_empty() {
            // 无有效保留区:整区入链。先按 order-12 放完整块,
            // 尾部不足 16MB 的部分按小阶块入链(回收尾部内存,M2)。
            let mut idx = 0usize;
            while idx + order_size <= count {
                self.push(idx, MAX_ORDER);
                idx += order_size;
            }
            while idx < count {
                let order = (count - idx).ilog2().min(MAX_ORDER as u32) as usize;
                self.push(idx, order);
                idx += 1usize << order;
            }
            return;
        }
        // 自顶向下刻蚀建链:对每个 order-12 块分别刻蚀,防止
        // 仅处理首块时其余块被遗漏(C1/审计 18 轮外部:前实现只
        // 调用一次 carve(0, MAX_ORDER, reserved),丢失所有后续块)。
        let order_size = 1usize << MAX_ORDER;
        let mut idx = 0;
        while idx < count {
            self.carve(idx, MAX_ORDER, reserved);
            idx += order_size;
        }
    }

    /// 递归刻蚀:构建空闲链表,同时把保留区页标记为永久占用。
    ///
    /// `reserved` 为保留区间列表(页索引,上下界均收敛到 real_count)。
    /// 块与**所有**保留区间均无交叠时入链;完全在任一区间内时标记
    /// 永久占用;部分交叠时递归拆分(M8:支持多个不相交区间)。
    fn carve(&mut self, idx: usize, order: usize, reserved: &[(usize, usize)]) {
        let size = 1usize << order;
        let s = idx;
        let e = idx + size;
        if s >= self.real_count {
            return;
        }
        if e > self.real_count {
            self.carve(idx, order - 1, reserved);
            self.carve(idx + (size >> 1), order - 1, reserved);
            return;
        }
        let mut inside_any = false;
        let mut overlap_any = false;
        for &(rs, re) in reserved {
            if s >= rs && e <= re {
                inside_any = true;
                break;
            }
            if e > rs && s < re {
                overlap_any = true;
            }
        }
        if !overlap_any {
            self.push(idx, order);
            return;
        }
        if inside_any {
            let m = self.meta(idx);
            m.order = u8::MAX;
            m.used = true;
            return;
        }
        self.carve(idx, order - 1, reserved);
        self.carve(idx + (size >> 1), order - 1, reserved);
    }

    /// 分配一个 `order` 阶块,返回物理地址;无足够内存返回 None。
    pub fn alloc(&mut self, order: usize) -> Option<usize> {
        if order > MAX_ORDER {
            return None;
        }
        // 从 order 向上找第一个非空链表。
        let mut o = order;
        let mut found = None;
        while o <= MAX_ORDER {
            if let Some(idx) = self.pop(o) {
                found = Some((idx, o));
                break;
            }
            o += 1;
        }
        let (idx, mut cur) = found?;
        // 自上而下拆分:每拆一层,把 buddy 压入下一阶。
        while cur > order {
            cur -= 1;
            self.push(idx + (1usize << cur), cur);
        }
        // 只标记块头页(O1):free/合并只读块头元数据;内页保持
        // {0,false} 使内页地址释放天然被拒(与 free 的对齐检查
        // 构成双重防护)。省去整块标记(大块分配省数千次写)。
        let m = self.meta(idx);
        m.order = order as u8;
        m.used = true;
        Some(self.block_addr(idx))
    }

    /// 释放 `addr` 处的块(合并 buddy 后回链)。
    pub fn free(&mut self, addr: usize) -> Result<(), ()> {
        // 指针合法性:在**真实**区域内、页对齐、元数据标记已分配。
        // 用 real_count 而非 page_count:补齐页(超出物理 RAM 的
        // 占位)元数据同为 used=true,若被误释放会把手伸向不存在的
        // 内存(M3) —— 用真实页数直接拒绝。
        let end = self.base + self.real_count * PAGE_SIZE;
        if addr < self.base || addr >= end || !addr.is_multiple_of(PAGE_SIZE) {
            return Err(());
        }
        let mut idx = self.block_index(addr);
        let mut order = self.meta(idx).order as usize;
        if order > MAX_ORDER || !self.meta(idx).used {
            return Err(()); // double-free 或未分配的指针
        }
        // 防御:F1 —— 释放地址必须是块头(页索引为 2^order 的倍数)。
        // 内页地址若被误传(API 滥用或指针损坏),拒绝而非静默破坏
        // 空闲链表(否则后续分配可能返回重叠块)。
        if !idx.is_multiple_of(1usize << order) {
            return Err(());
        }
        // 向上合并 buddy:仅当 order < MAX_ORDER 时尝试。
        // 两个 order-12 buddy 无法合并(不存在 order 13),保持为
        // 两条 order-12 空闲块即可(否则 push 会越界,自检实测抓到)。
        while order < MAX_ORDER {
            let buddy = idx ^ (1usize << order);
            let size = 1usize << order;
            if buddy + size > self.page_count {
                break;
            }
            let bm = self.meta(buddy);
            if bm.used || bm.order as usize != order {
                break;
            }
            self.unlink(buddy, order);
            idx = idx.min(buddy);
            order += 1;
        }
        self.push(idx, order);
        Ok(())
    }

    /// 从指定阶空闲链表摘除块(用于合并)。
    fn unlink(&mut self, target: usize, order: usize) {
        let mut prev = FREE_NONE;
        let mut cur = self.free_lists[order];
        while cur != FREE_NONE {
            assert!(cur < self.page_count, "buddy: corrupt free-list node");
            if cur == target {
                let next = unsafe { (self.block_addr(cur) as *const usize).read() };
                if prev == FREE_NONE {
                    self.free_lists[order] = next;
                } else {
                    unsafe { (self.block_addr(prev) as *mut usize).write(next) };
                }
                return;
            }
            prev = cur;
            cur = unsafe { (self.block_addr(cur) as *const usize).read() };
        }
    }
}

// ===== 公共接口 =====

extern "C" {
    // 链接脚本符号:保留区(镜像 + 栈)之后的分配区起点。
    static _alloc_start: u8;
}

/// 初始化物理内存管理。启动早期调用(先于 irq_enable)。
///
/// `params` 为 FDT 解析后的板级参数(含保留区,防止分配器覆盖
/// FDT 数据与固件保留内存)。
pub fn init(params: &fdt::BoardParams) {
    // _alloc_start 向上对齐到**最大块(16MB)**:buddy 层级只保证
    // 相对 base 的对齐,绝对地址对齐要求 base 本身对齐到最大块
    // (自检实测抓到:base 仅页对齐时,order-3 块绝对地址不 32KB 对齐,
    // 未来页表/超页/DMA 都需要绝对对齐)。
    let raw = (&raw const _alloc_start).addr();
    let max_block = (1usize << MAX_ORDER) * PAGE_SIZE;
    let base = raw.div_ceil(max_block) * max_block;
    let ram_end = crate::board::ram_end();
    if base >= ram_end {
        panic!("no physical memory available for allocator");
    }
    let count = (ram_end - base) / PAGE_SIZE;
    // H3(审计 18 轮外部):MAX_PAGES 系静态数组容量,运行时应检查
    // FDT 给出的 RAM 大小是否超出,防止 buddy 元数据与 slab 判别表
    // 越界写(panic 在 debug_assert 中,release 下仍需保证)。
    assert!(
        count <= MAX_PAGES,
        "RAM size ({count} pages) exceeds MAX_PAGES ({MAX_PAGES}); increase MAX_PAGES"
    );
    // 保留区:将 BoardParams 中的保留区间转换为页索引后传入
    // allocator 的 init(M8:支持多个不相交区间,避免过度保留)。
    let mut reserved_pairs: [(usize, usize); 8] = [(0usize, 0usize); 8];
    let mut n = 0;
    for i in 0..params.reserved_count.min(8) {
        let (r_addr, r_size) = params.reserved[i];
        if r_size == 0 {
            continue;
        }
        let r_start = r_addr / PAGE_SIZE * PAGE_SIZE;
        // 自审修复:恶意 FDT 可给巨大 r_size,使 saturating_add 后
        // div_ceil*PAGE_SIZE 溢出(panic 或回绕)。把区间收敛到
        // [0, ram_end) 且用 checked 边界计算,杜绝溢出路径。
        let r_end_raw = match r_addr.checked_add(r_size) {
            Some(e) => e,
            None => ram_end, // 溢出:视为覆盖到 RAM 末端(会被下方裁剪)
        };
        let r_end = r_end_raw.min(ram_end);
        let r_end = r_end
            .div_ceil(PAGE_SIZE)
            .checked_mul(PAGE_SIZE)
            .unwrap_or(ram_end);
        if r_end <= base || r_start >= ram_end {
            continue;
        }
        let idx_start = r_start.saturating_sub(base) / PAGE_SIZE;
        let idx_end = r_end.saturating_sub(base).div_ceil(PAGE_SIZE);
        if idx_start < idx_end && n < 8 {
            reserved_pairs[n] = (idx_start.min(count), idx_end.min(count));
            n += 1;
        }
    }
    // 只传非空区间,使 allocator 的 reserved.is_empty() 快速路径生效。
    with_allocator(|a| unsafe { a.init(base, count, &reserved_pairs[..n]) });
}

/// 真实可分配页数(不含补齐页;I2:报告给调用方的应是真实数,
/// 而非含永久占用补齐页的 padded 值)。
pub fn page_count() -> usize {
    with_allocator(|a| a.real_count)
}

/// 分配区物理基址(内核堆的页索引换算用)。
pub fn base() -> usize {
    with_allocator(|a| a.base)
}

/// 分配 2^order 页,返回物理地址。
///
/// # 契约
/// 返回值**必须**原样传给 `free_pages`;传入块内页地址(内页)会被
/// 拒绝(返回 Err),不会静默破坏分配器。
///
/// # 数据保密(M4)
/// 返回的页**未清零**(空闲时首字存有链表指针)。M2 向用户态交接
/// 页面之前必须整页清零,否则泄漏内核链表布局信息。
pub fn alloc_pages(order: usize) -> Option<usize> {
    with_allocator(|a| a.alloc(order))
}

/// 释放物理地址处的块。
///
/// # 契约
/// `addr` 必须是 `alloc_pages` 原样返回的地址;其他输入(内页、
/// 未分配页、预留区、越界页)返回 Err。
pub fn free_pages(addr: usize) -> Result<(), ()> {
    with_allocator(|a| a.free(addr))
}

/// 整页清零(页表页等需要确定初始内容的内存)。
///
/// 用非 volatile 裸指针写:页表初始化期间无并发读者,且编译器
/// 无法消除对裸指针的写入(不可见副作用),无需 volatile 屏障。
///
/// # Safety
/// `addr` 必须是页对齐且可写(由 `alloc_pages` 分配、未释放)的
/// 物理地址 —— 写 4KB 字节。安全代码不得以任意地址调用
/// (LOW-1/审计 17 轮:原 safe fn 允许安全代码损坏任意内存)。
pub unsafe fn zero_page(addr: usize) {
    debug_assert!(addr.is_multiple_of(PAGE_SIZE));
    let p = addr as *mut u64;
    for i in 0..PAGE_SIZE / core::mem::size_of::<u64>() {
        unsafe { core::ptr::write(p.add(i), 0) };
    }
}

/// 分配器自检:验证分配/释放/合并/对齐,失败返回错误描述。
///
/// 由 kernel_main 在 irq_enable 之前调用(尚无并发竞争者)。
pub fn self_test() -> Result<(), &'static str> {
    // V4(外部审计 MED):自适应最大可用阶 —— 小/碎片化 RAM 未必有
    // order-12(16 MiB)整块。探测最大可分配阶,以其为测试基准。
    let mut max_avail = MAX_ORDER;
    while max_avail > 0 {
        if let Some(p) = alloc_pages(max_avail) {
            free_pages(p).map_err(|_| "probe free failed")?;
            break;
        }
        max_avail -= 1;
    }
    // 1) 整块(max_avail)分配→释放→再分配:应复用同一区域。
    let a = alloc_pages(max_avail).ok_or("big alloc failed")?;
    free_pages(a).map_err(|_| "big free failed")?;
    let b = alloc_pages(max_avail).ok_or("re-big alloc failed")?;
    if a != b {
        return Err("buddy reuse mismatch");
    }
    free_pages(b).map_err(|_| "big free #2 failed")?;

    // 2) 全生命周期守恒:按阶**递减**排空(原生弹出不拆分)→
    //    耗尽后分配必须失败 → 全部释放 → max_avail 必须可再次分配。
    //    最后一步只有当低阶块全部合并回 max_avail 阶时才成立 ——
    //    这是对 buddy 合并语义的直接验证,且不依赖块相邻性。
    //    地址用**链表挂在块首字上**(与空闲链表同构):无堆环境
    //    下的无界存储(M2:固定数组在不同内存几何下会溢出误报)。
    let mut head: usize = FREE_NONE;
    for order in (0..=max_avail).rev() {
        while let Some(addr) = alloc_pages(order) {
            unsafe { core::ptr::write(addr as *mut usize, head) };
            head = addr;
        }
    }
    if alloc_pages(0).is_some() {
        return Err("allocator not exhausted");
    }
    while head != FREE_NONE {
        let next = unsafe { core::ptr::read(head as *const usize) };
        free_pages(head).map_err(|_| "free held failed")?;
        head = next;
    }
    let big = alloc_pages(max_avail).ok_or("post-free big alloc failed")?;
    free_pages(big).map_err(|_| "free big failed")?;

    // 3) 对齐:每个阶的块都须按自身大小绝对对齐
    //    (base 已对齐到最大块,所有拆分保持对齐)。
    for order in 0..=max_avail {
        let a = alloc_pages(order).ok_or("align alloc failed")?;
        let block = PAGE_SIZE * (1usize << order);
        if !a.is_multiple_of(block) {
            return Err("block misaligned");
        }
        free_pages(a).map_err(|_| "align free failed")?;
    }

    // 4) 双重释放被拒绝。
    let g = alloc_pages(0).ok_or("alloc(0) failed")?;
    free_pages(g).map_err(|_| "free g failed")?;
    if free_pages(g).is_ok() {
        return Err("double-free accepted");
    }

    // 5) 块内页地址被拒绝(防空闲链表静默损坏,F1)。
    //    用 max_avail 自适应的块阶(至少 order-1 有内页;若 max_avail==0
    //    仅 1 页,无内页可测,跳过)。
    if max_avail >= 1 {
        let h = alloc_pages(max_avail.min(2)).ok_or("interior alloc failed")?;
        if free_pages(h + PAGE_SIZE).is_ok() {
            return Err("interior page free accepted");
        }
        free_pages(h).map_err(|_| "free h failed")?;
    }

    Ok(())
}
