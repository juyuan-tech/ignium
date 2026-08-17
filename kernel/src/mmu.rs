//! Sv39 页表与内核自身映射(M1)。
//!
//! # 设计
//! - **身份映射**:虚拟地址 == 物理地址,内核无需重定位;切换 satp
//!   后代码/数据/栈/MMIO 地址全部不变,是引导期最安全的过渡方式。
//! - **RAM**:128MB 用 2MB 超页映射(RWX,supervisor-only,U=0);
//!   权限细分(代码 RX / 数据 RW)列入 M1.5。
//! - **UART MMIO**:4KB 映射(RW,无 X)。
//! - 页表页从 buddy 分配(order-0),逐级按需创建并清零。
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

/// RAM 区域(与 board.rs 保持一致,身份映射)。
const RAM_START: usize = crate::board::RAM_START;
const RAM_END: usize = crate::board::RAM_END;
/// UART MMIO 区域(身份映射;基址与 board.rs/uart.rs 一致,4KB 页)。
const UART_MMIO: usize = crate::board::UART_BASE;
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
const PTE_LEAF_RWX: u64 = PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D;
const PTE_LEAF_RW: u64 = PTE_V | PTE_R | PTE_W | PTE_A | PTE_D;

/// 初始化内核自身映射并启用分页。
///
/// 调用顺序:必须在 `mem::init` 之后(页表页来自 buddy)、
/// `irq_enable` 之前(中断路径依赖映射)。
pub fn init() {
    // 根表:buddy order-0 页(4KB 对齐,满足 PPN 要求)。
    let root = mem::alloc_pages(0).expect("root page table allocation failed");
    // SAFETY:`root` 为 alloc_pages 原样返回(页对齐、可写)。
    unsafe { mem::zero_page(root) };

    // RAM 身份映射:2MB 超页,RWX + A/D(supervisor-only,U=0)。
    // M1.5 细化:内核镜像按 代码 RX / 数据 RW 拆分权限。
    let mut vaddr = RAM_START;
    while vaddr < RAM_END {
        map_super(root, vaddr, vaddr, PTE_LEAF_RWX).expect("RAM superpage mapping failed");
        vaddr += SUPER_PAGE;
    }

    // UART MMIO 身份映射:4KB,RW + A/D(无 X;MMIO 不应执行代码)。
    map_4k(root, UART_MMIO, UART_MMIO, PTE_LEAF_RW).expect("UART MMIO mapping failed");

    enable(root);
    debug!("satp switched to Sv39, root={:#x}", root);
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
    //    MED-13(审计 15 轮):基址走 board 常量,不做地址硬编码。
    let lsr_addr = (crate::board::UART_BASE + 5) as *const u8;
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
