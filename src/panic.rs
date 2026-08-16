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

fn stack_bounds() -> (usize, usize) {
    let bottom = unsafe { &_stack_bottom as *const u8 as usize };
    let top = unsafe { &_stack_top as *const u8 as usize };
    (bottom, top)
}

fn stack_watermark() -> usize {
    let (bottom, top) = stack_bounds();
    let mut probe = bottom;
    while probe < top {
        if unsafe { core::ptr::read_volatile(probe as *const usize) } != 0 {
            return top - probe;
        }
        probe += core::mem::size_of::<usize>();
    }
    0
}

fn dump_cpu(state: CpuState) {
    error!("--- CPU state ---");
    error!("tick: {}", crate::logger::tick());
    error!("note: sepc/scause/stval are best-effort until M1 trap context capture");
    let (bottom, top) = stack_bounds();
    let sp = state.sp;
    if sp < bottom || sp >= top {
        error!(
            "WARNING: sp={:#x} outside stack range [{:#x}, {:#x})",
            sp, bottom, top
        );
    }
    error!(
        "stack watermark: {} / {} bytes used, uart dropped: {}",
        stack_watermark(),
        top - bottom,
        crate::uart::dropped()
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
