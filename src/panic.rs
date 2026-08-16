use crate::arch::{self, CpuState};
use crate::error;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
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

fn dump_cpu(state: CpuState) {
    error!("--- CPU state ---");
    error!("tick: {}", crate::logger::tick());
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
