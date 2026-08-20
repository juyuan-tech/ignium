//! CPU 能力检测与 ISA 信息(M1.5 RVA23 P1 / D17)。
//!
//! 引导早期从 FDT 的 `riscv,isa` 读取 ISA 字符串:
//! - 输出能力表(诊断);
//! - 驱动定时器路径选择(D17):含 `sstc` → stimecmp 直写,否则 SBI 回退。
//!   M2 的其它扩展运行时回退(如 Zicboz 清零)仍待实现。

use crate::info;

/// CPU 能力集合。
pub struct Capabilities {
    /// ISA 字符串(如 "rv64imafdch_zba_zbb_zbs_zicond")。
    pub isa_string: &'static str,
}

/// 从 FDT 解析的 BoardParams 中读取 ISA 信息。
///
/// 在 board::init_from_fdt 之后调用。同时依据 `riscv,isa` 检测
/// SSTC 扩展(D17):含 `sstc` → 定时器直写 stimecmp;否则保持
/// SBI set_timer 回退。
pub fn init_from_fdt(params: &crate::fdt::BoardParams) {
    let caps = detect_from_fdt(params);
    if let Some(ref caps) = caps {
        info!("M1.5: CPU capabilities: {}", caps.isa_string);
        // D17:ISA 字符串含 "sstc" 才启用 stimecmp 直写。
        let sstc = caps.isa_string.contains("sstc");
        crate::arch::set_sstc(sstc);
        if !sstc {
            info!("M1.5: platform lacks SSTC, using SBI timer (fallback)");
        }
    } else {
        info!("M1.5: CPU capabilities: rv64imafdc (default)");
        // FDT 未提供 riscv,isa:无法检测 SSTC,保守用 SBI 定时器。
        crate::arch::set_sstc(false);
    }
}

/// 从 FDT 参数中提取 ISA 信息。
fn detect_from_fdt(params: &crate::fdt::BoardParams) -> Option<Capabilities> {
    if params.isa_string.is_empty() {
        None
    } else {
        Some(Capabilities {
            isa_string: params.isa_string,
        })
    }
}
