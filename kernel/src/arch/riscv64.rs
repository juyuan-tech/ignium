//! RISC-V 64 架构实现(架构隔离层,契约见 arch/mod.rs)。
//!
//! 本模块是"通用代码唯一允许出现架构差异的地方"。x86_64 移植时
//! 新建 arch/x86_64.rs + 汇编,实现同样一组接口即可。

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error;

// 架构汇编:引导陷阱向量(trap_vector)与 CPU 状态读取(cpu_state_asm)。
// 具体约定见 riscv64.S 头部注释。
global_asm!(include_str!("riscv64.S"));

/// 陷阱帧 GPR 索引:索引 n 对应 x(n+1)(与 riscv64.S 保存顺序一致)。
///
/// **ABI 常量契约**:汇编保存顺序与这里的索引必须同步修改,
/// 否则诊断 dump 会把寄存器标签全部错位(pro 审计 #1)。
///
/// **`pub`(M2 T2a)**:sched.rs 向阻塞线程 TCB 帧写入 IPC 结果
/// (a0/a1-a5)、D22 展开 ctx 到帧时按这些索引填槽 —— gpr 索引是
/// ABI 单一来源,跨模块复用同一常量,禁止在 sched/syscall 手写魔数。
pub mod gpr {
    pub const X_RA: usize = 0; // x1
    pub const X_SP: usize = 1; // x2
    pub const X_GP: usize = 2; // x3
    pub const X_TP: usize = 3; // x4
    pub const X_T0: usize = 4; // x5
    pub const X_T1: usize = 5; // x6
    pub const X_T2: usize = 6; // x7
    pub const X_S0: usize = 7; // x8
    pub const X_S1: usize = 8; // x9
    pub const X_A0: usize = 9; // x10
    pub const X_A1: usize = 10; // x11
    pub const X_A2: usize = 11; // x12
    pub const X_A3: usize = 12; // x13
    pub const X_A4: usize = 13; // x14
    pub const X_A5: usize = 14; // x15
    pub const X_A6: usize = 15; // x16
    pub const X_A7: usize = 16; // x17
    pub const X_S2: usize = 17; // x18
    pub const X_S3: usize = 18; // x19
    pub const X_S4: usize = 19; // x20
    pub const X_S5: usize = 20; // x21
    pub const X_S6: usize = 21; // x22
    pub const X_S7: usize = 22; // x23
    pub const X_S8: usize = 23; // x24
    pub const X_S9: usize = 24; // x25
    pub const X_S10: usize = 25; // x26
    pub const X_S11: usize = 26; // x27
    pub const X_T3: usize = 27; // x28
    pub const X_T4: usize = 28; // x29
    pub const X_T5: usize = 29; // x30
    pub const X_T6: usize = 30; // x31
}

// 陷阱帧 CSR 槽位(riscv64.S 同样保存这些)。
const CS_SSTATUS: usize = 32;
const CS_SEPC: usize = 33;
const CS_SCAUSE: usize = 34;
const CS_STVAL: usize = 35;
// 36 个槽 = 31 GPR + 4 CSR + 1 个未用槽(索引 31,对齐填充;LOW#8)。
/// 陷阱帧槽位数:31 GPR + 4 CSR + 1 填充槽(索引 31;LOW#8)。
/// **对外 pub**:sched.rs 的 Thread.frame 尺寸引用本常量
/// (CRITICAL-1:此前各自维护,40 vs 36 导致越界读)。
pub const TRAP_FRAME_WORDS: usize = 36;

// 编译期锁定帧尺寸:riscv64.S 用 .equ TRAP_FRAME_SIZE, 288 与之对应
// (汇编无法引用 Rust 常量,用断言防止两侧漂移,pro 审计 max #4)。
const _: () = assert!(TRAP_FRAME_WORDS * 8 == 288);

/// CPU 寄存器快照,panic/诊断输出用。
///
/// `#[repr(C)]`:字段顺序与 `cpu_state_asm` 的写入偏移一一对应
/// (汇编按固定偏移写,编译器不得重排字段)。
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

unsafe extern "C" {
    // 纯汇编实现(riscv64.S)。为什么不用内联汇编:读取 ra/sp/gp/tp
    // 而不声明操作数在 Rust 内联汇编中是形式 UB,见 riscv64.S 注释。
    fn cpu_state_asm(out: *mut CpuState);
    // 协作线程切换(riscv64.S):保存调用者保存寄存器,加载新上下文。
    // 安全调用者须保证指针有效且中断关闭(由 sched 封装保证)。
    // LOW-2(审计 17 轮):unsafe 声明 —— 安全代码不得以无效指针调用。
    pub unsafe fn context_switch(old: *mut Context, new: *const Context);
    // 从全量陷阱帧恢复并以 sret 进入(riscv64.S;与 trap_vector 恢复
    // 路径相同,含 sscratch 恒置陷阱栈顶)。用于恢复**仅帧有效**
    // 的线程(被抢占后未再 yield 的线程)。永不返回。
    // C2(审计 18 轮外部):新增 old_ctx 参数,恢复前保存当前上下文。
    pub unsafe fn frame_restore(frame: *mut usize, old_ctx: *mut Context);
    // trap_vector 的符号地址(riscv64.S 中定义,16 字节对齐)。
    static trap_vector: u8;
    // per-hart 陷阱栈数组基址(D7,linker.ld 定义)。
    // 数组基址 32K 对齐;sscratch 恒在数组内(槽顶 T_h 或帧基址),
    // hartid = (sscratch - _trap_stack_base) >> 15。
    static _trap_stack_base: u8;
}

/// 陷阱槽常量(D7 per-hart)。与 linker.ld 的 TRAP_STRIDE/MAX_HARTS 必须一致。
/// 每槽 stride 32K = 16K 守护 + 16K 栈(stride 为 2 的幂 → 移位可寻址)。
pub const TRAP_STRIDE: usize = 32 * 1024;
/// 每槽守护区大小(MMU 不映射)。
pub const TRAP_GUARD: usize = 16 * 1024;
/// 最大 hart 数(QEMU -smp 4;超限 hart 在 entry.S 停 park)。
pub const MAX_HARTS: usize = 4;

/// 协作线程上下文:仅调用者保存寄存器(ra/sp/s0-s11)。
/// `#[repr(C)]` 与 context_switch 汇编的偏移一致。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Context {
    pub ra: usize,
    pub sp: usize,
    pub s: [usize; 12],
}

/// 陷阱栈数组基址(linker.ld;取地址仅为算术,不解引用)。
#[inline]
fn trap_stack_base() -> usize {
    (&raw const _trap_stack_base).addr()
}

/// 本 hart 陷阱槽顶 `T_h` = base + (hartid+1)*32K。
#[inline]
fn hart_trap_top(h: usize) -> usize {
    trap_stack_base() + (h + 1) * TRAP_STRIDE
}

/// 读取 sscratch(陷阱期间 = 本 hart 帧基址;陷阱外 = 本 hart 槽顶 T_h)。
#[inline]
pub fn read_sscratch() -> usize {
    let v: usize;
    unsafe {
        asm!("csrr {}, sscratch", out(reg) v, options(nomem, nostack));
    }
    v
}

/// 当前 hartid(D7):sscratch 恒在本 hart 陷阱槽内(槽顶 T_h 或首层帧底),
/// 两者偏移 >> 15 均给出本槽序号 —— 仅"恰在槽顶"(偏移 mod 32K == 0,
/// 即 T_h = base+(h+1)*32K)需减 1。陷阱处理中(帧基址)与内核上下文
/// (T_h)都可靠,**不依赖 tp**(用户线程会改 tp)。
#[inline]
pub fn hartid() -> usize {
    let off = read_sscratch().wrapping_sub(trap_stack_base());
    let h = off >> 15;
    if off & 0x7FFF == 0 {
        h.wrapping_sub(1)
    } else {
        h
    }
}

/// 安装陷阱向量并初始化本 hart 的陷阱栈指针。
///
/// # 参数
/// `hartid`:当前 hart 编号(kernel_main / secondary_main 传入;调用
/// 时 sscratch 尚未初始化,无法自推导)。
///
/// # Safety 说明(调用顺序要求)
/// 必须在 `uart::init` **之后**调用:stvec 装好后发生的 trap 会进入
/// `trap_handler` 输出日志,若串口尚未初始化,诊断输出将不可见。
/// 也必须在一切可能触发异常的用户代码之前调用(否则异常跳地址 0)。
pub fn init_traps(hartid: usize) {
    unsafe {
        // sscratch = 本 hart 槽顶 T_h;trap_vector 入口用它换出 t6 并在
        // 本槽栈上压帧(D7 per-hart 数组)。
        asm!(
            "csrw sscratch, {top}",
            top = in(reg) hart_trap_top(hartid),
            options(nostack)
        );
        // stvec 直接模式:低 2 位必须为 0,指向 4 字节对齐的入口
        // (trap_vector 在汇编中以 .align 4 = 16 字节对齐)。
        asm!(
            "csrw stvec, {}",
            in(reg) (&raw const trap_vector).addr(),
            options(nomem, nostack)
        );
    }
}

/// 重置 sscratch 为**当前 hart** 的陷阱槽顶(用于从 trap 上下文 exit 前
/// 确保切换后的目标线程下次 trap 的嵌套检测不会误判)。当前 hart 由
/// sscratch 自身推导(陷阱期间 = 帧基址,内核上下文 = T_h,均可靠)。
#[inline]
pub fn set_sscratch_trap_top() {
    let top = hart_trap_top(hartid());
    unsafe {
        asm!(
            "csrw sscratch, {top}",
            top = in(reg) top,
            options(nostack)
        );
    }
}

/// 读取 CPU 寄存器快照(委托给汇编实现,见 riscv64.S)。
///
/// 注意:读到的 `ra`/`sp` 是**当前调用上下文**(panic 处理器自身),
/// 不是故障点上下文;故障点的忠实寄存器帧由陷阱栈上的帧提供
/// (trap_handler 打印)。字段含义用于横向参考。
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
    unsafe { cpu_state_asm(&mut s) };
    s
}

/// 关闭全局中断(S 模式:清除 sstatus.SIE,位 1)。
///
/// 使用显式立即数形式 `csrci`(而非 `csrc` + 立即数):`csrc/csrs`
/// 是寄存器形式伪指令,整数操作数依赖汇编器推断,跨汇编器行为
/// 存在歧义(pro 审计 #1 争议点);`csrci/csrsi` 语义唯一。
///
/// 注意:不带 `nomem` —— 中断状态切换必须作为编译器的内存屏障,
/// 防止指令被重排跨越开关中断点(pro 审计 #5)。
#[inline]
pub fn irq_disable() {
    unsafe {
        asm!("csrci sstatus, 2", options(nostack));
    }
}

/// 打开全局中断(S 模式:置位 sstatus.SIE,位 1)。
/// 调用前必须已配置好所有中断源与 trap 向量,否则中断可能打到
/// 未初始化路径。屏障语义同 `irq_disable`。
#[inline]
pub fn irq_enable() {
    unsafe {
        asm!("csrsi sstatus, 2", options(nostack));
    }
}

/// 保存中断使能状态并关闭中断(IRQ 安全锁的基础)。
/// 返回:原 SIE 是否开启。
///
/// MED-12(审计 15 轮):必须用 `csrrci` **原子读-清** —— 分立的
/// `csrr + csrci` 之间存在窗口:csrr 读到 SIE=1 后、csrci 执行前,
/// 中断可能到达(ISR 在"宣称关闭"前运行,可能持锁死锁)。
#[inline]
pub fn irq_save() -> bool {
    let s: usize;
    unsafe {
        asm!("csrrci {}, sstatus, 2", out(reg) s, options(nostack));
    }
    s & 2 != 0
}

/// 按 `irq_save` 的返回值恢复中断使能状态。
#[inline]
pub fn irq_restore(on: bool) {
    if on {
        unsafe {
            asm!("csrsi sstatus, 2", options(nostack));
        }
    }
}

/// 置 `sstatus.SUM`(位 18):S 模式允许访问 U 页(用户缓冲拷贝用)。
///
/// 调用方须在拷贝完成后用 `clear_sum` 还原;trap 恢复路径会把 sstatus
/// 写回 trap 入口保存的原值,临时置位不会泄漏进被中断的用户上下文。
/// 仅在 SIE=0 的 trap 处理上下文使用(本 kernel 的 syscall 处理即此),
/// 拷贝期间不会被抢占。
#[inline]
pub fn set_sum() {
    unsafe {
        asm!("csrs sstatus, {}", in(reg) (1 << 18), options(nostack));
    }
}

/// 清 `sstatus.SUM`(位 18):恢复默认(禁止 S 模式访问 U 页)。
#[inline]
pub fn clear_sum() {
    unsafe {
        asm!("csrc sstatus, {}", in(reg) (1 << 18), options(nostack));
    }
}

/// 读取 mtimer 计数器(S 模式经 OpenSBI 委托,`csrr time` 直读)。
/// 单位:OpenSBI 平台时钟周期(QEMU virt = 10 MHz)。
#[inline]
pub fn get_time() -> usize {
    let t: usize;
    unsafe {
        asm!("csrr {}, time", out(reg) t, options(nomem, nostack));
    }
    t
}

/// 定时器节拍间隔(10ms)。
#[inline]
pub fn timer_interval() -> usize {
    crate::board::timer_interval()
}

/// 下一次定时器中断的截止时间(mtimer 周期)。
///
/// 用 **deadline 递增法**替代 `get_time() + INTERVAL`:后者把中断
/// 处理延迟(自截止到 handler 执行的时差)每个周期都累加进下次
/// 截止,产生持续漂移;前者只加固定间隔,节拍无累积误差。
/// 仅定时器 ISR 写入(fetch_add),启动时初始化一次。
/// D7:per-hart —— 每核独立节拍(索引 = 当前 hart,见 `enable_timer`
/// 与 ISR 路径)。
static TIMER_DEADLINE: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];

/// D17(自审推进):是否使用 SSTC(stimecmp)直写定时器。
/// - `enable_timer` 首次用 SBI(通用,无非法指令风险);
/// - FDT 解析后(cpu::init_from_fdt 读 riscv,isa)检测 `sstc`,
///   有则切回 stimecmp(性能),无则保持 SBI 回退。
///   与 sbi::set_timer 形成双路径;QEMU virt 有 sstc → 走直写。
static USE_SSTC: AtomicBool = AtomicBool::new(false);

/// SBI 定时器失败已被记录(只告警一次,避免 ISR 刷屏)。
static SBI_TIMER_FAILED_LOGGED: AtomicBool = AtomicBool::new(false);

/// 设置是否使用 SSTC 定时器路径(FDT ISA 解析后调用)。
pub fn set_sstc(v: bool) {
    USE_SSTC.store(v, Ordering::Relaxed);
}

/// 当前是否为 SSTC 定时器路径(M2 多核探测用)。
#[allow(dead_code)]
pub fn sstc_available() -> bool {
    USE_SSTC.load(Ordering::Relaxed)
}

/// 重排下一次定时器截止:按能力选择 stimecmp(直写)或 SBI set_timer。
///
/// SBI 失败即无后续定时器中断 → 抢占/心跳停摆。首次编程失败在
/// `enable_timer` panic(V4 外部审计 MED);tick 路径失败用**一次性**
/// rate-limited ERROR(ISR 日志例外:系统即将冻结,终局诊断)。
#[inline]
fn arm_timer(deadline: usize) {
    if USE_SSTC.load(Ordering::Relaxed) {
        set_stimecmp(deadline);
    } else {
        let rc = crate::sbi::set_timer(deadline);
        if rc != 0 && !SBI_TIMER_FAILED_LOGGED.swap(true, Ordering::Relaxed) {
            crate::error!("SBI set_timer failed (rc={rc}); timer will stop");
        }
    }
}

/// 清洗中断/特权相关 CSR 到已知安全状态(pro 审计 max #1)。
///
/// 背景:引导器或热重启可能遗留 `sie`/`sip` 的置位位与 `sstatus`
/// 的保护相关位(SUM/MXR/FS)。`enable_timer` 用 OR 设置 STIE 会
/// 保留这些残留 —— 未处理的中断会在 `irq_enable` 后立刻触发停机。
///
/// 必须在 `enable_timer` 之前调用(在 `kernel_main` 早期)。
pub fn sanitize_csr() {
    unsafe {
        // 关闭全部超级中断使能(含可能的残留位)。
        asm!("csrw sie, zero", options(nostack));
        // 清可写的挂起位:SSIP(位1)、STIP(位5);SEIP(位9)为
        // 只读,由硬件管理,无法也不应在此清除。
        asm!("csrw sip, zero", options(nostack));
        // 清 sstatus 保护相关位:SUM(位18)+ MXR(位19)+ FS(位13-14)。
        // 高位超出 csrci 5 位立即数范围,必须用寄存器形式 csrc
        // (寄存器形式无歧义,正是 csrc 的本义)。
        asm!(
            "csrc sstatus, {mask}",
            mask = in(reg) 0xC6000usize,
            options(nostack)
        );
    }
}

/// 使能超级定时器中断(STIE)并编程第一次定时器中断。
///
/// 调用顺序要求:必须在 `init_traps` 之后(否则定时器中断无陷阱向量
/// 可达);在 `irq_enable` 之前(先布好中断源,再开全局中断)。
///
/// D17(自审推进):首次定时器一律走 **SBI set_timer**(通用,任意平台
/// 可用),不用 stimecmp —— 无 SSTC 平台执行 `csrw stimecmp` 会触发
/// 非法指令陷阱 → 停机(原 HIGH-2 探测 panic 不可优雅降级)。FDT
/// 解析后 `cpu::init_from_fdt` 检测 `riscv,isa` 中是否含 `sstc`,
/// 含则 `set_sstc(true)`,此后 ISR 切回 stimecmp 直写。
pub fn enable_timer() {
    unsafe {
        // sie.STIE = bit 5(超级定时器中断使能)。
        // 注意:csrs/csrc 的立即数仅 5 位(0..31),位 5(0x20)必须
        // 走寄存器操作数形式。
        // 屏障语义同 irq_*(无 nomem):中断源切换必须作为编译器
        // 内存屏障,防止与后续 ecall 重排。
        asm!("csrs sie, {stie}", stie = in(reg) 0x20usize, options(nostack));
        // sie.SSIP = bit 1(D19:跨核调度唤醒 —— SBI IPI 送达 S 模式
        // 软中断;须在每核开中断前使能,否则 IPI 无法唤醒 wfi 中的
        // idle,target 核的唤醒只能等下一个定时器 tick)。
        asm!("csrs sie, {ssip}", ssip = in(reg) 2usize, options(nostack));
    }
    let deadline = get_time().wrapping_add(timer_interval());
    // D7:按当前 hart(sscratch = T_h,自推导可靠)索引 per-hart 槽。
    TIMER_DEADLINE[hartid()].store(deadline, Ordering::Relaxed);
    // 首次:USE_SSTC 尚为 false,走 SBI(通用)。
    let rc = crate::sbi::set_timer(deadline);
    // V4(外部审计 MED):首次编程失败 → 定时器永不触发,明确 panic
    // (呼应 D-旧"boot 定时器失败 warn 改 panic")。
    assert!(
        rc == 0,
        "SBI set_timer failed at boot (rc=0x{rc:x}); no timer interrupts possible"
    );
}

/// 直写 stimecmp(需平台支持 SSTC 扩展)。
///
/// V4(外部审计 LOW,文档同步):仅当 D17 检测到 `sstc` 时经 `arm_timer`
/// 使用;无 SSTC 平台走 SBI 回退,不再有"写读回断言"(原 enable_timer
/// 探测已移除,首次定时器一律 SBI)。
#[inline]
fn set_stimecmp(next: usize) {
    unsafe {
        asm!("csrw stimecmp, {}", in(reg) next, options(nostack));
    }
}

/// 空闲等待:wfi 令 CPU 进入低功耗等待;若中断被使能,等待可被唤醒。
#[inline]
pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) }
}

/// 停机:先关中断再反复 wfi。用于 panic 等不可恢复场景,防止
/// 输出过程被中断打断、日志被污染(pro 审计 #12:原实现未强制关中断)。
pub fn halt() -> ! {
    irq_disable();
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) }
    }
}

/// 中断位(scause 最高位):置 1 表示中断,置 0 表示同步异常。
const INTERRUPT_BIT: usize = 1 << (usize::BITS - 1);

/// 超级定时器中断 cause 编号(RISC-V 特权规范)。
const CAUSE_SUPERVISOR_TIMER: usize = 5;
/// D19:超级软中断 cause 编号(SBI IPI → S 模式软中断,跨核调度唤醒)。
/// RISC-V 特权规范:中断 cause **1** = S 模式软中断(cause 3 是 M 模式
/// 软中断,误写 3 会让 SSIP 落入 unhandled 分支直接停机 —— T3b 实测修复)。
const CAUSE_SUPERVISOR_SOFTWARE: usize = 1;
/// 用户态环境调用(ecall / M2 T1 系统调用入口)。
const CAUSE_ECALL_FROM_U: usize = 8;

/// 陷阱处理入口,由 trap_vector(汇编)以 C ABI 调用。
///
/// # 参数
/// - `scause`:异常/中断原因编码(最高位为 1 表示中断)。
/// - `sepc`:触发 trap 的指令地址(同步异常时为故障指令)。
/// - `stval`:trap 相关附加信息(如非法访存地址)。
/// - `frame`:陷阱栈上帧的基址(布局见 riscv64.S 头部注释)。
///
/// # 返回值(汇编恢复路径使用)
/// - 返回 `frame`(非空):恢复该帧并 `sret` 继续执行被中断的上下文。
/// - 返回 `null` / 停机:不恢复,直接停机(不可恢复故障)。
///
/// # Safety
/// `frame` 必须指向陷阱栈范围内的有效帧(由汇编压入);调用前会
/// 校验边界,非法指针直接停机而不是解引用(pro 审计 #7)。
///
/// # 中断上下文约束
/// 处理器在 trap 入口自动清除 SIE,本函数执行期间中断保持关闭,
/// 因此不会嵌套(嵌套异常仍由陷阱栈吸收)。定时器中断处理路径
/// **禁止日志输出**(无锁日志不允许 ISR 交错,见 logger 模块注释)。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn trap_handler(
    scause: usize,
    sepc: usize,
    stval: usize,
    frame: *mut usize,
) -> *mut usize {
    // 帧指针完整性校验(L2):用减法避免 `f + 帧长` 在损坏指针上
    // 溢出回绕(旧写法 f+288 可回绕越过 top,导致 OOB 解引用)。
    // 必须:帧在陷阱栈内、容纳整个帧、16 字节对齐。
    // D7:帧界检查改 per-hart 槽 —— 当前 hart 由 sscratch 推导
    // (陷阱期间 sscratch = 帧基址,可靠);槽 = [base + h*32K,
    // base + (h+1)*32K),栈区在其后 16K,帧贴槽顶,界内检查成立。
    let f = frame as usize;
    let frame_bytes = TRAP_FRAME_WORDS * 8;
    let h = hartid();
    let base = trap_stack_base();
    let bottom = base + h * TRAP_STRIDE;
    let top = bottom + TRAP_STRIDE;
    if top < bottom
        || top - bottom < frame_bytes
        || f < bottom
        || f > top - frame_bytes
        || !f.is_multiple_of(16)
    {
        // 帧指针非法(栈被破坏或入口异常):不再解引用,直接停机。
        error!(
            "FATAL: invalid trap frame {:#x} (hart {h} trap slot [{:#x}, {:#x}))",
            f, bottom, top
        );
        halt()
    }

    if scause & INTERRUPT_BIT != 0 {
        // ===== 中断路径 =====
        match scause & !INTERRUPT_BIT {
            CAUSE_SUPERVISOR_TIMER => {
                // 定时器节拍:deadline 递增(无累积漂移)+ tick 计数
                // + 重排下一次中断。wrapping_add(MED-4):overflow-checks
                // 开启下,极端时间(理论 18 万年后)不触发 panic。
                // SSTC 直写 stimecmp(替代 ecall,见 enable_timer)。
                // M7(审计 18 轮外部):若中断长时间关闭后 deadline 严重
                // 落后,用 max(now + interval) 防止 tick 风暴。
                crate::logger::tick_up();
                // 自审:interval 每 tick 单独读一次(原 3 次原子读)。
                // D7:按当前 hart(陷阱期间 sscratch = 帧基址,自推导可靠)
                // 索引 per-hart deadline 槽。
                let interval = timer_interval();
                let h = hartid();
                let ideal = TIMER_DEADLINE[h]
                    .fetch_add(interval, Ordering::Relaxed)
                    .wrapping_add(interval);
                let now = get_time();
                // 自审:now + interval 改用 wrapping —— 与 TIMER_DEADLINE 的
                // wrapping_add 一致,极端时间(10MHz 下 ~5.8 万年后)不 panic。
                let next = if ideal < now {
                    now.wrapping_add(interval)
                } else {
                    ideal
                };
                TIMER_DEADLINE[h].store(next, Ordering::Relaxed);
                // D17:按能力选 stimecmp 或 SBI 重排下一次中断。
                arm_timer(next);
                // 抢占决策:时间片到期且存在就绪线程时,返回下一线程
                // 的帧指针,汇编恢复路径据此 sret 进入新线程
                // (全寄存器恢复,含 t/a)。D19:按当前核(本函数早前已
                // 经 sscratch 推导 h)做 per-CPU 抢占。
                unsafe { crate::sched::on_tick(frame, h) }
            }
            CAUSE_SUPERVISOR_SOFTWARE => {
                // D19:跨核调度唤醒 —— wake() 对 idle 目标核发 SBI IPI,
                // 本核从 wfi 醒来收到 S 软中断。仅清挂起位后返回原帧
                // (wfi 被唤醒,idle 循环会重查本核就绪队列);不在此调度。
                // 必须清 sip.SSIP,否则挂起位使下次 wfi 立即返回(忙转)。
                unsafe {
                    asm!("csrc sip, {}", in(reg) 2usize, options(nostack));
                }
                frame
            }
            other => {
                // 未处理的中断:输出诊断并停机。
                error!("TRAP: unhandled interrupt cause={other}, sepc={sepc:#x}");
                dump_trap_frame(frame);
                halt()
            }
        }
    } else if scause == CAUSE_ECALL_FROM_U {
        // M2 T1:用户态系统调用(U 模式 ecall;S 模式 ecall 由 medeleg
        // 保留给 M,不会回到本向量)。同步异常路径(中断分支已 return)。
        // S2(本轮安全加固):防御纵深 —— 若 medeleg 被错误配置/改动,
        // scause=8 也可能来自 S 模式(SPP=1)。内核线程误入用户 syscall
        // (尤其 EXIT)会破坏调度器状态,拒绝并 fail-loudly。
        if unsafe { *frame.add(CS_SSTATUS) } & (1 << 8) != 0 {
            error!("FATAL: ecall from S-mode (SPP=1, sepc={sepc:#x}); refusing user syscall");
            dump_trap_frame(frame);
            halt()
        }
        if unsafe { crate::syscall::handle(frame) } {
            // 用户请求退出:exit_from_trap 在陷阱上下文直接切换,
            // 永不返回(见 sched.rs 该函数注释:必须走帧恢复,否则
            // sscratch 残留导致下一次 trap 嵌套检测误判而停机)。
            crate::sched::exit_from_trap();
        }
        // handle 已写入结果(帧 a0)并前移 sepc;sret 回用户态继续。
        frame
    } else {
        // ===== 其它同步异常 =====
        // M2 D12:按来源分派 —— 用户态故障(SPP=0)杀进程并切走(不返回);
        // 内核态故障(SPP=1)仍是内核 bug,保持 dump + halt。D12 的日志
        // 用 "D12:" 前缀而非 "TRAP:" —— 门禁把后者当内核故障标志。
        if unsafe { *frame.add(CS_SSTATUS) } & (1 << 8) == 0 {
            crate::sched::kill_current_process(scause, sepc, stval);
        }
        error!("TRAP: exception scause={scause:#x} sepc={sepc:#x} stval={stval:#x}");
        dump_trap_frame(frame);
        halt()
    }
}

/// 输出陷阱帧完整寄存器 dump(故障点的忠实快照)。
fn dump_trap_frame(frame: *mut usize) {
    use gpr::*;

    let regs = unsafe { core::slice::from_raw_parts(frame, TRAP_FRAME_WORDS) };
    error!(
        "ra={:#x} sp={:#x} gp={:#x} tp={:#x}",
        regs[X_RA], regs[X_SP], regs[X_GP], regs[X_TP]
    );
    error!(
        "t0={:#x} t1={:#x} t2={:#x} s0={:#x} s1={:#x}",
        regs[X_T0], regs[X_T1], regs[X_T2], regs[X_S0], regs[X_S1]
    );
    error!(
        "a0={:#x} a1={:#x} a2={:#x} a3={:#x}",
        regs[X_A0], regs[X_A1], regs[X_A2], regs[X_A3]
    );
    error!(
        "a4={:#x} a5={:#x} a6={:#x} a7={:#x}",
        regs[X_A4], regs[X_A5], regs[X_A6], regs[X_A7]
    );
    error!(
        "s2={:#x} s3={:#x} s4={:#x} s5={:#x}",
        regs[X_S2], regs[X_S3], regs[X_S4], regs[X_S5]
    );
    error!(
        "s6={:#x} s7={:#x} s8={:#x} s9={:#x}",
        regs[X_S6], regs[X_S7], regs[X_S8], regs[X_S9]
    );
    error!(
        "s10={:#x} s11={:#x} t3={:#x} t4={:#x}",
        regs[X_S10], regs[X_S11], regs[X_T3], regs[X_T4]
    );
    error!("t5={:#x} t6={:#x}", regs[X_T5], regs[X_T6]);
    error!(
        "sstatus={:#x} sepc_f={:#x} scause_f={:#x} stval_f={:#x}",
        regs[CS_SSTATUS], regs[CS_SEPC], regs[CS_SCAUSE], regs[CS_STVAL]
    );
}
