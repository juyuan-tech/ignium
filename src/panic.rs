use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{self, CpuState};
use crate::error;

extern "C" {
    static _stack_bottom: u8;
    static _stack_top: u8;
}

static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
#[cold]
#[inline(never)]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if PANICKING.swap(true, Ordering::Relaxed) {
        arch::halt()
    }
    arch::irq_disable();
    error!("KERNEL PANIC");
    match info.location() {
        Some(l) => error!("location: {}:{}:{}", l.file(), l.line(), l.column()),
        None => error!("location: unknown"),
    }
    error!("message: {}", info.message());
    dump_cpu(arch::cpu_state());
    arch::halt()
}

fn stack_watermark() -> usize {
    let bottom = unsafe { &_stack_bottom as *const u8 as usize };
    let top = unsafe { &_stack_top as *const u8 as usize };
    let mut probe = bottom;
    while probe < top {
        if unsafe { core::ptr::read_volatile(probe as *const u8) } != 0 {
            return top - probe;
        }
        probe += 1;
    }
    0
}

fn dump_cpu(state: CpuState) {
    error!("--- CPU state ---");
    error!("tick: {}", crate::logger::tick());
    let bottom = unsafe { &_stack_bottom as *const u8 as usize };
    let top = unsafe { &_stack_top as *const u8 as usize };
    let total = top - bottom;
    error!(
        "stack watermark: {} / {} bytes used",
        stack_watermark(),
        total
    );
    error!(
        "ra={:#x} sp={:#x} gp={:#x} tp={:#x}",
        state.ra, state.sp, state.gp, state.tp
    );
    error!("sstatus={:#x} sepc={:#x}", state.sstatus, state.sepc);
    error!(
        "scause={:#x} stval={:#x} satp={:#x}",
        state.scause, state.stval, state.satp
    );
    error!("--- end of dump ---");
}
