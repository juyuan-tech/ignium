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
//! - `satp` / `tlb_flush` —— 页表基址与 TLB 操作
//! - `map_4k` / `unmap_4k` —— 单页映射/取消映射
//! - `map_region_4k` —— 区域映射(4KB 粒度)
//!
//! # 架构隔离
//! 本模块是 RISC-V Sv39 的具体实现;x86_64 移植时在 arch 层提供
//! 等价的 mmu 接口(见 DESIGN.md 的 arch_mmu_* 契约与 ROADMAP 阶段 5)。

use core::arch::asm;

use crate::debug;
use crate::mem;

/// Sv39 PTE 标志位。
const PTE_V: u64 = 1 << 0; // 有效
const PTE_R: u64 = 1 << 1; // 可读
const PTE_W: u64 = 1 << 2; // 可写
const PTE_X: u64 = 1 << 3; // 可执行
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
    static _trap_stack_guard: u8;
    static _trap_stack_bottom: u8;
    static _trap_stack_top: u8;
}

/// 初始化内核自身映射并启用分页。
///
/// 调用顺序:必须在 `mem::init` 之后(页表页来自 buddy)、
/// `irq_enable` 之前(中断路径依赖映射)。
pub fn init() {
    let root = mem::alloc_pages(0).expect("root page table allocation failed");
    unsafe { mem::zero_page(root) };

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
    let stack_guard = (&raw const _stack_guard).addr();
    let trap_stack_guard = (&raw const _trap_stack_guard).addr();
    // H1(审计 18 轮外部):断言 unmap 成功,若栈跨越 2MB 边界导致
    // ensure_table 下沉超页失败,则 panic 而非静默忽略。
    assert!(
        unmap_4k(root, stack_guard).is_ok(),
        "failed to unmap stack guard page at {:#x} (crosses 2MB boundary?)",
        stack_guard
    );
    assert!(
        unmap_4k(root, trap_stack_guard).is_ok(),
        "failed to unmap trap stack guard page at {:#x} (crosses 2MB boundary?)",
        trap_stack_guard
    );

    // UART MMIO:4KB RW + A/D(无 X)。
    let uart_base = crate::board::uart_base();
    map_4k(root, uart_base, uart_base, PTE_LEAF_RW).expect("UART MMIO mapping failed");

    enable(root);
    debug!("satp switched to Sv39, root={:#x}", root);
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
    unsafe {
        asm!("sfence.vma zero, zero", options(nostack));
    }
    Ok(())
}

/// 冲刷 TLB(全部)。M2 用户态映射时使用。
#[allow(dead_code)]
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
