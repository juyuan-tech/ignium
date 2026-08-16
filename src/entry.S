.section .text.entry
.globl _start
_start:
    csrw satp, zero
    fence
    la t0, _bss_start
    la t1, _stack_top
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:
    la sp, _stack_top
    call kernel_main
3:
    j 3b
