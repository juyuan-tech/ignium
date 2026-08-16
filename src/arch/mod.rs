#[cfg(target_arch = "riscv64")]
mod riscv64;

#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[cfg(not(target_arch = "riscv64"))]
compile_error!("Ignium currently supports riscv64 only; x86_64 port planned (see ROADMAP.md)");
