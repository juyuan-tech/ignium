//! Ignium 炬元微内核 —— 内核入口与引导顺序。
//!
//! 启动链:OpenSBI(S 模式)→ `_start`(src/entry.S)→ 清 BSS → 设栈 →
//! `kernel_main(hartid, fdt)`。
//!
//! # 团队约定
//! - `kernel_main` 内的初始化顺序存在依赖关系,改动必须重新评估
//!   "trap 窗口"(见下方 init 顺序注释)。
//! - 本 crate 是微内核的**唯一特权层**;用户态兼容代码(OpenHarmony)
//!   永远不进入这里(见 docs/DESIGN.md)。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod arch;
mod board;
mod cpu;
mod fdt;
mod heap;
mod logger;
mod mem;
mod mmu;
mod panic;
mod sbi;
mod sched;
mod sync;
mod uart;

use core::arch::global_asm;

// 引导汇编:_start 入口、BSS 清零、栈指针设置。逻辑见 src/entry.S 注释。
global_asm!(include_str!("entry.S"));

/// 多核引导仲裁标志:entry.S 用 amoswap 竞争"引导权"
/// (boot hart 不一定是 hart 0)。**必须放在 .data**:
/// 初始值 0 会被编译器放进 .bss,而它必须在 BSS 清零**之前**
/// 可用(CRITICAL-2);link_section 强制入 .data(镜像加载即就绪)。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data")]
pub static mut BOOT_LOCK: u32 = 0;

/// 内核主入口,由 `_start`(src/entry.S)以 C ABI 调用,永不返回。
///
/// # 参数
/// - `hartid`:当前 hart 编号(OpenSBI 引导时经 a0 传入,原样透传)。
/// - `fdt`:设备树(FDT)指针(OpenSBI 经 a1 传入)。当前阶段仅记录;
///   硬件探测(如 UART 基址)仍依赖硬编码常量,真机移植时优先改造这里。
///
/// # 初始化顺序(依赖关系,勿随意调整)
/// 1. `arch::irq_disable` —— 在一切操作前建立确定的中断状态
/// 2. `arch::sanitize_csr` —— 清洗引导器遗留的 sie/sip/sstatus 位
/// 3. `uart::init` —— 日志基础设施,必须先于任何输出
/// 4. `arch::init_traps` —— stvec 装好之前发生异常会跳到地址 0
///    (不可恢复),因此必须尽早;但必须在 uart 之后(陷阱日志依赖串口)
/// 5. `arch::enable_timer` —— 定时器中断源(STIE),必须晚于陷阱向量
/// 6. `logger::set_level` —— 日志级别,先于任何日志调用
/// 7. `mem::init(fdt)` + 自检 —— 物理内存(含 FDT 刻蚀),须在页表前
/// 8. `mmu::init` + 自检 —— Sv39 身份映射,须在中断使能前
/// 9. `arch::irq_enable` —— 最后才开全局中断(中断源全部就绪之后)
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(hartid: usize, fdt: *const u8) -> ! {
    arch::irq_disable();
    arch::sanitize_csr();
    uart::init();
    arch::init_traps();
    arch::enable_timer();
    crate::logger::set_level(crate::logger::Level::Info);
    info!("Ignium 炬元微内核 v{} booting", env!("CARGO_PKG_VERSION"));
    debug!("uart console initialized (tick={})", crate::logger::tick());
    info!(
        "M0: boot ok - arch: riscv64, machine: qemu-virt, hartid={}, fdt={:#x}",
        hartid, fdt as usize
    );
    // FDT 解析:板级参数(RAM/UART/定时器频率/保留区),须在 mem::init 之前。
    let fdt_params = crate::fdt::Fdt::new(fdt as usize)
        .as_ref()
        .map(|f| f.parse())
        .unwrap_or_default();
    board::init_from_fdt(&fdt_params);
    // FDT 解析后重初始化 UART(使基址反映 FDT 值)。
    crate::uart::reinit();
    // CPU 能力检测(RVA23 P1:读取 ISA 信息,打印诊断)。
    crate::cpu::init_from_fdt(&fdt_params);
    // 物理内存初始化(含 FDT 保留区刻蚀)+ 自检。
    // 分配器(M1):IRQ 安全 SpinLock(MED-3),持锁不被抢占;
    // 此项自检须在 irq_enable 之前(避免定时器与自检交错)。
    mem::init(&fdt_params);
    match mem::self_test() {
        Ok(()) => info!(
            "M1: buddy allocator selftest ok ({} KiB managed)",
            mem::page_count() * 4
        ),
        Err(e) => panic!("buddy allocator selftest failed: {e}"),
    }
    // Sv39 页表 + 内核自身映射(身份映射)并自检。
    // 须在 irq_enable 之前:中断路径依赖映射成立。
    mmu::init();
    match mmu::self_test() {
        Ok(()) => info!(
            "M1: Sv39 paging ok (identity map, satp root={:#x})",
            mmu::satp()
        ),
        Err(e) => panic!("Sv39 paging selftest failed: {e}"),
    }
    // 内核堆初始化(缓存分配区基址)后自检。
    // 堆锁为 IRQ 安全 SpinLock(MED-3,审计 17 轮);ISR 零分配(容量预留)。
    // 自检须在 irq_enable 之前(irq_enable 后当前上下文即 idle 线程)。
    heap::init();
    match heap::self_test() {
        Ok(()) => info!("M1: kernel heap selftest ok (slab 16B..2KB + page path)"),
        Err(e) => panic!("kernel heap selftest failed: {e}"),
    }
    // 调度器初始化(idle 线程)+ 自检。
    // 须在 irq_enable 之前;此后当前上下文 = idle 线程。
    sched::init();
    match sched::self_test() {
        Ok(()) => info!("M1: scheduler selftest ok (cooperative + preemptive)"),
        Err(e) => panic!("scheduler selftest failed: {e}"),
    }
    match sync::self_test() {
        Ok(()) => info!("M1: sync primitives selftest ok (mutex + condvar)"),
        Err(e) => panic!("sync primitives selftest failed: {e}"),
    }
    // 性能基线(启动时打印,供后续优化对比)。
    heap::bench();
    sched::bench();
    arch::irq_enable();
    info!(
        "M1: timer enabled ({}us interval), interrupts on",
        arch::timer_interval() / 10
    );
    // 空闲线程体:当前上下文(调度器初始化后)即 idle 线程。
    // wfi 被定时器中断唤醒;抢占由 on_tick 在 ISR 中决策。
    // LOW-3(审计 17 轮):idle 无 yield 路径,退出线程的栈在此回收
    // (在 idle 自己的栈上释放他人栈,C2 安全)。
    let mut last_tick = 0u64;
    loop {
        sched::drain_reaper();
        arch::wait_for_interrupt();
        let t = crate::logger::tick();
        // wrapping_sub(L3):tick 回绕/损坏时避免减法下溢导致
        // 心跳永久停摆(正确性优先于理论上的绝对时序)。
        if t.wrapping_sub(last_tick) >= 100 {
            last_tick = t;
            // saturating_mul:overflow-checks 开启下,极端 tick 值
            // (u64 千年量级)也不会触发 panic(pro 审计 max #10)。
            info!("uptime: {} ticks ({} ms)", t, t.saturating_mul(10));
        }
    }
}
