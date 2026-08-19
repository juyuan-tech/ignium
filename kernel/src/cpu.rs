//! CPU 能力检测与 ISA 信息(M1.5 RVA23 P1)。
//!
//! 在引导早期读取 FDT 的 `riscv,isa` 属性,记录可用扩展。
//! 当前仅用于诊断输出;运行时回退由后续阶段实现。

use crate::info;

/// CPU 能力集合(当前未使用,预留 M2 运行时回退)。
#[allow(dead_code)]
pub struct Capabilities {
    /// ISA 字符串(如 "rv64imafdch_zba_zbb_zbs_zicond")。
    pub isa_string: &'static str,
}

/// 从 FDT 解析的 BoardParams 中读取 ISA 信息。
///
/// 在 board::init_from_fdt 之后调用。
pub fn init_from_fdt(params: &crate::fdt::BoardParams) {
    let caps = detect_from_fdt(params);
    if let Some(ref caps) = caps {
        info!("M1.5: CPU capabilities: {}", caps.isa_string);
    } else {
        info!("M1.5: CPU capabilities: rv64imafdc (default, no FDT riscv,isa)");
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
