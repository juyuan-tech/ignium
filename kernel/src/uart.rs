//! NS16550 兼容 UART 驱动(QEMU virt 的串口控制器)。
//!
//! # 寄存器布局(8 位 MMIO,基址 0x1000_0000)
//! 偏移 0x00:THR(发送保持寄存器)/ DLL(分频低字节,DLAB=1 时)
//! 偏移 0x01:IER(中断使能)/ DLM(分频高字节,DLAB=1 时)
//! 偏移 0x02:FCR(FIFO 控制)  偏移 0x03:LCR(线路控制,含 DLAB 位 7)
//! 偏移 0x04:MCR(调制解调控制) 偏移 0x05:LSR(线路状态,位 5=THR 空)
//!
//! # 初始化要点(DLAB 陷阱)
//! LCR 位 7(DLAB)=1 时,偏移 0/1 变为分频寄存器 DLL/DLM。
//! 因此 **必须先置 DLAB 再写分频,清 DLAB 后再写 IER**;
//! 顺序写反会把波特率高字节写进 IER 或反之,真机上表现为乱码。
//!
//! # 健壮性
//! 发送采用**有界等待**:真机上 TX 挂死时宁可丢字符(计数器记录)
//! 也不让整个内核死锁 —— 调试输出必须永远可用。

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

const UART_BASE: usize = 0x1000_0000;
#[allow(clippy::identity_op)] // 显式写出偏移 0x00,与寄存器手册对应
const UART_DLL: usize = UART_BASE + 0x00;
const UART_DLM: usize = UART_BASE + 0x01;
const UART_IER: usize = UART_BASE + 0x01;
const UART_FCR: usize = UART_BASE + 0x02;
const UART_LCR: usize = UART_BASE + 0x03;
const UART_MCR: usize = UART_BASE + 0x04;
const UART_LSR: usize = UART_BASE + 0x05;

/// `fmt::Write` 实现,把格式化输出导向串口(日志系统与 println! 共用)。
pub struct Writer;

/// 因 TX 超时被丢弃的字符计数(panic dump 中可查,诊断硬件问题)。
static TX_DROPPED: AtomicU64 = AtomicU64::new(0);

/// 读 MMIO 寄存器(volatile,防编译器合并/缓存)。
#[inline]
fn read_u8(addr: usize) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

/// 写 MMIO 寄存器(volatile)。
#[inline]
fn write_u8(addr: usize, val: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, val) }
}

/// LSR 位 5 = THR 空(发送保持寄存器可写)。
#[inline]
fn is_transmit_empty() -> bool {
    read_u8(UART_LSR) & 0x20 != 0
}

/// 初始化串口为 8N1、115200、FIFO 开启、关中断。
///
/// 顺序敏感,见模块头"DLAB 陷阱"说明:
/// 1. LCR=0x80(DLAB=1)→ 写 DLL=0x0C、DLM=0x00(115200 分频)
/// 2. LCR=0x03(DLAB=0,8N1)→ 此时才能写 IER=0(关中断)
/// 3. FCR=0x07(开 FIFO 并清空)、MCR=0x03(RTS/DTR 置位)
pub fn init() {
    write_u8(UART_LCR, 0x80);
    write_u8(UART_DLL, 0x0C);
    write_u8(UART_DLM, 0x00);
    write_u8(UART_LCR, 0x03);
    write_u8(UART_IER, 0x00);
    write_u8(UART_FCR, 0x07);
    write_u8(UART_MCR, 0x03);
}

/// 被丢弃字符计数(panic dump 用)。
pub fn dropped() -> u64 {
    TX_DROPPED.load(Ordering::Relaxed)
}

/// TX 忙等上限:超过即放弃本次写并计数。
/// 选择依据:QEMU 与正常硬件下 THR 空几乎立即可见,0x10000 次轮询
/// 足以覆盖慢速控制器,又不会让内核在 TX 挂死时无限阻塞。
const TX_WAIT_LIMIT: u32 = 0x1_0000;

/// 输出单字符。TX 忙等有界:超时记录 `TX_DROPPED` 并丢弃。
pub fn putc(c: u8) {
    let mut spins = 0;
    while !is_transmit_empty() {
        spins += 1;
        if spins > TX_WAIT_LIMIT {
            TX_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    write_u8(UART_BASE, c);
}

/// 输出字符串;`\n` 自动补 `\r\n`(终端换行兼容)。
pub fn write_str(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

/// Writer 的 fmt::Write 实现(日志宏与 println! 的底层出口)。
impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

/// 原始控制台输出(不带级别/时间戳;日志请用 logger 宏)。
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::uart::Writer, $($arg)*);
    }};
}
