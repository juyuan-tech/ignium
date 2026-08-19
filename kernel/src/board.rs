//! 板级平台参数(M1.5:FDT 引导期数据,回退默认值)。
//!
//! # 设计
//! - `init_from_fdt` 在引导早期调用,解析 FDT 得到 RAM/UART/定时器参数。
//! - 其余代码通过 `ram_start()`/`ram_end()`/`uart_base()`/`timer_interval()`
//!   等函数访问,不直接引用 FDT。
//! - 无 FDT 或解析失败时回退 QEMU virt 默认值。

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::fdt;

/// 默认物理内存范围(QEMU virt:128MB @ 0x8000_0000)。
const DEFAULT_RAM_START: usize = 0x8000_0000;
const DEFAULT_RAM_SIZE: usize = 128 * 1024 * 1024;

/// 默认 UART 基址(QEMU virt)。
const DEFAULT_UART_BASE: usize = 0x1000_0000;

/// 默认 mtimer 频率(HZ):QEMU virt = 10 MHz。
const DEFAULT_TIMER_FREQ: usize = 10_000_000;

/// 板级运行时参数(原子存储)。
static BOARD_RAM_START: AtomicUsize = AtomicUsize::new(0);
static BOARD_RAM_SIZE: AtomicUsize = AtomicUsize::new(0);
static BOARD_UART_BASE: AtomicUsize = AtomicUsize::new(0);
static BOARD_TIMER_FREQ: AtomicUsize = AtomicUsize::new(0);

/// 2MB 超页对齐(用于 RAM 超页映射)。
const SUPER_PAGE: usize = 2 * 1024 * 1024;

/// 内核链接基址(QEMU virt,OpenSBI 跳转地址)。
const KERNEL_LINK_BASE: usize = 0x8020_0000;

/// Sv39 物理地址上限(2^39)。
const SV39_MAX_PHYS: usize = 1usize << 39;

// 链接脚本符号:内核镜像结束地址。
extern "C" {
    static _kernel_end: u8;
}

/// 从 FDT 解析结果初始化板级参数。须在 mem::init 之前调用。
///
/// 会对 FDT 值做基本合理性校验,异常值回退默认值。
pub fn init_from_fdt(params: &fdt::BoardParams) {
    // 自审修复:先确定候选 RAM 范围,再统一校验,避免用不同的
    // ram_start 分别校验 ram_size 与 uart(逻辑不一致)。
    // 1) 候选 ram_start:非零且 2MB 对齐。
    let cand_start = if params.ram_start != 0 && params.ram_start.is_multiple_of(SUPER_PAGE) {
        params.ram_start
    } else {
        DEFAULT_RAM_START
    };
    // 2) 候选 ram_size:1MB..MAX_PAGES,2MB 对齐。
    let cand_size = if params.ram_size >= 1024 * 1024
        && params.ram_size <= crate::mem::MAX_PAGES * 4096
        && params.ram_size.is_multiple_of(SUPER_PAGE)
    {
        params.ram_size
    } else {
        DEFAULT_RAM_SIZE
    };
    let cand_end = cand_start.saturating_add(cand_size);
    // 3) 校验最终 RAM 范围:
    //    - ram_end 2MB 对齐(超页映射要求)
    //    - ram_end 在 Sv39 物理窗口内
    //    - ram_start <= 内核链接基址(covers kernel)
    //    - ram_end 覆盖内核镜像末尾(分配器有页可用)
    let kernel_end = (&raw const _kernel_end).addr();
    let ram_valid = cand_end.is_multiple_of(SUPER_PAGE)
        && cand_end <= SV39_MAX_PHYS
        && cand_start <= KERNEL_LINK_BASE
        && cand_end > kernel_end;
    let (ram_start, ram_size) = if ram_valid {
        (cand_start, cand_size)
    } else {
        (DEFAULT_RAM_START, DEFAULT_RAM_SIZE)
    };
    let ram_end = ram_start.saturating_add(ram_size);
    let uart_base = if params.uart_base != 0
        && params.uart_base.is_multiple_of(4)
        // UART 基址不能在 RAM 区间内(hostile FDT 任意物理写防护)。
        // 用最终 ram_start/ram_end 校验。
        && (params.uart_base < ram_start || params.uart_base >= ram_end)
    {
        params.uart_base
    } else {
        DEFAULT_UART_BASE
    };
    let timer_freq = if params.timebase_freq >= 1000 && params.timebase_freq <= 1_000_000_000 {
        params.timebase_freq
    } else {
        DEFAULT_TIMER_FREQ
    };
    BOARD_RAM_START.store(ram_start, Ordering::Relaxed);
    BOARD_RAM_SIZE.store(ram_size, Ordering::Relaxed);
    BOARD_UART_BASE.store(uart_base, Ordering::Relaxed);
    BOARD_TIMER_FREQ.store(timer_freq, Ordering::Relaxed);
}

/// 物理内存起始地址。
#[inline]
pub fn ram_start() -> usize {
    let v = BOARD_RAM_START.load(Ordering::Relaxed);
    if v != 0 {
        v
    } else {
        DEFAULT_RAM_START
    }
}

/// 物理内存结束地址(不含)。
#[inline]
pub fn ram_end() -> usize {
    ram_start() + ram_size()
}

/// 物理内存大小(字节)。
#[inline]
pub fn ram_size() -> usize {
    let v = BOARD_RAM_SIZE.load(Ordering::Relaxed);
    if v != 0 {
        v
    } else {
        DEFAULT_RAM_SIZE
    }
}

/// UART 基址。
#[inline]
pub fn uart_base() -> usize {
    let v = BOARD_UART_BASE.load(Ordering::Relaxed);
    if v != 0 {
        v
    } else {
        DEFAULT_UART_BASE
    }
}

/// mtimer 频率(HZ)。
#[inline]
pub fn timer_freq() -> usize {
    let v = BOARD_TIMER_FREQ.load(Ordering::Relaxed);
    if v != 0 {
        v
    } else {
        DEFAULT_TIMER_FREQ
    }
}

/// 定时器节拍间隔(10ms)。
#[inline]
pub fn timer_interval() -> usize {
    timer_freq() / 100
}
