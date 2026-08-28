//! 引导期冒烟测试(M2 T1 / T1.5)。
//!
//! # 定位
//! 集中放置启动自检(用户态线程 + 每进程地址空间),使 `main.rs` 聚焦
//! 于初始化顺序本身,不被测试逻辑淹没。测试在 `kernel_main` 的 init
//! 顺序末尾、`irq_enable` **之前**执行(隔离定时器中断干扰,保证
//! 确定性)。
//!
//! # 约束
//! - 零依赖顺序(各测试彼此独立、只依赖 init 已完成);
//! - 幂等:仅运行一次(boot 期),不在运行时重复;
//! - 失败即 panic(冒烟测试是门禁的一部分,见 Makefile 的 banner 断言)。
//!
//! # 门禁同步
//! 每个测试输出一条 `M2 ... ok` banner;Makefile `test`/`test-rva23` 与
//! `.github/workflows/ci.yml` 用 `grep -q` 断言其存在 —— 新增测试必须
//! **同步**三处 grep 列表,否则本地过、CI 挂。

use crate::info;

/// 运行全部引导期冒烟测试(M2)。
///
/// 由 `kernel_main` 在 `sync::self_test` 之后、`arch::irq_enable`
/// 之前调用。
pub fn boot_tests() {
    boot_user_thread_test();
    boot_process_addrspace_test();
}

/// M2 T1:用户态线程 + ecall 冒烟。
///
/// 验证链路:建进程(独立地址空间)→ 分配用户代码/共享/栈页 → 映射到
/// **进程根表**(U 权限,不再进内核根表)→ 写入用户程序 → spawn_user →
/// yield 让用户线程 sret 进 U 模式 → `ecall`(sys_get_ticks)→ 写回共享
/// 页 → `ecall`(sys_exit)→ 主上下文恢复。共享页被写入 tick 即证明
/// **U 模式执行 + ecall 往返 + 进程根表**成立。
fn boot_user_thread_test() {
    // 1) 建进程(独立 Sv39 根表,含内核区映射;不切换 satp)。
    let pid = crate::process::create().expect("create process");
    let root = crate::process::root(pid);
    // 2) 分配用户页(清零:防 D10 信息泄漏)。栈分配 2 页,低页作守护。
    let code_pa = crate::mem::alloc_pages_zeroed(0).expect("user code page");
    let shared_pa = crate::mem::alloc_pages_zeroed(0).expect("user shared page");
    let _stack_lo = crate::mem::alloc_pages_zeroed(0).expect("user stack guard page");
    let stack_hi = crate::mem::alloc_pages_zeroed(0).expect("user stack page");
    // 3) 映射到进程根表。用户 VA 窗口:code/shared/栈守护(不映射)/栈。
    let code_va = 0x4000_0000usize;
    let shared_va = 0x4000_1000usize;
    let stack_guard_va = 0x4000_2000usize; // D20:守护页不映射
    let stack_va = 0x4000_3000usize;
    assert!(
        crate::mmu::map_user_page(root, code_va, code_pa, 0xCB).is_ok(),
        "map user code"
    );
    assert!(
        crate::mmu::map_user_page(root, shared_va, shared_pa, 0xC7).is_ok(),
        "map user shared"
    );
    assert!(
        crate::mmu::map_user_page(root, stack_va, stack_hi, 0xC7).is_ok(),
        "map user stack"
    );
    // D20 结构性校验:栈下页未映射、栈页已映射。
    assert!(
        !crate::mmu::is_mapped(root, stack_guard_va),
        "stack guard page must be unmapped"
    );
    assert!(
        crate::mmu::is_mapped(root, stack_va),
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
    let user_id = crate::sched::spawn_user(pid, code_va, stack_va + 4096, crate::sched::PRIO_HIGH);
    assert!(
        user_id != crate::sched::current_id(),
        "user thread id sanity"
    );
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
        crate::sched::yield_();
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
    let pid_a = crate::process::create().expect("create process A");
    let pid_b = crate::process::create().expect("create process B");
    let root_a = crate::process::root(pid_a);
    let root_b = crate::process::root(pid_b);
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
    crate::sched::spawn_user(pid_a, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    crate::sched::spawn_user(pid_b, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
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
        crate::sched::yield_();
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
        !crate::mmu::is_mapped(root_a, 0x4000_2000),
        "A stack guard must be unmapped"
    );
    assert!(
        crate::mmu::is_mapped(root_a, 0x4000_3000),
        "A stack must be mapped"
    );
    assert!(
        crate::mmu::is_mapped(root_a, crate::board::uart_base()),
        "A kernel region (UART) mapped"
    );
    assert!(
        crate::mmu::is_mapped(root_a, 0x8000_0000),
        "A kernel region mapped"
    );
    assert!(
        !crate::mmu::is_mapped(root_b, 0x4000_2000),
        "B stack guard must be unmapped"
    );
    assert!(
        crate::mmu::is_mapped(root_b, 0x4000_3000),
        "B stack must be mapped"
    );
    assert!(
        crate::mmu::is_mapped(root_b, crate::board::uart_base()),
        "B kernel region (UART) mapped"
    );
    assert!(
        crate::mmu::is_mapped(root_b, 0x8000_0000),
        "B kernel region mapped"
    );
    // 内核根表不再含用户页(用户映射只在各进程根表)。
    assert!(
        !crate::mmu::is_mapped(crate::mmu::kernel_root(), 0x4000_0000),
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
    let code_pa = crate::mem::alloc_pages_zeroed(0).expect("iso code page");
    let shared_pa = crate::mem::alloc_pages_zeroed(0).expect("iso shared page");
    let _stack_lo = crate::mem::alloc_pages_zeroed(0).expect("iso stack guard page");
    let stack_hi = crate::mem::alloc_pages_zeroed(0).expect("iso stack page");
    assert!(
        crate::mmu::map_user_page(root, 0x4000_0000, code_pa, 0xCB).is_ok(),
        "iso map code"
    );
    assert!(
        crate::mmu::map_user_page(root, 0x4000_1000, shared_pa, 0xC7).is_ok(),
        "iso map shared"
    );
    assert!(
        crate::mmu::map_user_page(root, 0x4000_3000, stack_hi, 0xC7).is_ok(),
        "iso map stack"
    );
    for (i, w) in prog.iter().enumerate() {
        // SAFETY:code_pa 为刚分配的页(未释放),写用户程序字。
        unsafe { core::ptr::write_volatile((code_pa + i * 4) as *mut u32, *w) };
    }
    (code_pa, shared_pa)
}
