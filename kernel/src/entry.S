# Ignium 引导入口(RISC-V 64,S 模式,OpenSBI 跳转到 0x80200000 进入此处)。
#
# 寄存器约定:
#   a0 = hartid(OpenSBI 传入)
#   a1 = FDT 指针(OpenSBI 传入)
#   a0/a1 必须保留到 `call kernel_main`(BSS 循环只使用 t0/t1;
#   副 hart 停车分支在触碰内存之前完成)。
#
# 启动顺序:
#   1) 清 sstatus.SIE(遗留引导器可能开着中断)
#   2) satp 清零(Bare 模式)+ sfence.vma 冲刷 TLB
#   3) 初始化 gp(__global_pointer$ 锚点;norelax 防止自身被松弛成 no-op)
#   4) 副 hart(非 0)立即停车,不触碰共享内存
#   5) hart 0:清零 BSS(含栈区)→ 设栈 → kernel_main

.section .text.entry
.globl _start
_start:
    # 清除全局中断使能:在第一条 S 模式指令就确定中断状态,
    # 避免 BSS 清零期间被遗留中断打断(pro 审计 LOW)。
    csrc sstatus, 2
    # 若上一级引导(或 SBI)遗留了分页开启状态,不清 satp 会带着
    # 残留页表执行,立即崩溃。写入 0 = Bare 模式(直接物理寻址)。
    csrw satp, zero
    # satp 切换后必须冲刷 TLB;`fence` 只是指令序屏障,不是 TLB 冲刷。
    sfence.vma zero, zero

    # gp = __global_pointer$:数据量增长后链接器会生成 gp 相对寻址,
    # 必须在早期初始化。norelax 块防止本指令自身被松弛为 gp 相对
    # 寻址(那样会变成 no-op,gp 仍是垃圾值)。
    .option norelax
    la gp, __global_pointer$
    .option relax

    # 副 hart 停车:多 hart 启动时,非 0 hart 在触碰任何内存前原地
    # 等待(hart 0 完成引导后再唤醒 —— M1 实现)。这防止多个 hart
    # 并发清 BSS、共享引导栈/陷阱栈导致的数据竞争。
    bnez a0, park

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

# 副 hart 停车点:wfi 等待唤醒。不触碰内存,无共享状态。
.align 2
park:
    wfi
    j park
