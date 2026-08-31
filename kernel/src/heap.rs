//! 内核堆:slab 分配器(小对象)+ buddy 整块页分配(大对象)。
//!
//! # 设计
//! - **slab 槽**:8 个 2 的幂档(16B ~ 2KB),每页首部一个 `SlabHeader`,
//!   空闲槽以首字 intrusive 链表挂在页头;槽对齐 = 档大小(2 的幂),
//!   满足 `align <= size` 的小对象对齐。
//! - **大对象**(> 2KB 或 `align > 档大小`):直接从 buddy 取整块页,
//!   块基址记录在返回指针前 8 字节;释放时回读基址归还 buddy。
//!   高对齐(`align > 页`)按"需 + align"过量分配,向上对齐。
//! - **判别**:`SLAB_PAGE_CLASS` 表(页 → 档,0xFF = 非 slab)。
//! - **同步**:IRQ 安全 SpinLock(MED-3,加锁关中断);持锁临界区
//!   不被定时器抢占(消除 convoy)。ISR 仍零分配(容量预留,
//!   见 sync.rs 约束与 DEFERRED D11)。
//! - **OOM**:分配内部 panic(`handle_alloc_error` 走稳定路径,
//!   无需 unstable 属性)。
//! - **slab 页归还(M2)**:全空的**非 head** slab 页在下次任一档
//!   grow 时被懒回收扫描摘链、复位判别表后归还 buddy(空页检测不
//!   依赖 used 计数,沿空闲槽链数到容量即空)。head 页保留作每档
//!   快复用缓存 —— 单槽 alloc→dealloc churn 恒复用 head,不触发
//!   取/还页,bench 快路径与引入前逐指令一致(数值不变)。

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::info;

use crate::mem;
use crate::sync::SpinLock;

/// slab 档大小(2 的幂)。
const SLAB_SIZES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// 非 slab 页标记。
const NOT_SLAB: u8 = 0xFF;
/// 每页 slab 头(页首 16 字节;槽自 page+size 起,16B 档时 header
/// 恰好占满首槽位,32B 档起留 16B 空档)。
struct SlabHeader {
    /// 同档 slab 页链(head → next → …);空页懒回收时摘链。
    next: *mut SlabHeader,
    /// 空闲槽链表头(槽内首字存 next 地址;哨兵 = usize::MAX)。
    free_list: usize,
}

/// 单档状态:slab 页链头。
#[derive(Clone, Copy)]
struct SlabClass {
    head: *mut SlabHeader,
}

impl SlabClass {
    const fn empty() -> Self {
        SlabClass {
            head: core::ptr::null_mut(),
        }
    }
}

/// 页 → 档 判别表(32KB):0xFF = 非 slab 页。
/// static mut:仅在 slab 页创建时写入一次(引导期,单线程),
/// 之后只读;访问一律经裸指针。
static mut SLAB_PAGE_CLASS: [u8; mem::MAX_PAGES] = [NOT_SLAB; mem::MAX_PAGES];

/// 分配区基址缓存(性能优化):`mem::base()` 每次经分配器锁,
/// 堆快路径(每次 alloc/dealloc)不承受该开销;init 时缓存一次。
static ALLOC_BASE: AtomicUsize = AtomicUsize::new(0);
/// 分配区可分配页数缓存(*PAGE_SIZE):`mem::page_count()` 每次经分配器锁,
/// dealloc 的界检查走本缓存,避免每释放一次就重入一次分配器锁。
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

/// 初始化堆(缓存基址;须在 mem::init 之后、任何堆操作之前)。
pub fn init() {
    ALLOC_BASE.store(mem::base(), Ordering::Relaxed);
    ALLOC_BYTES.store(mem::page_count() * mem::PAGE_SIZE, Ordering::Relaxed);
}

#[inline]
fn heap_base() -> usize {
    ALLOC_BASE.load(Ordering::Relaxed)
}

/// 分配区可写字节数(缓存,界检查用)。
#[inline]
fn heap_bytes() -> usize {
    ALLOC_BYTES.load(Ordering::Relaxed)
}

fn slab_class_of(page: usize) -> u8 {
    let page_idx = (page - heap_base()) / mem::PAGE_SIZE;
    unsafe {
        (&raw const SLAB_PAGE_CLASS)
            .cast::<u8>()
            .add(page_idx)
            .read()
    }
}

fn slab_class_set(page: usize, class: u8) {
    let page_idx = (page - heap_base()) / mem::PAGE_SIZE;
    unsafe {
        (&raw const SLAB_PAGE_CLASS)
            .cast::<u8>()
            .add(page_idx)
            .cast_mut()
            .write(class)
    }
}

/// 内核堆(经 `HEAP` SpinLock 访问)。
pub struct KernelHeap {
    classes: [SlabClass; SLAB_SIZES.len()],
}

// 含裸指针(SlabHeader 链表):单上下文 + SpinLock 互斥下安全
// (供 SpinLock<T: Send> 的 Sync 约束)。
unsafe impl Send for KernelHeap {}

impl KernelHeap {
    const fn new() -> Self {
        KernelHeap {
            classes: [SlabClass::empty(); SLAB_SIZES.len()],
        }
    }

    /// 尺寸 → 档索引(不足 8B 按 8B;超过最大档 → None)。
    fn class_index(size: usize) -> Option<usize> {
        let size = size.max(8);
        for (i, &s) in SLAB_SIZES.iter().enumerate() {
            if size <= s {
                return Some(i);
            }
        }
        None
    }

    /// 分配入口。
    ///
    /// # Safety
    /// `layout` 必须合法;本函数 OOM 时 panic(永不返回 null)。
    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 {
            // 零尺寸:返回合法对齐悬挂指针(不占内存)。
            return layout.align() as *mut u8;
        }
        if let Some(idx) = Self::class_index(size) {
            if layout.align() <= SLAB_SIZES[idx] {
                return unsafe { self.slab_alloc(idx) };
            }
        }
        unsafe { self.page_alloc(layout) }
    }

    /// 从 slab 档分配一个槽(页满则新开一页)。
    /// HIGH-2(审计 17 轮):**遍历页链**复用任何有空闲槽的页
    /// (旧实现只看 head 页,非 head 页释放的槽永不分配)。
    unsafe fn slab_alloc(&mut self, idx: usize) -> *mut u8 {
        // 遍历页链,取第一个有空闲槽的页。
        let mut page = self.classes[idx].head;
        while !page.is_null() {
            let header = unsafe { &mut *page };
            if header.free_list != usize::MAX {
                let slot = header.free_list;
                header.free_list = unsafe { *(slot as *const usize) };
                return slot as *mut u8;
            }
            page = header.next;
        }
        // 所有页已满:新建一页并链入。
        let nh = unsafe { self.new_slab(idx) };
        let nhdr = unsafe { &mut *nh };
        nhdr.next = self.classes[idx].head;
        self.classes[idx].head = nh;
        // CRITICAL-1(审计 13 轮):必须**弹出**新页的首个槽 ——
        // 直接返回会让新页 free_list 仍指向它,同一槽被分配两次。
        let slot = nhdr.free_list;
        nhdr.free_list = unsafe { *(slot as *const usize) };
        slot as *mut u8
    }

    /// 新建一页 slab 并构建空闲槽链表,返回页头。
    unsafe fn new_slab(&mut self, idx: usize) -> *mut SlabHeader {
        // M2 懒回收:grow 前扫描全部档,把"全空且非 head"的 slab 页
        // 摘链归还 buddy(冷路径;head 页保留,热路径逐指令不变)。
        unsafe { self.sweep_empty_pages() };
        let page = mem::alloc_pages(0).expect("kernel heap: slab page OOM");
        let size = SLAB_SIZES[idx];
        // 槽从 page+size 开始(页首 size 字节被 header 占用;
        // 16B 档时 header 恰好占满,32B 档起留 16B 空档)。
        let mut prev: *mut usize = core::ptr::null_mut();
        let mut slot: *mut usize = (page + size) as *mut usize;
        while slot as usize + size <= page + mem::PAGE_SIZE {
            if !prev.is_null() {
                unsafe { *prev = slot as usize };
            }
            prev = slot;
            slot = (slot as usize + size) as *mut usize;
        }
        if !prev.is_null() {
            unsafe { *prev = usize::MAX };
        }
        let header = page as *mut SlabHeader;
        unsafe {
            *header = SlabHeader {
                next: core::ptr::null_mut(),
                free_list: page + size,
            };
        }
        // 登记判别表。
        slab_class_set(page, idx as u8);
        header
    }

    /// 懒回收:扫描全部档,把"全空且非 head"的 slab 页摘链归还 buddy。
    ///
    /// 在 `new_slab`(任一档 grow)前调用 —— 冷路径;把归还机制移出
    /// dealloc 热路径,单槽 alloc→dealloc churn 逐指令等价引入前
    /// (bench 数值不变)。head 页恒保留作每档快复用缓存。
    ///
    /// 空页检测不依赖计数器:沿空闲槽链数到**页容量**即空(链尾哨兵
    /// usize::MAX)。摘链先于归还,链上永不残留已释放页;归还前先复位
    /// 判别表,防该页被 buddy 复用后仍被误判为 slab 页。
    ///
    /// #[inline(never)]:fat-LTO 若把它内联进 new_slab → slab_alloc,
    /// 热路径 alloc 的寄存器压力暴涨(实测 9→16 个被调用方保存寄存器,
    /// bench +10%)。保持冷路径分离,热路径逐指令不变。
    #[inline(never)]
    unsafe fn sweep_empty_pages(&mut self) {
        for idx in 0..SLAB_SIZES.len() {
            let head = self.classes[idx].head;
            if head.is_null() {
                continue;
            }
            let mut prev = head;
            let mut page = unsafe { (*head).next };
            while !page.is_null() {
                let next = unsafe { (*page).next };
                if self.slab_page_empty(page, idx) {
                    // 摘链(跳过本页)后复位判别表、归还 buddy。
                    unsafe { (*prev).next = next };
                    slab_class_set(page as usize, NOT_SLAB);
                    mem::free_pages(page as usize).expect("kernel heap: invalid slab sweep free");
                } else {
                    prev = page;
                }
                page = next;
            }
        }
    }

    /// 页是否已全空:空闲槽链长 == 页容量(槽数)。
    ///
    /// 页满时 free_list 为哨兵(链长 0);`n > capacity` 为防御性早退,
    /// 防链表损坏导致的死循环。
    fn slab_page_empty(&self, page: *const SlabHeader, idx: usize) -> bool {
        let size = SLAB_SIZES[idx];
        let capacity = (mem::PAGE_SIZE - size) / size;
        let mut slot = unsafe { (*page).free_list };
        let mut n = 0usize;
        while slot != usize::MAX {
            n += 1;
            if n > capacity {
                return false;
            }
            slot = unsafe { *(slot as *const usize) };
        }
        n == capacity
    }

    /// 大对象:buddy 整块页,基址记录在返回指针前 8 字节。
    unsafe fn page_alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        // CRITICAL-4:对 **所有** align > 8 的情况统一过量分配
        // (align-1 对齐余量),返回指针对齐到 align。
        // MEDIUM-2(审计 14 轮):恶意/极端 Layout 可能使加法溢出
        // 回绕成小值 → 计算出的 order 过小 → 欠分配 → 堆破坏。
        // 用 checked 算术,溢出即 panic(与 order 超限一致)。
        let needed = match size.checked_add(8).and_then(|v| v.checked_add(align - 1)) {
            Some(v) => v,
            None => panic!("kernel heap: allocation size overflow"),
        };
        let pages = needed.div_ceil(mem::PAGE_SIZE);
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        // MEDIUM-3:所需超过最大块(16MB)时明确失败,而非静默截断。
        if order > mem::MAX_ORDER {
            panic!("kernel heap: allocation too large ({size} bytes)");
        }
        let block = match mem::alloc_pages(order) {
            Some(b) => b,
            None => panic!(
                "kernel heap: page OOM (order={order}, size={size}, free_pages={})",
                mem::page_count()
            ),
        };
        // 统一:块内对齐,基址记录在对齐指针前 8 字节。
        let aligned = (block + 8).div_ceil(align) * align;
        unsafe { *((aligned - 8) as *mut usize) = block };
        aligned as *mut u8
    }

    /// 释放入口。
    ///
    /// # Safety
    /// `ptr` 必须来自本分配器同 layout 的分配;零尺寸 → no-op。
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let page = (ptr as usize) & !(mem::PAGE_SIZE - 1);
        // MEDIUM-2 + MED-9(审计 16 轮):界检查收窄到**真实可分配
        // 页数** —— 过宽的 MAX_PAGES 检查会让 padding/未分配区的
        // 指针进入大对象路径读 ptr-8(越界)。
        let base = heap_base();
        if page < base || page - base >= heap_bytes() {
            panic!(
                "kernel heap: invalid pointer {:#x} (outside allocatable region)",
                ptr as usize
            );
        }
        let class = slab_class_of(page);
        if class != NOT_SLAB {
            // H6(审计 18 轮外部):校验指针是否槽对齐且在页内。
            let class_size = SLAB_SIZES[class as usize];
            let offset = (ptr as usize) - page;
            if offset < class_size
                || !offset.is_multiple_of(class_size)
                || offset + class_size > mem::PAGE_SIZE
            {
                panic!(
                    "kernel heap: invalid slab pointer {:#x} (page={:#x}, class={}, offset={})",
                    ptr as usize, page, class, offset
                );
            }
            // slab 槽:压回页头空闲链表(空页归还由 grow 前懒回收负责,
            // dealloc 热路径无计数器、无分支)。
            let header = page as *mut SlabHeader;
            unsafe {
                *(ptr as *mut usize) = (*header).free_list;
                (*header).free_list = ptr as usize;
            }
        } else {
            // 大对象:回读块基址(返回指针前 8 字节)归还 buddy。
            // 注意:ptr 是 *mut u8,`ptr.sub(1)` 只减 1 字节 ——
            // 必须按地址-8 计算(bring-up 实测抓到的指针算术 bug)。
            let base = unsafe { *((ptr as usize - 8) as *const usize) };
            mem::free_pages(base).expect("kernel heap: invalid free");
        }
    }
}

/// 全局分配器(经 `#[global_allocator]` 供 Vec/Box 等使用)。
pub struct KernelAllocator;

// HEAP 的 SpinLock 约束见 sync.rs:IRQ 安全锁(MED-3,审计 17 轮);
// ISR 仍零分配(容量预留)。
static HEAP: SpinLock<KernelHeap> = SpinLock::new(KernelHeap::new());

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { HEAP.lock().alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { HEAP.lock().dealloc(ptr, layout) }
    }
}

#[global_allocator]
pub static ALLOCATOR: KernelAllocator = KernelAllocator;

/// 堆吞吐基线:64B 档分配+释放各 100k 次(含堆锁开销)。
pub fn bench() {
    let layout = Layout::from_size_align(64, 8).unwrap();
    let t0 = crate::arch::get_time();
    for _ in 0..100_000 {
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            panic!("bench: alloc oom");
        }
        // M2 自审:black_box 防止编译器消除 alloc→dealloc 对(LTO 后
        // LLVM 会把无逃逸的 malloc/free 对整体消除,基准虚报 0 ns/op)。
        core::hint::black_box(p);
        unsafe { alloc::alloc::dealloc(p, layout) };
    }
    let dt = crate::arch::get_time().wrapping_sub(t0);
    // V4(外部审计 LOW):用运行时 timebase 频率换算 ns,不硬编码 10MHz。
    let freq = crate::board::timer_freq();
    let ns_per_tick = 1_000_000_000u64 / freq as u64;
    let ns_per_op = (dt as u64).saturating_mul(ns_per_tick) / 100_000;
    info!("bench: slab 64B alloc+dealloc ≈ {ns_per_op} ns/op");
}

/// 堆自检:Vec/Box/大对象/高对齐 四路径。
pub fn self_test() -> Result<(), &'static str> {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    // 1) Vec(slab 路径,多槽分配/释放)。
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1000 {
        v.push(i as u64);
    }
    if v.len() != 1000 {
        return Err("vec len");
    }
    let sum: u64 = v.iter().sum();
    if sum != (0..1000).sum::<u64>() {
        return Err("vec sum");
    }
    drop(v);

    // 2) Box(单对象,64B 档)。
    let b: Box<[u8; 64]> = Box::new([0xABu8; 64]);
    if b[63] != 0xAB {
        return Err("box");
    }
    drop(b);

    // 3) 大对象(4KB > 阈值,页路径)。
    let big: Box<[u8; 4096]> = Box::new([0xCDu8; 4096]);
    if big[4095] != 0xCD {
        return Err("big box");
    }
    drop(big);

    // 4) 高对齐(align = 8KB > 页,过量分配 + 对齐路径)。
    let layout = Layout::from_size_align(64, 8192).map_err(|_| "bad layout")?;
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() || !(p as usize).is_multiple_of(8192) {
        return Err("high-align alloc");
    }
    unsafe { alloc::alloc::dealloc(p, layout) };

    // 5) 重复分配/释放(slab 复用,防链表损坏)。
    for _ in 0..100 {
        let x: Box<u32> = Box::new(7);
        drop(x);
    }

    // 6) HIGH-2(审计 17 轮)回归:slab 多页复用 —— 填满 ≥2 页后
    //    释放**旧页**(非 head)的槽,再分配必须遍历页链复用它
    //    (页数不增长;旧实现只查 head 页,新开一页 → 泄漏)。
    let idx = 1; // 32B 档(此前各步骤未触碰该档)
    let size = SLAB_SIZES[idx];
    let slots = (mem::PAGE_SIZE - size) / size; // 页内槽数
    let layout = Layout::from_size_align(size, 8).map_err(|_| "bad layout")?;
    let mut ptrs: Vec<*mut u8> = Vec::new();
    // 填满两页(2×slots):page1(旧页)与 page2(head 页)全满。
    for _ in 0..2 * slots {
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Err("slab multi-page alloc");
        }
        ptrs.push(p);
    }
    // 释放旧页(page1)首槽:此时唯一可复用的空闲槽在非 head 页。
    unsafe { alloc::alloc::dealloc(ptrs[0], layout) };
    let page_old = (ptrs[0] as usize) & !(mem::PAGE_SIZE - 1);
    // 再分配:必须遍历页链复用旧页槽(地址落回旧页),不得新建页。
    let extra = unsafe { alloc::alloc::alloc(layout) };
    if extra.is_null() {
        return Err("slab reuse alloc");
    }
    let page_new = (extra as usize) & !(mem::PAGE_SIZE - 1);
    if page_old != page_new {
        return Err("slab multi-page reuse failed (new page allocated)");
    }
    // 清理。
    for &p in ptrs.iter().skip(1) {
        unsafe { alloc::alloc::dealloc(p, layout) };
    }
    unsafe { alloc::alloc::dealloc(extra, layout) };
    drop(ptrs);

    // 7) M2:slab 空页懒回收 —— 全空**非 head** 页在下次任一档 grow
    //    时归还 buddy(grow 前扫描,冷路径;head 页保留,热路径逐指令
    //    不变,等价 bench 单槽 churn 不触发取/还页)。
    {
        // 用全新档避免跨用例耦合:256B 档 churn 出 3 个空页(1 head +
        // 2 非 head),再以 1024B 档 grow 触发扫描。指针缓冲用
        // with_capacity 一次性落在 512B 档(远离 256B,不干扰 churn)。
        const SIZE: usize = 256;
        let cap = (mem::PAGE_SIZE - SIZE) / SIZE; // 15 槽/页
        let layout = Layout::from_size_align(SIZE, 8).map_err(|_| "bad layout")?;
        let mut ptrs: Vec<*mut u8> = Vec::with_capacity(3 * cap);
        // 填满 3 页(峰值:head + 2 非 head)。
        for _ in 0..3 * cap {
            let p = unsafe { alloc::alloc::alloc(layout) };
            if p.is_null() {
                return Err("slab return alloc");
            }
            ptrs.push(p);
        }
        // 全部释放 → 3 个空页(dealloc 热路径无回收,页仍被链持有)。
        for &p in ptrs.iter() {
            unsafe { alloc::alloc::dealloc(p, layout) };
        }
        drop(ptrs);
        let mid = mem::free_page_count();
        // 触发**另一档** grow 以运行懒回收:逐步持有 1024B 对象,直到
        // free 计数变化 —— 即新增了页(new_slab 已执行,扫描已跑)。
        // 不预知该档既有空槽数(步骤 1 的 Vec 缓冲曾在该档留页);首次
        // 计数变化即停止,故至多新保留 1 个 grow 页。指针用栈数组持有,
        // 避免循环内堆分配扰动被测档。
        let grow = Layout::from_size_align(1024, 8).map_err(|_| "bad layout")?;
        let mut grown: [*mut u8; 64] = [core::ptr::null_mut(); 64];
        let mut n = 0usize;
        loop {
            let p = unsafe { alloc::alloc::alloc(grow) };
            if p.is_null() {
                return Err("slab return grow");
            }
            grown[n] = p;
            n += 1;
            if mem::free_page_count() != mid {
                break; // 已 grow:new_slab 内懒回收已把空页归还 buddy
            }
            if n >= grown.len() {
                return Err("slab return grow bound"); // 防御:不会发生
            }
        }
        for &p in grown[..n].iter() {
            unsafe { alloc::alloc::dealloc(p, grow) };
        }
        let free_after = mem::free_page_count();
        // 归还(≥2 页)抵消 grow 新页(1)后仍有净增 → 空页确实回了 buddy。
        if free_after <= mid {
            return Err("slab empty page not returned to buddy");
        }
        // head 保留:单槽 alloc→dealloc(等价 bench 快路径)不触发取/还页
        // —— 若 head 被过早归还,下次 alloc 会重新取页 → free 计数 -1。
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Err("slab return realloc");
        }
        let steady = mem::free_page_count();
        unsafe { alloc::alloc::dealloc(p, layout) };
        if mem::free_page_count() != steady {
            return Err("slab head page prematurely reclaimed");
        }
    }

    Ok(())
}
