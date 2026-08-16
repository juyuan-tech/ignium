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
        asm!(
            "mv {ra_o}, ra",
            "mv {sp_o}, sp",
            "mv {gp_o}, gp",
            "mv {tp_o}, tp",
            "csrr {sstatus_o}, sstatus",
            "csrr {sepc_o}, sepc",
            "csrr {scause_o}, scause",
            "csrr {stval_o}, stval",
            "csrr {satp_o}, satp",
            ra_o = out(reg) s.ra,
            sp_o = out(reg) s.sp,
            gp_o = out(reg) s.gp,
            tp_o = out(reg) s.tp,
            sstatus_o = out(reg) s.sstatus,
            sepc_o = out(reg) s.sepc,
            scause_o = out(reg) s.scause,
            stval_o = out(reg) s.stval,
            satp_o = out(reg) s.satp,
        );
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
