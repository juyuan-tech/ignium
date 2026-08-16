use core::arch::asm;

pub struct CpuState {
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub sstatus: usize,
    pub sepc: usize,
    pub scause: usize,
    pub stval: usize,
    pub satp: usize,
}

pub fn cpu_state() -> CpuState {
    let mut s = CpuState {
        ra: 0,
        sp: 0,
        gp: 0,
        tp: 0,
        sstatus: 0,
        sepc: 0,
        scause: 0,
        stval: 0,
        satp: 0,
    };
    unsafe {
        asm!("mv {}, ra", out(reg) s.ra);
        asm!("mv {}, sp", out(reg) s.sp);
        asm!("mv {}, gp", out(reg) s.gp);
        asm!("mv {}, tp", out(reg) s.tp);
        asm!("csrr {}, sstatus", out(reg) s.sstatus);
        asm!("csrr {}, sepc", out(reg) s.sepc);
        asm!("csrr {}, scause", out(reg) s.scause);
        asm!("csrr {}, stval", out(reg) s.stval);
        asm!("csrr {}, satp", out(reg) s.satp);
    }
    s
}

#[inline]
pub fn irq_disable() {
    unsafe {
        asm!("csrc sstatus, {}", in(reg) 1 << 1, options(nomem, nostack));
    }
}

#[inline]
pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) }
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) }
    }
}
