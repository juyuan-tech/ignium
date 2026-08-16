use core::arch::{asm, global_asm};

use crate::error;

global_asm!(include_str!("riscv64.S"));

#[repr(C)]
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

#[unsafe(no_mangle)]
pub static mut TRAP_FRAME: [usize; 32] = [0; 32];

extern "C" {
    static trap_vector: u8;
}

pub fn init_traps() {
    unsafe {
        asm!(
            "csrw stvec, {}",
            in(reg) &trap_vector as *const u8 as usize,
            options(nomem, nostack)
        );
    }
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
        asm!("csrc sstatus, {imm}", imm = const 2, options(nomem, nostack));
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

#[unsafe(no_mangle)]
pub extern "C" fn trap_handler(scause: usize, sepc: usize, stval: usize, frame: *mut usize) -> ! {
    error!(
        "TRAP: scause={:#x} sepc={:#x} stval={:#x}",
        scause, sepc, stval
    );
    let regs = unsafe { core::slice::from_raw_parts(frame, 31) };
    error!(
        "ra={:#x} sp={:#x} gp={:#x} tp={:#x}",
        regs[0], regs[1], regs[2], regs[3]
    );
    error!(
        "t0={:#x} t1={:#x} t2={:#x} s0={:#x}",
        regs[4], regs[5], regs[6], regs[7]
    );
    error!(
        "a0={:#x} a1={:#x} a2={:#x} a3={:#x}",
        regs[8], regs[9], regs[10], regs[11]
    );
    error!(
        "a4={:#x} a5={:#x} a6={:#x} a7={:#x}",
        regs[12], regs[13], regs[14], regs[15]
    );
    error!(
        "s2={:#x} s3={:#x} s4={:#x} s5={:#x}",
        regs[16], regs[17], regs[18], regs[19]
    );
    error!(
        "s6={:#x} s7={:#x} s8={:#x} s9={:#x}",
        regs[20], regs[21], regs[22], regs[23]
    );
    error!(
        "s10={:#x} s11={:#x} t3={:#x} t4={:#x}",
        regs[24], regs[25], regs[26], regs[27]
    );
    error!("t5={:#x} t6={:#x}", regs[28], regs[29]);
    halt()
}
