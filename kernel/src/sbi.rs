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
//! # 定时器:当前使用 SSTC 直写 stimecmp(性能优化)
//! `set_timer`(TIME 扩展)保留为**无 SSTC 平台的回退/参考实现**;
//! 主路径见 arch/riscv64.rs 的 `set_stimecmp`。RVA23 强制平台均
//! 具备 SSTC。

/// SBI TIME 扩展号(ASCII "TIME")。回退实现保留,当前未使用。
#[allow(dead_code)]
const SBI_EXT_TIME: usize = 0x5449_4D45;

/// 编程定时器(SBI TIME 扩展)。**回退实现**:当前主路径为 SSTC
/// 直写 stimecmp(见 arch/riscv64.rs);无 SSTC 平台接入时启用本函数。
#[allow(dead_code)]
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
