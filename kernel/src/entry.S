# Ignium 引导入口(RISC-V 64,S 模式,OpenSBI 跳转到 0x80200000 进入此处)。
#
# 寄存器约定:
#   a0 = hartid(OpenSBI 传入)
#   a1 = FDT 指针(OpenSBI 传入)
#   以上两值必须保留到 `call kernel_main`(BSS 循环只使用 t0/t1)。
#
# 启动顺序:
#   1) satp 清零,显式进入 Bare(物理)模式
#   2) sfence.vma 冲刷 TLB
#   3) 清零 BSS(含栈区,保证水位检测等未初始化数据确定)
#   4) 设置栈指针(向下增长,指向栈顶)
#   5) 跳转 kernel_main

.section .text.entry
.globl _start
_start:
    # 若上一级引导(或 SBI)遗留了分页开启状态,不清 satp 会带着
    # 残留页表执行,立即崩溃。写入 0 = Bare 模式(直接物理寻址)。
    csrw satp, zero
    # satp 切换后必须冲刷 TLB;`fence` 只是指令序屏障,不是 TLB 冲刷。
    sfence.vma zero, zero

    # 清零 [ _bss_start, _stack_top ),8 字节步进。
    # 两端都在链接脚本中按 16 字节对齐,循环无越界风险。
    la t0, _bss_start
    la t1, _stack_top
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b
2:
    # 栈向下增长,sp 指向栈顶(16 字节对齐,符合 RISC-V ABI)。
    la sp, _stack_top
    # hartid(a0)、FDT(a1)原样传入内核主函数。
    call kernel_main
    # 理论不可达(kernel_main 返回类型为 !);防御性死循环。
3:
    j 3b
