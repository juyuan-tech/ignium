# RISC-V 64 架构层汇编(由 arch/riscv64.rs 以 global_asm 包含)。
#
# 本文件是 arch 隔离层的一部分:x86_64 移植时在 arch/x86_64 下
# 提供等价符号即可,通用代码不感知差异。

# ---------------------------------------------------------------------------
# trap_vector:异常/中断统一入口(stvec 指向此处)。
#
# 设计(基于 pro 审计 #2/#3 的重构):
#   - **陷阱栈**:sscratch 保存 per-hart 陷阱栈顶;入口用 `csrrw` 换出
#     t6 取得栈顶,在陷阱栈上压入陷阱帧 —— 处理器绝不在被中断者的
#     sp 上运行(栈溢出/用户态 sp 均无法危害内核)。
#   - **可重入**:每层陷阱向下压新帧,并把 sscratch 更新为新帧底;
#     嵌套陷阱的写入被限制在陷阱栈区域内,不再破坏任意内存。
#   - **完整帧**:保存全部 31 个通用寄存器 + 4 个关键 CSR。
#   - M0 阶段 trap_handler 输出诊断后停机;M1 扩展为中断分发 + sret。
#
# 帧布局(与 riscv64.rs 的 ABI 索引常量保持一致!改一侧必须改另一侧):
#   [0..31)  GPR x1..x31(索引 n 对应 x(n+1))
#   [32]     sstatus
#   [33]     sepc
#   [34]     scause
#   [35]     stval
#   [36..40) 预留(M1 使用)
#
# 陷阱栈与引导栈在 linker.ld 中定义(均位于 _kernel_end 之后的保留区)。
# ---------------------------------------------------------------------------

.section .text
.align 4                    # stvec 要求 4 字节对齐(直接模式下低 2 位必须为 0)
.globl trap_vector
trap_vector:
    # t6 ← 陷阱栈顶(sscratch),被中断的 t6 暂存于 sscratch。
    csrrw t6, sscratch, t6
    # 在陷阱栈上预留帧(t6 = 帧基址)。
    addi t6, t6, -320
    # 先保存原始 t0(t0 即将被用作临时寄存器)。
    sd t0, 4*8(t6)
    # 取出被中断的 t6(暂存在 sscratch)并写入帧槽 30。
    csrr t0, sscratch
    sd t0, 30*8(t6)
    # 更新陷阱栈顶 = 本帧底:嵌套陷阱将向下压新帧(写入被限制在
    # 陷阱栈内,不会破坏任意内存)。
    csrw sscratch, t6
    sd ra, 0*8(t6)
    sd sp, 1*8(t6)
    sd gp, 2*8(t6)
    sd tp, 3*8(t6)
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
    # 保存关键 CSR:硬件在异常/中断入口已更新这些值,是定位故障的核心。
    csrr t0, sstatus
    sd t0, 32*8(t6)
    csrr t0, sepc
    sd t0, 33*8(t6)
    csrr t0, scause
    sd t0, 34*8(t6)
    csrr t0, stval
    sd t0, 35*8(t6)
    # C ABI 传参:trap_handler(scause, sepc, stval, frame)
    csrr a0, scause
    csrr a1, sepc
    csrr a2, stval
    mv a3, t6
    # 处理器自身栈:帧下方预留 1KB(嵌套陷阱会压向更深处,仍在
    # 陷阱栈内)。若 handler 递归深度超过预留,写入会越过陷阱栈底
    # 进入引导栈顶 —— 仍是内核保留区,不破坏内核数据。
    addi sp, t6, -1024
    call trap_handler
    # trap_handler 返回 a0:
    #   a0 = 帧指针 → 恢复该帧并 sret 继续执行
    #   a0 = 0      → 不可恢复,停机
    beqz a0, 1f
    # ===== 恢复路径 =====
    mv t6, a0
    # 先把 sscratch 恢复为陷阱栈顶(帧底 + 帧大小 320),供下次 trap 使用。
    # 不这样做会导致陷阱栈顶每次 trap 都向下漂移,最终耗尽。
    addi t0, t6, 320
    csrw sscratch, t0
    # 恢复 sstatus:清掉 SIE 位再写回 —— sret 时硬件用 SPIE(硬件在
    # trap 入口置 1)恢复 SIE,从而"原子地"重新使能中断,避免在
    # sret 前被打断(被打断时帧已部分恢复,会产生双重恢复错误)。
    ld t0, 32*8(t6)
    andi t0, t0, -3
    csrw sstatus, t0
    ld t0, 33*8(t6)
    csrw sepc, t0
    # 恢复 GPR;t6 必须最后恢复(帧基址在 t6 中,先恢复会丢地址)。
    ld ra, 0*8(t6)
    ld sp, 1*8(t6)
    ld gp, 2*8(t6)
    ld tp, 3*8(t6)
    ld t0, 4*8(t6)
    ld t1, 5*8(t6)
    ld t2, 6*8(t6)
    ld s0, 7*8(t6)
    ld s1, 8*8(t6)
    ld a0, 9*8(t6)
    ld a1, 10*8(t6)
    ld a2, 11*8(t6)
    ld a3, 12*8(t6)
    ld a4, 13*8(t6)
    ld a5, 14*8(t6)
    ld a6, 15*8(t6)
    ld a7, 16*8(t6)
    ld s2, 17*8(t6)
    ld s3, 18*8(t6)
    ld s4, 19*8(t6)
    ld s5, 20*8(t6)
    ld s6, 21*8(t6)
    ld s7, 22*8(t6)
    ld s8, 23*8(t6)
    ld s9, 24*8(t6)
    ld s10, 25*8(t6)
    ld s11, 26*8(t6)
    ld t3, 27*8(t6)
    ld t4, 28*8(t6)
    ld t5, 29*8(t6)
    ld t6, 30*8(t6)
    sret
    # ===== 停机路径(trap_handler 返回 0)=====
1:
    j 1b

# ---------------------------------------------------------------------------
# cpu_state_asm:CpuState 快照读取(纯汇编实现)。
#
# 为什么不用 Rust 内联汇编?
#   在 asm! 中读取 ra/sp/gp/tp 而不声明操作数是 Rust 内联汇编的
#   形式 UB(编译器有权假设这些寄存器未被读写,可能被优化破坏)。
#   汇编函数 + C ABI 则完全在编译器的视野之外,结果确定。
#
# C ABI:void cpu_state_asm(CpuState *out) —— 指针在 a0。
# CpuState(repr(C))字段偏移与写入对应关系:
#   +0  ra  +8  sp  +16 gp  +24 tp
#   +32 sstatus  +40 sepc  +48 scause  +56 stval  +64 satp
# 注意:读到的 ra/sp 是"cpu_state_asm 自身被调用时"的值(panic 处理
# 上下文),而非故障点上下文 —— 故障点的忠实帧由 trap_vector 提供。
# ---------------------------------------------------------------------------
.align 2
.globl cpu_state_asm
cpu_state_asm:
    sd ra, 0(a0)
    sd sp, 8(a0)
    sd gp, 16(a0)
    sd tp, 24(a0)
    csrr t0, sstatus
    sd t0, 32(a0)
    csrr t0, sepc
    sd t0, 40(a0)
    csrr t0, scause
    sd t0, 48(a0)
    csrr t0, stval
    sd t0, 56(a0)
    csrr t0, satp
    sd t0, 64(a0)
    ret
