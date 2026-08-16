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
//! 非 SMP/ISR 安全:调用方须保证互斥(单核 + 关中断)。启动自检在
//! `irq_enable` 之前运行。M1.5 引入自旋锁后开放多核/中断上下文,
//! 并配套"页状态 bitmap + 守卫页"。

/// 物理页大小(4KB)。
pub const PAGE_SIZE: usize = 4096;

/// 最大阶:块大小上限 = 4KB × 2^12 = 16MB。
pub const MAX_ORDER: usize = 12;

/// 空闲链表空指针标记。
const FREE_NONE: usize = usize::MAX;

/// QEMU virt 默认 128MB RAM(与 `-m 128M` 一致;真机/FDT 定尺寸为
/// M1.5 工作,届时替换 RAM_END 来源)。
const RAM_START: usize = 0x8000_0000;
const RAM_END: usize = RAM_START + 128 * 1024 * 1024;

/// 元数据数组覆盖最大可能的页数(RAM_START 起算)。
const MAX_PAGES: usize = (RAM_END - RAM_START) / PAGE_SIZE;

/// 每页元数据:`order` = 所属块阶,`used` = 已分配。
#[derive(Clone, Copy)]
#[repr(C)]
struct PageMeta {
    order: u8,
    used: bool,
}

/// 页元数据静态数组(64KB,.bss,启动时清零)。
/// 与 `_alloc_start` 实际页数无关,只保证容量足够。
static mut PAGE_META: [PageMeta; MAX_PAGES] = [PageMeta {
    order: 0,
    used: false,
}; MAX_PAGES];

/// 伙伴分配器单例。
static mut ALLOCATOR: BuddyAllocator = BuddyAllocator {
    base: 0,
    real_count: 0,
    page_count: 0,
    meta: core::ptr::null_mut(),
    free_lists: [FREE_NONE; MAX_ORDER + 1],
};

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

/// 访问分配器单例(static mut 经裸指针解引用,规避 static_mut_refs lint)。
fn allocator() -> &'static mut BuddyAllocator {
    unsafe { (&raw mut ALLOCATOR).as_mut().unwrap() }
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
        let next = unsafe { (self.block_addr(head) as *const usize).read() };
        self.free_lists[order] = next;
        Some(head)
    }

    /// 初始化:记录区域并整区入链(按最大阶拆分)。
    ///
    /// # Safety
    /// 只允许调用一次;`base` 必须页对齐;元数据数组容量足够。
    unsafe fn init(&mut self, base: usize, count: usize) {
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
        // 整区按 order-12 块入链。
        let mut idx = 0usize;
        while idx < padded {
            self.push(idx, MAX_ORDER);
            idx += order_size;
        }
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
pub fn init() {
    // _alloc_start 向上对齐到**最大块(16MB)**:buddy 层级只保证
    // 相对 base 的对齐,绝对地址对齐要求 base 本身对齐到最大块
    // (自检实测抓到:base 仅页对齐时,order-3 块绝对地址不 32KB 对齐,
    // 未来页表/超页/DMA 都需要绝对对齐)。
    let raw = unsafe { &_alloc_start as *const u8 as usize };
    let max_block = (1usize << MAX_ORDER) * PAGE_SIZE;
    let base = raw.div_ceil(max_block) * max_block;
    if base >= RAM_END {
        panic!("no physical memory available for allocator");
    }
    let count = (RAM_END - base) / PAGE_SIZE;
    unsafe { allocator().init(base, count) };
}

/// 可分配页数。
pub fn page_count() -> usize {
    allocator().page_count
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
    allocator().alloc(order)
}

/// 释放物理地址处的块。
///
/// # 契约
/// `addr` 必须是 `alloc_pages` 原样返回的地址;其他输入(内页、
/// 未分配页、越界页)返回 Err。
pub fn free_pages(addr: usize) -> Result<(), ()> {
    allocator().free(addr)
}

/// 分配器自检:验证分配/释放/合并/对齐,失败返回错误描述。
///
/// 由 kernel_main 在 irq_enable 之前调用(分配器尚未并发安全)。
pub fn self_test() -> Result<(), &'static str> {
    // 1) 整区(16MB)分配→释放→再分配:应复用同一区域。
    let a = alloc_pages(MAX_ORDER).ok_or("alloc(12) failed")?;
    free_pages(a).map_err(|_| "free(12) failed")?;
    let b = alloc_pages(MAX_ORDER).ok_or("realloc(12) failed")?;
    if a != b {
        return Err("buddy reuse mismatch");
    }
    free_pages(b).map_err(|_| "free(12) #2 failed")?;

    // 2) 分裂与合并:两个 order-11 块应互为 buddy,释放后合并回 order-12。
    // 注意:buddy 关系用**页索引 XOR**,地址 XOR 在 base 非 2 的幂
    // 对齐时因进位失效(0x80210000 位 23 置位)—— 测试用减法判定。
    let c = alloc_pages(11).ok_or("alloc(11) #1 failed")?;
    let d = alloc_pages(11).ok_or("alloc(11) #2 failed")?;
    let half = (1usize << 11) * PAGE_SIZE;
    if d != c + half {
        return Err("order-11 blocks are not buddies");
    }
    free_pages(c).map_err(|_| "free c failed")?;
    free_pages(d).map_err(|_| "free d failed")?;
    let e = alloc_pages(MAX_ORDER).ok_or("merged alloc(12) failed")?;
    if e != c.min(d) {
        return Err("buddy merge failed");
    }
    free_pages(e).map_err(|_| "free e failed")?;

    // 3) 对齐:order-3 块须 32KB 绝对对齐(base 已对齐到最大块)。
    let f = alloc_pages(3).ok_or("alloc(3) failed")?;
    if !f.is_multiple_of(8 * PAGE_SIZE) {
        return Err("order-3 block misaligned");
    }
    free_pages(f).map_err(|_| "free f failed")?;

    // 4) 双重释放被拒绝。
    let g = alloc_pages(0).ok_or("alloc(0) failed")?;
    free_pages(g).map_err(|_| "free g failed")?;
    if free_pages(g).is_ok() {
        return Err("double-free accepted");
    }

    // 5) 块内页地址被拒绝(防空闲链表静默损坏,F1)。
    let h = alloc_pages(2).ok_or("alloc(2) failed")?;
    if free_pages(h + PAGE_SIZE).is_ok() {
        return Err("interior page free accepted");
    }
    free_pages(h).map_err(|_| "free h failed")?;

    Ok(())
}
