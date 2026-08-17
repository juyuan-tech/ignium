# RISC-V 64 架构层汇编(由 arch/riscv64.rs 以 global_asm 包含)。
#
# 本文件是 arch 隔离层的一部分:x86_64 移植时在 arch/x86_64 下
# 提供等价符号即可,通用代码不感知差异。

# ---------------------------------------------------------------------------
# trap_vector:异常/中断统一入口(stvec 指向此处)。
#
# 设计(基于 pro 审计 #2/#3 的重构):
#   - **陷阱栈**:sscratch(每 hart 一个 CSR,但当前存的是**全局**
#     单栈顶值)保存陷阱栈顶;入口用 `csrrw` 换出 t6 取得栈顶,
#     在陷阱栈上压入陷阱帧 —— 处理器绝不在被中断者的 sp 上运行
#     (栈溢出/用户态 sp 均无法危害内核)。
#     多核注意:多 hart 唤醒前必须把单栈换成 per-hart 栈数组
#     (H1,审计 11 轮),否则两个 hart 会在同一地址压帧互相覆盖。
#   - **嵌套语义(L2)**:handler 活跃期间的第二次 trap(嵌套)一律
#     直接停机 —— 当前不提供可恢复嵌套。帧基于陷阱栈压入是为未来
#     per-hart 应急栈预留的形态,不是已实现的嵌套能力。
#   - **完整帧**:保存全部 31 个通用寄存器 + 4 个关键 CSR。
#   - M0 阶段 trap_handler 输出诊断后停机;M1 扩展为中断分发 + sret。
#
# 帧布局(与 riscv64.rs 的 ABI 索引常量保持一致!改一侧必须改另一侧):
#   [0..31)  GPR x1..x31(索引 n 对应 x(n+1))
#   [32]     sstatus
#   [33]     sepc
#   [34]     scause
#   [35]     stval
#   (TRAP_FRAME_WORDS = 36,M1 若需扩展 CSR 槽位,须同步增加
#    TRAP_FRAME_WORDS 与 TRAP_FRAME_SIZE)
#
# 陷阱栈与引导栈在 linker.ld 中定义(均位于 _kernel_end 之后的保留区)。
# ---------------------------------------------------------------------------

# 陷阱帧大小(字节)= TRAP_FRAME_WORDS(36)*8 = 288。
# 与 riscv64.rs 的 TRAP_FRAME_WORDS 必须一致;入口预留与恢复路径
# 共用本常量,消除"两个魔法数漂移"风险(pro 审计 #2)。
.equ TRAP_FRAME_SIZE, 288

.section .text
.align 4                    # stvec 要求 4 字节对齐(直接模式下低 2 位必须为 0)
.globl trap_vector
trap_vector:
    # t6 ← 陷阱栈顶/外层帧底(sscratch),被中断的 t6 暂存于 sscratch。
    csrrw t6, sscratch, t6
    # 在陷阱栈上预留帧(t6 = 帧基址)。
    addi t6, t6, -TRAP_FRAME_SIZE
    # 先保存原始 t0(t0 即将被用作临时寄存器;顺序至关重要,
    # 任何"先 clobber 后保存"都会使 dump 显示假值)。
    sd t0, 4*8(t6)
    # 取出被中断的 t6(暂存在 sscratch)并写入帧槽 30。
    csrr t0, sscratch
    sd t0, 30*8(t6)
    # 嵌套检测(H2):第一层陷阱的帧底 == 陷阱栈顶 - 帧长。
    # 若不等 → handler 已活跃(第二层 trap)→ 停机。此时帧写入
    # 位置在外层 handler 活跃栈上方,无重叠;外层将停机,无害。
    # 只用已入帧的 t0(不能再用其他未保存寄存器)。
    # norelax(LOW#6):此 la 若被松弛成 gp 相对寻址,会用到尚未
    # 重载的(可能被中断上下文污染的)gp。
    .option norelax
    la t0, _trap_stack_top
    .option relax
    addi t0, t0, -TRAP_FRAME_SIZE
    bne t6, t0, 4f
    # 保存被中断的 gp,然后重载内核 gp(H1):此后任何可被松弛成
    # gp 相对的访存与 trap_handler 的全局访问都落在内核 gp 上;
    # 恢复路径会把被中断的 gp 还给原上下文。norelax 防自松弛。
    sd gp, 2*8(t6)
    .option norelax
    la gp, __global_pointer$
    .option relax
    # 更新陷阱栈顶 = 本帧底(供下一层嵌套检测使用)。
    csrw sscratch, t6
    sd ra, 0*8(t6)
    sd sp, 1*8(t6)
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
    # 处理器自身栈:帧下方预留 8KB(HIGH-3:实测 error! 输出链 < 1KB,
    # 但 Rust 格式化栈用随参数/优化浮动,2KB 不足以保证)。**强制校验
    # 余量在陷阱栈内**:若 sp 计算值越过陷阱栈底,说明栈已近耗尽
    # (嵌套过深),此时调用 handler 会把数据写进引导栈顶 —— 改为
    # 安全停机(pro 审计 max #2)。
    la t0, _trap_stack_bottom
    # -8192 超出 addi 立即数范围,用 li+add 组合。
    li t1, -8192
    add t1, t1, t6
    bltu t1, t0, 2f
    mv sp, t1
    call trap_handler
    # trap_handler 返回 a0:
    #   a0 = 帧指针 → 恢复该帧并 sret 继续执行
    #   a0 = 0      → 不可恢复,停机
    beqz a0, 1f
    # ===== 恢复路径 =====
    mv t6, a0
    # 先把 sscratch 恢复为陷阱栈顶(帧底 + 帧大小),供下次 trap 使用。
    # 不这样做会导致陷阱栈顶每次 trap 都向下漂移,最终耗尽。
    addi t0, t6, TRAP_FRAME_SIZE
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
    # ===== 陷阱栈耗尽路径(handler 头寸不足,安全停机)=====
2:
    csrci sstatus, 2
    wfi
    j 2b
    # ===== 嵌套陷阱路径(H2:handler 活跃时再次 trap)=====
    # 不触碰陷阱栈/帧,直接安全停机。
4:
    csrci sstatus, 2
    wfi
    j 4b

# ---------------------------------------------------------------------------
# context_switch:协作线程切换(调用边界语义)。
#
# 为什么只保存调用者保存寄存器(ra/sp/s0-s11)?
#   - 协作切换发生在函数调用点(yield 等):按 RISC-V ABI,
#     跨调用存活的是 ra/sp/s0-s11;t/a 寄存器跨调用可被丢弃,
#     被切换出去的线程恢复时不会依赖它们。
#   - 抢占切换不走本路径(走陷阱帧全量切换,见 sched.rs)。
#
# C ABI:void context_switch(Context *old, const Context *new)
# Context(repr(C))布局:
#   +0  ra  +8  sp  +16..+104  s0..s11
# ---------------------------------------------------------------------------
.align 2
.globl context_switch
context_switch:
    sd ra, 0(a0)
    sd sp, 8(a0)
    sd s0, 16(a0)
    sd s1, 24(a0)
    sd s2, 32(a0)
    sd s3, 40(a0)
    sd s4, 48(a0)
    sd s5, 56(a0)
    sd s6, 64(a0)
    sd s7, 72(a0)
    sd s8, 80(a0)
    sd s9, 88(a0)
    sd s10, 96(a0)
    sd s11, 104(a0)
    ld ra, 0(a1)
    ld sp, 8(a1)
    ld s0, 16(a1)
    ld s1, 24(a1)
    ld s2, 32(a1)
    ld s3, 40(a1)
    ld s4, 48(a1)
    ld s5, 56(a1)
    ld s6, 64(a1)
    ld s7, 72(a1)
    ld s8, 80(a1)
    ld s9, 88(a1)
    ld s10, 96(a1)
    ld s11, 104(a1)
    ret

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
