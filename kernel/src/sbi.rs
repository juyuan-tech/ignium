//! SBI(Supervisor Binary Interface)调用封装。
//!
//! 内核运行在 S 模式,特权操作(如定时器编程)通过 `ecall` 委托给
//! M 模式的 OpenSBI 固件。本模块是唯一允许出现 `ecall` 的地方。
//!
//! # 约定
//! - 调用约定:`a7` = 扩展号,`a6` = 功能号,`a0-a5` = 参数;
//!   返回 `a0` = 错误码(SBI_SUCCESS=0),`a1` = 附加返回值。
//! - 内核当前不校验返回值:定时器编程失败时 tick 不推进,uptime
//!   日志会暴露问题;M2+ 引入错误传播。
//! - OpenSBI 1.0 规范:扩展号 0x54494D45("TIME")功能 0 = set_timer,
//!   参数 = 绝对 stime 值(mtimer 时钟周期)。

/// SBI TIME 扩展号(ASCII "TIME")。
const SBI_EXT_TIME: usize = 0x5449_4D45;

/// 编程定时器:在 `stime_value`(绝对时间,mtimer 周期)触发
/// 超级定时器中断(STIE)。替代已废弃的 legacy sbi_set_timer。
///
/// 返回 SBI 错误码(0 = SBI_SUCCESS)。调用方应检查返回值;
/// ISR 内可忽略(失败表现为 tick 冻结,由 uptime 暴露)。
///
/// # Safety
/// 仅用于编程 S 模式定时器;调用后 a0/a1 被 SBI 覆写,已声明为
/// 输出操作数。**ISR 内可调用**(定时器中断处理正是主调用方):
/// 该上下文中断关闭、ecall 自身不重入,无嵌套语义问题。
#[inline]
pub fn set_timer(stime_value: usize) -> usize {
    let error: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_TIME,
            in("a6") 0,
            in("a0") stime_value,
            // SBI 调用约定(M4):调用方必须假定 a0-a7 全部被覆写,
            // 编译器可能在这些寄存器里保存活跃值。
            lateout("a0") error,
            lateout("a1") _,
            lateout("a2") _,
            lateout("a3") _,
            lateout("a4") _,
            lateout("a5") _,
            lateout("a6") _,
            lateout("a7") _,
            options(nostack)
        );
    }
    error
}
