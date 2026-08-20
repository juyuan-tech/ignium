//! SBI(Supervisor Binary Interface)调用封装。
//!
//! 内核运行在 S 模式,特权操作通过 `ecall` 委托给 M 模式的 OpenSBI
//! 固件。本模块是唯一允许出现 `ecall` 的地方。
//!
//! # 约定
//! - 调用约定:`a7` = 扩展号,`a6` = 功能号,`a0-a5` = 参数;
//!   返回 `a0` = 错误码(SBI_SUCCESS=0),`a1` = 附加返回值。
//! - 错误处理:**所有调用方必须检查返回值**。
//!
//! # 定时器:主路径按能力选择(D17)
//! `enable_timer` 首次用 SBI `set_timer` 编程(通用,无非法指令风险),
//! FDT 解析后检测到 `sstc` 扩展才切回 stimecmp 直写(见
//! arch/riscv64.rs 的 arm_timer)。因此 `set_timer` 是**活跃回退路径**。

/// SBI TIME 扩展号(ASCII "TIME")。
const SBI_EXT_TIME: usize = 0x5449_4D45;

/// 编程定时器(SBI TIME 扩展)。**回退实现**:当前主路径为 SSTC
/// 直写 stimecmp(见 arch/riscv64.rs);无 SSTC 平台接入时启用本函数。
///
/// Safety
/// 通过裸 `ecall` 调用 M 模式固件。调用约定由 `clobber_abi("C")`
/// 声明全部 caller-saved 寄存器被覆写;错误码经 a0 返回,必须检查。
#[inline]
pub fn set_timer(stime_value: usize) -> usize {
    let mut error: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_TIME,
            in("a6") 0,
            in("a0") stime_value,
            // SBI 约定:调用方必须假定 a0-a7 与 **所有 caller-saved**
            // 寄存器(t0-t6 等)均被覆写。clobber_abi("C") 声明它们全部
            // 被破坏 —— 否则编译器可能在 ecall 期间把活跃值存于 t0-t6,
            // OpenSBI 覆写它们导致误编译(审计 V2 H4,此前漏改)。
            lateout("a0") error,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}
