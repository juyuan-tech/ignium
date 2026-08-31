//! 引导期冒烟测试(M2 T1 / T1.5 / T2a / T2b)。
//!
//! # 定位
//! 集中放置启动自检(用户态线程 + 每进程地址空间 + 同步 IPC/能力表 +
//! IPC 压力),使 `main.rs` 聚焦于初始化顺序本身,不被测试逻辑淹没。
//! 测试在 `kernel_main` 的 init 顺序末尾、`irq_enable` **之前**执行
//! (隔离定时器中断干扰,保证确定性)。IPC 测试依赖 syscall 分发与
//! 调度器阻塞/唤醒原语,均已在 init 中就绪。
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

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::info;

/// 运行全部引导期冒烟测试(M2)。
///
/// 由 `kernel_main` 在 `sync::self_test` 之后、`arch::irq_enable`
/// 之前调用。
pub fn boot_tests() {
    boot_user_thread_test();
    boot_process_addrspace_test();
    boot_ipc_test();
    boot_ipc_stress_test();
    boot_shm_test();
    boot_cap_test();
    boot_fault_recovery_test();
    boot_elf_test();
    boot_ipc_latency_bench();
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

/// M2 T2a:同步 IPC(寄存器消息)+ 简化能力表(未授权拒绝)冒烟。
///
/// 场景(recv 先到、send 后到,验证阻塞配对与唤醒投递):
/// 1. 建进程 S/R,授权 `cap(S,0,R)`、`cap(R,0,S)`;S 的槽 2 保持**未授权**。
/// 2. 先 spawn R:R 写 ready 标记 0x51 → `recv(0)`(无 send 配对 → 阻塞);
/// 3. 主上下文看到 0x51 后 spawn S:S 先 `send(2, …)`(未授权,须**不阻塞**
///    返回 `-EACCES` 并存储),再 `send(0, [0x111,0x222,0x333,0x444,0x555])`
///    —— 与 R 的 pending recv 配对,唤醒 R 并投递消息 → S 存储状态后退出;
/// 4. R 被唤醒,`recv` 返回 status=0 + 5 字消息,存储后退出。
///
/// 断言:R status==0、R 收到 5 个 msg 字、S 未授权槽结果 == `-EACCES`、
/// S 授权 send status==0、双 done 标记到齐。
fn boot_ipc_test() {
    // 1) 建进程 S/R + 能力授权(S 槽 2 留空 = 未授权)。
    let s_pid = crate::process::create().expect("create S");
    let r_pid = crate::process::create().expect("create R");
    let root_s = crate::process::root(s_pid);
    let root_r = crate::process::root(r_pid);
    assert!(
        crate::process::grant_cap(s_pid, 0, r_pid).is_ok(),
        "grant cap(S,0,R)"
    );
    assert!(
        crate::process::grant_cap(r_pid, 0, s_pid).is_ok(),
        "grant cap(R,0,S)"
    );
    // 2) 用户程序(编码经逐条核对 S 型/I 型立即数;R 先注册 pending recv)。
    let prog_r: [u32; 16] = [
        0x4000_12b7, // lui   t0, 0x40001   (t0 = shared)
        0x0510_0313, // addi  t1, x0, 0x51  (ready marker)
        0x0062_a023, // sw    t1, 0(t0)     (shared[0] = 0x51)
        0x0000_0513, // addi  a0, x0, 0     (recv cap slot 0)
        0x0040_0893, // addi  a7, x0, 4     (SYSCALL_IPC_RECV)
        0x0000_0073, // ecall
        0x00a2_a223, // sw    a0, 4(t0)     (shared[1] = status)
        0x00b2_a423, // sw    a1, 8(t0)     (shared[2] = msg[0])
        0x00c2_a623, // sw    a2, 12(t0)    (shared[3] = msg[1])
        0x00d2_a823, // sw    a3, 16(t0)    (shared[4] = msg[2])
        0x00e2_aa23, // sw    a4, 20(t0)    (shared[5] = msg[3])
        0x00f2_ac23, // sw    a5, 24(t0)    (shared[6] = msg[4])
        0x0e50_0313, // addi  t1, x0, 0xE5  (done marker)
        0x0062_ae23, // sw    t1, 28(t0)    (shared[7] = 0xE5)
        0x0010_0893, // addi  a7, x0, 1     (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let prog_s: [u32; 23] = [
        0x4000_12b7, // lui   t0, 0x40001   (t0 = shared)
        0x0020_0513, // addi  a0, x0, 2     (send cap slot 2 = 未授权)
        0x1110_0593, // addi  a1, x0, 0x111
        0x2220_0613, // addi  a2, x0, 0x222
        0x3330_0693, // addi  a3, x0, 0x333
        0x4440_0713, // addi  a4, x0, 0x444
        0x5550_0793, // addi  a5, x0, 0x555
        0x0030_0893, // addi  a7, x0, 3     (SYSCALL_IPC_SEND)
        0x0000_0073, // ecall
        0x00a2_a023, // sw    a0, 0(t0)     (shared[0] = -EACCES)
        0x0000_0513, // addi  a0, x0, 0     (send cap slot 0 = 授权)
        0x1110_0593, // addi  a1, x0, 0x111
        0x2220_0613, // addi  a2, x0, 0x222
        0x3330_0693, // addi  a3, x0, 0x333
        0x4440_0713, // addi  a4, x0, 0x444
        0x5550_0793, // addi  a5, x0, 0x555
        0x0030_0893, // addi  a7, x0, 3     (SYSCALL_IPC_SEND)
        0x0000_0073, // ecall
        0x00a2_a223, // sw    a0, 4(t0)     (shared[1] = send status)
        0x7770_0313, // addi  t1, x0, 0x777 (done marker, 12-bit 正立即数)
        0x0062_a423, // sw    t1, 8(t0)     (shared[2] = 0x777)
        0x0010_0893, // addi  a7, x0, 1     (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let (_, r_shared_pa) = map_iso_proc(root_r, &prog_r);
    let (_, s_shared_pa) = map_iso_proc(root_s, &prog_s);
    let r_shared = r_shared_pa as *const u32;
    let s_shared = s_shared_pa as *const u32;
    // 3) 先 spawn R(prio HIGH):主上下文 yield 轮询,直至 R 写 ready 标记
    //    (R 随即 recv 阻塞)。R 阻塞后主上下文恢复,才 spawn S。
    crate::sched::spawn_user(r_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    let mut guard = 0;
    while unsafe { core::ptr::read_volatile(r_shared) } != 0x51 {
        assert!(guard < 200_000, "R did not become ready (recv pending)");
        crate::sched::yield_();
        guard += 1;
    }
    // 4) spawn S:未授权 send 立即返回 -EACCES,授权 send 与 R 配对。
    crate::sched::spawn_user(s_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    // 5) 轮询双方 done 标记(R shared[7]=0xE5, S shared[2]=0x777)。
    let mut guard = 0;
    loop {
        let r_done = unsafe { core::ptr::read_volatile(r_shared.add(7)) };
        let s_done = unsafe { core::ptr::read_volatile(s_shared.add(2)) };
        if r_done == 0xE5 && s_done == 0x777 {
            break;
        }
        assert!(
            guard < 200_000,
            "IPC roundtrip timeout: r_done={r_done:#x} s_done={s_done:#x}"
        );
        crate::sched::yield_();
        guard += 1;
    }
    // 6) 断言:R 收到 status=0 + 5 字消息;S 未授权结果 == -EACCES。
    let r_status = unsafe { core::ptr::read_volatile(r_shared.add(1)) };
    let r_msg = [
        unsafe { core::ptr::read_volatile(r_shared.add(2)) },
        unsafe { core::ptr::read_volatile(r_shared.add(3)) },
        unsafe { core::ptr::read_volatile(r_shared.add(4)) },
        unsafe { core::ptr::read_volatile(r_shared.add(5)) },
        unsafe { core::ptr::read_volatile(r_shared.add(6)) },
    ];
    let s_unauth = unsafe { core::ptr::read_volatile(s_shared) };
    let s_status = unsafe { core::ptr::read_volatile(s_shared.add(1)) };
    assert!(
        s_unauth == crate::ipc::IPC_ERR_EACCES as u32,
        "unauthorized send must reject -EACCES (got {s_unauth:#x})"
    );
    assert!(s_status == 0, "authorized send must succeed");
    assert!(r_status == 0, "recv must succeed");
    assert!(
        r_msg == [0x111, 0x222, 0x333, 0x444, 0x555],
        "msg roundtrip mismatch: {r_msg:?}"
    );
    info!("M2 T2a: sync IPC ok (reg-msg roundtrip, cap -EACCES reject)");
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
    // 栈守护页 = 0x4000_2000 处的**未映射空洞**(D20 语义:溢出越界即页故障),
    // 无需单独分配 —— 曾有一个从未映射/从未释放的 `_stack_lo` 占位页,
    // 每进程泄漏 1 页(M2 D12 自审清理)。stack_hi 映射于 0x4000_3000。
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

/// M2 T2b:IPC 压力测试 —— `STRESS_R` 次 send/recv 配对(内核线程环)。
///
/// 两个**内核**线程(`spawn_owned` 挂到进程 pa/pb)交替 send/recv,每次
/// NoPeer 路径登记捐赠、配对完成撤销捐赠 —— 捐赠机制随配对一并受压。
/// 场景:
/// - sender(send `[BASE+i,0,0,0,0]`,NoPeer → block_current);
/// - recver(recv,Done 直取或 NoPeer → block → `take_ipc_msg` 取消息),
///   校验 `m[0]==BASE+i` 并累加和。
///
/// 用内核线程而非手工汇编用户程序:避免 T2a 的编码错误风险,且覆盖
/// `Thread.ipc_msg`/`take_ipc_msg`(内核线程收消息路径)。断言:配对数
/// 到齐、消息和 == 等差数列和(无丢失无损坏;顺序由 pending FIFO 保证)、
/// 捐赠表配对后清空。
fn boot_ipc_stress_test() {
    let pa = crate::process::create().expect("stress: create pa");
    let pb = crate::process::create().expect("stress: create pb");
    assert!(
        crate::process::grant_cap(pa, 0, pb).is_ok(),
        "stress: grant cap(pa,0,pb)"
    );
    assert!(
        crate::process::grant_cap(pb, 0, pa).is_ok(),
        "stress: grant cap(pb,0,pa)"
    );
    STRESS_PA.store(pa, Ordering::Relaxed);
    STRESS_PB.store(pb, Ordering::Relaxed);
    STRESS_RECV_COUNT.store(0, Ordering::Relaxed);
    STRESS_RECV_SUM.store(0, Ordering::Relaxed);
    STRESS_SENDER_DONE.store(false, Ordering::Relaxed);
    STRESS_RECVER_DONE.store(false, Ordering::Relaxed);
    crate::sched::spawn_owned(stress_sender, crate::sched::PRIO_HIGH, pa);
    crate::sched::spawn_owned(stress_recver, crate::sched::PRIO_HIGH, pb);
    // 协作轮询直至双方完成(超时守卫;boot 期 SIE=0,无抢占)。
    let mut guard = 0;
    while (!STRESS_SENDER_DONE.load(Ordering::Relaxed)
        || !STRESS_RECVER_DONE.load(Ordering::Relaxed))
        && guard < 2_000_000
    {
        crate::sched::yield_();
        guard += 1;
    }
    if !STRESS_SENDER_DONE.load(Ordering::Relaxed) || !STRESS_RECVER_DONE.load(Ordering::Relaxed) {
        panic!(
            "IPC stress timeout: count={} sum={:#x}",
            STRESS_RECV_COUNT.load(Ordering::Relaxed),
            STRESS_RECV_SUM.load(Ordering::Relaxed)
        );
    }
    let count = STRESS_RECV_COUNT.load(Ordering::Relaxed);
    let sum = STRESS_RECV_SUM.load(Ordering::Relaxed);
    let expect_sum = STRESS_R * STRESS_BASE + STRESS_R * (STRESS_R - 1) / 2;
    assert!(count == STRESS_R, "IPC stress: count {count} != {STRESS_R}");
    assert!(
        sum == expect_sum,
        "IPC stress: sum {sum:#x} != {expect_sum:#x}"
    );
    assert!(
        crate::sched::donation_count() == 0,
        "IPC stress: donations not drained after pairing"
    );
    info!("M2 T2b: IPC stress ok ({STRESS_R} pairings, sum ok)");
}

/// M2 T2b:压力发送线程 —— 顺序发 `[BASE+i,0,0,0,0]`;NoPeer → 阻塞。
fn stress_sender() {
    let pa = STRESS_PA.load(Ordering::Relaxed);
    for i in 0..STRESS_R {
        match crate::ipc::send(pa, 0, [STRESS_BASE + i, 0, 0, 0, 0]) {
            Ok(crate::ipc::SendBlock::Done) => {}
            Ok(crate::ipc::SendBlock::NoPeer) => crate::sched::block_current(),
            Err(e) => panic!("stress sender: send {i} err {e:#x}"),
        }
    }
    STRESS_SENDER_DONE.store(true, Ordering::Relaxed);
}

/// M2 T2b:压力接收线程 —— 顺序 recv;Done 直取,NoPeer → 阻塞后经
/// `take_ipc_msg` 取内核线程消息;校验消息号并累加和。
fn stress_recver() {
    let pb = STRESS_PB.load(Ordering::Relaxed);
    for i in 0..STRESS_R {
        let m = match crate::ipc::recv(pb, 0) {
            Ok(crate::ipc::RecvBlock::Done(m)) => m,
            Ok(crate::ipc::RecvBlock::NoPeer) => {
                crate::sched::block_current();
                crate::sched::take_ipc_msg().expect("stress recver: no msg")
            }
            Err(e) => panic!("stress recver: recv {i} err {e:#x}"),
        };
        if m[0] != STRESS_BASE + i {
            panic!("stress recver: msg {i} corrupt {:#x}", m[0]);
        }
        STRESS_RECV_COUNT.fetch_add(1, Ordering::Relaxed);
        STRESS_RECV_SUM.fetch_add(m[0], Ordering::Relaxed);
    }
    STRESS_RECVER_DONE.store(true, Ordering::Relaxed);
}

/// 压力测试配对次数。
const STRESS_R: usize = 1000;
/// 消息序列基数(消息 0 号 = STRESS_BASE)。
const STRESS_BASE: usize = 0x1000;
static STRESS_PA: AtomicUsize = AtomicUsize::new(0);
static STRESS_PB: AtomicUsize = AtomicUsize::new(0);
static STRESS_RECV_COUNT: AtomicUsize = AtomicUsize::new(0);
static STRESS_RECV_SUM: AtomicUsize = AtomicUsize::new(0);
static STRESS_SENDER_DONE: AtomicBool = AtomicBool::new(false);
static STRESS_RECVER_DONE: AtomicBool = AtomicBool::new(false);

// ===== M2 T3c:共享内存(mmap_share)+ 能力 revoke/dup =====

/// M2 T3c:共享内存测试 —— 一页物理内存映射进 A/B 两进程地址空间,双槽改授
/// `Cap::Shm(id)`;A 写 0xA5、B 读回并写 0xB5;用户态 `syscall 6`(CAP_REVOKE)
/// 撤销后双 root 映射消失、页回收、双槽失效、注册表出列。
///
/// 场景:
/// 1. A 槽 0 持 `Cap::Proc(B)`;`syscall 5 (SHM_MAP)`(a0=本槽0,a1=对端槽1,
///    a2=len=4096)→ `mmap_share`:分配一页、映射到 A/B 的 `SHM_VA`,
///    改授 A 槽0/B 槽1 为 `Cap::Shm(id)`,返回 shm_id(存 A shared[0]);
/// 2. A 写 `SHM_VA[0]=0xA5` → A done(shared[1]=0x333);
/// 3. 主上下文经 `shm_paddr(id)` 读物理页,断言 0xA5;
/// 4. B 读 `SHM_VA[0]`(=0xA5)→ 存 B shared[0];写 `SHM_VA[1]=0xB5` →
///    存 B shared[1](revoke 后 B 的私有页仍可读的记录);
/// 5. B `syscall 6 (CAP_REVOKE)`(a0=槽 1,Shm cap)→ 整页撤销,status 存
///    B shared[2];B done(shared[3]=0x777);
/// 6. 断言:双 root `is_mapped(SHM_VA)==false`、双槽失效(NotFound)、
///    `shm_paddr(id)==None`(注册表出列)。页回收经 revoke status==0 验证
///    (shm_revoke 内部 `free_pages` 失败会返回错误)。
fn boot_shm_test() {
    // 1) 建进程 A/B;A 槽 0 持 Cap::Proc(B)。B 槽 1 初始为空,由 mmap_share 改授。
    let a_pid = crate::process::create().expect("shm: create A");
    let b_pid = crate::process::create().expect("shm: create B");
    let root_a = crate::process::root(a_pid);
    let root_b = crate::process::root(b_pid);
    assert!(
        crate::process::grant_cap(a_pid, 0, b_pid).is_ok(),
        "shm: grant cap(A,0,B)"
    );
    // 2) 用户程序(机器码逐条核对 S 型/I 型立即数;A 建共享,B 读回+撤销)。
    let prog_a: [u32; 14] = [
        0x4000_12b7, // lui   t0, 0x40001        (t0 = shared)
        0x0000_0513, // addi  a0, x0, 0         (SHM_MAP: 本槽 0)
        0x0010_0593, // addi  a1, x0, 1         (对端槽 1)
        0x0000_1637, // lui   a2, 1             (len = 4096)
        0x0050_0893, // addi  a7, x0, 5         (SYSCALL_SHM_MAP)
        0x0000_0073, // ecall
        0x00a2_a023, // sw    a0, 0(t0)         (shared[0] = shm_id)
        0x5000_0337, // lui   t1, 0x50000       (t1 = SHM_VA)
        0x0a50_0393, // addi  t2, x0, 0xA5
        0x0073_2023, // sw    t2, 0(t1)         (SHM_VA[0] = 0xA5)
        0x3330_0313, // addi  t1, x0, 0x333     (done marker)
        0x0062_a223, // sw    t1, 4(t0)         (shared[1] = 0x333)
        0x0010_0893, // addi  a7, x0, 1         (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let prog_b: [u32; 15] = [
        0x4000_12b7, // lui   t0, 0x40001        (t0 = shared)
        0x5000_0337, // lui   t1, 0x50000        (t1 = SHM_VA)
        0x0003_2e03, // lw    t3, 0(t1)          (t3 = SHM_VA[0] = 0xA5)
        0x01c2_a023, // sw    t3, 0(t0)          (shared[0] = 0xA5)
        0x0b50_0393, // addi  t2, x0, 0xB5
        0x0073_2223, // sw    t2, 4(t1)          (SHM_VA[1] = 0xB5)
        0x0072_a223, // sw    t2, 4(t0)          (shared[1] = 0xB5 记录)
        0x0010_0513, // addi  a0, x0, 1          (CAP_REVOKE: 槽 1 = Shm cap)
        0x0060_0893, // addi  a7, x0, 6          (SYSCALL_CAP_REVOKE)
        0x0000_0073, // ecall
        0x00a2_a423, // sw    a0, 8(t0)          (shared[2] = revoke status)
        0x7770_0313, // addi  t1, x0, 0x777      (done marker)
        0x0062_a623, // sw    t1, 12(t0)         (shared[3] = 0x777)
        0x0010_0893, // addi  a7, x0, 1          (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let (_, a_shared_pa) = map_iso_proc(root_a, &prog_a);
    let (_, b_shared_pa) = map_iso_proc(root_b, &prog_b);
    let a_shared = a_shared_pa as *const u32;
    let b_shared = b_shared_pa as *const u32;
    // 3) spawn A:主上下文 yield 轮询,直至 A 建好共享页并写 done 标记。
    crate::sched::spawn_user(a_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    let mut guard = 0;
    while unsafe { core::ptr::read_volatile(a_shared.add(1)) } != 0x333 {
        assert!(guard < 200_000, "A did not create shm");
        crate::sched::yield_();
        guard += 1;
    }
    let shm_id = unsafe { core::ptr::read_volatile(a_shared) } as usize;
    // 4) revoke 前:物理页已有 A 写的 0xA5(同一页 = SHM_VA+0)。
    let paddr = crate::shm::shm_paddr(shm_id).expect("shm: must be registered");
    assert!(
        unsafe { core::ptr::read_volatile(paddr as *const u32) } == 0xA5,
        "shm: A marker before revoke"
    );
    // 5) spawn B:轮询 B done(读回、写记录、revoke 全部完成)。
    crate::sched::spawn_user(b_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    let mut guard = 0;
    while unsafe { core::ptr::read_volatile(b_shared.add(3)) } != 0x777 {
        assert!(guard < 200_000, "B shm roundtrip timeout");
        crate::sched::yield_();
        guard += 1;
    }
    // 6) B 读到的共享页值 == 0xA5;B 写回记录 == 0xB5(revoke 后仍可读);
    //    revoke status == 0(整页撤销成功,页已回收)。
    let b_read = unsafe { core::ptr::read_volatile(b_shared) };
    assert!(
        b_read == 0xA5,
        "shm: B must read A marker (got {b_read:#x})"
    );
    assert!(
        unsafe { core::ptr::read_volatile(b_shared.add(1)) } == 0xB5,
        "shm: B must write B marker"
    );
    assert!(
        unsafe { core::ptr::read_volatile(b_shared.add(2)) } == 0,
        "shm: B revoke must succeed"
    );
    // 7) revoke 后:双 root 均不映射 SHM_VA;双槽失效;注册表出列。
    assert!(
        !crate::mmu::is_mapped(root_a, crate::shm::SHM_VA),
        "shm: A map must be gone"
    );
    assert!(
        !crate::mmu::is_mapped(root_b, crate::shm::SHM_VA),
        "shm: B map must be gone"
    );
    assert!(
        crate::process::cap_target(a_pid, 0) == Err(crate::process::CapError::NotFound),
        "shm: A cap slot must be cleared"
    );
    assert!(
        crate::process::cap_target(b_pid, 1) == Err(crate::process::CapError::NotFound),
        "shm: B cap slot must be cleared"
    );
    assert!(
        crate::shm::shm_paddr(shm_id).is_none(),
        "shm: registry must be drained"
    );
    info!("M2 T3c: shared mem ok (map/revoke)");
}

/// M2 T3c:能力 dup/revoke 测试 —— Proc 能力的复制与撤销语义。
///
/// A 槽 0 持 `Cap::Proc(B)`:
/// 1. `syscall 7 (CAP_DUP)`(a0=源槽0, a1=目标槽2)→ 复制 Cap 值到槽 2,
///    status 存 shared[0];
/// 2. `syscall 6 (CAP_REVOKE)`(a0=槽 0,Proc cap)→ 仅清原槽,status 存
///    shared[1];
/// 3. A done(shared[2]=0x777)。
///
/// 断言:dup/revoke 均成功;槽 2 仍持 `Cap::Proc(B)`(dup 副本不受 revoke
/// 影响);槽 0 已失效(NotFound)。
fn boot_cap_test() {
    let a_pid = crate::process::create().expect("cap: create A");
    let b_pid = crate::process::create().expect("cap: create B");
    let root_a = crate::process::root(a_pid);
    assert!(
        crate::process::grant_cap(a_pid, 0, b_pid).is_ok(),
        "cap: grant cap(A,0,B)"
    );
    let prog: [u32; 14] = [
        0x4000_12b7, // lui   t0, 0x40001       (t0 = shared)
        0x0000_0513, // addi  a0, x0, 0         (CAP_DUP: 源槽 0)
        0x0020_0593, // addi  a1, x0, 2         (目标槽 2)
        0x0070_0893, // addi  a7, x0, 7         (SYSCALL_CAP_DUP)
        0x0000_0073, // ecall
        0x00a2_a023, // sw    a0, 0(t0)         (shared[0] = dup status)
        0x0000_0513, // addi  a0, x0, 0         (CAP_REVOKE: 槽 0)
        0x0060_0893, // addi  a7, x0, 6         (SYSCALL_CAP_REVOKE)
        0x0000_0073, // ecall
        0x00a2_a223, // sw    a0, 4(t0)         (shared[1] = revoke status)
        0x7770_0313, // addi  t1, x0, 0x777     (done marker)
        0x0062_a423, // sw    t1, 8(t0)         (shared[2] = 0x777)
        0x0010_0893, // addi  a7, x0, 1         (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let (_, a_shared_pa) = map_iso_proc(root_a, &prog);
    let a_shared = a_shared_pa as *const u32;
    crate::sched::spawn_user(a_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    let mut guard = 0;
    while unsafe { core::ptr::read_volatile(a_shared.add(2)) } != 0x777 {
        assert!(guard < 200_000, "cap dup/revoke timeout");
        crate::sched::yield_();
        guard += 1;
    }
    // dup 成功(status 0);revoke 成功(status 0)。
    assert!(
        unsafe { core::ptr::read_volatile(a_shared) } == 0,
        "cap: dup must succeed"
    );
    assert!(
        unsafe { core::ptr::read_volatile(a_shared.add(1)) } == 0,
        "cap: revoke must succeed"
    );
    // dup 槽 2 仍指向 B;revoke 只清原槽 0。
    use crate::process::Cap;
    assert!(
        crate::process::cap_target(a_pid, 2) == Ok(Cap::Proc(b_pid)),
        "cap: dup slot must point to B"
    );
    assert!(
        crate::process::cap_target(a_pid, 0) == Err(crate::process::CapError::NotFound),
        "cap: revoked slot must be gone"
    );
    info!("M2 T3c: cap dup/revoke ok");
}

// ===== M2 D12:用户态异常恢复(进程故障 → 杀进程) =====

/// M2 D12:用户态故障恢复 —— 进程故障被隔离为"杀进程",系统存活。
///
/// 场景:
/// - 受害者进程 F:spawn 用户线程执行 `lw t0, 0(zero)`(机器码逐条核对:
///   0x0000_2283 = opcode 0x03(lw) + funct3 010 + rd=t0(x5) + imm=0 →
///   U 模式读 VA 0(根表未映射)→ scause=0xd 页故障、stval=0、SPP=0);
///   经 D12 kill 路径被杀:地址空间销毁(`destroy_root` 回收页)、IPC 清理、
///   调度器线程标记退出、切走;
/// - 健康进程 H:spawn 用户线程写 marker 后正常退出 —— 验证系统未被
///   F 的故障拖垮。
///
/// 断言:
/// - kill 计数 == 1(F 恰好被杀一次;若故障未被隔离,F 会正常 exit →
///   计数 0 → 断言失败,暴露回归);
/// - H 的 marker 到齐(系统存活,调度仍正常);
/// - `process::pid_root(F)==None`(F 已销毁)、`pid_root(H)==Some(root_h)`;
/// - 空闲页数回到"F 创建前"水平(F 的地址空间页已全部回收)。
///
/// M2 D12 自审。被杀进程的**阻塞 donor 线程** —— send 到 H 后阻塞(登记
/// 指向 H 的捐赠),随后 F 被用户态故障杀掉。验证 kill 路径撤销被杀进程
/// 线程作为 donor 发出的捐赠(否则陈旧捐赠永久抬升 H 并占死捐赠表槽)。
fn fault_donor() {
    let f = FAULT_F_PID.load(Ordering::Relaxed);
    match crate::ipc::send(f, 0, [0x55, 0, 0, 0, 0]) {
        // 无人 recv → NoPeer 登记 pending + 捐赠,随后阻塞至被杀。
        Ok(crate::ipc::SendBlock::NoPeer) => crate::sched::block_current(),
        Ok(crate::ipc::SendBlock::Done) => panic!("fault donor: unexpected pairing"),
        Err(e) => panic!("fault donor: send err {e:#x}"),
    }
}

/// F 进程 id(fault_donor 读 cap 目标用)。
static FAULT_F_PID: AtomicUsize = AtomicUsize::new(0);

fn boot_fault_recovery_test() {
    // 1) 建 F / H 两个进程。
    let f_pid = crate::process::create().expect("fault: create F");
    let h_pid = crate::process::create().expect("fault: create H");
    let root_f = crate::process::root(f_pid);
    let root_h = crate::process::root(h_pid);
    // F 程序:`lw t0, 0(x0)` → 页故障;后续 exit 为**死代码**(若故障未被
    // 隔离,F 会正常退出 → kill 计数 0 → 断言失败)。
    let prog_f: [u32; 3] = [
        0x0000_2283, // lw   t0, 0(x0)   (U 模式读 VA 0:页故障 scause=0xd)
        0x0010_0893, // addi a7, x0, 1   (SYSCALL_EXIT, 死代码)
        0x0000_0073, // ecall
    ];
    let prog_h: [u32; 5] = [
        0x4000_12b7, // lui  t0, 0x40001  (t0 = shared)
        0x0510_0313, // addi t1, x0, 0x51 (marker)
        0x0062_a023, // sw   t1, 0(t0)    (shared[0] = 0x51)
        0x0010_0893, // addi a7, x0, 1    (SYSCALL_EXIT)
        0x0000_0073, // ecall
    ];
    let (_, _f_shared_pa) = map_iso_proc(root_f, &prog_f);
    let (_, h_shared_pa) = map_iso_proc(root_h, &prog_h);
    let h_shared = h_shared_pa as *const u32;
    // 2) F 授权 cap(F, 0, H):供 F 的 donor 线程 send 到 H。
    assert!(
        crate::process::grant_cap(f_pid, 0, h_pid).is_ok(),
        "fault: grant cap(F,0,H)"
    );
    FAULT_F_PID.store(f_pid, Ordering::Relaxed);
    // 3) spawn F 的 donor 内核线程(**先**于故障线程,先入队先运行:send →
    //    NoPeer → 阻塞并登记 F→H 捐赠)。此时其内核栈已分配。
    crate::sched::spawn_owned(fault_donor, crate::sched::PRIO_HIGH, f_pid);
    // 4) 全部分配完成后取空闲页基线(F 地址空间 + donor 栈的回收断言参照)。
    let free_before_f = crate::mem::free_page_count();
    // 5) 再 spawn 故障线程(HIGH,donor 阻塞后首启即故障),最后 spawn H。
    crate::sched::spawn_user(f_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    crate::sched::spawn_user(h_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    // 6) 轮询 H 的 marker(系统存活 = 调度仍正常)。
    let mut guard = 0;
    while unsafe { core::ptr::read_volatile(h_shared) } != 0x51 {
        assert!(
            guard < 200_000,
            "fault: system dead after user fault (H marker missing)"
        );
        crate::sched::yield_();
        guard += 1;
    }
    // 7) 显式清空 reaper(被杀 donor 的栈移交 reaper,回收前先排空,
    //    使空闲页断言确定性成立 —— idle 轮询时间不可作强假设)。
    crate::sched::drain_reaper();
    // 8) 断言:kill 计数 == 1;F 已销毁、H 存活;F 地址空间页已回收;
    //    被杀进程线程发出的陈旧捐赠已撤销。
    assert!(
        crate::sched::fault_kill_count() == 1,
        "fault: expected exactly 1 kill (got {})",
        crate::sched::fault_kill_count()
    );
    assert!(
        crate::process::pid_root(f_pid).is_none(),
        "fault: victim process must be destroyed"
    );
    assert!(
        crate::process::pid_root(h_pid) == Some(root_h),
        "fault: healthy process must survive"
    );
    assert!(
        crate::sched::donation_count() == 0,
        "fault: stale donation from killed process not revoked (got {})",
        crate::sched::donation_count()
    );
    let free_after = crate::mem::free_page_count();
    assert!(
        free_after >= free_before_f,
        "fault: address space not reclaimed (free {free_after} < {free_before_f})"
    );
    info!("M2: user fault recovery ok (process killed, system alive)");
}

// ===== M3 T1:ELF 加载器(独立编译用户程序) =====

/// 共享标记页 VA:须与 `user/src/main.rs` 的 `MARKER_VA`(0x4000_2000)
/// 一致;且在 hello ELF 段映射范围之外(下方以 parse 结果断言 max_end)。
const ELF_MARKER_VA: usize = 0x4000_2000;

/// M3 T1:ELF 加载器冒烟(主交付)。
///
/// 验证链路:建进程 → `elf::parse`(校验 + 段不侵占 marker 页)→
/// `elf::load`(include_bytes! 的 hello ELF 逐段映射 + 建初始栈 argc/argv)
/// → `spawn_user_args`(argc/argv 注入 a0/a1)→ U 模式运行 → 用户程序写
/// marker 页 `0xC0DE0000|argc` → 轮询断言 → 结构性断言(栈映射/守护页
/// 空洞)→ 负面用例 → 销毁进程回收地址空间页。banner:
/// `M3 T1: ELF loader ok (user ELF ran)`(6 处 grep 同步)。
fn boot_elf_test() {
    // 1) 建进程。
    let pid = crate::process::create().expect("elf: create process");
    let root = crate::process::root(pid);
    // 2) 校验内嵌 ELF:入口非零、段不侵占 marker 页(加载器与测试自洽)。
    let info = crate::elf::parse(crate::elf::HELLO_ELF).expect("elf: parse hello");
    assert!(info.entry != 0, "elf: entry must be non-zero");
    let max_end = info
        .segments
        .iter()
        .map(|s| s.vaddr + s.memsz)
        .max()
        .unwrap_or(0);
    assert!(
        max_end <= ELF_MARKER_VA,
        "hello ELF segments grew beyond marker page (max_end={max_end:#x})"
    );
    // 3) 映射 marker 页(用户程序写 `0xC0DE0000 | argc` 于此)。
    let marker_pa = crate::mem::alloc_pages_zeroed(0).expect("elf: marker page");
    assert!(
        crate::mmu::map_user_page(root, ELF_MARKER_VA, marker_pa, 0xC7).is_ok(),
        "elf: map marker page"
    );
    // 4) 加载 ELF + 建初始栈(argv[0] = "hello")。
    let args: [&[u8]; 1] = [b"hello"];
    let loaded = crate::elf::load(pid, crate::elf::HELLO_ELF, &args).expect("elf: load hello");
    assert!(loaded.argc == 1, "elf: argc must be 1");
    // 5) spawn 用户线程(argc/argv 经初始帧 a0/a1 注入)。
    crate::sched::spawn_user_args(
        pid,
        loaded.entry,
        loaded.stack_top,
        crate::sched::PRIO_HIGH,
        loaded.argc,
        loaded.argv,
    );
    // 6) 轮询 marker 直至用户程序写入(超时守卫)。
    let marker = marker_pa as *const usize;
    let expect = 0xC0DE_0000 | args.len();
    let mut guard = 0;
    loop {
        let v = unsafe { core::ptr::read_volatile(marker) };
        if v == expect {
            break;
        }
        assert!(
            guard < 200_000,
            "user ELF did not write marker (value={v:#x}, expect={expect:#x})"
        );
        crate::sched::yield_();
        guard += 1;
    }
    // 7) 结构性断言:用户栈顶页已映射、栈底守护页未映射(D20 语义)。
    assert!(
        crate::mmu::is_mapped(root, crate::elf::USER_STACK_TOP - 4096),
        "elf: user stack top page must be mapped"
    );
    assert!(
        !crate::mmu::is_mapped(
            root,
            crate::elf::USER_STACK_TOP - (crate::elf::USER_STACK_PAGES + 1) * 4096,
        ),
        "elf: stack guard page must be unmapped"
    );
    // 8) 负面用例(parse 全字段校验)。
    elf_negative_cases();
    // 9) 清理:切内核根表 + 销毁进程(回收地址空间页;destroy_root 前提)。
    crate::mmu::switch_root(crate::mmu::kernel_root());
    let free_before = crate::mem::free_page_count();
    crate::process::destroy(pid);
    crate::sched::drain_reaper();
    let free_after = crate::mem::free_page_count();
    assert!(
        free_after >= free_before,
        "elf: address space not reclaimed (free {free_after} < {free_before})"
    );
    info!("M3 T1: ELF loader ok (user ELF ran)");
}

/// 构造最小合法 ELF64(1 个 PT_LOAD),供负面用例逐字段破坏。
/// `phdr = (flags, offset, vaddr, filesz, memsz)`。
fn make_elf(entry: u64, phdr: (u32, u64, u64, u64, u64)) -> alloc::vec::Vec<u8> {
    let mut e = alloc::vec![0u8; 64 + 56];
    e[0..4].copy_from_slice(b"\x7fELF");
    e[4] = 2; // EI_CLASS = ELFCLASS64
    e[5] = 1; // EI_DATA = ELFDATA2LSB
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    e[18..20].copy_from_slice(&243u16.to_le_bytes()); // e_machine = EM_RISCV
    e[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
    e[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    e[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
    let (flags, offset, vaddr, filesz, memsz) = phdr;
    let off = 64;
    e[off..off + 4].copy_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    e[off + 4..off + 8].copy_from_slice(&flags.to_le_bytes());
    e[off + 8..off + 16].copy_from_slice(&offset.to_le_bytes());
    e[off + 16..off + 24].copy_from_slice(&vaddr.to_le_bytes());
    // p_paddr = 0(加载器忽略)
    e[off + 32..off + 40].copy_from_slice(&filesz.to_le_bytes());
    e[off + 40..off + 48].copy_from_slice(&memsz.to_le_bytes());
    e
}

/// M3 T1:ELF 解析负面用例(每个字段破坏 → 对应 ElfError)。
fn elf_negative_cases() {
    use crate::elf::ElfError;
    // 合法基线。
    let ok = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 8, 8));
    assert!(crate::elf::parse(&ok).is_ok(), "elf: baseline parse ok");
    // 坏 magic。
    let mut bad = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 8, 8));
    bad[0] = 0;
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::BadMagic)));
    // 非 64 位。
    let mut bad = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 8, 8));
    bad[4] = 1;
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::Not64)));
    // 非 ET_EXEC。
    let mut bad = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 8, 8));
    bad[16] = 1;
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::NotExec)));
    // 非 RISC-V(e_machine = bytes[18..20] LE = 243 = 0x00F3;改低字节 18)。
    let mut bad = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 8, 8));
    bad[18] = 0;
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::NotRiscv)));
    // memsz < filesz(offset 有效,使检查命中 BadSegment 而非先 Truncated)。
    let bad = make_elf(0x4000_0000, (5, 0x0, 0x4000_0000, 0x10, 8));
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::BadSegment)));
    // 文件内越界(p_offset + p_filesz > len)。
    let bad = make_elf(0x4000_0000, (5, 0x1000, 0x4000_0000, 0x1000, 0x1000));
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::Truncated)));
    // vaddr 越界(Sv39 用户区上限 2^38;高于或等于即拒绝)。
    let bad = make_elf(0x4000_0000, (5, 0x1000, 0x40_0000_0000, 8, 8));
    assert!(matches!(
        crate::elf::parse(&bad),
        Err(ElfError::AddressTooHigh)
    ));
    // 页内偏移失配(offset 有效,使检查命中 BadSegment 而非先 Truncated)。
    let bad = make_elf(0x4000_0000, (5, 0x1, 0x4000_0000, 8, 8));
    assert!(matches!(crate::elf::parse(&bad), Err(ElfError::BadSegment)));
    // 截断(头不足)。
    assert!(matches!(
        crate::elf::parse(&[0u8; 32]),
        Err(ElfError::Truncated)
    ));
    // 段间覆盖:两 PT_LOAD 映射范围重叠(offset=0 有效,使检查命中
    // Overlap 而非先 Truncated)。
    let mut e = alloc::vec![0u8; 64 + 56 * 2];
    e[0..4].copy_from_slice(b"\x7fELF");
    e[4] = 2;
    e[5] = 1;
    e[16..18].copy_from_slice(&2u16.to_le_bytes());
    e[18..20].copy_from_slice(&243u16.to_le_bytes());
    e[24..32].copy_from_slice(&0x4000_0000u64.to_le_bytes());
    e[32..40].copy_from_slice(&64u64.to_le_bytes());
    e[56..58].copy_from_slice(&2u16.to_le_bytes());
    for (i, (offset, vaddr, memsz)) in [
        (0u64, 0x4000_0000u64, 0x1000u64),
        (0u64, 0x4000_0000u64, 0x1000u64),
    ]
    .iter()
    .enumerate()
    {
        let off = 64 + i * 56;
        e[off..off + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        e[off + 4..off + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R|PF_X
        e[off + 8..off + 16].copy_from_slice(&offset.to_le_bytes());
        e[off + 16..off + 24].copy_from_slice(&vaddr.to_le_bytes());
        e[off + 32..off + 40].copy_from_slice(&8u64.to_le_bytes()); // filesz
        e[off + 40..off + 48].copy_from_slice(&memsz.to_le_bytes());
    }
    assert!(matches!(crate::elf::parse(&e), Err(ElfError::Overlap)));
}

// ===== M2 性能:IPC 延迟基准(注册消息往返) =====

/// IPC 延迟基准:两个进程(经 cap 授权)的 ping-pong 线程各执行
/// `send → recv`(请求/响应)`,配对完成即一轮往返。计时在发送方
/// 线程内部、**预热 10 轮之后**开始(warmup 建立稳定配对节奏并覆盖
/// 首次捐赠/唤醒开销),测得 `IPC_BENCH_N` 轮往返总 timebase 计数,
/// 除以 `IPC_BENCH_N` 折算每轮延迟。
///
/// 语义说明:一轮往返 = 4 次系统调用(send/recv × 2)+ 2 次上下文切换
/// (sender→recver→sender)+ 2 次 PIP 捐赠注册/撤销 —— 即"寄存器消息
/// 同步 IPC 往返"的端到端成本。QEMU 时钟抖动 ±20%,数值仅作相对基准。
fn boot_ipc_latency_bench() {
    let pa = crate::process::create().expect("ipc bench: create pa");
    let pb = crate::process::create().expect("ipc bench: create pb");
    assert!(
        crate::process::grant_cap(pa, 0, pb).is_ok(),
        "ipc bench: grant cap(pa,0,pb)"
    );
    assert!(
        crate::process::grant_cap(pb, 0, pa).is_ok(),
        "ipc bench: grant cap(pb,0,pa)"
    );
    BENCH_IPC_PA.store(pa, Ordering::Relaxed);
    BENCH_IPC_PB.store(pb, Ordering::Relaxed);
    BENCH_IPC_TOTAL.store(0, Ordering::Relaxed);
    BENCH_IPC_DONE.store(false, Ordering::Relaxed);
    crate::sched::spawn_owned(ipc_bench_sender, crate::sched::PRIO_HIGH, pa);
    crate::sched::spawn_owned(ipc_bench_recver, crate::sched::PRIO_HIGH, pb);
    // 协作轮询直至测量完成(超时守卫;boot 期 SIE=0,无抢占)。
    let mut guard = 0;
    while !BENCH_IPC_DONE.load(Ordering::Relaxed) && guard < 4_000_000 {
        crate::sched::yield_();
        guard += 1;
    }
    assert!(BENCH_IPC_DONE.load(Ordering::Relaxed), "ipc bench timeout");
    let dt = BENCH_IPC_TOTAL.load(Ordering::Relaxed);
    // V4 纪律:用运行时 timebase 频率换算,不硬编码 10MHz。
    let freq = crate::board::timer_freq() as u64;
    let ns_per_tick = 1_000_000_000u64 / freq;
    let us_roundtrip = (dt as u64).saturating_mul(ns_per_tick) / IPC_BENCH_N as u64 / 1000;
    info!("M2: IPC latency ok (reg-msg ~{dt} ticks ~{us_roundtrip} us/roundtrip)");
}

/// IPC 延迟基准:发送方 —— 预热后计时 N 轮 `send→recv`,校验响应消息。
fn ipc_bench_sender() {
    let pa = BENCH_IPC_PA.load(Ordering::Relaxed);
    // 1) 预热:建立"send 阻塞、recv 配对"的稳定节奏,覆盖首次开销。
    for i in 0..IPC_BENCH_WARMUP {
        ipc_bench_send(pa, 0x8000 + i);
        ipc_bench_recv(pa, 0x9000 + i);
    }
    // 2) 计时主体。
    let t0 = crate::arch::get_time();
    for i in 0..IPC_BENCH_N {
        ipc_bench_send(pa, 0xA000 + i);
        ipc_bench_recv(pa, 0xB000 + i);
    }
    let t1 = crate::arch::get_time();
    BENCH_IPC_TOTAL.store(t1.wrapping_sub(t0), Ordering::Relaxed);
    BENCH_IPC_DONE.store(true, Ordering::Relaxed);
}

/// IPC 延迟基准:接收方 —— 服务 `WARMUP+N` 轮(先于发送方完成,不参与计时)。
fn ipc_bench_recver() {
    let pb = BENCH_IPC_PB.load(Ordering::Relaxed);
    let total = IPC_BENCH_WARMUP + IPC_BENCH_N;
    for _ in 0..total {
        // 收到请求后回响应(消息 0 号 + 0x1000 供发送方校验配对完整性)。
        let req = ipc_bench_recv_any(pb);
        ipc_bench_send(pb, req + 0x1000);
    }
}

/// recv,不校验消息号(接收方不知道具体请求号,完整性由发送方校验)。
fn ipc_bench_recv_any(pid: usize) -> usize {
    match crate::ipc::recv(pid, 0) {
        Ok(crate::ipc::RecvBlock::Done(m)) => m[0],
        Ok(crate::ipc::RecvBlock::NoPeer) => {
            crate::sched::block_current();
            crate::sched::take_ipc_msg().expect("ipc bench: recv msg")[0]
        }
        Err(e) => panic!("ipc bench: recv err {e:#x}"),
    }
}

/// send + 处理 NoPeer 阻塞(与 stress_sender 同一阻塞协议)。
fn ipc_bench_send(pid: usize, word0: usize) {
    match crate::ipc::send(pid, 0, [word0, 0, 0, 0, 0]) {
        Ok(crate::ipc::SendBlock::Done) => {}
        Ok(crate::ipc::SendBlock::NoPeer) => crate::sched::block_current(),
        Err(e) => panic!("ipc bench: send err {e:#x}"),
    }
}

/// recv + 处理 NoPeer 阻塞(与 stress_recver 同一消息取得协议)。
fn ipc_bench_recv(pid: usize, expect: usize) -> usize {
    let m = match crate::ipc::recv(pid, 0) {
        Ok(crate::ipc::RecvBlock::Done(m)) => m,
        Ok(crate::ipc::RecvBlock::NoPeer) => {
            crate::sched::block_current();
            crate::sched::take_ipc_msg().expect("ipc bench: recv msg")
        }
        Err(e) => panic!("ipc bench: recv err {e:#x}"),
    };
    assert!(
        m[0] == expect,
        "ipc bench: msg mismatch got {:#x} want {expect:#x}",
        m[0]
    );
    m[0]
}

/// 计时主体轮数。
const IPC_BENCH_N: usize = 1000;
/// 预热轮数(不计入统计)。
const IPC_BENCH_WARMUP: usize = 10;
static BENCH_IPC_PA: AtomicUsize = AtomicUsize::new(0);
static BENCH_IPC_PB: AtomicUsize = AtomicUsize::new(0);
static BENCH_IPC_TOTAL: AtomicUsize = AtomicUsize::new(0);
static BENCH_IPC_DONE: AtomicBool = AtomicBool::new(false);

// ===== M2 T3b:per-CPU 调度(D19) =====

/// 测试线程槽位上限(与 sched 的 MAX_THREADS=64 对齐;T3b 测试最多用 4)。
const SMP_MAX_THREADS: usize = 64;
/// 每核结果槽:线程记录其**实际运行核**(= 亲和核则正常;错核写
/// `usize::MAX`,断言据此检出)。
static SMP_SLOT_HART: [AtomicUsize; crate::arch::MAX_HARTS] =
    [const { AtomicUsize::new(usize::MAX) }; crate::arch::MAX_HARTS];
/// 每线程的目标亲和核(tid 索引;spawn 返回后记录,供 smp_thread 读取)。
static SMP_TARGET_HART: [AtomicUsize; SMP_MAX_THREADS] =
    [const { AtomicUsize::new(usize::MAX) }; SMP_MAX_THREADS];
/// 完成计数(每个测试线程完成一次 +1)。
static SMP_DONE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// T3b 阶段 2(IPI 跨核唤醒)标志:线程已被唤醒并恢复运行。
static SMP_WAKE_RAN: AtomicBool = AtomicBool::new(false);

/// T3b 测试线程:在亲和核上记录运行核并计数,随后返回退出
/// (thread_entry → exit;副核上 exit 的 pick 回退选回 idle)。
fn smp_thread() {
    let tid = crate::sched::current_id();
    let target = SMP_TARGET_HART[tid].load(Ordering::Relaxed);
    let ran_on = crate::arch::hartid();
    SMP_SLOT_HART[target].store(
        if ran_on == target { ran_on } else { usize::MAX },
        Ordering::Relaxed,
    );
    SMP_DONE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// T3b 阶段 2:跨核唤醒线程 —— `block_current` 阻塞,由 boot hart 跨核
/// `wake`(发 SBI IPI)唤醒,恢复后置 RAN 并退出。
fn smp_wake_thread() {
    crate::sched::block_current();
    SMP_WAKE_RAN.store(true, Ordering::Relaxed);
}

/// M2 T3b:per-CPU 调度冒烟(boot hart、`irq_enable` 后调用)。
///
/// 对每个**在线**核 spawn 一个内核线程并 `set_affinity(t, h)`;线程在
/// 运行核上记录 hartid 并计数。boot hart 协作 `yield_` 轮询直至全部完成。
/// 断言:每槽 hart == 其亲和核(证明线程确实被分配到各核运行,而非全在
/// boot hart;错核运行 → `usize::MAX`)、完成计数 == 在线核数。单核
/// (make test / boot hart 无副核)下 N=1 同样通过。随后**阶段 2** 验证跨核
/// block/wake:SBI IPI + SSIP 中断唤醒副核 wfi idle 的整条链路(线程阻塞于
/// 核 1,由 boot hart 跨核 `wake` 发 IPI 唤醒)。banner 供 Makefile/CI grep。
pub fn smp_sched_test() {
    let n = crate::board::cpu_count().min(crate::arch::MAX_HARTS);
    SMP_DONE_COUNT.store(0, Ordering::Relaxed);
    for (h, slot) in SMP_SLOT_HART.iter().enumerate().take(n) {
        slot.store(usize::MAX, Ordering::Relaxed);
        let tid = crate::sched::spawn(smp_thread, crate::sched::PRIO_HIGH);
        crate::sched::set_affinity(tid, h);
        SMP_TARGET_HART[tid].store(h, Ordering::Relaxed);
    }
    // 协作轮询直至完成(boot hart 的 yield 同时给本核线程调度机会;
    // 副核线程由其 idle 循环直接 pick 运行)。
    let mut guard = 0u32;
    while SMP_DONE_COUNT.load(Ordering::Relaxed) < n {
        guard += 1;
        assert!(
            guard < 500_000,
            "T3b smp: timeout ({}/{} done)",
            SMP_DONE_COUNT.load(Ordering::Relaxed),
            n
        );
        crate::sched::yield_();
    }
    for (h, slot) in SMP_SLOT_HART.iter().enumerate().take(n) {
        let v = slot.load(Ordering::Relaxed);
        assert!(v == h, "T3b smp: thread for hart {h} ran on hart {v}");
    }
    // 阶段 2:跨核 block/wake → 验证 SBI IPI + SSIP 中断唤醒 wfi idle 的
    // 整条链路(sbi.rs/T3b 要求"重新验证 IPI 在新机制的可用性")。线程
    // 置亲和核 = 1(单核退化回 0)。**先等线程真正 Blocked**(is_blocked
    // 在 SCHED 锁内读 state)再 wake —— 保证 wake 必走 Blocked→enqueue
    // +IPI 路径,确定性覆盖 IPI 链路;否则 wake 先于 block_current 到达
    // 时走 woken 标志路径绕过 IPI(测试对 IPI 无覆盖)。IPI 失败仅降级
    // 为 ≤1 tick 的定时器唤醒(仍通过),不引入时序失败。
    let tgt = 1usize.min(n - 1);
    SMP_WAKE_RAN.store(false, Ordering::Relaxed);
    let wtid = crate::sched::spawn(smp_wake_thread, crate::sched::PRIO_HIGH);
    crate::sched::set_affinity(wtid, tgt);
    let mut guard = 0;
    let mut blocked = false;
    while guard < 500_000 {
        if crate::sched::is_blocked(wtid) {
            blocked = true;
            break;
        }
        guard += 1;
        crate::sched::yield_();
    }
    assert!(blocked, "T3b smp: wake thread never blocked");
    crate::sched::wake(wtid);
    guard = 0;
    while !SMP_WAKE_RAN.load(Ordering::Relaxed) && guard < 500_000 {
        guard += 1;
        crate::sched::yield_();
    }
    assert!(
        SMP_WAKE_RAN.load(Ordering::Relaxed),
        "T3b smp: cross-hart wake timeout"
    );
    info!("M2 T3b: per-CPU sched ok ({n} harts)");
}

// ===== M3 T2:跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown =====

/// 核 1 busy-loop 用户程序:写 marker 0x5A 到 shared(0x4000_1000 物理页)
/// 后自旋(核 1 独享、不退出 —— 跨核 kill 的目标;若被杀,其下一指令
/// 永不执行,故自旋体内无副作用)。编码经 rustc 汇编器逐条核对。
const CC_KILL_PROG: [u32; 4] = [
    0x4000_12b7, // lui  t0, 0x40001  (t0 = 0x4000_1000 = shared)
    0x05a0_0313, // addi t1, x0, 0x5a (marker)
    0x0062_a023, // sw   t1, 0(t0)    (shared[0] = 0x5a)
    0x0000_006f, // jal  x0, 0        (自旋)
];
/// 核 1 SHM 读程序:写 marker 0x5B 到 SHM_VA 后自旋读 SHM_VA。revoke 后
/// 下一次 `lw` 必须页故障 —— 若核 1 TLB 未被 shootdown 冲刷,线程会从
/// 陈旧 TLB 读已释放物理页(陈旧数据),永不故障 → 断言超时暴露回归。
const CC_SHOOTDOWN_PROG: [u32; 5] = [
    0x5000_02b7, // lui  t0, 0x50000  (t0 = SHM_VA = 0x5000_0000)
    0x05b0_0313, // addi t1, x0, 0x5b (marker)
    0x0062_a023, // sw   t1, 0(t0)    (SHM_VA[0] = 0x5b)
    0x0002_a383, // lw   t2, 0(t0)    (读 SHM_VA;revoke 后此处页故障)
    0xfe00_0ee3, // beq  x0, x0, -4   (回到 lw,自旋)
];
/// 核 1 重新 spawn 的运行标志(停杀后调度器可再分配)。
static CC_RESPAWN_RAN: AtomicBool = AtomicBool::new(false);

fn cc_respawn_thread() {
    CC_RESPAWN_RAN.store(true, Ordering::Relaxed);
}

/// M3 T2 阶段 1:跨核 IPI 停杀 —— 进程 A 的用户线程 Running 于**非 boot
/// hart**(busy-loop 自旋不退出),boot hart `kill_process(pid)` → 置
/// `THREAD_KILL_REQUEST` + 发 FORCE_KILL IPI → 目标核 SSIP handler
/// `force_kill_current` 立即停杀回收(栈 → reaper、槽 → free_slots、状态 →
/// Exited)。断言:请求全清、槽回收、`pid_root` 失效、目标核可再 spawn。
fn smp_kill_phase(n: usize) {
    if n < 2 {
        return; // 单核退化:无"其它核 Running 线程"场景,由 banner 覆盖。
    }
    // 目标核须**非 boot hart**:busy 线程自旋不退出,若亲和 boot hart,测试
    // 上下文一次 yield 后永不恢复。boot hart 不一定是 hart 0(实测 -smp 4
    // 为 hart 1),取首个非 boot 核;n≥2 时该核恒存在。
    let boot = crate::arch::hartid();
    let tgt = if boot != 0 { 0 } else { 1 };
    // 1) 建进程 + 用户 busy-loop 线程(亲和核 1)。
    let pid = crate::process::create().expect("cckill: create process");
    let root = crate::process::root(pid);
    let (_, shared_pa) = map_iso_proc(root, &CC_KILL_PROG);
    let shared = shared_pa as *const u32;
    let tid = crate::sched::spawn_user(pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    crate::sched::set_affinity(tid, tgt);
    // 停杀回收基线 = spawn **之后**的空闲槽数:busy 线程可能复用已退出
    // 线程的槽位(spawn 前 count 已含该槽,回收后回到 spawn 前值,不满足
    // "> free_before";复用/新槽两种情况在 "> free_after_spawn" 下都能
    // 正确判定泄漏)。
    // 2) 等线程在核 1 运行并写 marker(物理页直读;写后自旋,不退出)。
    let mut guard = 0u32;
    while unsafe { core::ptr::read_volatile(shared) } != 0x5a {
        assert!(
            guard < 200_000,
            "cckill: busy thread never ran on hart {tgt}"
        );
        crate::sched::yield_();
        guard += 1;
    }
    // 3) 跨核杀进程:线程 Running 于核 1,应被 IPI 停杀并回收。
    let free_after_spawn = crate::sched::free_slot_count();
    let exited = crate::sched::kill_process(pid);
    assert!(!exited, "cckill: idle caller must not exit");
    assert!(
        crate::process::pid_root(pid).is_none(),
        "cckill: process must be destroyed"
    );
    let free_after_kill = crate::sched::free_slot_count();
    assert!(
        free_after_kill > free_after_spawn,
        "cckill: running thread slot not reclaimed (free {free_after_kill} <= {free_after_spawn} after spawn)"
    );
    assert!(
        crate::sched::pending_kill_request_count() == 0,
        "cckill: stale kill request remains"
    );
    // 4) 核 1 可再 spawn 并运行(停杀后调度器正常)。
    CC_RESPAWN_RAN.store(false, Ordering::Relaxed);
    let tid2 = crate::sched::spawn(cc_respawn_thread, crate::sched::PRIO_HIGH);
    crate::sched::set_affinity(tid2, tgt);
    guard = 0;
    while !CC_RESPAWN_RAN.load(Ordering::Relaxed) && guard < 500_000 {
        guard += 1;
        crate::sched::yield_();
    }
    assert!(
        CC_RESPAWN_RAN.load(Ordering::Relaxed),
        "cckill: hart {tgt} cannot respawn after kill"
    );
    crate::sched::drain_reaper();
}

/// M3 T2 阶段 2:跨核 TLB shootdown —— 进程 B 的用户线程 Running 于**非
/// boot hart** 反复读 SHM_VA(其 TLB 已缓存该页)。boot hart `shm_revoke`
/// 撤双方映射后对目标核发 TLB_FLUSH IPI(远端 sfence)。断言:B 线程**下一
/// 次**访存页故障并被 D12 自愈杀死 —— 而非从陈旧 TLB 读到已释放物理页
/// 的陈旧数据。
fn smp_shootdown_phase(n: usize) {
    if n < 2 {
        return;
    }
    // 同 smp_kill_phase:目标核须非 boot hart(busy 读循环自旋不退出)。
    let boot = crate::arch::hartid();
    let tgt = if boot != 0 { 0 } else { 1 };
    // 1) 建 A/B,mmap_share(A,0,B,1) → SHM_VA 双根表映射 + 双槽改授。
    let a_pid = crate::process::create().expect("ccsd: create A");
    let b_pid = crate::process::create().expect("ccsd: create B");
    let root_b = crate::process::root(b_pid);
    assert!(
        crate::process::grant_cap(a_pid, 0, b_pid).is_ok(),
        "ccsd: grant cap(A,0,B)"
    );
    let shm_id = crate::shm::mmap_share(a_pid, 0, 1, crate::shm::SHM_LEN).expect("ccsd: mmap");
    // B 的用户程序写 SHM marker 后自旋读 SHM_VA(map_iso_proc 映射代码页)。
    let (_, _b_shared_pa) = map_iso_proc(root_b, &CC_SHOOTDOWN_PROG);
    let tid = crate::sched::spawn_user(b_pid, 0x4000_0000, 0x4000_4000, crate::sched::PRIO_HIGH);
    crate::sched::set_affinity(tid, tgt);
    // 2) 等 B 线程在核 1 运行并写 SHM marker(经 shm_paddr 直读物理页)。
    let paddr = crate::shm::shm_paddr(shm_id).expect("ccsd: shm must be registered");
    let shm = paddr as *const u32;
    let mut guard = 0u32;
    while unsafe { core::ptr::read_volatile(shm) } != 0x5b {
        assert!(
            guard < 200_000,
            "ccsd: B thread never touched SHM on hart {tgt}"
        );
        crate::sched::yield_();
        guard += 1;
    }
    // 3) 跨核 revoke:撤双映射 + 释放页 + 对核 1 发 TLB_FLUSH IPI。
    let kill_before = crate::sched::fault_kill_count();
    assert!(crate::shm::shm_revoke(shm_id).is_ok(), "ccsd: revoke");
    assert!(
        crate::shm::shm_paddr(shm_id).is_none(),
        "ccsd: shm must be deregistered"
    );
    // 4) B 线程下一次访存 SHM_VA 须页故障 → D12 杀 B(证明核 1 TLB 已刷;
    //    未刷则读陈旧数据、永不故障 → 超时断言失败)。
    guard = 0;
    while crate::sched::fault_kill_count() == kill_before {
        assert!(
            guard < 500_000,
            "ccsd: hart {tgt} read stale SHM (TLB not flushed)"
        );
        guard += 1;
        crate::sched::yield_();
    }
    assert!(
        crate::process::pid_root(b_pid).is_none(),
        "ccsd: B must be killed by D12 after revoke"
    );
    // B 已由 D12 销毁;A 仅持已被 revoke 的 Shm cap,destroy 幂等清理。
    crate::process::destroy(a_pid);
    crate::sched::drain_reaper();
}

/// M3 T2:跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown 冒烟
/// (boot hart、irq_enable 后、smp_sched_test 之后调用)。两阶段均只对
/// 多核有意义;单核(n=1)退化仍打印 banner。banner:
/// `M3 T2: cross-core kill/shootdown ok (N harts)`(6 处 grep 同步)。
pub fn smp_crosscore_test() {
    let n = crate::board::cpu_count().min(crate::arch::MAX_HARTS);
    smp_kill_phase(n);
    smp_shootdown_phase(n);
    info!("M3 T2: cross-core kill/shootdown ok ({n} harts)");
}
