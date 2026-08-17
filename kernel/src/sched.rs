//! 内核线程与调度器(M1)。
//!
//! # 设计
//! - **线程**:内核线程,每线程独立内核栈 + 全量陷阱帧(抢占恢复用)
//!   + 协作上下文(Context)。
//! - **协作切换**(`yield_`/阻塞):调用边界保存调用者保存寄存器,
//!   经 `context_switch` 切换(见 riscv64.S)。
//! - **抢占切换**:定时器 ISR 中,若当前线程时间片到期且存在就绪
//!   线程,把被中断线程的**全量陷阱帧**复制进其 TCB,返回下一线程
//!   的帧指针 —— 汇编恢复路径据此 sret 进入下一线程(全寄存器恢复,
//!   t/a 也不丢)。
//! - **优先级**:2 级(HIGH/LOW),级内轮转;idle 为最低。
//! - **时间片**:每线程 `SLICE_TICKS`(10 tick = 100ms)内不主动
//!   yield 则被抢占。
//! - **同步**:调度器自身在 SpinLock 保护下运行;ISR 路径(on_tick)
//!   只在 tick 与帧复制上操作,不做复杂队列修改(抢占决定在
//!   `on_tick` 中提前完成)。

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::arch::{self, Context};
use crate::sync::SpinLock;

/// 线程栈大小(16KB)。
const THREAD_STACK_SIZE: usize = 16 * 1024;
/// 时间片(tick 数,10 tick = 100ms)。
const SLICE_TICKS: u64 = 10;
/// 优先级级数。
const PRIO_LEVELS: usize = 2;
/// 高优先级。
pub const PRIO_HIGH: u8 = 0;
/// 低优先级。
pub const PRIO_LOW: u8 = 1;

/// 陷阱帧槽位(与 riscv64.S/riscv64.rs 一致)。
const FRAME_WORDS: usize = 40;
/// 帧内 sstatus 槽。
const FRAME_SSTATUS: usize = 32;
/// 帧内 sepc 槽。
const FRAME_SEPC: usize = 33;

/// 线程状态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadState {
    Ready,
    Running,
    Blocked,
    Exited,
}

/// 内核线程控制块。
struct Thread {
    prio: u8,
    state: ThreadState,
    /// 协作切换上下文。
    ctx: Context,
    /// 抢占切换用全量陷阱帧(被抢占时复制至此)。
    frame: [usize; FRAME_WORDS],
    /// 线程入口(thread_entry 经 id 查表调用)。
    entry: fn(),
    /// 线程栈(Box:栈内存来自内核堆)。
    #[allow(dead_code)]
    stack: Box<[u8]>,
}

/// 调度器。
struct Scheduler {
    threads: Vec<Thread>,
    /// 就绪队列(按优先级)。
    ready: [VecDeque<usize>; PRIO_LEVELS],
    /// 当前线程 id。
    current: usize,
    /// idle 线程 id(最低优先级,永不阻塞)。
    idle: usize,
    /// 当前线程已运行 tick 数。
    ticks_run: u64,
    /// 下一个线程 id。
    next_id: usize,
}

/// 空闲线程入口:永远让出。
fn idle_entry() {
    loop {
        // 空闲线程不主动跑业务;yield 把 CPU 让给就绪线程。
        yield_();
    }
}

/// 线程包装:运行入口函数,返回后退出。
fn thread_entry() {
    // 首个上下文从 SIE=0 切入,显式开启中断(线程常态运行)。
    arch::irq_enable();
    let irq = arch::irq_save();
    let entry = {
        let s = SCHED.lock();
        let id = s.current;
        s.threads[id].entry
    };
    arch::irq_restore(irq);
    entry();
    exit();
}

impl Scheduler {
    /// 查找下一个可运行线程(轮转),返回 id。
    /// 调用方须持锁。
    fn pick_next(&mut self) -> usize {
        for level in 0..PRIO_LEVELS {
            while let Some(id) = self.ready[level].pop_front() {
                if self.threads[id].state == ThreadState::Ready {
                    return id;
                }
                // 状态非 Ready(如已 Exit):丢弃。
            }
        }
        self.idle
    }

    /// 把线程加入就绪队列。
    fn enqueue(&mut self, id: usize) {
        let prio = self.threads[id].prio as usize;
        self.ready[prio].push_back(id);
        self.threads[id].state = ThreadState::Ready;
    }

    /// 协作让出:锁内更新状态与 pick,锁外切换(中断仍关闭,
    /// 单核无抢占介入,切换原子性由 irq_save 保证)。
    fn yield_self(&mut self) -> (usize, usize) {
        let cur = self.current;
        self.enqueue(cur);
        self.ticks_run = 0;
        let next = self.pick_next();
        self.current = next;
        (cur, next)
    }

    /// 阻塞当前线程(从就绪中移除,不加入)。
    fn block_self(&mut self) -> (usize, usize) {
        let cur = self.current;
        self.threads[cur].state = ThreadState::Blocked;
        self.ticks_run = 0;
        let next = self.pick_next();
        self.current = next;
        (cur, next)
    }

    /// 抢占决策(定时器 ISR 内,中断关闭,不可阻塞):
    /// 时间片到期且存在其他就绪线程 → 复制当前帧、返回下一线程帧。
    /// 否则返回原帧(继续当前线程)。
    fn on_tick(&mut self, frame: *mut usize) -> *mut usize {
        self.ticks_run += 1;
        if self.ticks_run < SLICE_TICKS {
            return frame;
        }
        self.ticks_run = 0;
        // 是否存在可抢占的就绪线程(非自身)。
        let has_other = (0..PRIO_LEVELS).any(|l| {
            self.ready[l]
                .iter()
                .any(|&id| id != self.current && self.threads[id].state == ThreadState::Ready)
        });
        if !has_other {
            return frame;
        }
        let cur = self.current;
        // 把被中断线程的全量帧复制进其 TCB。
        self.threads[cur]
            .frame
            .copy_from_slice(unsafe { core::slice::from_raw_parts(frame, FRAME_WORDS) });
        self.threads[cur].state = ThreadState::Ready;
        // 加入就绪(排到队尾,轮转)。
        self.enqueue(cur);
        let next = self.pick_next();
        self.current = next;
        // 返回下一线程的帧指针,汇编据此 sret 全量恢复。
        self.threads[next].frame.as_mut_ptr()
    }

    /// 线程退出:标记 Exited。
    fn exit_self(&mut self) -> (usize, usize) {
        let cur = self.current;
        self.threads[cur].state = ThreadState::Exited;
        self.ticks_run = 0;
        let next = self.pick_next();
        self.current = next;
        (cur, next)
    }

    /// 新建线程:分配栈 + 构造初始帧(sepc=thread_entry,sp=栈顶)。
    fn spawn(&mut self, entry: fn(), prio: u8) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let stack = alloc::vec![0u8; THREAD_STACK_SIZE].into_boxed_slice();
        let sp = stack.as_ptr() as usize + THREAD_STACK_SIZE;
        // 初始帧:sepc = 线程包装器,sstatus:SPIE=1(进线程后开中断)。
        let mut frame = [0usize; FRAME_WORDS];
        frame[FRAME_SEPC] = thread_entry as *const () as usize;
        frame[FRAME_SSTATUS] = 1 << 5; // SPIE
        let t = Thread {
            prio,
            state: ThreadState::Ready,
            ctx: Context {
                // 协作路径首启:ra = 线程包装器(ret 即进入)。
                ra: thread_entry as *const () as usize,
                sp,
                s: [0; 12],
            },
            frame,
            entry,
            stack,
        };
        self.threads.push(t);
        self.enqueue(id);
        id
    }
}

/// 调度器单例(SpinLock:主上下文/ISR 均不可重入)。
static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    threads: Vec::new(),
    ready: [VecDeque::new(), VecDeque::new()],
    current: 0,
    idle: 0,
    ticks_run: 0,
    next_id: 1,
});

/// 线程 id 分配(供外部日志/调试)。
static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(1);

/// 初始化调度器(创建 idle 线程;须在堆就绪后、irq_enable 前调用)。
pub fn init() {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    // idle 线程:id=0,最低优先级,永不阻塞。
    let idle_id = 0;
    let stack = alloc::vec![0u8; THREAD_STACK_SIZE].into_boxed_slice();
    let sp = stack.as_ptr() as usize + THREAD_STACK_SIZE;
    let mut frame = [0usize; FRAME_WORDS];
    frame[FRAME_SEPC] = idle_entry as *const () as usize;
    frame[FRAME_SSTATUS] = 1 << 5;
    s.threads.push(Thread {
        prio: PRIO_LOW,
        state: ThreadState::Running,
        ctx: Context {
            ra: idle_entry as *const () as usize,
            sp,
            s: [0; 12],
        },
        frame,
        entry: idle_entry,
        stack,
    });
    s.current = idle_id;
    s.idle = idle_id;
    drop(s);
    arch::irq_restore(irq);
}

/// 新建内核线程,返回 id。
pub fn spawn(entry: fn(), prio: u8) -> usize {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    let id = s.spawn(entry, prio);
    let _ = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
    drop(s);
    arch::irq_restore(irq);
    id
}

/// 协作让出 CPU。
pub fn yield_() {
    let irq = arch::irq_save();
    let (cur, next) = {
        let mut s = SCHED.lock();
        s.yield_self()
    };
    // 锁外切换(中断关闭保证原子性;新线程首启经 thread_entry,
    // 复切经其自身 yield 的恢复点 —— 都不会再抢本锁)。
    let (old, new) = {
        let mut s = SCHED.lock();
        let old = (&mut s.threads[cur].ctx) as *mut Context;
        let new = (&s.threads[next].ctx) as *const Context;
        (old, new)
    };
    unsafe { arch::context_switch(old, new) };
    arch::irq_restore(irq);
}

/// 阻塞当前线程(须由调度器外的同步原语唤醒)。
pub fn block_current() {
    let irq = arch::irq_save();
    let (cur, next) = {
        let mut s = SCHED.lock();
        s.block_self()
    };
    let (old, new) = {
        let mut s = SCHED.lock();
        let old = (&mut s.threads[cur].ctx) as *mut Context;
        let new = (&s.threads[next].ctx) as *const Context;
        (old, new)
    };
    unsafe { arch::context_switch(old, new) };
    arch::irq_restore(irq);
}

/// 唤醒指定线程(置 Ready 并入队)。
pub fn wake(id: usize) {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    if s.threads[id].state == ThreadState::Blocked {
        s.enqueue(id);
    }
    drop(s);
    arch::irq_restore(irq);
}

/// 当前线程 id。
pub fn current_id() -> usize {
    let irq = arch::irq_save();
    let s = SCHED.lock();
    let id = s.current;
    drop(s);
    arch::irq_restore(irq);
    id
}

/// 线程退出(入口函数返回后调用,不返回)。
pub fn exit() -> ! {
    let irq = arch::irq_save();
    let (cur, next) = {
        let mut s = SCHED.lock();
        s.exit_self()
    };
    let (old, new) = {
        let mut s = SCHED.lock();
        let old = (&mut s.threads[cur].ctx) as *mut Context;
        let new = (&s.threads[next].ctx) as *const Context;
        (old, new)
    };
    unsafe { arch::context_switch(old, new) };
    // exit_self 切走,不会到达这里;防御性停机。
    arch::irq_restore(irq);
    arch::halt()
}

/// 定时器 ISR 回调(中断关闭上下文):抢占决策,返回恢复帧。
///
/// # Safety
/// `frame` 必须是当前陷阱帧(见 trap_handler)。
pub unsafe fn on_tick(frame: *mut usize) -> *mut usize {
    let mut s = SCHED.lock();
    s.on_tick(frame)
}

/// 调度器自检(须在 irq_enable 前或测试专用线程中运行)。
pub fn self_test() -> Result<(), &'static str> {
    // 1) 协作切换:两个线程交替 yield 计数,总和正确。
    spawn(test_inc, PRIO_HIGH);
    spawn(test_inc, PRIO_HIGH);
    let mut guard = 0;
    while TEST_CTR.load(Ordering::Relaxed) < 100 && guard < 10_000 {
        yield_();
        guard += 1;
    }
    if TEST_CTR.load(Ordering::Relaxed) != 100 {
        return Err("cooperative counter mismatch");
    }
    // 2) 线程退出:入口返回后应自动 exit 并让出。
    spawn(test_done, PRIO_HIGH);
    guard = 0;
    while !TEST_DONE.load(Ordering::Relaxed) && guard < 10_000 {
        yield_();
        guard += 1;
    }
    if !TEST_DONE.load(Ordering::Relaxed) {
        return Err("thread exit not observed");
    }
    // 3) 抢占:忙循环线程不 yield,若定时器抢占失效则主线程永远
    //    得不到 CPU(超时即证明失效)。
    TEST_BUSY.store(0, Ordering::Relaxed);
    spawn(test_busy, PRIO_LOW);
    guard = 0;
    while TEST_BUSY.load(Ordering::Relaxed) < BUSY_TARGET && guard < 200_000 {
        yield_();
        guard += 1;
    }
    if TEST_BUSY.load(Ordering::Relaxed) < BUSY_TARGET {
        return Err("preemption failed: busy thread starved main");
    }
    Ok(())
}

// ===== 自检线程(自由函数 + 静态原子,无捕获) =====

/// 协作计数线程:50 次递增 + yield。
fn test_inc() {
    for _ in 0..50 {
        TEST_CTR.fetch_add(1, Ordering::Relaxed);
        yield_();
    }
}

/// 退出标记线程。
fn test_done() {
    TEST_DONE.store(true, Ordering::Relaxed);
}

/// 忙循环线程:不 yield,靠定时器抢占让出 CPU。
/// 目标值跨多个时间片(10 tick/片),保证主线程至少被抢占回一次。
const BUSY_TARGET: usize = 1_000_000;
fn test_busy() {
    while TEST_BUSY.load(Ordering::Relaxed) < BUSY_TARGET {
        TEST_BUSY.fetch_add(1, Ordering::Relaxed);
    }
}

static TEST_CTR: AtomicUsize = AtomicUsize::new(0);
static TEST_DONE: AtomicBool = AtomicBool::new(false);
static TEST_BUSY: AtomicUsize = AtomicUsize::new(0);
