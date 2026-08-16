.section .text
.align 4
.globl trap_vector
trap_vector:
    la t6, TRAP_FRAME
    sd ra, 0*8(t6)
    sd sp, 1*8(t6)
    sd gp, 2*8(t6)
    sd tp, 3*8(t6)
    sd t0, 4*8(t6)
    sd t1, 5*8(t6)
    sd t2, 6*8(t6)
    sd s0, 7*8(t6)
    sd s1, 8*8(t6)
    sd a0, 9*8(t6)
    sd a1, 10*8(t6)
    sd a2, 11*8(t6)
    sd a3, 12*8(t6)
    sd a4, 13*8(t6)
    sd a5, 14*8(t6)
    sd a6, 15*8(t6)
    sd a7, 16*8(t6)
    sd s2, 17*8(t6)
    sd s3, 18*8(t6)
    sd s4, 19*8(t6)
    sd s5, 20*8(t6)
    sd s6, 21*8(t6)
    sd s7, 22*8(t6)
    sd s8, 23*8(t6)
    sd s9, 24*8(t6)
    sd s10, 25*8(t6)
    sd s11, 26*8(t6)
    sd t3, 27*8(t6)
    sd t4, 28*8(t6)
    sd t5, 29*8(t6)
    sd t6, 30*8(t6)
    csrr a0, scause
    csrr a1, sepc
    csrr a2, stval
    mv a3, t6
    call trap_handler
1:
    j 1b
