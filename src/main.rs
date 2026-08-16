#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod arch;
mod logger;
mod panic;
mod uart;

use core::arch::global_asm;

global_asm!(include_str!("entry.S"));

#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() -> ! {
    arch::irq_disable();
    uart::init();
    arch::init_traps();
    crate::logger::set_level(crate::logger::Level::Info);
    info!("Ignium 炬元微内核 v{} booting", env!("CARGO_PKG_VERSION"));
    debug!("uart console initialized (tick={})", crate::logger::tick());
    info!("M0: boot ok - arch: riscv64, machine: qemu-virt");
    warn!("timer not yet enabled; tick stays at 0 until M1");
    loop {
        arch::wait_for_interrupt();
    }
}
