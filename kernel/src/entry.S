# Ignium 引导入口(RISC-V 64,S 模式,OpenSBI 跳转到 0x80200000 进入此处)。
#
# 寄存器约定:
#   a0 = hartid(OpenSBI 传入,保留到 `call kernel_main`)
#   a1 = FDT 指针(OpenSBI 传入,保留到 `call kernel_main`)
#
# 启动顺序:
#   1) 清 sstatus.SIE(遗留引导器可能开着中断)
#   2) tp = hartid(per-hart 状态约定)
#   3) stvec = early_trap stub + sscratch 清零(早期陷阱安全停机)
#   4) satp 清零(Bare 模式)+ sfence.vma 冲刷 TLB
#   5) 初始化 gp(norelax 防自松弛)
#   6) **引导权仲裁**:BOOT_LOCK 原子标志竞争 —— boot hart 不一定是
#      hart 0(QEMU -smp 4 实测 boot hart = 3);赢得仲裁者执行
#      BSS 清零与引导,其余停车(防全员停车/重复清零)。
#   7) 引导者:清零 BSS(含栈区)→ 设栈 → kernel_main

.section .text.entry
.globl _start
_start:
    # 清除全局中断使能(csrci 立即数形式,无跨汇编器歧义):
    # 在第一条 S 模式指令就确定中断状态,避免 BSS 清零期间被遗留
    # 中断打断(pro 审计 LOW)。
    csrci sstatus, 2
    # tp = hartid:RISC-V 内核惯例(多核 per-hart 状态的基础)。
    # 后续多核代码以 tp 索引 per-hart 陷阱栈/上下文,当前阶段无使用方,
    # 但提前建立约定避免迁移成本。
    mv tp, a0
    # 清洗引导器遗留的陷阱状态(L1):安装"早期停机 stub"并清 sscratch
    # (在 init_traps() 之前任何异常 → 关中断停机,M3 陷阱窗口)。
    .option norelax
    la t0, early_trap
    .option relax
    csrw stvec, t0
    csrw sscratch, zero
    # 若上一级引导(或 SBI)遗留了分页开启状态,不清 satp 会带着
    # 残留页表执行,立即崩溃。写入 0 = Bare 模式(直接物理寻址)。
    csrw satp, zero
    # satp 切换后必须冲刷 TLB;`fence` 只是指令序屏障,不是 TLB 冲刷。
    sfence.vma zero, zero
    # L10(审计 18 轮外部):I-cache 冲刷,防热重启时取指陈旧代码。
    fence.i

    # gp = __global_pointer$:数据量增长后链接器会生成 gp 相对寻址,
    # 必须在早期初始化。norelax 块防止本指令自身被松弛为 gp 相对
    # 寻址(那样会变成 no-op,gp 仍是垃圾值)。
    .option norelax
    la gp, __global_pointer$
    .option relax

    # ===== 引导权仲裁 =====
    # amoswap.w.aq rd, rs2, (rs1):rd = 旧值,内存 = rs2。
    # BOOT_LOCK 初始为 0(在 .data,镜像加载即就绪);返回 0 者
    # 赢得引导权(原子性保证只有一个赢家),其余立即停车。
    # .option arch:LLVM 把 'a' 细分为 zaamo/zalrsc,需就地启用。
    .option arch, +zaamo
    .option norelax
    la t0, BOOT_LOCK
    .option relax
    li t1, 1
    amoswap.w.aq t1, t1, 0(t0)
    # 赢家继续;输家停车(不触碰任何共享内存)。
    bnez t1, park
    # 仲裁的 acquire 语义之后,用全 fence 保证后续 BSS 清零对
    # 所有 hart 可见(真实多核平台的存储顺序)。
    fence rw, rw

    # 清零 [ _bss_start, _trap_stack_top ),8 字节步进。
    # 覆盖:BSS + 引导栈 + 陷阱栈 —— 三处全部归零(M6:陷阱栈清零
    # 使 trap 栈上的水位诊断成立;引导栈清零是 panic 水位前提)。
    # 两端都在链接脚本中按 16 字节对齐,循环无越界风险。
    la t0, _bss_start
    la t1, _trap_stack_top
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

# 早期陷阱停机 stub(init_traps 之前由 stvec 指向):
# 引导期任何异常都不可恢复,直接关中断停机,避免跳到地址 0。
.align 2
early_trap:
    csrci sstatus, 2
    wfi
    j early_trap
