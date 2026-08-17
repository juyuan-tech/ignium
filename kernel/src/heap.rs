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
//! - slab 页释放策略:当前**永不归还** buddy(每档最多 1~2 页,
//!   M1 规模可接受;归还与页回收在 M2 引入)。

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::info;

use crate::mem;
use crate::sync::SpinLock;

/// slab 档大小(2 的幂)。
const SLAB_SIZES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// 非 slab 页标记。
const NOT_SLAB: u8 = 0xFF;
/// 每页 slab 头(页首 16 字节)。
struct SlabHeader {
    /// 同档 slab 页链(预留;M1 无页回收,仅维护)。
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

/// 初始化堆(缓存基址;须在 mem::init 之后、任何堆操作之前)。
pub fn init() {
    ALLOC_BASE.store(mem::base(), Ordering::Relaxed);
}

#[inline]
fn heap_base() -> usize {
    ALLOC_BASE.load(Ordering::Relaxed)
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
        if page < base || page - base >= mem::page_count() * mem::PAGE_SIZE {
            panic!(
                "kernel heap: invalid pointer {:#x} (outside allocatable region)",
                ptr as usize
            );
        }
        let class = slab_class_of(page);
        if class != NOT_SLAB {
            // slab 槽:压回页头空闲链表。
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
        unsafe { alloc::alloc::dealloc(p, layout) };
    }
    let dt = crate::arch::get_time().wrapping_sub(t0);
    let ns_per_op = dt.saturating_mul(100) / 100_000;
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

    Ok(())
}
