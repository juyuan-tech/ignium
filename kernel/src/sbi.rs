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
/// SBI IPI 扩展号(ASCII "IPI")。EID = 0x735049('I''P''I' 三字符,
/// 高位字节被省略;此前误用 0x7350494D 导致 SBI_ERR_NOT_SUPPORTED)。
///
/// T3a 实测:**IPI 不是副核唤醒的可靠手段**(副核停 OpenSBI HSM 时收不到,
/// 见 T3a 报告;唤醒改用 SBI HSM `hart_start`)。保留 `send_ipi` 供 T3b
/// 跨核调度唤醒使用(T3b 报告需重新验证其在新机制的可用性)。
#[allow(dead_code)]
const SBI_EXT_IPI: usize = 0x0073_5049;
/// SBI HSM(Hart State Management)扩展号(ASCII "HSM")。
const SBI_EXT_HSM: usize = 0x0048_534D;

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

/// 向目标 hart 发送核间中断(SBI IPI 扩展,功能号 0)。
///
/// T3a 实测:**IPI 无法唤醒副核**(副核停 OpenSBI HSM 时收不到
/// m_software/S 软中断),唤醒改用 SBI HSM `hart_start`(见上)。
/// 本函数保留供 **T3b 跨核调度唤醒**:per-CPU 就绪队列把线程放到目标
/// 核时,发 IPI 唤醒该核 wfi 中的 idle(须在 T3b 报告重新验证其可用性,
/// 见 `SBI_EXT_IPI` 注释)。使用方须检查返回值。
///
/// # 参数(SBI 规范)
/// - `hart_mask`:目标 hart 位掩码。`hart_mask_base = 0` 时位 i 对应
///   hart i(本平台 hartid < 64,直接用掩码)。
/// - `hart_mask_base`:掩码起始 hart 编号(本平台恒为 0)。
///
/// 返回 a0 错误码(SBI_SUCCESS=0 表示成功);调用方必须检查。
///
/// # Safety
/// 裸 `ecall` 调用 M 模式固件;`clobber_abi("C")` 声明全部 caller-saved
/// 寄存器被覆写。与 `set_timer` 同一约定。
///
/// T3b:跨核调度唤醒用(见 `SBI_EXT_IPI` 注释);T3a 当前未使用。
#[allow(dead_code)]
#[inline]
pub fn send_ipi(hart_mask: u64, hart_mask_base: u64) -> usize {
    let mut error: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_IPI,
            in("a6") 0,
            in("a0") hart_mask,
            in("a1") hart_mask_base,
            lateout("a0") error,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

/// SBI HSM hart_start(功能号 0):把处于 **HSM STOPPED** 状态的副核启动起来。
///
/// **D8 关键**:QEMU + OpenSBI 下副核并不会在 OpenSBI warm boot 后自动进入
/// 内核 `_start` —— 它们停在 OpenSBI 的 HSM 状态机(STOPPED → 等 START_PENDING)
/// 的 wfi 循环里(实测 `-d in_asm` 可见,见 T3a 报告)。载荷必须显式调用本
/// 函数逐个启动副核(与 Linux 等 RISC-V OS 的多核引导协议一致)。
///
/// # 参数(SBI 规范 v1.0+)
/// - `hartid`:目标副核编号。
/// - `start_addr`:副核被启动后跳转的入口地址(本内核 = `_start` @ 0x80200000)。
/// - `spriv`:0 = 从 S 模式启动,1 = M 模式。本内核运行于 S 模式,恒传 0。
/// - `sarg1`:传给目标 hart 的 a0 寄存器值(约定为 hartid,见 entry.S)。
///
/// 返回 a0 错误码(SBI_SUCCESS=0 表示成功);调用方必须检查。若目标核不在
/// STOPPED 状态(已启动)则返回 SBI_ERR_INVALID_PARAM。
#[inline]
pub fn hsm_hart_start(hartid: usize, start_addr: usize, spriv: usize, sarg1: usize) -> usize {
    let mut error: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_HSM,
            in("a6") 0,
            in("a0") hartid,
            in("a1") start_addr,
            in("a2") spriv,
            in("a3") sarg1,
            lateout("a0") error,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}
