//! Sv39 页表与内核自身映射(M1.5)。
//!
//! # 设计
//! - **身份映射**:虚拟地址 == 物理地址,内核无需重定位;切换 satp
//!   后代码/数据/栈/MMIO 地址全部不变,是引导期最安全的过渡方式。
//! - **RAM**:128MB 用 2MB 超页映射(堆/栈区域 RW,supervisor-only,U=0);
//!   内核镜像区域按段拆分:代码 RX / 只读数据 R / 可写数据 RW。
//! - **UART MMIO**:4KB 映射(RW,无 X)。
//! - 页表页从 buddy 分配(order-0),逐级按需创建并清零。
//!
//! # 公开接口
//! - `init` / `self_test` —— 初始化与验证
//! - `kernel_root` —— 内核页表根目录物理地址(init 后有效)
//! - `satp` / `tlb_flush` —— 页表基址与 TLB 操作(arch 层契约)
//! - `unmap_4k` —— 取消单页映射(内含 TLB 刷新)
//! - `map_user_page` —— **M2 用户映射**:拒绝覆盖已有 PTE、路径置 U 位
//! - `create_user_root` / `switch_root` / `is_mapped` —— **M2 T1.5 每进程
//!   地址空间**:建用户根表(复制内核区)/ 切换 satp(相同则 no-op)/
//!   只读检查映射
//!
//! `map_4k`/`map_region_4k`/`map_super`/`map_kernel_region` 为私有,
//! 供 init 期与每进程根表建立内核区映射。
//!
//! # 架构隔离
//! 本模块是 RISC-V Sv39 的具体实现;x86_64 移植时在 arch 层提供
//! 等价的 mmu 接口(见 DESIGN.md 的 arch_mmu_* 契约与 ROADMAP 阶段 5)。

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::debug;
use crate::mem;

/// Sv39 PTE 标志位。
const PTE_V: u64 = 1 << 0; // 有效
const PTE_R: u64 = 1 << 1; // 可读
const PTE_W: u64 = 1 << 2; // 可写
const PTE_X: u64 = 1 << 3; // 可执行
const PTE_U: u64 = 1 << 4; // 用户可访问(M2 用户映射:叶子 PTE 置 U 位)
const PTE_A: u64 = 1 << 6; // 已访问
const PTE_D: u64 = 1 << 7; // 已脏
const PTE_PPN_SHIFT: u64 = 10;
/// PPN 44 位掩码。
const PTE_PPN_MASK: u64 = (1u64 << 44) - 1;

/// 2MB 超页。
const SUPER_PAGE: usize = 2 * 1024 * 1024;

/// satp 的 Sv39 模式位。
const SATP_MODE_SV39: usize = 8usize << 60;
/// satp 的 PPN 字段掩码(位 0-43,**44 位**;H1:此前误写为 36 位,
/// 大内存下会截断根表地址)。
const SATP_PPN_MASK: usize = 0xFFF_FFFF_FFFF;

/// 物理地址 → PTE 中的 PPN 字段。
#[inline]
fn ppn(paddr: usize) -> u64 {
    ((paddr >> 12) & PTE_PPN_MASK as usize) as u64
}

/// 构造一条 PTE。
#[inline]
fn pte(paddr: usize, flags: u64) -> u64 {
    (ppn(paddr) << PTE_PPN_SHIFT) | flags
}

/// 读 PTE(volatile:页表页可能被硬件并行访问,禁止编译器缓存)。
#[inline]
fn pte_read(table: *const u64, idx: usize) -> u64 {
    unsafe { core::ptr::read_volatile(table.add(idx)) }
}

/// 写 PTE。
#[inline]
fn pte_write(table: *const u64, idx: usize, value: u64) {
    unsafe { core::ptr::write_volatile(table.add(idx) as *mut u64, value) }
}

/// 确保 vaddr 的下一级表存在(不存在则从 buddy 分配并清零),
/// 返回子表地址。
///
/// # Safety
/// `parent` 必须是有效页表地址(身份映射下可安全解引用)。
/// 若该 PTE 已存在:须是**表指针**(V 置位且非叶子 R/W/X),
/// 否则返回 Err —— 叶子超页不能当作表继续下沉(H2:此前会把
/// 超页的数据当表写,产生静默损坏)。
unsafe fn ensure_table(parent: *const u64, idx: usize) -> Result<*const u64, ()> {
    let entry = pte_read(parent, idx);
    if entry & PTE_V != 0 {
        if entry & (PTE_R | PTE_W | PTE_X) != 0 {
            // 叶子(超页):不可继续下沉,调用方应 fail-loudly。
            return Err(());
        }
        let sub = (((entry >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as usize;
        Ok(sub as *const u64)
    } else {
        let page = mem::alloc_pages(0).ok_or(())?;
        // SAFETY:`page` 为 alloc_pages 原样返回(页对齐、可写)。
        unsafe { mem::zero_page(page) };
        pte_write(parent, idx, pte(page, PTE_V));
        Ok(page as *const u64)
    }
}

/// 映射 4KB 页(叶子在 L0)。
///
/// V3 审计 #5:此函数用于 init 期建立映射。post-boot 重映射时必须
/// 先 `unmap_4k`(会刷新 TLB)再 `map_4k`,避免陈旧 TLB 命中;
/// 且调用方须保证目标地址当前未映射有效 PTE(否则静默覆盖)。
/// M2 用户态映射前应将此函数收敛为"拒绝覆盖已有 PTE"的 API。
fn map_4k(root: usize, vaddr: usize, paddr: usize, flags: u64) -> Result<(), ()> {
    let l2 = (vaddr >> 30) & 0x1FF;
    let l1 = (vaddr >> 21) & 0x1FF;
    let l0 = (vaddr >> 12) & 0x1FF;
    let root_t = root as *const u64;
    let l1_t = unsafe { ensure_table(root_t, l2)? };
    let l0_t = unsafe { ensure_table(l1_t, l1)? };
    pte_write(l0_t, l0, pte(paddr, flags));
    Ok(())
}

// ===========================================================================
// ===========================================================================
// M2 前置:用户地址空间映射契约(路径表指针只用 V,叶子才带 U)。
// ===========================================================================

/// 确保下一级表存在(表指针 **仅 V 位,R/W/X/U/A/D 为 0**)。
///
/// RISC-V 规范(Sv39):中间级 PTE 是表指针,除 V 外其它位必须为 0;
/// 用户访问权限由**叶子** PTE 的 U 位决定。QEMU 8.2 严格实现此点:
/// 中间级条目带 U/A/D 位直接 `TRANSLATE_FAIL`(实测 scause=0xc,
/// 用户页看似有效却无法翻译)。此前误用 `PTE_V | PTE_U` 是 M2 T1
/// U 模式取指故障的根因。
///
/// # Safety
/// 同 `ensure_table`:parent 必须是有效页表地址。
unsafe fn ensure_table_user(parent: *const u64, idx: usize) -> Result<*const u64, ()> {
    let entry = pte_read(parent, idx);
    if entry & PTE_V != 0 {
        if entry & (PTE_R | PTE_W | PTE_X) != 0 {
            return Err(());
        }
        let sub = (((entry >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as usize;
        Ok(sub as *const u64)
    } else {
        let page = mem::alloc_pages_zeroed(0).ok_or(())?;
        // 表指针:仅 V 位(R/W/X/U/A/D 均为 0,见函数注释)。
        pte_write(parent, idx, pte(page, PTE_V));
        Ok(page as *const u64)
    }
}

/// 映射**用户可访问**的 4KB 页(M2 每进程地址空间用)。
///
/// 契约(mmu 模块头 V4 已注明):
/// - 若 vaddr 处已有有效 PTE(**拒绝覆盖**),返回 Err —— 防误覆盖内核
///   映射或其它用户映射;
/// - 中间表指针仅 V 位;用户访问权限由叶子 PTE 的 U 位授予;
/// - 调用方须保证 vaddr 在用户地址空间范围,且 paddr 已用
///   `mem::alloc_pages_zeroed`(或确认无需清零);
/// - 映射后立即冲刷 TLB(首次访问前无陈旧项,保险起见统一刷)。
///
/// M2 T1/T1.5 调用方:boot 冒烟测试(tests.rs)与未来用户地址空间建立。
pub fn map_user_page(root: usize, vaddr: usize, paddr: usize, flags: u64) -> Result<(), ()> {
    // 自审(挑剔视角):Sv39 用户地址空间上限 2^38。越界 vaddr 的
    // L2/L1/L0 索引会错位(高位省略),映射到错误区域 —— 必须拒绝。
    if vaddr >= 0x4000_0000_0000 {
        return Err(());
    }
    // paddr 必须页对齐,否则 ppn 掩掉低 12 位 → 映射错页。
    if !paddr.is_multiple_of(4096) {
        return Err(());
    }
    // S1(本轮安全加固):只允许映射分配器管理的物理页。内核镜像/
    // 固件/MMIO/FDT 保留区一旦标 U 位映射,用户态即可读写内核数据
    // —— 一次调用方失误即内核完全失守。分配器区内的堆/页表页
    // 与用户页同区,属 T2 页所有权问题,此处先封住非分配器区域。
    if !crate::mem::page_in_range(paddr) {
        return Err(());
    }
    let l2 = (vaddr >> 30) & 0x1FF;
    let l1 = (vaddr >> 21) & 0x1FF;
    let l0 = (vaddr >> 12) & 0x1FF;
    let root_t = root as *const u64;
    let l1_t = unsafe { ensure_table_user(root_t, l2)? };
    let l0_t = unsafe { ensure_table_user(l1_t, l1)? };
    // 拒绝覆盖:目标 L0 PTE 必须为空。
    if pte_read(l0_t, l0) & PTE_V != 0 {
        return Err(());
    }
    // M2 用户映射默认叶子需 U 位;调用方可传已含 PTE_U 的 flags。
    pte_write(l0_t, l0, pte(paddr, flags | PTE_U));
    // TLB 冲刷:映射后立即可见(首次访问前无陈旧项,防护用途)。
    unsafe {
        asm!("sfence.vma zero, zero", options(nostack));
    }
    Ok(())
}

/// 映射 2MB 超页(叶子在 L1)。
fn map_super(root: usize, vaddr: usize, paddr: usize, flags: u64) -> Result<(), ()> {
    let l2 = (vaddr >> 30) & 0x1FF;
    let l1 = (vaddr >> 21) & 0x1FF;
    let root_t = root as *const u64;
    let l1_t = unsafe { ensure_table(root_t, l2)? };
    pte_write(l1_t, l1, pte(paddr, flags));
    Ok(())
}

/// 读取 satp 当前值。
pub fn satp() -> usize {
    let v: usize;
    unsafe {
        asm!("csrr {}, satp", out(reg) v, options(nomem, nostack));
    }
    v
}

/// 启用分页:写 satp(Sv39 + 根表 PPN)+ TLB 冲刷。
///
/// 身份映射保证此指令后的取指/访存地址不变;`sfence.vma` 在切换后
/// 立即执行(必须,否则可能使用切换前的 TLB 状态)。
fn enable(root_paddr: usize) {
    unsafe {
        asm!(
            "csrw satp, {satp}",
            satp = in(reg) SATP_MODE_SV39 | (root_paddr >> 12),
            options(nostack)
        );
        asm!("sfence.vma zero, zero", options(nostack));
    }
}

/// 叶子 PTE 标志:RWX + A/D(H2)。
///
/// A/D 位必须在建页时置位:硬件管理 A/D 的实现(QEMU)会自动更新,
/// 置位无害;软件管理 A/D 的核(部分真机)在 A/D 为 0 时首次访问
/// 会触发页故障,而我们的异常路径是停机 —— 建页即置位兼容两种
/// 模型,消除真机 boot 期故障。
const PTE_LEAF_RW: u64 = PTE_V | PTE_R | PTE_W | PTE_A | PTE_D;
/// D2:代码段 RX,只读数据段 R。
const PTE_LEAF_RX: u64 = PTE_V | PTE_R | PTE_X | PTE_A | PTE_D;
const PTE_LEAF_R: u64 = PTE_V | PTE_R | PTE_A | PTE_D;

// 链接脚本符号:内核镜像段边界(4KB 对齐)。
extern "C" {
    static _kernel_start: u8;
    static _text_start: u8;
    static _rodata_start: u8;
    static _data_start: u8;
    static _kernel_end: u8;
    // 栈守护页(4KB,MMU 不映射)。
    static _stack_guard: u8;
    static _stack_bottom: u8;
    static _stack_top: u8;
    // D7 per-hart 数组边界(陷阱栈 / idle 栈;基址 32K 对齐)。
    static _trap_stack_base: u8;
    static _trap_stack_top: u8;
    static _idle_stack_base: u8;
    static _idle_stack_top: u8;
}

/// 内核页表根目录物理地址(M2 T1:用户映射/进程表用它作参照)。
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

/// 内核页表根目录物理地址(init 后有效)。
pub fn kernel_root() -> usize {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// 初始化内核自身映射并启用分页。
///
/// 调用顺序:必须在 `mem::init` 之后(页表页来自 buddy)、
/// `irq_enable` 之前(中断路径依赖映射)。
pub fn init() {
    let root = mem::alloc_pages(0).expect("root page table allocation failed");
    // M2 T1:记录内核根目录,供用户映射(M2 API)与进程表参照。
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    unsafe { mem::zero_page(root) };

    // 内核区映射(M2 T1.5 抽为独立函数,init 与每进程根表共用)。
    map_kernel_region(root);
    enable(root);
    debug!("satp switched to Sv39, root={:#x}", root);
}

/// 在给定根表上建立**内核驻留区**映射(固件/镜像/堆栈/MMIO/UART)。
///
/// `init` 用它在内核根表建立身份映射;每进程根表(M2 T1.5)同样调用
/// 它复制内核区(S 权限,U=0),实现 M2-DESIGN §3.2 的"内核驻留区
/// 共享、用户区页级隔离"。调用方须保证 `root` 已清零。
fn map_kernel_region(root: usize) {
    let ram_start = crate::board::ram_start();
    let ram_end = crate::board::ram_end();
    assert!(
        ram_start.is_multiple_of(SUPER_PAGE),
        "RAM start {:#x} not 2MB-aligned",
        ram_start
    );
    assert!(
        ram_end.is_multiple_of(SUPER_PAGE),
        "RAM end {:#x} not 2MB-aligned",
        ram_end
    );

    let kernel_start = (&raw const _kernel_start).addr();
    let text_start = (&raw const _text_start).addr();
    let rodata_start = (&raw const _rodata_start).addr();
    let data_start = (&raw const _data_start).addr();
    let kernel_end = (&raw const _kernel_end).addr();

    // 1) 内核镜像前的区域(OpenSBI 固件):2MB 超页 **只读 R**(无 W/X)。
    // 自审加深:内核不写固件区(副核停车在自设 park 循环,M2 前无
    // 共享 mailbox)。映射为 R(而非 RW)使误写触发页故障(fail-loudly),
    // 而非静默破坏固件。L17 参考:PMP 亦可能禁止 S 写。
    let mut vaddr = ram_start;
    while vaddr < kernel_start {
        map_super(root, vaddr, vaddr, PTE_LEAF_R).expect("RAM superpage failed");
        vaddr += SUPER_PAGE;
    }

    // 2) 内核镜像:4KB 页,每段按权限映射。
    map_region_4k(root, kernel_start, text_start, PTE_LEAF_RW);
    map_region_4k(root, text_start, rodata_start, PTE_LEAF_RX);
    map_region_4k(root, rodata_start, data_start, PTE_LEAF_R);
    map_region_4k(root, data_start, kernel_end, PTE_LEAF_RW);

    // 3) 内核镜像所在超页的剩余部分(若有):4KB RW。
    let kernel_super_end = kernel_end.next_multiple_of(SUPER_PAGE);
    if kernel_end < kernel_super_end {
        map_region_4k(root, kernel_end, kernel_super_end, PTE_LEAF_RW);
    }

    // 4) 内核镜像后的 RAM:2MB 超页 RW,无 X。
    //    栈区域已在 4KB 映射中覆盖,guard 页被下方 unmap 移除。
    let mut vaddr = kernel_super_end;
    while vaddr < ram_end {
        map_super(root, vaddr, vaddr, PTE_LEAF_RW).expect("RAM superpage failed");
        vaddr += SUPER_PAGE;
    }

    // 5) 栈守护页:unmap 4KB 页,使栈溢出触发页故障(而非静默损坏)。
    //    引导栈守护页单页;陷阱/idle 数组(D7)每槽前 16K 守护区
    //    逐 4KB 页 unmap(stride 32K,守护跨 4 页)。
    let stack_guard = (&raw const _stack_guard).addr();
    // H1(审计 18 轮外部):断言 unmap 成功,若栈跨越 2MB 边界导致
    // ensure_table 下沉超页失败,则 panic 而非静默忽略。
    assert!(
        unmap_4k(root, stack_guard).is_ok(),
        "failed to unmap stack guard page at {:#x} (crosses 2MB boundary?)",
        stack_guard
    );
    // D7:per-hart 数组守护区。守卫区位于 [kernel_end, kernel_super_end)
    // 的 4KB 映射尾区内(arrays 先于 _alloc_start,紧随 kernel_end),
    // 逐页 ensure_table 不会命中超页叶子。
    let trap_base = (&raw const _trap_stack_base).addr();
    let trap_top = (&raw const _trap_stack_top).addr();
    unmap_guard_pages(root, "trap stack", trap_base, trap_top);
    let idle_base = (&raw const _idle_stack_base).addr();
    let idle_top = (&raw const _idle_stack_top).addr();
    unmap_guard_pages(root, "idle stack", idle_base, idle_top);

    // UART MMIO:4KB RW + A/D(无 X)。
    let uart_base = crate::board::uart_base();
    map_4k(root, uart_base, uart_base, PTE_LEAF_RW).expect("UART MMIO mapping failed");
}

/// D7:unmap per-hart 数组每槽前 16K 守护区(每槽 4 个 4KB 页)。
///
/// 槽布局(linker.ld):`[槽底, 槽底+16K)` = 守护页(MMU 不映射,越界触发
/// 页故障),`[槽底+16K, 槽底+32K)` = 栈区。仅 unmap 守护区,栈区保留。
/// 调用方保证 `[base, top)` 位于 [kernel_end, kernel_super_end) 的 4KB
/// 尾区(数组先于 _alloc_start,紧随 kernel_end)—— 逐页 ensure_table
/// 不会命中超页叶子;任一页 unmap 失败即 panic(H1:结构性错误 fail-loudly)。
fn unmap_guard_pages(root: usize, name: &str, base: usize, top: usize) {
    const PAGE_SIZE: usize = 4 * 1024;
    let stride = crate::arch::TRAP_STRIDE;
    let guard = crate::arch::TRAP_GUARD;
    let mut slot = base;
    while slot < top {
        let mut page = slot;
        while page < slot + guard {
            assert!(
                unmap_4k(root, page).is_ok(),
                "failed to unmap {name} guard page at {page:#x} (crosses 2MB boundary?)"
            );
            page += PAGE_SIZE;
        }
        slot += stride;
    }
}

/// 创建**用户进程**的独立地址空间根表(M2 T1.5)。
///
/// - 分配 1 个清零页作根表;
/// - 复制内核驻留区映射(见 `map_kernel_region`,S 权限 U=0);
/// - **不写 satp**:根表在调度器切到该进程线程时经 `switch_root` 启用。
///
/// 返回根表物理地址,供 `map_user_page` 映射用户页与调度器切换。
/// 调用方(process::create)须在 irq_save 下调用(建表含分配/写页表)。
pub fn create_user_root() -> Result<usize, ()> {
    let root = mem::alloc_pages(0).ok_or(())?;
    unsafe { mem::zero_page(root) };
    map_kernel_region(root);
    Ok(root)
}

/// 销毁用户进程地址空间(M2 D12):回收进程自有页与各级页表。
///
/// 走查 Sv39 根表并释放**进程自有**页(仅 U=1 的 L0 叶子物理页 + 各级
/// 表页 + 根页;内核驻留区 U=0 叶子/超页归内核共享 —— 释放会破坏内核
/// 自身或其它进程,一律跳过)。
///
/// # 调用前提
/// - 当前 satp **不得**指向 `root`(调用方须已 `switch_root(kernel_root())`,
///   否则释放后取指/访存立即故障);
/// - 本进程持有的 Shm cap 已先行 revoke(共享页仍 U 映射时释放会
///   double-free)。
pub fn destroy_root(root: usize) {
    let root_t = root as *const u64;
    for l2 in 0..512 {
        let e2 = pte_read(root_t, l2);
        if e2 & PTE_V == 0 {
            continue;
        }
        if e2 & (PTE_R | PTE_W | PTE_X) != 0 {
            continue; // L2 叶子(1GB 超页)= 内核区,跳过
        }
        let l1_pa = (((e2 >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as usize;
        let l1_t = l1_pa as *const u64;
        for l1 in 0..512 {
            let e1 = pte_read(l1_t, l1);
            if e1 & PTE_V == 0 {
                continue;
            }
            if e1 & (PTE_R | PTE_W | PTE_X) != 0 {
                continue; // L1 叶子(2MB 超页)= 内核区,跳过
            }
            let l0_pa = (((e1 >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as usize;
            let l0_t = l0_pa as *const u64;
            for l0 in 0..512 {
                let e0 = pte_read(l0_t, l0);
                if e0 & PTE_V == 0 {
                    continue;
                }
                if e0 & PTE_U != 0 {
                    // 用户叶子页:进程自有(order-0),归还 buddy。
                    let pa = (((e0 >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as usize;
                    mem::free_pages(pa).expect("destroy_root: free user page");
                }
                // U=0 叶子 = 内核区 4KB 页,归内核共享,跳过。
            }
            mem::free_pages(l0_pa).expect("destroy_root: free L0 table");
        }
        mem::free_pages(l1_pa).expect("destroy_root: free L1 table");
    }
    mem::free_pages(root).expect("destroy_root: free root table");
}

/// 切换到指定根表(每进程地址空间切换用)。
///
/// 与当前 satp 相同则 **no-op**(零开销:频繁切换同进程线程时不无谓
/// 冲刷 TLB);不同则写 satp + 全量 `sfence.vma`。身份映射下内核区
/// 地址不变,切换根表不影响当前指令流/栈访问。
///
/// ISR 内可用:仅 CSR 读写 + sfence,无分配、无锁。
pub fn switch_root(root_paddr: usize) {
    let target = SATP_MODE_SV39 | (root_paddr >> 12);
    if satp() == target {
        return;
    }
    enable(root_paddr);
}

/// 只读检查 `vaddr` 在 `root` 下是否已映射(M2 T1.5 结构性校验用)。
///
/// 从根表逐级走查(L2→L1→L0),各级缺失/叶子早退,**不分配、不发
/// TLB 刷新**。中间级叶子(1GB/2MB 超页)视为已映射。
pub fn is_mapped(root: usize, vaddr: usize) -> bool {
    let l2 = (vaddr >> 30) & 0x1FF;
    let l1 = (vaddr >> 21) & 0x1FF;
    let l0 = (vaddr >> 12) & 0x1FF;
    let root_t = root as *const u64;
    let e2 = pte_read(root_t, l2);
    if e2 & PTE_V == 0 {
        return false;
    }
    if e2 & (PTE_R | PTE_W | PTE_X) != 0 {
        return true; // L2 叶子(1GB 超页)
    }
    let l1_t = (((e2 >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as *const u64;
    let e1 = pte_read(l1_t, l1);
    if e1 & PTE_V == 0 {
        return false;
    }
    if e1 & (PTE_R | PTE_W | PTE_X) != 0 {
        return true; // L1 叶子(2MB 超页)
    }
    let l0_t = (((e1 >> PTE_PPN_SHIFT) & PTE_PPN_MASK) << 12) as *const u64;
    pte_read(l0_t, l0) & PTE_V != 0
}

/// 映射 [start, end) 4KB 对齐区域,每页调用 map_4k。
fn map_region_4k(root: usize, start: usize, end: usize, flags: u64) {
    assert!(start.is_multiple_of(4096));
    assert!(end.is_multiple_of(4096));
    let mut vaddr = start;
    while vaddr < end {
        map_4k(root, vaddr, vaddr, flags).expect("4K page mapping failed");
        vaddr += 4096;
    }
}

/// 取消映射 4KB 页(写 PTE=0),使访问触发页故障。
/// 写 PTE 后立即冲刷 TLB,确保后续访问观察到新映射。
/// 仅对已用 4KB 页映射的区域有效(超页需先拆分)。
pub fn unmap_4k(root: usize, vaddr: usize) -> Result<(), ()> {
    let l2 = (vaddr >> 30) & 0x1FF;
    let l1 = (vaddr >> 21) & 0x1FF;
    let l0 = (vaddr >> 12) & 0x1FF;
    let root_t = root as *const u64;
    let l1_t = unsafe { ensure_table(root_t, l2)? };
    let l0_t = unsafe { ensure_table(l1_t, l1)? };
    pte_write(l0_t, l0, 0);
    // H2(审计 18 轮外部):unmap 后立即刷新 TLB,防后续访问命中陈旧映射。
    // P4(本轮性能):unmap 只影响该虚拟地址的映射,用单地址 sfence.vma
    // (rs1=vaddr,rs2=x0)取代全 TLB 冲刷(rs1=x0) —— 守卫页解映射等
    // 高频路径不再清空整条 TLB。
    unsafe {
        asm!("sfence.vma {}, zero", in(reg) vaddr, options(nostack));
    }
    Ok(())
}

/// 冲刷 TLB(全部)。
///
/// arch 层契约接口(DESIGN.md arch_mmu_*):`map_user_page`/`unmap_4k`
/// 用单地址 sfence,`switch_root` 切换时全量 sfence。T3c 共享页 revoke
/// 撤销双方映射后调用本函数作当前核 TLB 全量兜底(跨核 shootdown 以
/// "satp 切换全刷 + 本核 sfence"简化,M2 单页共享、revoke 即销毁页)。
pub fn tlb_flush() {
    unsafe {
        asm!("sfence.vma zero, zero", options(nostack));
    }
}

/// 分页自检:satp 模式、身份映射读写、分页后 buddy、根表结构。
pub fn self_test() -> Result<(), &'static str> {
    // 1) satp MODE == Sv39。
    let s = satp();
    if s >> 60 != 8 {
        return Err("satp mode is not Sv39");
    }
    // 2) 身份映射可访问:读 UART LSR(MMIO 映射验证)。
    //    注意:不能用 0x80000000 做写探针 —— 那是 OpenSBI 固件区,
    //    PMP 禁止 S 模式访问(实测 cause=7 访问故障,映射本身正确)。
    //    MED-13(审计 15 轮):基址走 board 函数,不做地址硬编码。
    let lsr_addr = (crate::board::uart_base() + 5) as *const u8;
    let lsr = unsafe { core::ptr::read_volatile(lsr_addr) };
    if lsr & 0x20 == 0 {
        return Err("UART identity mapping read failed");
    }
    // 3) 分页启用后 buddy 仍可用,且映射为 RW(身份映射下物理=虚拟)。
    let page = mem::alloc_pages(0).ok_or("page alloc after paging failed")?;
    unsafe {
        core::ptr::write_volatile(page as *mut u64, 0x1234_5678_9ABC_DEF0);
    }
    if unsafe { core::ptr::read_volatile(page as *const u64) } != 0x1234_5678_9ABC_DEF0 {
        return Err("post-paging allocation readback failed");
    }
    mem::free_pages(page).map_err(|_| "free after paging failed")?;
    // 4) 根表 L2[0] 有效(V 位)。
    let root = (s & SATP_PPN_MASK) << 12;
    let entry = pte_read(root as *const u64, 0);
    if entry & PTE_V == 0 {
        return Err("root L2[0] not valid");
    }
    Ok(())
}
