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
//! - **同步**:SpinLock(仅主上下文;ISR 禁止,见 sync.rs 约束)。
//! - **OOM**:分配内部 panic(`handle_alloc_error` 走稳定路径,
//!   无需 unstable 属性)。
//! - slab 页释放策略:当前**永不归还** buddy(每档最多 1~2 页,
//!   M1 规模可接受;归还与页回收在 M2 引入)。

use core::alloc::{GlobalAlloc, Layout};

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

fn slab_class_of(page: usize) -> u8 {
    let page_idx = (page - mem::base()) / mem::PAGE_SIZE;
    unsafe {
        (&raw const SLAB_PAGE_CLASS)
            .cast::<u8>()
            .add(page_idx)
            .read()
    }
}

fn slab_class_set(page: usize, class: u8) {
    let page_idx = (page - mem::base()) / mem::PAGE_SIZE;
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
    unsafe fn slab_alloc(&mut self, idx: usize) -> *mut u8 {
        if self.classes[idx].head.is_null() {
            // 首次:新建页并挂为档头(此前曾丢弃返回值导致空指针访存)。
            let nh = unsafe { self.new_slab(idx) };
            self.classes[idx].head = nh;
        }
        let head = self.classes[idx].head;
        let header = unsafe { &mut *head };
        let slot = header.free_list;
        if slot == usize::MAX {
            // 当前页已满:新开一页并链入。
            let nh = unsafe { self.new_slab(idx) };
            let nhdr = unsafe { &mut *nh };
            nhdr.next = head;
            self.classes[idx].head = nh;
            return nhdr.free_list as *mut u8;
        }
        header.free_list = unsafe { *(slot as *const usize) };
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
        // 高对齐需在块内留出对齐余量(否则对齐后越界)。
        let needed = if align <= mem::PAGE_SIZE {
            size + 8
        } else {
            size + 8 + align
        };
        let pages = needed.div_ceil(mem::PAGE_SIZE);
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let block = mem::alloc_pages(order.min(mem::MAX_ORDER)).expect("kernel heap: page OOM");
        if align <= mem::PAGE_SIZE {
            unsafe { *(block as *mut usize) = block };
            (block + 8) as *mut u8
        } else {
            let aligned = (block + 8).div_ceil(align) * align;
            unsafe { *((aligned - 8) as *mut usize) = block };
            aligned as *mut u8
        }
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

// HEAP 的 SpinLock 约束见 sync.rs:仅主上下文(ISR 零分配)。
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

    Ok(())
}
