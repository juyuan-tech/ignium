# RISC-V 64 架构层汇编(由 arch/riscv64.rs 以 global_asm 包含)。
#
# 本文件是 arch 隔离层的一部分:x86_64 移植时在 arch/x86_64 下
# 提供等价符号即可,通用代码不感知差异。

# ---------------------------------------------------------------------------
# trap_vector:异常/中断统一入口(stvec 指向此处)。
#
# 设计要点:
#   - 用 sscratch 换出 t6 获得陷阱帧基址,从而**不破坏被中断的 t6**
#     (M1 的 sret 恢复路径依赖帧的完整性)。
#   - 保存全部 31 个通用寄存器 + 4 个关键 CSR(sstatus/sepc/scause/stval)。
#   - M0 阶段 trap_handler 输出诊断后停机;M1 扩展为中断分发 + sret。
#
# TRAP_FRAME 布局(与 riscv64.rs 的声明保持一致):
#   [0..31)  GPR x1..x31(索引 30 为原始 t6)
#   [32]     sstatus
#   [33]     sepc
#   [34]     scause
#   [35]     stval
#   [36..40) 预留(M1 使用)
#
# ⚠ 已知限制(嵌套 trap):
#   若在 trap_handler 执行期间再次发生 trap(如 trap 代码自身的 bug),
#   入口的 `csrrw` 会把第一次 trap 的原始 t6(一个代码地址)换进 t6,
#   后续保存会写入该地址 —— 破坏代码段。
#   当前 M0 阶段不可达(trap_handler 只输出并停机,中断未使能)。
#   M1 使能中断时必须重构:sscratch 改为 per-hart **陷阱栈**指针,
#   帧挂在栈上(天然支持嵌套),或入口先检查帧占用标志。
# ---------------------------------------------------------------------------

.section .text
.align 4                    # stvec 要求 4 字节对齐(直接模式下低 2 位必须为 0)
.globl trap_vector
trap_vector:
    # 交换 t6 与 sscratch:t6 ← 陷阱帧基址,被中断的 t6 暂存于 sscratch。
    # 前置条件:init_traps() 已将 sscratch 初始化为 TRAP_FRAME 地址。
    csrrw t6, sscratch, t6
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
    # 恢复被中断的 t6(暂存于 sscratch);t0 已在 4*8(t6) 保存,可安全复用。
    csrr t0, sscratch
    sd t0, 30*8(t6)
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
    call trap_handler
    # trap_handler 返回类型为 !,此处不可达;防御性死循环。
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
