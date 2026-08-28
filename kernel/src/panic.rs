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

// 链接脚本符号(kernel/linker.ld):栈区边界。
// 说明:extern 声明**仅用于取符号地址**((&raw const X).addr(),
// 不形成引用),从不解引用该 extern 本身;后续的扫描用裸指针
// volatile 逐字节读,无类型化读的 provenance 问题(pro 审计 max #5)。
// D7:陷阱/idle 栈为 per-hart 数组(每槽 stride 32K = 16K 守护 + 16K 栈),
// 只登记数组边界;槽界由 stride 计算(见 current_stack_bounds)。
extern "C" {
    static _stack_bottom: u8;
    static _stack_top: u8;
    static _trap_stack_base: u8;
    static _trap_stack_top: u8;
    static _idle_stack_base: u8;
    static _idle_stack_top: u8;
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
    // D9:进入 panic 输出模式 —— 之后 uart::write_str 放弃 CONSOLE_LOCK
    // 直接裸写(panic 可能打断正持锁的主上下文,取锁必然死锁)。
    crate::uart::set_panic_output();
    error!("KERNEL PANIC");
    match info.location() {
        Some(l) => error!("location: {}:{}:{}", l.file(), l.line(), l.column()),
        None => error!("location: unknown"),
    }
    error!("message: {}", info.message());
    dump_cpu(arch::cpu_state());
    arch::halt()
}

/// 返回栈区物理边界 `(bottom, top)`;sp 不在任何已知栈时返回 None
/// (LOW-2:不默认回退 boot 栈,水位标记 unknown,防误导诊断)。
///
/// panic 可能发生在三类栈上:主上下文(引导栈)、trap_handler(陷阱栈
/// per-hart 数组)或副核 idle 上下文(idle 栈 per-hart 数组)。水位扫描
/// 必须针对**当前栈**,否则会得到误导性结果(pro 审计 max #6)。
///
/// D7 多核:陷阱/idle 均为 per-hart 数组(linker.ld),每槽 stride 32K =
/// 16K 守护 + 16K 栈,栈区 = `[槽底+16K, 槽底+32K)`。**守护页部分不扫**
/// (未映射,panic 期间读它自己会触发故障)。
fn current_stack_bounds(sp: usize) -> Option<(usize, usize)> {
    let boot_bottom = (&raw const _stack_bottom).addr();
    let boot_top = (&raw const _stack_top).addr();
    if sp >= boot_bottom && sp < boot_top {
        return Some((boot_bottom, boot_top));
    }
    let stride = crate::arch::TRAP_STRIDE;
    let guard = crate::arch::TRAP_GUARD;
    // per-hart 数组:sp 落在任一槽的**栈区**即命中(只扫栈区,不扫守护)。
    let arrays = [
        (
            (&raw const _trap_stack_base).addr(),
            (&raw const _trap_stack_top).addr(),
        ),
        (
            (&raw const _idle_stack_base).addr(),
            (&raw const _idle_stack_top).addr(),
        ),
    ];
    for (base, top) in arrays {
        let mut slot = base;
        while slot < top {
            let stack_bottom = slot + guard;
            let stack_top = slot + stride;
            if sp >= stack_bottom && sp < stack_top {
                return Some((stack_bottom, stack_top));
            }
            slot += stride;
        }
    }
    None
}

/// 计算栈水位 = 栈顶到"最深被使用字节"的距离(字节)。
///
/// 原理:boot 时栈区被 BSS 清零循环清零;此后任何非零字节都意味着
/// 该地址曾被写过。从栈底向上第一个非零字节 ≈ 历史最深栈帧底。
/// 按**字节**扫描(pro 审计 #13:usize 类型读取带严格 provenance
/// 的活跃栈内存存在可疑性;panic 路径的性能开销可忽略)。
fn stack_watermark(sp: usize) -> Option<usize> {
    let (bottom, top) = current_stack_bounds(sp)?;
    let mut probe = bottom;
    while probe < top {
        if unsafe { core::ptr::read_volatile(probe as *const u8) } != 0 {
            return Some(top - probe);
        }
        probe += 1;
    }
    Some(0)
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
    let sp = state.sp;
    match current_stack_bounds(sp) {
        Some((bottom, top)) => {
            let wm = stack_watermark(sp).unwrap_or(0);
            error!(
                "stack watermark: {wm} / {} bytes used, uart dropped: {}",
                top - bottom,
                crate::uart::dropped()
            );
        }
        None => {
            // sp 不在任何已知栈:LOW-2 + V3 审计 #8 —— 可能是
            // 调度器线程的堆分配栈(16KB,未登记),非损坏。措辞区分。
            error!(
                "WARNING: sp={:#x} not in boot/trap stacks (may be a thread stack; watermark unavailable)",
                sp
            );
            error!(
                "stack watermark: UNKNOWN, uart dropped: {}",
                crate::uart::dropped()
            );
        }
    }
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
