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

use core::sync::atomic::{AtomicUsize, Ordering};

mod arch;
mod board;
mod cpu;
mod device;
mod elf;
mod fdt;
mod heap;
mod ipc;
mod logger;
mod mem;
mod mmu;
mod pages;
mod panic;
mod process;
mod sbi;
mod sched;
mod services;
mod shm;
mod sync;
mod syscall;
mod tests;
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

/// D8:boot hart 发布的 satp 值(副核从 Bare 切换进同一 Sv39 身份映射)。
/// **必须放 .data**:副核在 BSS 清零之后才读它,但保持与 BOOT_RELEASE
/// 同区域(镜像加载即就绪)以统一发布语义。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data")]
pub static mut BOOT_SATP: u64 = 0;

/// D8:副核唤醒发布标志(1 = satp 已发布,可引导)。**必须放 .data**:
/// 副核 park 循环在 BSS 未清时就开始轮询本标志,若落 .bss 会读到
/// 垃圾值误引导(CRITICAL,同 BOOT_LOCK 语义)。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data")]
pub static mut BOOT_RELEASE: u32 = 0;

/// D8:已进入 `secondary_main` 的副核数(不含 boot hart)。
static HARTS_ONLINE: AtomicUsize = AtomicUsize::new(0);

/// M3 T2:已上线副核位图(bit h = hart h 已 mark_online;boot hart 不含,
/// 跨核请求方总是跳过自身)。`tlb_shootdown_remote` 据此只对**已在跑内核
/// 的核**发 IPI —— 未上线核 park 在 OpenSBI,无内核 satp/TLB,投递会
/// 无响应地 200ms 超时(实测 boot 期 shm_revoke 曾触发)。
static HARTS_ONLINE_MASK: AtomicUsize = AtomicUsize::new(0);

/// 副核进入 idle 后调用(`secondary_main` 内;Relaxed 足够,仅计数)。
fn mark_online() {
    let h = crate::arch::hartid();
    HARTS_ONLINE.fetch_add(1, Ordering::Relaxed);
    HARTS_ONLINE_MASK.fetch_or(1usize << h, Ordering::Relaxed);
}

/// 已上线副核数(不含 boot hart)。
fn harts_online() -> usize {
    HARTS_ONLINE.load(Ordering::Relaxed)
}

/// M3 T2:已上线副核位图(boot hart 位不置;调用方跳过自身)。
pub(crate) fn harts_online_mask() -> usize {
    HARTS_ONLINE_MASK.load(Ordering::Relaxed)
}

/// 发布序屏障(boot hart 发布 BOOT_SATP/BOOT_RELEASE 时使用)。
#[inline]
fn fence_rw() {
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack));
    }
}

/// 内核主入口,由 `_start`(src/entry.S)以 C ABI 调用,永不返回。
///
/// # 参数
/// - `hartid`:当前 hart 编号(OpenSBI 引导时经 a0 传入,原样透传)。
/// - `fdt`:设备树(FDT)指针(OpenSBI 经 a1 传入)。M1.5 已完整解析
///   FDT 得到 RAM/UART/定时器频率/保留区等参数(见 board::init_from_fdt)。
///
/// # 初始化顺序(依赖关系,勿随意调整)
/// 1. `arch::irq_disable` —— 建立确定的中断状态
/// 2. `arch::sanitize_csr` —— 清洗引导器遗留的 sie/sip/sstatus 位
/// 3. `uart::init` —— 日志基础设施(先用默认基址,供早期输出)
/// 4. `arch::init_traps` —— stvec 装好前异常会跳地址 0(不可恢复)
/// 5. `arch::enable_timer` —— 定时器中断源(STIE),晚于陷阱向量
/// 6. `logger::set_level` —— 日志级别
/// 7. FDT 解析 + `board::init_from_fdt` —— 得到 RAM/UART/定时器/保留区
/// 8. `crate::uart::reinit` —— 用 FDT 基址重配置串口
/// 9. `cpu::init_from_fdt` —— 打印 ISA 能力(诊断)
/// 10. `mem::init(&fdt_params)` + 自检 —— 物理内存(含保留区刻蚀)
/// 11. `mmu::init` + 自检 —— Sv39 身份映射(须在中断使能前)
/// 12. `heap::init` + 自检 —— 内核堆(slab + 页路径)
/// 13. `sched::init` + 自检 —— 调度器(idle 线程)
/// 14. `sync::self_test` —— 同步原语
/// 15. bench —— 性能基线
/// 16. `arch::irq_enable` —— 最后开全局中断(idle 线程上下文)
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(hartid: usize, fdt: *const u8) -> ! {
    arch::irq_disable();
    arch::sanitize_csr();
    uart::init();
    // D7:per-hart 陷阱栈 —— 传当前 hartid(boot hart 由 OpenSBI a0 传入)。
    arch::init_traps(hartid);
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
    // M2 T3c:共享页注册表容量预留(在 boot_tests 之前,引导期非 ISR 分配)。
    shm::init();
    // M3-3:物理页能力注册表容量预留(Cap::Page;同样引导期非 ISR 分配)。
    pages::init();
    // M2 引导期冒烟测试(T1 用户态 + T1.5 每进程地址空间 + T3c 共享内存,
    // 定义与说明见 tests.rs)。置于 irq_enable 之前,隔离定时器中断干扰。
    tests::boot_tests();
    // D8:唤醒副核。放 boot_tests 之后(单核确定性不受扰)与
    // irq_enable 之前(副核上线即 idle 停等,不开中断不影响其引导)。
    wake_secondaries(hartid);
    // 性能基线(启动时打印,供后续优化对比)。
    heap::bench();
    sched::bench();
    arch::irq_enable();
    // V4(外部审计 LOW):用运行时频率换算 µs,不硬编码 10MHz。
    let interval_us =
        (arch::timer_interval() as u64 * 1_000_000) / crate::board::timer_freq() as u64;
    info!("M1: timer enabled ({interval_us}us interval), interrupts on");
    // M2 T3b:多核调度冒烟(boot hart、irq_enable 后调用 —— 副核已上线并
    // 进入各自 idle 循环,可接收按亲和性分配的线程)。定义见 tests.rs。
    tests::smp_sched_test();
    // M3 T2:跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown 冒烟
    // (跨核场景;单核退化为 N=1 仍打印 banner)。定义见 tests.rs。
    tests::smp_crosscore_test();
    // M3-2 T2:跨核 uart_server 服务 IPC(D6 跨核 IPI 实测;uart_server 亲和
    // 副核 A、client 亲和副核 B,双向即时配对)。定义见 tests.rs。
    tests::smp_uart_ipc_test();
    // M3-3 T2:跨核 mem_server 服务 IPC + Cap::Page 移交(跨核场景;单核退化
    // 仍打印 banner)。定义见 tests.rs。
    tests::smp_memory_ipc_test();
    // M3-4 T2:跨核 ramfs 文件服务 IPC(服务链:client → ramfs_server →
    // mem_server,数据面 SHM 窗;跨核场景;单核退化仍打印 banner)。
    tests::smp_ramfs_ipc_test();
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

/// D8:唤醒副核(boot_tests 之后、irq_enable 之前调用)。
///
/// 发布顺序(副核 park 循环依赖,见 entry.S):
///   写 BOOT_SATP → fence rw,rw → 写 BOOT_RELEASE=1 → fence rw,rw
///   → 对每个副核 `sbi_hsm_hart_start(h, _start, S, h)`。
///
/// **启动机制**:QEMU + OpenSBI 下副核 warm boot 后停在其 HSM 状态机的
/// `sbi_hsm_hart_wait`(STOPPED → 等 START_PENDING = 2 的 wfi 循环),**不会**
/// 自动进入内核 `_start`。载荷必须用 SBI HSM `hart_start` 显式启动每个副核
/// (与 Linux 多核引导协议一致)。副核被启动后从 Bare 进入 `_start`,输掉
/// BOOT_LOCK 仲裁后 park 轮询 BOOT_RELEASE;发布标志就绪后载入 satp 进入
/// 同一 Sv39 身份映射,设 per-hart idle 栈/陷阱栈,进入 secondary_main 并
/// mark_online。boot hart 有界自旋等全部副核上线,再打 T3a banner。
///
/// 在线核数 = FDT cpu@* 节点数(board::cpu_count)∩ MAX_HARTS。
/// SBI HSM 不可用/出错时回退:副核 park 循环仍会轮询 BOOT_RELEASE(若
/// OpenSBI 自行释放则仍可达标;实测 QEMU 上必须 HSM 显式启动)。
fn wake_secondaries(boot_hartid: usize) {
    let expected = board::cpu_count().min(arch::MAX_HARTS);
    if expected <= 1 {
        info!("M2 T3a: multi-core boot ok ({} harts online)", 1);
        return;
    }
    unsafe {
        // 发布 satp(mmu::init 已把本核切到 Sv39 身份映射;副核从
        // Bare 载入同一值 + sfence.vma 进入同一地址空间)。
        BOOT_SATP = mmu::satp() as u64;
    }
    fence_rw();
    unsafe {
        BOOT_RELEASE = 1;
    }
    fence_rw();
    // 逐个显式启动副核(HSM 状态机:STOPPED → START_PENDING → 唤醒跳转
    // _start,a0 = hartid)。**跳过 boot hart 自身**(已在运行,启动会报
    // SBI_ERR_INVALID_STATE);其余核含 hart 0(实测 boot hart 不一定是 0)。
    // 失败仅告警:副核仍轮询 BOOT_RELEASE。
    let start_addr = board::kernel_start_addr();
    for h in 0..expected {
        if h == boot_hartid {
            continue;
        }
        let rc = crate::sbi::hsm_hart_start(h, start_addr, 0, h);
        if rc != 0 {
            warn!("SBI hsm_hart_start(hart {h}) failed (rc=0x{rc:x}); it polls BOOT_RELEASE");
        }
    }
    // 有界自旋等全部副核上线(mark_online;无定时器,纯轮询)。
    let mut spins = 0u64;
    while harts_online() + 1 < expected {
        spins += 1;
        if spins > 50_000_000 {
            panic!(
                "M2 T3a: timed out waiting for {expected} harts online (got {})",
                harts_online() + 1
            );
        }
    }
    info!(
        "M2 T3a: multi-core boot ok ({} harts online)",
        harts_online() + 1
    );
}

/// D8:副核主入口(entry.S `boot_done` 以 C ABI 调用,a0 = hartid)。
///
/// T3b(D19)起副核进入 **per-CPU 调度器**:每核定时器 + `secondary_idle`
/// idle 循环(pick 本核就绪线程运行,无就绪则 wfi,定时器/IPI 唤醒)。
/// `sched::init` 已为每核建好 idle TCB,`current[hart]`/`idle[hart]` 已就位。
#[unsafe(no_mangle)]
pub extern "C" fn secondary_main(hartid: usize) -> ! {
    arch::irq_disable();
    arch::sanitize_csr();
    arch::init_traps(hartid);
    // "hart N online" 用 locked_line 整行原子打印(三个副核同时上线,
    // D9 try_lock 在争用下会字符交错;boot 期 SIE 全关,阻塞锁安全)。
    crate::uart::locked_line(|| info!("hart {hartid} online"));
    // mark_online **在打印之后**:boot hart 的等待循环以本计数为
    // 上线信号,只有全部副核打印完并归 idle 后,boot hart 才打印
    // T3a banner(独占输出 → banner 行原子,test-smp 断言可靠)。
    mark_online();
    // T3b:先布中断源(定时器 + SSIP)再开全局中断,与 boot hart 一致;
    // 随后进入本核调度器 idle 循环(永不返回)。
    arch::enable_timer();
    arch::irq_enable();
    crate::sched::secondary_idle(hartid)
}
