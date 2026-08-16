//! RISC-V 64 架构实现(架构隔离层,契约见 arch/mod.rs)。
//!
//! 本模块是"通用代码唯一允许出现架构差异的地方"。x86_64 移植时
//! 新建 arch/x86_64.rs + 汇编,实现同样一组接口即可。

use core::arch::{asm, global_asm};

use crate::error;

// 架构汇编:引导陷阱向量(trap_vector)与 CPU 状态读取(cpu_state_asm)。
// 具体约定见 riscv64.S 头部注释。
global_asm!(include_str!("riscv64.S"));

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
}

/// 陷阱帧:trap_vector(汇编)的寄存器保存区。
///
/// 布局(与 riscv64.S 一致,修改必须同步):
///   [0..31)  GPR x1..x31(索引 30 = 原始 t6)
///   [32]     sstatus  [33] sepc  [34] scause  [35] stval
///   [36..40) 预留(M1:sret 恢复路径可能扩展)
///
/// 当前为静态缓冲(单核假设);M1 多 hart 化时改为 per-hart 数组,
/// 并用 sscratch 保存 hart 私有帧指针。
#[unsafe(no_mangle)]
pub static mut TRAP_FRAME: [usize; 40] = [0; 40];

/// 安装陷阱向量并初始化陷阱帧指针。
///
/// # Safety 说明(调用顺序要求)
/// 必须在 `uart::init` **之后**调用:stvec 装好后发生的 trap 会进入
/// `trap_handler` 输出日志,若串口尚未初始化,诊断输出将不可见。
/// 也必须在一切可能触发异常的用户代码之前调用(否则异常跳地址 0)。
pub fn init_traps() {
    unsafe {
        // sscratch 保存陷阱帧基址,供 trap_vector 入口的 `csrrw` 换出使用。
        // 注意:当前为单 hart 静态帧;多 hart 时此处改为 per-hart 帧地址。
        asm!(
            "la {tmp}, {frame}",
            "csrw sscratch, {tmp}",
            tmp = out(reg) _,
            frame = sym TRAP_FRAME,
            options(nostack)
        );
        // stvec 直接模式:低 2 位必须为 0,指向 4 字节对齐的入口
        // (trap_vector 在汇编中以 .align 4 = 16 字节对齐)。
        asm!(
            "csrw stvec, {}",
            in(reg) &trap_vector as *const u8 as usize,
            options(nomem, nostack)
        );
    }
}

/// 读取 CPU 寄存器快照(委托给汇编实现,见 riscv64.S)。
///
/// 注意:读到的 `ra`/`sp` 是**当前调用上下文**(panic 处理器自身),
/// 不是故障点上下文;故障点的忠实寄存器帧由 `TRAP_FRAME` 提供
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
#[inline]
pub fn irq_disable() {
    unsafe {
        asm!("csrc sstatus, {imm}", imm = const 2, options(nomem, nostack));
    }
}

/// 空闲等待:wfi 令 CPU 进入低功耗等待;若中断被使能,等待可被唤醒。
#[inline]
pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) }
}

/// 停机:关中断后反复 wfi。用于 panic 等不可恢复场景,防止日志被污染。
pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) }
    }
}

/// 陷阱处理入口,由 trap_vector(汇编)以 C ABI 调用,永不返回。
///
/// # 参数
/// - `scause`:异常/中断原因编码(最高位为 1 表示中断)。
/// - `sepc`:触发 trap 的指令地址(同步异常时为故障指令)。
/// - `stval`:trap 相关附加信息(如非法访存地址)。
/// - `frame`:TRAP_FRAME 基址(31 GPR + 4 CSR,布局见 TRAP_FRAME 注释)。
///
/// M0 阶段:仅做完整诊断输出后停机。M1 将按 scause 分发:
/// 中断 → 对应设备处理(定时器/串口),异常 → 输出帧并停机。
#[unsafe(no_mangle)]
pub extern "C" fn trap_handler(scause: usize, sepc: usize, stval: usize, frame: *mut usize) -> ! {
    // 帧内前 31 项为 GPR,是故障点的忠实快照。
    let regs = unsafe { core::slice::from_raw_parts(frame, 31) };
    error!(
        "TRAP: scause={:#x} sepc={:#x} stval={:#x}",
        scause, sepc, stval
    );
    error!(
        "ra={:#x} sp={:#x} gp={:#x} tp={:#x}",
        regs[0], regs[1], regs[2], regs[3]
    );
    error!(
        "t0={:#x} t1={:#x} t2={:#x} s0={:#x}",
        regs[4], regs[5], regs[6], regs[7]
    );
    error!(
        "a0={:#x} a1={:#x} a2={:#x} a3={:#x}",
        regs[8], regs[9], regs[10], regs[11]
    );
    error!(
        "a4={:#x} a5={:#x} a6={:#x} a7={:#x}",
        regs[12], regs[13], regs[14], regs[15]
    );
    error!(
        "s2={:#x} s3={:#x} s4={:#x} s5={:#x}",
        regs[16], regs[17], regs[18], regs[19]
    );
    error!(
        "s6={:#x} s7={:#x} s8={:#x} s9={:#x}",
        regs[20], regs[21], regs[22], regs[23]
    );
    error!(
        "s10={:#x} s11={:#x} t3={:#x} t4={:#x}",
        regs[24], regs[25], regs[26], regs[27]
    );
    error!("t5={:#x} t6={:#x}", regs[28], regs[29]);
    halt()
}
