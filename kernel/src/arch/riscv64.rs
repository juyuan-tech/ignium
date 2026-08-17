//! RISC-V 64 架构实现(架构隔离层,契约见 arch/mod.rs)。
//!
//! 本模块是"通用代码唯一允许出现架构差异的地方"。x86_64 移植时
//! 新建 arch/x86_64.rs + 汇编,实现同样一组接口即可。

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::error;

// 架构汇编:引导陷阱向量(trap_vector)与 CPU 状态读取(cpu_state_asm)。
// 具体约定见 riscv64.S 头部注释。
global_asm!(include_str!("riscv64.S"));

/// 陷阱帧 GPR 索引:索引 n 对应 x(n+1)(与 riscv64.S 保存顺序一致)。
///
/// **ABI 常量契约**:汇编保存顺序与这里的索引必须同步修改,
/// 否则诊断 dump 会把寄存器标签全部错位(pro 审计 #1)。
mod gpr {
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
const TRAP_FRAME_WORDS: usize = 36;

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

extern "C" {
    // 纯汇编实现(riscv64.S)。为什么不用内联汇编:读取 ra/sp/gp/tp
    // 而不声明操作数在 Rust 内联汇编中是形式 UB,见 riscv64.S 注释。
    fn cpu_state_asm(out: *mut CpuState);
    // trap_vector 的符号地址(riscv64.S 中定义,16 字节对齐)。
    static trap_vector: u8;
    // 陷阱栈边界(linker.ld 定义):异常处理器的专用栈,帧压在其上。
    static _trap_stack_bottom: u8;
    static _trap_stack_top: u8;
}

/// 安装陷阱向量并初始化陷阱栈指针。
///
/// # Safety 说明(调用顺序要求)
/// 必须在 `uart::init` **之后**调用:stvec 装好后发生的 trap 会进入
/// `trap_handler` 输出日志,若串口尚未初始化,诊断输出将不可见。
/// 也必须在一切可能触发异常的用户代码之前调用(否则异常跳地址 0)。
pub fn init_traps() {
    unsafe {
        // sscratch = 陷阱栈顶;trap_vector 入口用它换出 t6 并在
        // 栈上压帧(多 hart 时改为 per-hart 栈,此处为单 hart 静态栈)。
        asm!(
            "la {tmp}, {top}",
            "csrw sscratch, {tmp}",
            tmp = out(reg) _,
            top = sym _trap_stack_top,
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

/// 定时器节拍间隔(10ms):由板级常量给出(QEMU virt = 10 MHz)。
pub const TIMER_INTERVAL: usize = crate::board::TIMER_INTERVAL;

/// 下一次定时器中断的截止时间(mtimer 周期)。
///
/// 用 **deadline 递增法**替代 `get_time() + INTERVAL`:后者把中断
/// 处理延迟(自截止到 handler 执行的时差)每个周期都累加进下次
/// 截止,产生持续漂移;前者只加固定间隔,节拍无累积误差。
/// 仅定时器 ISR 写入(fetch_add),启动时初始化一次。
static TIMER_DEADLINE: AtomicUsize = AtomicUsize::new(0);

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
pub fn enable_timer() {
    unsafe {
        // sie.STIE = bit 5(超级定时器中断使能)。
        // 注意:csrs/csrc 的立即数仅 5 位(0..31),位 5(0x20)必须
        // 走寄存器操作数形式。
        asm!("csrs sie, {stie}", stie = in(reg) 0x20usize, options(nomem, nostack));
    }
    let deadline = get_time() + TIMER_INTERVAL;
    TIMER_DEADLINE.store(deadline, Ordering::Relaxed);
    // 首次编程失败 = 内核失去节拍源(M1):warn+挂死不如明确失败,
    // panic 输出完整诊断后停机(CI 负向断言也能捕获)。
    assert!(
        crate::sbi::set_timer(deadline) == 0,
        "sbi_set_timer failed at boot"
    );
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
    let f = frame as usize;
    let bottom = (&raw const _trap_stack_bottom).addr();
    let top = (&raw const _trap_stack_top).addr();
    let frame_bytes = TRAP_FRAME_WORDS * 8;
    if top < bottom
        || top - bottom < frame_bytes
        || f < bottom
        || f > top - frame_bytes
        || !f.is_multiple_of(16)
    {
        // 帧指针非法(栈被破坏或入口异常):不再解引用,直接停机。
        error!(
            "FATAL: invalid trap frame {:#x} (trap stack [{:#x}, {:#x}))",
            f, bottom, top
        );
        halt()
    }

    if scause & INTERRUPT_BIT != 0 {
        // ===== 中断路径 =====
        match scause & !INTERRUPT_BIT {
            CAUSE_SUPERVISOR_TIMER => {
                // 定时器节拍:deadline 递增(无累积漂移)+ tick 计数
                // + 重排下一次中断。
                crate::logger::tick_up();
                let next =
                    TIMER_DEADLINE.fetch_add(TIMER_INTERVAL, Ordering::Relaxed) + TIMER_INTERVAL;
                // M1(审计 11 轮):SBI 失败 = 节拍源丢失(调度/看门狗
                // 都会挂死),ISR 内不可日志 —— 直接 panic 给出明确
                // 诊断,而不是静默停摆。
                assert!(
                    crate::sbi::set_timer(next) == 0,
                    "sbi_set_timer failed in timer ISR"
                );
                // 恢复被中断的上下文继续执行。
                frame
            }
            other => {
                // 未处理的中断:输出诊断并停机。
                error!("TRAP: unhandled interrupt cause={other}, sepc={sepc:#x}");
                dump_trap_frame(frame);
                halt()
            }
        }
    } else {
        // ===== 同步异常路径 =====
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
