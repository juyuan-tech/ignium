//! 架构隔离层。
//!
//! # 契约(所有架构必须实现,通用代码只能通过本模块访问硬件)
//! - `init_traps` / `trap_handler` —— 陷阱向量安装与处理(中断分发)
//! - `cpu_state` —— 诊断用寄存器快照
//! - `irq_disable` / `irq_enable` —— 关/开全局中断
//! - `enable_timer` / `get_time` / `TIMER_INTERVAL` —— 周期定时器
//! - `wait_for_interrupt` / `halt` —— 空闲等待 / 停机
//! - `CpuState` —— 快照数据结构(repr(C),与汇编 ABI 对齐)
//!
//! # 新增架构(x86_64 移植)
//! 在 `arch/x86_64.rs` + `arch/x86_64.S` 实现上述接口,
//! 并用 `#[cfg(target_arch = "x86_64")]` 分支导出;通用代码零改动
//! (参考 ROADMAP 阶段 5)。

#[cfg(target_arch = "riscv64")]
mod riscv64;

#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[cfg(not(target_arch = "riscv64"))]
compile_error!("Ignium currently supports riscv64 only; x86_64 port planned (see ROADMAP.md)");
