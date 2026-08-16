//! Ignium 炬元微内核 —— 内核入口与引导顺序。
//!
//! 启动链:OpenSBI(S 模式)→ `_start`(src/entry.S)→ 清 BSS → 设栈 →
//! `kernel_main(hartid, fdt)`。
//!
//! # 团队约定
//! - `kernel_main` 内的初始化顺序存在依赖关系,改动必须重新评估
//!   "trap 窗口"(见下方 init 顺序注释)。
//! - 本 crate 是微内核的**唯一特权层**;用户态兼容代码(OpenHarmony)
//!   永远不进入这里(见 docs/DESIGN.md)。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod arch;
mod logger;
mod panic;
mod uart;

use core::arch::global_asm;

// 引导汇编:_start 入口、BSS 清零、栈指针设置。逻辑见 src/entry.S 注释。
global_asm!(include_str!("entry.S"));

/// 内核主入口,由 `_start`(src/entry.S)以 C ABI 调用,永不返回。
///
/// # 参数
/// - `hartid`:当前 hart 编号(OpenSBI 引导时经 a0 传入,原样透传)。
/// - `fdt`:设备树(FDT)指针(OpenSBI 经 a1 传入)。当前阶段仅记录;
///   硬件探测(如 UART 基址)仍依赖硬编码常量,真机移植时优先改造这里。
///
/// # 初始化顺序(依赖关系,勿随意调整)
/// 1. `arch::irq_disable` —— 在一切操作前建立确定的中断状态
/// 2. `uart::init` —— 日志基础设施,必须先于任何输出
/// 3. `arch::init_traps` —— stvec 装好之前发生异常会跳到地址 0
///    (不可恢复),因此必须尽早;但必须在 uart 之后(陷阱日志依赖串口)
/// 4. `logger::set_level` —— 日志级别,先于任何日志调用
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(hartid: usize, fdt: *const u8) -> ! {
    arch::irq_disable();
    uart::init();
    arch::init_traps();
    crate::logger::set_level(crate::logger::Level::Info);
    info!("Ignium 炬元微内核 v{} booting", env!("CARGO_PKG_VERSION"));
    debug!("uart console initialized (tick={})", crate::logger::tick());
    info!(
        "M0: boot ok - arch: riscv64, machine: qemu-virt, hartid={}, fdt={:#x}",
        hartid, fdt as usize
    );
    warn!("timer not yet enabled; tick stays at 0 until M1");
    // 空闲循环:无任务可跑时进入 wfi 等待(中断关闭期间等效空转)。
    loop {
        arch::wait_for_interrupt();
    }
}
