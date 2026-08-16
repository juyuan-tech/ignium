//! panic 处理器:把崩溃变成**可诊断的输出**,而不是静默死机。
//!
//! 输出内容:位置、消息、CPU 寄存器快照、栈水位、串口丢字符计数。
//!
//! # 防御设计
//! - `PANICKING` 标志:panic 过程中再次 panic(如格式化异常)直接停机,
//!   防止无限递归把栈打爆、输出被垃圾淹没。
//! - 停机前关闭中断:保证输出过程不被中断打断、状态不被污染。
//! - 栈水位:boot 时 BSS 清零使栈区全零,panic 时从栈底向上找第一个
//!   非零字节 ≈ 历史最深栈用量(诊断栈溢出的第一手证据)。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{self, CpuState};
use crate::error;

// 链接脚本符号(kernel/linker.ld):栈区边界。extern 声明读取的是
// 符号的**地址**,因此用 `&符号 as usize` 取地址值。
extern "C" {
    static _stack_bottom: u8;
    static _stack_top: u8;
}

/// 双 panic 保护:第一次进入置位;第二次(panic 中再 panic)直接停机。
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Rust panic 统一入口(编译器在 panic! 时调用)。
///
/// 流程:防递归检查 → 关中断 → 输出位置/消息 → CPU dump → 停机。
#[panic_handler]
#[cold]
#[inline(never)]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // 若已处于 panic 流程,说明发生了"panic 中 panic"(如日志格式化
    // 再次失败)。此时 UART/栈状态不可信,最安全的做法是直接停机。
    if PANICKING.swap(true, Ordering::Relaxed) {
        arch::halt()
    }
    arch::irq_disable();
    error!("KERNEL PANIC");
    match info.location() {
        Some(l) => error!("location: {}:{}:{}", l.file(), l.line(), l.column()),
        None => error!("location: unknown"),
    }
    error!("message: {}", info.message());
    dump_cpu(arch::cpu_state());
    arch::halt()
}

/// 返回栈区物理边界 `(bottom, top)`。
fn stack_bounds() -> (usize, usize) {
    let bottom = unsafe { &_stack_bottom as *const u8 as usize };
    let top = unsafe { &_stack_top as *const u8 as usize };
    (bottom, top)
}

/// 计算栈水位 = 栈顶到"最深被使用字节"的距离(字节)。
///
/// 原理:boot 时栈区被 BSS 清零循环清零;此后任何非零字节都意味着
/// 该地址曾被写过。从栈底向上第一个非零字节 ≈ 历史最深栈帧底。
/// 按**字节**扫描(pro 审计 #13:usize 类型读取带严格 provenance
/// 的活跃栈内存存在可疑性;panic 路径的性能开销可忽略)。
fn stack_watermark() -> usize {
    let (bottom, top) = stack_bounds();
    let mut probe = bottom;
    while probe < top {
        if unsafe { core::ptr::read_volatile(probe as *const u8) } != 0 {
            return top - probe;
        }
        probe += 1;
    }
    0
}

/// 输出 CPU 状态 dump。
///
/// 说明:这里打印的 `ra/sp` 是 **panic 处理器自身上下文**(cpu_state
/// 的语义,见 arch/riscv64.rs),不是故障点;故障点的忠实寄存器帧由
/// trap_vector 的 trap_handler 提供。因此 dump 输出统一标注 best-effort。
fn dump_cpu(state: CpuState) {
    error!("--- CPU state ---");
    error!("tick: {}", crate::logger::tick());
    error!("note: ra/sp 与 CSR 为 panic 上下文 best-effort 值;故障点帧见 trap dump");
    let (bottom, top) = stack_bounds();
    let sp = state.sp;
    if sp < bottom || sp >= top {
        // sp 出界是栈溢出/内存破坏的强信号,必须醒目提示。
        error!(
            "WARNING: sp={:#x} outside stack range [{:#x}, {:#x})",
            sp, bottom, top
        );
    }
    error!(
        "stack watermark: {} / {} bytes used, uart dropped: {}",
        stack_watermark(),
        top - bottom,
        crate::uart::dropped()
    );
    error!(
        "ra={:#x} sp={:#x} gp={:#x} tp={:#x}",
        state.ra, state.sp, state.gp, state.tp
    );
    error!("sstatus={:#x} sepc={:#x}", state.sstatus, state.sepc);
    error!(
        "scause={:#x} stval={:#x} satp={:#x}",
        state.scause, state.stval, state.satp
    );
    error!("--- end of dump ---");
}
