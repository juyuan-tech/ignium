#![no_std]
#![no_main]

mod arch;
mod uart;

use core::arch::global_asm;

global_asm!(include_str!("entry.S"));

#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() -> ! {
    uart::init();
    println!("[Ignium] 炬元微内核 v0.1.0 booting");
    println!("[Ignium] M0: boot ok - arch: riscv64, machine: qemu-virt");
    println!("[Ignium] next milestone: trap handling (M1)");
    loop {
        arch::wait_for_interrupt();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[Ignium] panic: {info}");
    loop {
        arch::wait_for_interrupt();
    }
}
