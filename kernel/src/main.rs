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
mod process;
mod sbi;
mod sched;
mod sync;
mod syscall;
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
    // M2 T1:用户态线程 + ecall 冒烟(irq_enable 之前,隔离中断干扰)。
    boot_user_thread_test();
    // M2 T1.5:每进程独立地址空间 + 用户栈守护页(D20)冒烟。
    boot_process_addrspace_test();
    // 性能基线(启动时打印,供后续优化对比)。
    heap::bench();
    sched::bench();
    arch::irq_enable();
    // V4(外部审计 LOW):用运行时频率换算 µs,不硬编码 10MHz。
    let interval_us =
        (arch::timer_interval() as u64 * 1_000_000) / crate::board::timer_freq() as u64;
    info!("M1: timer enabled ({interval_us}us interval), interrupts on");
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

/// M2 T1:用户态线程 + ecall 冒烟(在 irq_enable 之前、sync 自检之后)。
///
/// 验证链路:建进程(独立地址空间)→ 分配用户代码/共享/栈页 → 映射到
/// **进程根表**(U 权限,不再进内核根表)→ 写入用户程序 → spawn_user →
/// yield 让用户线程 sret 进 U 模式 → `ecall`(sys_get_ticks)→ 写回共享
/// 页 → `ecall`(sys_exit)→ 主上下文恢复。共享页被写入 tick 即证明
/// **U 模式执行 + ecall 往返 + 进程根表**成立。
fn boot_user_thread_test() {
    // 1) 建进程(独立 Sv39 根表,含内核区映射;不切换 satp)。
    let pid = process::create().expect("create process");
    let root = process::root(pid);
    // 2) 分配用户页(清零:防 D10 信息泄漏)。栈分配 2 页,低页作守护。
    let code_pa = mem::alloc_pages_zeroed(0).expect("user code page");
    let shared_pa = mem::alloc_pages_zeroed(0).expect("user shared page");
    let _stack_lo = mem::alloc_pages_zeroed(0).expect("user stack guard page");
    let stack_hi = mem::alloc_pages_zeroed(0).expect("user stack page");
    // 3) 映射到进程根表。用户 VA 窗口:code/shared/栈守护(不映射)/栈。
    let code_va = 0x4000_0000usize;
    let shared_va = 0x4000_1000usize;
    let stack_guard_va = 0x4000_2000usize; // D20:守护页不映射
    let stack_va = 0x4000_3000usize;
    assert!(
        mmu::map_user_page(root, code_va, code_pa, 0xCB).is_ok(),
        "map user code"
    );
    assert!(
        mmu::map_user_page(root, shared_va, shared_pa, 0xC7).is_ok(),
        "map user shared"
    );
    assert!(
        mmu::map_user_page(root, stack_va, stack_hi, 0xC7).is_ok(),
        "map user stack"
    );
    // D20 结构性校验:栈下页未映射、栈页已映射。
    assert!(
        !mmu::is_mapped(root, stack_guard_va),
        "stack guard page must be unmapped"
    );
    assert!(
        mmu::is_mapped(root, stack_va),
        "user stack page must be mapped"
    );
    // 4) 用户程序:get_ticks → 写 shared[0] → exit(编码经解码器校准)。
    let prog: [u32; 6] = [
        0x0020_0893, // addi a7, x0, 2   (SYSCALL_GET_TICKS)
        0x0000_0073, // ecall
        0x4000_12b7, // lui  t0, 0x40001 (t0 = 0x40001000 = shared)
        0x00a2_a023, // sw   a0, 0(t0)   (写回 tick)
        0x0010_0893, // addi a7, x0, 1   (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    for (i, w) in prog.iter().enumerate() {
        // SAFETY:code_pa 为刚分配的页(未释放),写用户程序字。
        unsafe { core::ptr::write_volatile((code_pa + i * 4) as *mut u32, *w) };
    }
    // 5) 新建用户线程(挂到本进程)。用户栈顶 = stack_va + 4KB。
    let user_id = sched::spawn_user(pid, code_va, stack_va + 4096, sched::PRIO_HIGH);
    assert!(user_id != sched::current_id(), "user thread id sanity");
    // 6) 让出直到用户写入共享 tick(用户退出后主上下文恢复)。
    let shared = shared_pa as *const u32;
    let mut guard = 0;
    loop {
        let v = unsafe { core::ptr::read_volatile(shared) };
        if v != 0 {
            info!("M2 T1: user-mode thread ecall ok (user tick={v})");
            break;
        }
        assert!(
            guard < 200_000,
            "user thread did not write shared tick (U-mode ecall broken)"
        );
        sched::yield_();
        guard += 1;
    }
    // 注意:用户页映射与进程根表保留(T2 地址空间回收)。
}

/// M2 T1.5:每进程独立地址空间 + 用户栈守护页(D20)。
///
/// 两个进程使用**同一组用户虚拟地址**(code/shared/栈),但映射到
/// **不同的物理页**,各进程用户程序写入不同 marker。断言:
/// - A 共享页收到 0xA5、B 收到 0xB5 —— 若切到 B 时未换 satp,B 会在
///   A 的根表下执行 A 的代码(写 A 的共享页),B 的共享页保持 0;
/// - 两共享页的 tick(shared[1])非 0 —— 各进程均真实执行过;
/// - 结构性:`is_mapped` 校验栈守护页未映射、栈页/内核区已映射;
/// - 内核根表不含用户页(0x4000_0000)—— 用户映射不再污染内核根表。
fn boot_process_addrspace_test() {
    // 1) 建两个进程(独立 satp 根表)。
    let pid_a = process::create().expect("create process A");
    let pid_b = process::create().expect("create process B");
    let root_a = process::root(pid_a);
    let root_b = process::root(pid_b);
    // 2) 各进程用户程序(marker 不同;流程 get_ticks→写 marker+tick→exit)。
    let prog_a: [u32; 8] = [
        0x0020_0893, // addi a7, x0, 2     (SYSCALL_GET_TICKS)
        0x0000_0073, // ecall
        0x4000_12b7, // lui  t0, 0x40001  (t0 = shared)
        0x0a50_0313, // addi t1, x0, 0xA5 (marker A)
        0x0062_a023, // sw   t1, 0(t0)    (shared[0] = marker)
        0x00a2_a223, // sw   a0, 4(t0)    (shared[1] = tick)
        0x0010_0893, // addi a7, x0, 1     (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let prog_b: [u32; 8] = [
        0x0020_0893, // addi a7, x0, 2     (SYSCALL_GET_TICKS)
        0x0000_0073, // ecall
        0x4000_12b7, // lui  t0, 0x40001  (t0 = shared)
        0x0b50_0313, // addi t1, x0, 0xB5 (marker B)
        0x0062_a023, // sw   t1, 0(t0)    (shared[0] = marker)
        0x00a2_a223, // sw   a0, 4(t0)    (shared[1] = tick)
        0x0010_0893, // addi a7, x0, 1     (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let (_, shared_a_pa) = map_iso_proc(root_a, &prog_a);
    let (_, shared_b_pa) = map_iso_proc(root_b, &prog_b);
    // 3) 各进程挂一个用户线程(同一组用户 VA)。
    sched::spawn_user(pid_a, 0x4000_0000, 0x4000_4000, sched::PRIO_HIGH);
    sched::spawn_user(pid_b, 0x4000_0000, 0x4000_4000, sched::PRIO_HIGH);
    // 4) 轮询两个共享页,直至各进程 marker 到齐。
    let shared_a = shared_a_pa as *const u32;
    let shared_b = shared_b_pa as *const u32;
    let mut guard = 0;
    loop {
        let va = unsafe { core::ptr::read_volatile(shared_a) };
        let vb = unsafe { core::ptr::read_volatile(shared_b) };
        if va == 0xA5 && vb == 0xB5 {
            break;
        }
        assert!(
            guard < 200_000,
            "per-process isolation broken: shared_a={va:#x} shared_b={vb:#x}"
        );
        sched::yield_();
        guard += 1;
    }
    // 5) 各进程均真实执行过(tick 非 0)。
    let tick_a = unsafe { core::ptr::read_volatile(shared_a.add(1)) };
    let tick_b = unsafe { core::ptr::read_volatile(shared_b.add(1)) };
    assert!(
        tick_a != 0 && tick_b != 0,
        "both processes must run (tick_a={tick_a}, tick_b={tick_b})"
    );
    // 6) 结构性校验:守护页未映射,栈页/内核区已映射。
    assert!(
        !mmu::is_mapped(root_a, 0x4000_2000),
        "A stack guard must be unmapped"
    );
    assert!(
        mmu::is_mapped(root_a, 0x4000_3000),
        "A stack must be mapped"
    );
    assert!(
        mmu::is_mapped(root_a, crate::board::uart_base()),
        "A kernel region (UART) mapped"
    );
    assert!(
        mmu::is_mapped(root_a, 0x8000_0000),
        "A kernel region mapped"
    );
    assert!(
        !mmu::is_mapped(root_b, 0x4000_2000),
        "B stack guard must be unmapped"
    );
    assert!(
        mmu::is_mapped(root_b, 0x4000_3000),
        "B stack must be mapped"
    );
    assert!(
        mmu::is_mapped(root_b, crate::board::uart_base()),
        "B kernel region (UART) mapped"
    );
    assert!(
        mmu::is_mapped(root_b, 0x8000_0000),
        "B kernel region mapped"
    );
    // 内核根表不再含用户页(用户映射只在各进程根表)。
    assert!(
        !mmu::is_mapped(mmu::kernel_root(), 0x4000_0000),
        "user pages must not leak into kernel root"
    );
    info!("M2: per-process address space ok (2 proc @ same VA, satp switch, guard page)");
}

/// 为一个进程映射隔离测试所需的用户页并写入程序(进程根表)。
///
/// 用户 VA 窗口固定:code=0x4000_0000、shared=0x4000_1000、栈守护
/// =0x4000_2000(分配但不映射)、栈=0x4000_3000(映射高页)。返回
/// (code_pa, shared_pa)。栈守护页的物理页与进程根表一样保留到
/// T2 地址空间回收。
fn map_iso_proc(root: usize, prog: &[u32]) -> (usize, usize) {
    let code_pa = mem::alloc_pages_zeroed(0).expect("iso code page");
    let shared_pa = mem::alloc_pages_zeroed(0).expect("iso shared page");
    let _stack_lo = mem::alloc_pages_zeroed(0).expect("iso stack guard page");
    let stack_hi = mem::alloc_pages_zeroed(0).expect("iso stack page");
    assert!(
        mmu::map_user_page(root, 0x4000_0000, code_pa, 0xCB).is_ok(),
        "iso map code"
    );
    assert!(
        mmu::map_user_page(root, 0x4000_1000, shared_pa, 0xC7).is_ok(),
        "iso map shared"
    );
    assert!(
        mmu::map_user_page(root, 0x4000_3000, stack_hi, 0xC7).is_ok(),
        "iso map stack"
    );
    for (i, w) in prog.iter().enumerate() {
        // SAFETY:code_pa 为刚分配的页(未释放),写用户程序字。
        unsafe { core::ptr::write_volatile((code_pa + i * 4) as *mut u32, *w) };
    }
    (code_pa, shared_pa)
}
