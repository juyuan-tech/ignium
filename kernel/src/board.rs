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

/// 从 FDT 解析结果初始化板级参数。须在 mem::init 之前调用。
///
/// 会对 FDT 值做基本合理性校验,异常值回退默认值。
pub fn init_from_fdt(params: &fdt::BoardParams) {
    // M2(审计 18 轮外部):RAM 边界必须 2MB 对齐,否则 mmu::init 超页映射会 panic。
    let ram_start = if params.ram_start != 0 && params.ram_start.is_multiple_of(SUPER_PAGE) {
        params.ram_start
    } else {
        DEFAULT_RAM_START
    };
    let ram_size = if params.ram_size >= 1024 * 1024
        && params.ram_size <= 1024 * 1024 * 1024
        && params.ram_size.is_multiple_of(SUPER_PAGE)
    {
        params.ram_size
    } else {
        DEFAULT_RAM_SIZE
    };
    let uart_base = if params.uart_base != 0
        && params.uart_base.is_multiple_of(4)
        && (params.uart_base < ram_start || params.uart_base >= ram_start.saturating_add(ram_size))
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
