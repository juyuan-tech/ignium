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
/// # Safety
/// 仅用于编程 S 模式定时器;调用后 a0/a1 被 SBI 覆写,已声明为
/// 输出操作数。不可从中断关闭期间的不可重入上下文调用本函数
/// (ecall 本身是可重入的,但嵌套定时器语义未定义)。
#[inline]
pub fn set_timer(stime_value: usize) {
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_TIME,
            in("a6") 0,
            in("a0") stime_value,
            lateout("a0") _,
            lateout("a1") _,
            options(nostack)
        );
    }
}
