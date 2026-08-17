//! 板级平台常量(单文件集中,真机 bring-up 只需改这里 + FDT 解析)。
//!
//! 当前全部为 QEMU virt 假设(M4):M1.5 解析 FDT 后,
//! RAM 大小/UART 基址/定时器频率改由引导数据提供,本文件退化为
//! 默认值与 MMIO 辅助常量。

/// 物理内存范围(QEMU virt:128MB @ 0x8000_0000)。
pub const RAM_START: usize = 0x8000_0000;
pub const RAM_END: usize = RAM_START + 128 * 1024 * 1024;

/// UART NS16550 基址(QEMU virt)。
pub const UART_BASE: usize = 0x1000_0000;

/// mtimer 频率(HZ):QEMU virt = 10 MHz。
pub const TIMER_FREQ: usize = 10_000_000;

/// 定时器节拍间隔(10ms)= TIMER_FREQ / 100。
pub const TIMER_INTERVAL: usize = TIMER_FREQ / 100;

/// FDT 保守预留大小(解析前;解析后按实际大小)。
pub const FDT_RESERVE_SIZE: usize = 1024 * 1024;
