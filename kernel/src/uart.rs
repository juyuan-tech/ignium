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
//! # MMIO 定序(fence)
//! `volatile` 只阻止编译器重排,**不产生硬件定序**;乱序 RISC-V 核
//! 可能重排对不同 MMIO 地址的写入,破坏 DLAB 时序。关键写之间
//! 插入 `fence iorw, iorw`(pro 审计 #10)。
//!
//! # 平台依赖(已知限制)
//! 基址 0x1000_0000 与分频值均为 QEMU virt 约定:M1 阶段改为解析
//! FDT 得到基址/时钟,按实际时钟计算分频(pro 审计 #6)。
//!
//! # 健壮性
//! 发送采用**有界等待**:真机上 TX 挂死时宁可丢字符(计数器记录)
//! 也不让整个内核死锁 —— 调试输出必须永远可用。
//!
//! # 多核注意(副核唤醒前必须完成)
//! 当前无输出锁:单核 + 副核停车下无竞争。唤醒副核前必须在
//! write_str 外加自旋锁;锁实现须保证 panic 路径(中断关闭、
//! 可能打断持锁的主上下文)不死锁 —— 建议 panic 时放弃锁直接
//! 输出,或 panic 路径使用独立的紧急输出通道。

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

const UART_BASE: usize = crate::board::UART_BASE;
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

/// 设备寄存器定序屏障:排序前后的 MMIO 读写。
/// RISC-V 中 volatile 不产生硬件定序,此处为硬件可见的 fence。
#[inline]
fn mmio_fence() {
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// 读 MMIO 寄存器(volatile,防编译器合并/缓存)。
///
/// # Safety
/// `addr` 必须指向本内核已映射的 MMIO 寄存器地址,且可读。
/// 本驱动内部只传入固定 UART 寄存器常量。
#[inline]
unsafe fn read_u8(addr: usize) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

/// 写 MMIO 寄存器(volatile)。
///
/// # Safety
/// 同 `read_u8`:addr 必须为合法 MMIO 地址。
#[inline]
unsafe fn write_u8(addr: usize, val: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, val) }
}

/// LSR 位 5 = THR 空(发送保持寄存器可写)。
#[inline]
fn is_transmit_empty() -> bool {
    unsafe { read_u8(UART_LSR) & 0x20 != 0 }
}

/// 初始化串口为 8N1、FIFO 开启、关中断。
///
/// 顺序敏感,见模块头"DLAB 陷阱"说明;关键写之间插入 MMIO fence:
/// 1. LCR=0x80(DLAB=1)→ fence → 写 DLL/DLM 分频
/// 2. LCR=0x03(DLAB=0,8N1)→ fence → 写 IER=0(关中断)
/// 3. FCR=0x07(开 FIFO 并清空)、MCR=0x03(RTS/DTR 置位)
///
/// 分频值说明:分频 = 参考时钟 / (16 × 波特率)。QEMU virt 的虚拟
/// 串口对波特率不敏感,此处数值仅为形式正确(经典 1.8432MHz 参考
/// 时钟下为 9600 波特,与注释"115200"不符 —— 真机必须按实际时钟
/// 计算,见模块头"平台依赖")。
pub fn init() {
    unsafe {
        write_u8(UART_LCR, 0x80);
        mmio_fence();
        write_u8(UART_DLL, 0x0C);
        write_u8(UART_DLM, 0x00);
        mmio_fence();
        write_u8(UART_LCR, 0x03);
        mmio_fence();
        write_u8(UART_IER, 0x00);
        write_u8(UART_FCR, 0x07);
        write_u8(UART_MCR, 0x03);
    }
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
    unsafe {
        // 写前屏障(MED#3):LSR 读 → THR 写 的定序,乱序核上
        // 防止 LSR 轮询结果与本次写交错。
        mmio_fence();
        write_u8(UART_BASE, c);
        // 写后屏障:保证后续 LSR 轮询观察到本次写已到达设备。
        mmio_fence();
    }
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
