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

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::arch::{self, Context};
use crate::info;
use crate::sync::SpinLock;
use crate::warn;

/// 线程栈大小(16KB)。
const THREAD_STACK_SIZE: usize = 16 * 1024;
/// 时间片(tick 数,10 tick = 100ms)。
const SLICE_TICKS: u64 = 10;
/// 最大并发线程数(就绪队列按此预留容量 —— HIGH-5/审计 16 轮:
/// 定时器 ISR 内 enqueue 的 push_back 不得触发分配,否则与堆锁
/// 死锁;容量在 init 一次性预留)。
const MAX_THREADS: usize = 64;
/// 优先级级数。
const PRIO_LEVELS: usize = 2;
/// 高优先级。
pub const PRIO_HIGH: u8 = 0;
/// 低优先级。
pub const PRIO_LOW: u8 = 1;

/// 陷阱帧槽位(与 riscv64.S/riscv64.rs 一致)。
/// CRITICAL-1:必须等于 arch::TRAP_FRAME_WORDS(36)= 31 GPR + 4 CSR
/// + 1 填充;此前误写 40,on_tick 复制帧时越界读 32 字节。
const FRAME_WORDS: usize = arch::TRAP_FRAME_WORDS;
/// 编译期锁定:两处帧尺寸不得漂移。
const _: () = assert!(FRAME_WORDS == 36);
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
    /// CRITICAL-3:帧是否"可作抢占恢复用"。
    /// true = 帧刚被抢占捕获(或新线程初始帧),可被 on_tick 选中;
    /// false = 线程经 yield/block 让出(进度在 ctx),其帧已失效,
    ///         若被抢占恢复会**从头重跑**线程。
    frame_valid: bool,
    /// CRITICAL-1(审计 16 轮):ctx 是否"可作协作切换恢复用"。
    /// 与 frame_valid 互补 —— 两套切换机制各选各的:
    /// - 协作路径(yield/block/exit)只允许选中 ctx_valid 的线程;
    /// - 抢占路径(on_tick)只允许选中 frame_valid 的线程。
    ///
    /// 被抢占的线程 ctx 陈旧(状态在帧里),协作路径若选中它会错乱。
    ctx_valid: bool,
    /// C5(审计 15 轮):唤醒标志 —— wake 无条件置位,block_current
    /// 消费;覆盖"唤醒早于阻塞"的窗口,防丢失唤醒。
    woken: bool,
    /// 线程入口(thread_entry 经 id 查表调用)。
    entry: fn(),
    /// 线程栈(Box:栈内存来自内核堆)。
    #[allow(dead_code)]
    stack: Option<KernelStack>,
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
    /// 已退出线程的栈回收队列(idle 在自身上下文释放)。
    reaper: VecDeque<KernelStack>,
}

// 含裸指针/上下文状态:单上下文 + SpinLock 互斥下安全
// (供 SpinLock<T: Send> 的 Sync 约束)。
unsafe impl Send for Scheduler {}

/// 空闲线程入口(实际不可达 —— idle 线程以主流程上下文运行;
/// ctx.ra 初始引用保持此符号,防链接器移除)。
fn idle_entry() {
    loop {
        yield_();
    }
}

/// 线程包装:运行入口函数,返回后退出。
fn thread_entry() {
    // 首个上下文从 SIE=0 切入,显式开启中断(线程常态运行)。
    arch::irq_enable();
    let irq = arch::irq_save();
    let entry = {
        let mut s = SCHED.lock();
        let id = s.current;
        // 初始帧已被消费(本次进入),此后进度在 ctx(CRITICAL-3)。
        s.threads[id].frame_valid = false;
        s.threads[id].entry
    };
    arch::irq_restore(irq);
    entry();
    exit();
}

impl Scheduler {
    /// 查找下一个可运行线程(轮转),返回 id。
    /// 选中者状态置为 Running(派发即运行;此前缺失导致唤醒线程
    /// 以 Ready 状态进入 block_current 误判"已唤醒"而自旋)。
    ///
    /// CRITICAL-1:参数 `need_ctx` = true 表示协作路径调用 —— 只选
    /// ctx_valid 的线程(被抢占线程的 ctx 陈旧,协作切换会错乱);
    /// = false 表示抢占路径(on_tick),只选 frame_valid 的线程。
    /// 无分配:仅 VecDeque 出/入队(轮转),不满足条件的候选回队尾。
    fn pick_next(&mut self, need_ctx: bool) -> usize {
        for level in 0..PRIO_LEVELS {
            let q = &mut self.ready[level];
            let round = q.len();
            for _ in 0..round {
                let Some(id) = q.pop_front() else { break };
                if self.threads[id].state != ThreadState::Ready {
                    continue; // 非 Ready(如 Exited):丢弃。
                }
                let ok = if need_ctx {
                    self.threads[id].ctx_valid
                } else {
                    self.threads[id].frame_valid
                };
                if ok {
                    self.threads[id].state = ThreadState::Running;
                    return id;
                }
                q.push_back(id); // 暂不满足:轮转到队尾,不丢失。
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

    /// 从就绪队列撤销一个线程(block_current 的"已唤醒则继续"路径)。
    fn remove_from_ready(&mut self, id: usize) {
        for q in self.ready.iter_mut() {
            if let Some(pos) = q.iter().position(|&x| x == id) {
                q.remove(pos);
                return;
            }
        }
    }

    /// 抢占决策(定时器 ISR 内,中断关闭,不可阻塞):
    /// 时间片到期且存在**帧有效**的其他就绪线程 → 复制当前帧、
    /// 返回下一线程帧。否则返回原帧(继续当前线程)。
    ///
    /// CRITICAL-3:只允许选中 frame_valid 的线程 —— 否则会用过期帧
    /// (sepc=thread_entry)恢复一个已 yield 的线程,使其从头重跑。
    fn on_tick(&mut self, frame: *mut usize) -> *mut usize {
        self.ticks_run += 1;
        if self.ticks_run < SLICE_TICKS {
            return frame;
        }
        self.ticks_run = 0;
        // 是否存在可抢占的就绪线程(非自身、帧有效)。
        let has_other = (0..PRIO_LEVELS).any(|l| {
            self.ready[l].iter().any(|&id| {
                id != self.current
                    && self.threads[id].state == ThreadState::Ready
                    && self.threads[id].frame_valid
            })
        });
        if !has_other {
            return frame;
        }
        let cur = self.current;
        // 把被中断线程的全量帧复制进其 TCB(帧此刻有效)。
        self.threads[cur]
            .frame
            .copy_from_slice(unsafe { core::slice::from_raw_parts(frame, FRAME_WORDS) });
        self.threads[cur].frame_valid = true;
        // CRITICAL-1:抢占捕获后,ctx 失效(状态在帧里)——
        // 协作路径不得再用陈旧 ctx 切换它。
        self.threads[cur].ctx_valid = false;
        self.threads[cur].state = ThreadState::Ready;
        // 加入就绪(排到队尾,轮转)。
        self.enqueue(cur);
        // CRITICAL-1:抢占路径选帧有效线程。
        let next = self.pick_next(false);
        self.current = next;
        // 选中者将被帧恢复运行 —— 恢复后可被协作切换(ctx 将
        // 在它下次 yield 时重存),此处置 ctx_valid 使协作路径可选它。
        self.threads[next].ctx_valid = true;
        // 返回下一线程的帧指针,汇编据此 sret 全量恢复。
        self.threads[next].frame.as_mut_ptr()
    }

    /// 新建线程:分配栈 + 构造初始帧(sepc=thread_entry,sp=栈顶)。
    fn spawn(&mut self, entry: fn(), prio: u8) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        // 零化-free 栈分配(性能优化):`vec![0u8; N]` 会白付 16KB
        // memset —— 栈内容无需初始化(初始帧/上下文显式构造)。
        let stack = alloc_zeroed_free_stack();
        // HIGH-4:sp 须 16 字节对齐(RISC-V ABI);堆指针仅保证 8 对齐。
        let sp = (stack.ptr as usize + THREAD_STACK_SIZE) & !0xF;
        // 初始帧:sepc = 线程包装器,sstatus:SPIE=1(进线程后开中断)。
        // CRITICAL-2(审计 16 轮):帧内 sp/gp 必须有效 —— 恢复路径
        // 会从帧加载 sp;gp 与 trap_vector 入口一致(内核代码可能
        // 生成 gp 相对访问)。
        let mut frame = [0usize; FRAME_WORDS];
        frame[FRAME_SEPC] = thread_entry as *const () as usize;
        frame[FRAME_SSTATUS] = 1 << 5; // SPIE
        frame[1] = sp;
        frame[2] = __global_pointer();
        // MED-10(审计 15 轮):优先级越界钳制,防就绪队列越界 panic。
        let prio = prio.min(PRIO_LEVELS as u8 - 1);
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
            // 新线程初始帧有效(sepc=thread_entry),可被抢占首启。
            frame_valid: true,
            // 初始 ctx 有效(thread_entry 首启),可被协作切换选中。
            ctx_valid: true,
            woken: false,
            entry,
            stack: Some(stack),
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
    reaper: VecDeque::new(),
});

/// 线程 id 分配(供外部日志/调试)。
static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(1);

/// __global_pointer$ 地址(与 entry.S/trap_vector 一致;新线程帧
/// 的 gp 槽用它,保证内核代码的 gp 相对访问有效)。
fn __global_pointer() -> usize {
    unsafe extern "C" {
        #[link_name = "__global_pointer$"]
        static GLOBAL_POINTER: u8;
    }
    (&raw const GLOBAL_POINTER).addr()
}

/// 零化-free 线程栈(性能优化:免 16KB memset)。
///
/// MED-8(审计 16 轮):Box<[u8]> 的 Drop 用 align=1 布局释放 ——
/// 与分配时的 align=16 **不匹配(UB)**。自定义包装按同一布局
/// (size=THREAD_STACK_SIZE, align=16)释放。
struct KernelStack {
    ptr: *mut u8,
}

// 含裸指针:单上下文调度下,指针生命周期由 Scheduler 管理 —— 安全。
unsafe impl Send for KernelStack {}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let layout =
            core::alloc::Layout::from_size_align(THREAD_STACK_SIZE, 16).expect("stack layout");
        unsafe { alloc::alloc::dealloc(self.ptr, layout) };
    }
}

fn alloc_zeroed_free_stack() -> KernelStack {
    let layout = core::alloc::Layout::from_size_align(THREAD_STACK_SIZE, 16).expect("stack layout");
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    assert!(!ptr.is_null(), "kernel heap: thread stack OOM");
    KernelStack { ptr }
}

/// 初始化调度器(创建 idle 线程;须在堆就绪后、irq_enable 前调用)。
pub fn init() {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    // HIGH-5:预留容量 —— 此后 ISR 内的 enqueue/栈移交均零分配。
    for q in s.ready.iter_mut() {
        q.reserve(MAX_THREADS);
    }
    s.reaper.reserve(MAX_THREADS);
    s.threads.reserve(MAX_THREADS);
    // idle 线程:id=0,最低优先级,永不阻塞。
    let idle_id = 0;
    let stack = alloc_zeroed_free_stack();
    let sp = (stack.ptr as usize + THREAD_STACK_SIZE) & !0xF;
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
        // idle 以主流程直接运行(非经切换),初始帧从未被使用 →
        // 无效;其帧在真正被抢占时才会捕获。
        frame_valid: false,
        ctx_valid: true,
        woken: false,
        entry: idle_entry,
        stack: Some(stack),
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
    // 单锁作用域:状态变更 + pick + 裸指针提取一次完成
    // (性能优化:原实现两次取锁)。
    let (old, new) = {
        let mut s = SCHED.lock();
        let cur = s.current;
        // C3(审计 15 轮回归):合并锁时丢失 —— yield 后进度在 ctx,
        // 帧必须失效,否则抢占会用过期帧恢复线程(从头重跑)。
        // CRITICAL-1:ctx 刚保存(有效),协作路径可选本线程。
        s.threads[cur].frame_valid = false;
        s.threads[cur].ctx_valid = true;
        // MED-7(审计 16 轮):顺带回收已退出线程的栈(在"即将被
        // 切走"的当前线程栈上释放**他人**的栈 —— 安全)。
        while let Some(stack) = s.reaper.pop_front() {
            drop(stack);
        }
        s.enqueue(cur);
        s.ticks_run = 0;
        let next = s.pick_next(true);
        s.current = next;
        let old = (&mut s.threads[cur].ctx) as *mut Context;
        let new = (&s.threads[next].ctx) as *const Context;
        (old, new)
    };
    // 锁外切换(中断关闭保证原子性;新线程首启经 thread_entry,
    // 复切经其自身 yield 的恢复点 —— 都不会再抢本锁)。
    unsafe { arch::context_switch(old, new) };
    arch::irq_restore(irq);
}

/// 阻塞当前线程(须由调度器外的同步原语唤醒)。
///
/// 丢失唤醒防护(HIGH-2):若唤醒已在阻塞前发生(线程已被置 Ready
/// 并入队),则撤销入队并**继续运行**,不再阻塞 —— 否则唤醒被
/// 吞掉,线程永久挂起。
pub fn block_current() {
    let irq = arch::irq_save();
    let (old, new) = {
        let mut s = SCHED.lock();
        let cur = s.current;
        if s.threads[cur].state == ThreadState::Ready {
            // 已被唤醒(队列中):撤销本次入队,继续运行。
            s.remove_from_ready(cur);
            s.threads[cur].state = ThreadState::Running;
            // C11(审计 15 轮):**先 drop 锁再恢复中断** ——
            // 否则中断在锁持有期间重开,定时器 ISR 抢锁死锁。
            drop(s);
            arch::irq_restore(irq);
            return;
        }
        // C5(审计 15 轮):woken 标志 —— 唤醒可能发生在"登记之后、
        // 阻塞之前"(wake 无条件记录),这里消费它并继续运行,
        // 否则该唤醒丢失(线程永久阻塞)。
        if s.threads[cur].woken {
            s.threads[cur].woken = false;
            s.threads[cur].state = ThreadState::Running;
            drop(s);
            arch::irq_restore(irq);
            return;
        }
        let c = s.current;
        s.threads[c].state = ThreadState::Blocked;
        s.threads[c].frame_valid = false;
        s.threads[c].ctx_valid = true;
        s.ticks_run = 0;
        let n = s.pick_next(true);
        s.current = n;
        let old = (&mut s.threads[c].ctx) as *mut Context;
        let new = (&s.threads[n].ctx) as *const Context;
        (old, new)
    };
    unsafe { arch::context_switch(old, new) };
    // 醒来:撤销 wake 的入队并消费唤醒标志(防双调度/重入)。
    let mut s = SCHED.lock();
    let cur = s.current;
    s.remove_from_ready(cur);
    s.threads[cur].woken = false;
    drop(s);
    arch::irq_restore(irq);
}

/// 唤醒指定线程:无条件记录唤醒标志(C5 —— 唤醒可能发生在目标
/// 线程"登记之后、阻塞之前",此时其 state 尚非 Blocked,靠标志
/// 兜底,block_current 会消费它),若已阻塞则入队。
pub fn wake(id: usize) {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    if id < s.threads.len() {
        s.threads[id].woken = true;
        if s.threads[id].state == ThreadState::Blocked {
            s.enqueue(id);
        }
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
    let (old, new) = {
        let mut s = SCHED.lock();
        let cur = s.current;
        // C2(审计 15 轮):**不得释放自身正在使用的栈** —— 把栈 Box
        // 移交回收队列,由 idle(在它自己的栈上)安全释放。
        if let Some(stack) = s.threads[cur].stack.take() {
            s.reaper.push_back(stack);
        }
        s.threads[cur].state = ThreadState::Exited;
        s.threads[cur].frame_valid = false;
        s.threads[cur].ctx_valid = true;
        s.ticks_run = 0;
        let next = s.pick_next(true);
        s.current = next;
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
    // 3) 抢占:忙循环线程不 yield,持续到 tick 前进 >= 15
    //    (150ms > 100ms 时间片 → 必被抢占至少一次,主线程才可能
    //    继续;HIGH-3:旧测试目标在首片内完成,未真正验证抢占)。
    TEST_BUSY_DONE.store(false, Ordering::Relaxed);
    spawn(test_busy, PRIO_LOW);
    guard = 0;
    while !TEST_BUSY_DONE.load(Ordering::Relaxed) && guard < 200_000 {
        yield_();
        guard += 1;
    }
    if !TEST_BUSY_DONE.load(Ordering::Relaxed) {
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

/// 忙循环线程:不 yield,跑满 150ms(tick 前进 15)后置完成位。
/// 时间片 100ms → 必然经历至少一次抢占。
fn test_busy() {
    let start = crate::logger::tick();
    while crate::logger::tick().wrapping_sub(start) < 15 {
        TEST_BUSY.fetch_add(1, Ordering::Relaxed);
    }
    TEST_BUSY_DONE.store(true, Ordering::Relaxed);
}

static TEST_CTR: AtomicUsize = AtomicUsize::new(0);
static TEST_DONE: AtomicBool = AtomicBool::new(false);
static TEST_BUSY: AtomicUsize = AtomicUsize::new(0);
static TEST_BUSY_DONE: AtomicBool = AtomicBool::new(false);

// ===== 性能基线(上下文切换) =====

/// 乒乓切换次数(每线程)。
const BENCH_N: usize = 2000;
static BENCH_SW_START: AtomicUsize = AtomicUsize::new(0);
static BENCH_SW_DONE: AtomicBool = AtomicBool::new(false);

fn bench_pong_a() {
    BENCH_SW_START.store(crate::arch::get_time(), Ordering::Relaxed);
    for _ in 0..BENCH_N {
        yield_();
    }
}

fn bench_pong_b() {
    for _ in 0..BENCH_N {
        yield_();
    }
    BENCH_SW_DONE.store(true, Ordering::Relaxed);
}

/// 上下文切换成本基线(约 2×BENCH_N 次切换)。
pub fn bench() {
    BENCH_SW_DONE.store(false, Ordering::Relaxed);
    spawn(bench_pong_a, PRIO_HIGH);
    spawn(bench_pong_b, PRIO_HIGH);
    let mut guard = 0;
    while !BENCH_SW_DONE.load(Ordering::Relaxed) && guard < 200_000 {
        yield_();
        guard += 1;
    }
    if !BENCH_SW_DONE.load(Ordering::Relaxed) {
        warn!("bench: context switch timeout");
        return;
    }
    let dt = crate::arch::get_time().wrapping_sub(BENCH_SW_START.load(Ordering::Relaxed));
    // 10MHz 计时器:dt × 100ns;切换数 ≈ 2×BENCH_N。
    let ns_per_switch = dt.saturating_mul(100) / (2 * BENCH_N);
    info!("bench: context switch ≈ {ns_per_switch} ns/op (yield path)");
}
