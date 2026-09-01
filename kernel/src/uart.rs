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
//! # 平台依赖(D21 已解决)
//! 基址 0x1000_0000 为 QEMU virt 约定;M1.5 已支持 FDT 解析串口基址
//! (兼容 uart@/serial@ 节点,从节点名或 reg 属性提取)。**D21(2026-09-01):
//! 分频值按 FDT uart 节点 `clock-frequency` 计算**(divisor = clk/(16×波特率),
//! 见 `uart_divisor`);QEMU 忽略波特率,真机按实际参考时钟即得正确波特率。
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::sync::SpinLock;

/// 缓存 UART 基址(避免每次 putc 调用 board::uart_base 的跨模块开销)。
/// 初始化后写入,此后只读(Relaxed 足够,因 init 与 putc 有 happens-before)。
static UART_REG_BASE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// 控制台输出锁(D9 多核):write_str 输出先 try_lock;失败(panic 重入 /
/// 他核持有)回退无锁裸写。
///
/// # 锁序契约
/// 输出函数**永不持有 SCHED 等其它锁**;ISR 仍零日志(红线 5)。
static CONSOLE_LOCK: SpinLock<()> = SpinLock::new(());

/// panic 输出模式:置位后 write_str 不再取锁,直接裸写。
/// panic 时中断已关,且可能打断正持 CONSOLE_LOCK 的主上下文 ——
/// 若仍走取锁路径必然死锁。此标志由 panic 处理器置位,不可逆。
static PANIC_OUTPUT: AtomicBool = AtomicBool::new(false);

/// 进入 panic 输出模式(panic 处理器调用;此后所有输出不加锁)。
pub fn set_panic_output() {
    PANIC_OUTPUT.store(true, Ordering::Relaxed);
}

#[inline]
fn uart_base() -> usize {
    let v = UART_REG_BASE.load(Ordering::Relaxed);
    if v != usize::MAX {
        v
    } else {
        crate::board::uart_base()
    }
}
#[allow(clippy::identity_op)]
#[inline]
fn uart_dll() -> usize {
    uart_base() + 0x00
}
#[inline]
fn uart_dlm() -> usize {
    uart_base() + 0x01
}
#[inline]
fn uart_ier() -> usize {
    uart_base() + 0x01
}
#[inline]
fn uart_fcr() -> usize {
    uart_base() + 0x02
}
#[inline]
fn uart_lcr() -> usize {
    uart_base() + 0x03
}
#[inline]
fn uart_mcr() -> usize {
    uart_base() + 0x04
}
#[inline]
fn uart_lsr() -> usize {
    uart_base() + 0x05
}

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
    unsafe { read_u8(uart_lsr()) & 0x20 != 0 }
}

/// 目标波特率(分频计算基准;QEMU 忽略,真机常用值)。
const BAUD_RATE: usize = 115_200;

/// D21:计算 NS16550 分频器值 = 参考时钟 / (16 × 波特率)。
///
/// board::uart_clock 源自 FDT uart 节点 `clock-frequency`;对 QEMU virt
/// (3.6864MHz)得 3686400/(16×115200) = 2(DLL=0x02)。异常时回退旧固定值
/// 0x0C(QEMU 忽略波特率,仅形式正确)。结果 clamp 到 16 位分频寄存器
/// 有效范围 [1, 0xFFFF](除零/时钟过低防回绕)。
#[inline]
fn uart_divisor() -> u16 {
    let clk = crate::board::uart_clock();
    if clk == 0 {
        return 0x0C;
    }
    let div = clk / (16 * BAUD_RATE);
    div.clamp(1, 0xFFFF) as u16
}

/// 初始化串口为 8N1、FIFO 开启、关中断。
///
/// 顺序敏感,见模块头"DLAB 陷阱"说明;关键写之间插入 MMIO fence:
/// 1. LCR=0x80(DLAB=1)→ fence → 写 DLL/DLM 分频
/// 2. LCR=0x03(DLAB=0,8N1)→ fence → 写 IER=0(关中断)
/// 3. FCR=0x07(开 FIFO 并清空)、MCR=0x03(RTS/DTR 置位)
///
/// 分频值来源:见 `uart_divisor`(D21,FDT clock-frequency 计算)。
///
/// 缓存 UART 基址,加速后续 putc 热路径。
pub fn init() {
    // 缓存基址(避免每次 putc 调用 board::uart_base 的跨模块开销)。
    UART_REG_BASE.store(crate::board::uart_base(), Ordering::Relaxed);
    unsafe { init_hw() }
}

/// FDT 解析后重初始化:更新缓存基址并重新配置硬件。
/// 在 board::init_from_fdt 之后调用,使 UART 反映 FDT 值。
pub fn reinit() {
    UART_REG_BASE.store(crate::board::uart_base(), Ordering::Relaxed);
    unsafe { init_hw() }
}

/// 硬件初始化序列(寄存器地址从缓存基址计算)。
///
/// # Safety
/// 必须确保 UART_REG_BASE 已更新为正确基址(由 init/reinit 保证)。
unsafe fn init_hw() {
    // D21:分频 = FDT clock-frequency 计算值(DLL/DLM 16 位)。
    let dll = uart_divisor();
    unsafe {
        write_u8(uart_lcr(), 0x80);
        mmio_fence();
        write_u8(uart_dll(), (dll & 0xFF) as u8);
        // 自审修复(真机健壮性):DLL/DLM 构成 16 位分频,DML 写入时
        // UART 锁存完整分频 —— 乱序核上若 DLM 先写会以旧 DLL 锁存,
        // 波特率错误。关键写之间必须有 fence。
        mmio_fence();
        write_u8(uart_dlm(), (dll >> 8) as u8);
        mmio_fence();
        write_u8(uart_lcr(), 0x03);
        mmio_fence();
        write_u8(uart_ier(), 0x00);
        mmio_fence();
        write_u8(uart_fcr(), 0x07);
        mmio_fence();
        write_u8(uart_mcr(), 0x03);
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
        write_u8(uart_base(), c);
        // 写后屏障:保证后续 LSR 轮询观察到本次写已到达设备。
        mmio_fence();
    }
}

/// 输出字符串;`\n` 自动补 `\r\n`(终端换行兼容)。
///
/// # 多核语义(D9)
/// 正常路径先 `try_lock` CONSOLE_LOCK(跨核互斥,防止字符交错);
/// 失败(panic 输出模式 / 他核正持有且本核在不可自旋上下文)直接
/// 无锁裸写 —— best-effort,宁可字符错位也不死锁。
///
/// **注意**:多核并发输出时 try_lock 在争用下立即回退 → 可能逐字符
/// 交错(非确定性)。需要**整行原子**的 boot 期输出请用 `locked_line`
/// (T3a 实测:副核上线与 banner 同时打印,QEMU -nographic 串口慢写
/// 放大窗口;见 `locked_line` 说明)。
pub fn write_str(s: &str) {
    write_bytes(s.as_bytes());
}

/// T3a:多核 boot 期**整行原子**控制台输出(阻塞拿锁后执行 `f`)。
///
/// 背景:三个副核上线(`hart N online`)与 boot hart 的 T3a banner
/// 在同一时刻打印,`write_str` 的 D9 try_lock 在争用下立即回退裸写
/// → 逐字符交错(QEMU -nographic 串口逐字符慢写放大窗口;test-smp
/// "3 条 online" 断言曾偶发失配)。本函数**阻塞自旋**拿 CONSOLE_LOCK,
/// 保证整行原子。
///
/// # 为什么阻塞锁在此安全(D9 拒绝全局阻塞锁的原因不适用)
/// - 调用方限定 **boot 期各核 SIE 全关**:无定时器 ISR,持锁者不会被
///   抢占打断、无重入(否则阻塞锁在自身重入时死锁)。
/// - 写入者**有界**:每个副核只打印一行、boot hart 只打印一行;持锁者
///   必然在有限时间内释放 → 等待者必然拿到锁。
/// - panic 短接:已置 PANIC_OUTPUT 时直接执行 `f` 不取锁(与 D9 同一
///   契约,panic=abort 下守卫不会 Drop,阻塞锁会让等待核挂死 —— 故
///   本函数绝不能用于 panic 之后的通用输出)。
///
/// 通用多核日志仍走 `write_str` 的 D9 best-effort 语义(交错可接受)。
pub fn locked_line(f: impl FnOnce()) {
    if PANIC_OUTPUT.load(Ordering::Relaxed) {
        f();
        return;
    }
    let _guard = CONSOLE_LOCK.lock();
    f();
}

/// 输出任意字节序列(M3 T1 `sys_write` 用);`\n` 自动补 `\r\n`。
///
/// # 多核语义(D9)
/// 与 `write_str` 同契约:正常路径 `try_lock` CONSOLE_LOCK,失败(panic
/// 模式 / 他核持有)无锁裸写 —— best-effort,宁可字符错位也不死锁。
pub fn write_bytes(bytes: &[u8]) {
    if PANIC_OUTPUT.load(Ordering::Relaxed) {
        write_bytes_raw(bytes);
        return;
    }
    let _guard = CONSOLE_LOCK.try_lock();
    write_bytes_raw(bytes);
}

/// 无锁裸写(底层出口)。调用方负责互斥(PANIC_OUTPUT / CONSOLE_LOCK)。
fn write_bytes_raw(bytes: &[u8]) {
    for &b in bytes {
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
