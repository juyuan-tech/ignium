//! 内核线程与调度器(M1)。
//!
//! # 设计
//! - **线程**:内核线程,每线程独立内核栈 + 全量陷阱帧(抢占恢复用)
//!   + 协作上下文(Context)。
//! - **协作切换**(`yield_`/阻塞):调用边界保存调用者保存寄存器,
//!   经 `context_switch` 切换(见 riscv64.S)。
//! - **抢占切换**:定时器 ISR 中,若当前线程时间片到期或更高优先级
//!   线程就绪(MED-2),且存在**帧有效**的线程,把被中断线程的
//!   **全量陷阱帧**复制进其 TCB,返回下一线程的帧指针 —— 汇编
//!   恢复路径据此 sret 进入下一线程(全寄存器恢复,t/a 也不丢)。
//! - **恢复机制协议**(审计 17 轮收严):线程的恢复数据恰有一个有效
//!   (新线程除外,二者皆有效)——
//!   `ctx_valid`(协作切换保存点新鲜)/ `frame_valid`(抢占捕获帧新鲜)
//!   互斥,抢占使 ctx 失效,yield 使帧失效。协作路径接受任一有效,
//!   do_switch 按机制分发(ctx → context_switch;仅帧 → frame_restore),
//!   防止被抢占线程在"唯一可运行线程退出/阻塞"时永久滞留。
//! - **优先级**:2 级(HIGH/LOW),级内轮转;idle 为最低。
//! - **时间片**:每线程 `SLICE_TICKS`(10 tick = 100ms)内不主动
//!   yield 则被抢占(同优先级)。
//! - **同步**:调度器自身在 IRQ 安全 SpinLock(MED-3)保护下运行;
//!   ISR 路径(on_tick)零分配(容量预留)、零日志,只做帧复制与
//!   就绪队列出/入队(抢占决定在 `on_tick` 中完成)。

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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// 与 frame_valid 互补(审计 17 轮收严为双机制,见模块头):
    /// - 协作路径选中者经 do_switch 按机制恢复 —— ctx_valid 用
    ///   context_switch,仅 frame_valid 用 frame_restore;
    /// - 抢占路径(on_tick)只允许选中 frame_valid 的线程。
    ///
    /// 被抢占的线程 ctx 陈旧(状态在帧里),协作路径用陈旧 ctx
    /// 切换它会错乱(复现旧程序点),故抢占后必须置 false。
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
    /// M2 T0(V4 自审):已退出/可复用的 TCB 槽(FIFO 复用)。
    /// 使累计 spawn 不受 `threads.len() < MAX_THREADS` 的"累计硬顶"
    /// 限制 —— 退出线程槽位重新分配(活动线程数决定上限)。
    /// 复用安全性:退出线程不留在等待队列(wake 已弹出),旧 waiter
    /// 残留由 woken 协议吸收(良性虚假唤醒);SwitchTarget 裸指针仅
    /// 在切换期间存续,退出即时可复用。
    free_slots: VecDeque<usize>,
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

/// 用户线程占位入口:用户线程经 frame_restore 直接 sret 进 U 模式代码,
/// **不经过**本 S 模式包装;若被调用即内部错误(fail-loudly)。
fn user_entry_stub() {
    panic!("user thread entered kernel wrapper (internal error)");
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
    /// 恢复机制协议(审计 17 轮 MED-1 收严):线程的恢复数据恰有一个
    /// 有效:`ctx_valid` = 协作切换保存点新鲜(yield/block/exit 后);
    /// `frame_valid` = 抢占捕获帧新鲜(被定时器抢占后)。二者互斥
    /// (抢占使 ctx 失效,yield 使帧失效),do_switch 按各自机制恢复
    /// (ctx → context_switch;仅帧 → frame_restore)。
    ///
    /// `need_ctx` = true 表示协作路径调用(yield/block/exit):
    /// 接受 ctx_valid **或** frame_valid 候选 —— 仅帧有效的线程
    /// (被抢占后未再 yield)必须可被协作恢复,否则会在
    /// "唯一可运行线程被抢占后他人退出"时永久滞留。
    /// = false 表示抢占路径(on_tick):只选 frame_valid 的线程
    /// (帧恢复是被抢占线程的唯一合法恢复方式)。
    /// 无分配:仅 VecDeque 出/入队(轮转),不满足条件的候选回队尾。
    fn pick_next(&mut self, need_ctx: bool) -> usize {
        for level in 0..PRIO_LEVELS {
            let q = &mut self.ready[level];
            let round = q.len();
            for _ in 0..round {
                let Some(id) = q.pop_front() else { break };
                if self.threads[id].state != ThreadState::Ready {
                    // L13(审计 18 轮外部):非 Ready 线程不应在就绪队列中。
                    debug_assert!(
                        self.threads[id].state == ThreadState::Exited,
                        "pick_next: non-Ready thread {} in ready queue (state={:?})",
                        id,
                        self.threads[id].state
                    );
                    continue; // 非 Ready(如 Exited):丢弃。
                }
                let ok = if need_ctx {
                    self.threads[id].ctx_valid || self.threads[id].frame_valid
                } else {
                    self.threads[id].frame_valid
                };
                if ok {
                    self.threads[id].state = ThreadState::Running;
                    // V4(外部审计 CRITICAL):**不得在此清 woken**。
                    // 丢失唤醒保护时序:线程在 Mutex/Condvar 的
                    // irq_restore→block_current 窗口被抢占(Ready+入队),
                    // 之后被 wake(woken=true,不入队)→ 被本函数选中。
                    // 若在此清 woken,该线程恢复到 block_current 时会
                    // 因 woken=false 而真的阻塞 → 通知已消费,死锁。
                    // 正确做法:woken 只有在 **block_current 自己消费**
                    // (Ready 早退分支 / woken 分支 / 醒来路径)时才清除。
                    // M6 的"陈旧 woken 虚假继续"是良性的(互斥/条件循环
                    // 会重查重等),不以死锁为代价换取该"优化"。
                    return id;
                }
                q.push_back(id); // 暂不满足:轮转到队尾,不丢失。
            }
        }
        // 无候选:回当前线程"原地继续"(三个调用方 pick 前均已把
        // current 的 ctx 置有效;old==new 的 context_switch 即原地
        // 返回;exit 场景随后停机)。不会选中陈旧恢复数据。
        // M3(审计 18 轮外部):若当前线程已退出,回 idle 防停机。
        // H3(审计 18 轮外部):回退时若当前线程不是 Running(如
        // Blocked),强制回 idle 避免 livelock。
        if self.threads[self.current].state == ThreadState::Exited
            || self.threads[self.current].state != ThreadState::Running
        {
            self.idle
        } else {
            self.current
        }
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
    /// 时间片到期**或更高优先级线程就绪**(MED-2/审计 17 轮:
    /// 优先级抢占应即时,不等时间片)且存在**帧有效**的其他就绪
    /// 线程 → 复制当前帧、返回下一线程帧。否则返回原帧。
    fn on_tick(&mut self, frame: *mut usize) -> *mut usize {
        // INFO-2(审计 17 轮):wrapping —— overflow-checks 开启下
        // 2^64 tick 后 ISR 内 panic(工程上不可达,与工程约定一致)。
        self.ticks_run = self.ticks_run.wrapping_add(1);
        if self.ticks_run < SLICE_TICKS {
            // 时间片未到:仅当更高优先级就绪(帧有效)才抢占。
            let cur_prio = self.threads[self.current].prio as usize;
            // P2(本轮性能):当前线程已是最高优先级(0)时,更高优先级
            // 扫描范围 `0..cur_prio` 必然为空 → higher 恒 false ——
            // 直接早退,省掉最常见的无抢占路径(tick 内)的就绪队列
            // 扫描。与原始逻辑严格等价(0..0 的 any 恒为 false)。
            if cur_prio == 0 {
                return frame;
            }
            let higher = (0..cur_prio).any(|l| {
                self.ready[l].iter().any(|&id| {
                    id != self.current
                        && self.threads[id].state == ThreadState::Ready
                        && self.threads[id].frame_valid
                })
            });
            if !higher {
                return frame;
            }
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
        if next == cur {
            // 前瞻(审计 16 轮自审):轮转可能把刚入队的 cur 选回
            // (其帧有效)—— 白做一次捕获/恢复循环。撤销入队并
            // 恢复原线程(has_other 已证明曾存在候选,但候选可能
            // 在轮转间隙失去资格;重试路径在下一 tick 自然发生)。
            self.remove_from_ready(cur);
            self.threads[cur].state = ThreadState::Running;
            // 帧恢复运行;ctx 仍陈旧(帧恢复不更新 ctx),保持 false。
            self.threads[cur].frame_valid = true;
            self.threads[cur].ctx_valid = false;
            return frame;
        }
        self.current = next;
        // 注意:恢复目标 next 不置 ctx_valid —— 其 ctx 陈旧(自上次
        // yield 后未更新;审计 17 轮自审发现:置有效会让协作路径用
        // 陈旧 ctx 切换它,复现旧程序点)。它经帧恢复后,下次
        // yield/block/exit 会重新保存 ctx。
        // 返回下一线程的帧指针,汇编据此 sret 全量恢复。
        self.threads[next].frame.as_mut_ptr()
    }

    /// 新建线程:分配栈 + 构造初始帧(sepc=thread_entry,sp=栈顶)。
    fn spawn(&mut self, entry: fn(), prio: u8) -> usize {
        // M2 T0(V4 自审):优先复用已退出线程的 TCB 槽。
        let reuse = self.free_slots.pop_front();
        let id = match reuse {
            Some(idx) => idx,
            None => {
                // INFO-1(审计 17 轮)+ T0:强制容量上限 —— 容量为
                // ISR 零分配预留(reserve MAX_THREADS),超过即违反。
                assert!(
                    self.threads.len() < MAX_THREADS,
                    "thread table full ({MAX_THREADS})"
                );
                self.threads.len()
            }
        };
        // 零化-free 栈分配(性能优化):`vec![0u8; N]` 会白付 16KB
        // memset —— 栈内容无需初始化(初始帧/上下文显式构造)。
        // V3 审计 #10:函数名不再误导为"zeroed"。
        let stack = alloc_free_stack();
        // HIGH-4:sp 须 16 字节对齐(RISC-V ABI);堆指针仅保证 8 对齐。
        let sp = (stack.ptr as usize + THREAD_STACK_SIZE) & !0xF;
        // 初始帧:sepc = 线程包装器,sstatus:SPIE=1(进线程后开中断)
        // + SPP=1(审计 17 轮:初始帧可能经**抢占路径 sret 首启**
        // (on_tick 选中未运行线程)—— SPP=0 会降入 U 模式,取指
        // 内核文本立即缺页)。此前仅 SPIE,首启必走协作 ctx 掩盖了它。
        // CRITICAL-2(审计 16 轮):帧内 sp/gp 必须有效 —— 恢复路径
        // 会从帧加载 sp;gp 与 trap_vector 入口一致(内核代码可能
        // 生成 gp 相对访问)。
        let mut frame = [0usize; FRAME_WORDS];
        frame[FRAME_SEPC] = thread_entry as *const () as usize;
        frame[FRAME_SSTATUS] = (1 << 5) | (1 << 8); // SPIE | SPP
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
        if id < self.threads.len() {
            // 复用已退出线程的槽
            self.threads[id] = t;
        } else {
            // 新线程表扩展(idx == len)
            self.threads.push(t);
        }
        self.enqueue(id);
        id
    }

    /// M2 T1:新建**用户态**线程(执行 U 模式代码,经 ecall 进内核)。
    ///
    /// `entry_pc` 必须指向用户可执行(U 位)的虚拟地址;`stack_top` 为
    /// 用户栈顶(用户可访问)。该线程**始终经 frame_restore(sret)恢复**:
    /// ctx_valid 恒 false,因此协作选中也走帧恢复 → U 模式保持。
    fn spawn_user(&mut self, entry_pc: usize, stack_top: usize, prio: u8) -> usize {
        // M2 T0:同 spawn,优先复用已退出线程的 TCB 槽。
        let reuse = self.free_slots.pop_front();
        let id = match reuse {
            Some(idx) => idx,
            None => {
                assert!(
                    self.threads.len() < MAX_THREADS,
                    "thread table full ({MAX_THREADS})"
                );
                self.threads.len()
            }
        };
        // MED-10(审计 15 轮):优先级越界钳制,防就绪队列越界 panic。
        let prio = prio.min(PRIO_LEVELS as u8 - 1);
        // 初始帧:sepc = 用户代码入口;SPIE=1(进 U 后开中断)、
        // **SPP=0 → sret 进入 U 模式**(与内核线程 S 模式不同);
        // sp = 用户栈顶;gp = 0(用户程序不用内核 gp)。
        // CRITICAL-2(审计 16 轮):帧内 sp 必须有效 —— 恢复路径会
        // 从帧加载 sp;gp 与 trap_vector 入口无关(内核 gp 在 trap
        // 入口重载,此处占位 0)。
        let mut frame = [0usize; FRAME_WORDS];
        frame[FRAME_SEPC] = entry_pc;
        frame[FRAME_SSTATUS] = 1 << 5; // SPIE=1, SPP=0(U 模式)
        frame[1] = stack_top;
        frame[2] = 0;
        let t = Thread {
            prio,
            state: ThreadState::Ready,
            // ctx 占位(用户线程不经协作 ctx 恢复)。
            ctx: Context {
                ra: entry_pc,
                sp: stack_top,
                s: [0; 12],
            },
            frame,
            // 新线程初始帧有效:可被抢占路径或协作路径选中恢复。
            frame_valid: true,
            // 用户线程只经帧恢复(U 模式),协作选中亦走 frame_restore。
            ctx_valid: false,
            woken: false,
            // 占位:用户不经过 thread_entry(S 模式包装);进入即错误。
            entry: user_entry_stub,
            // 用户线程无内核栈(其 trap 用全局陷阱栈)。
            stack: None,
        };
        if id < self.threads.len() {
            self.threads[id] = t;
        } else {
            self.threads.push(t);
        }
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
    free_slots: VecDeque::new(),
    reaper: VecDeque::new(),
});

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

fn alloc_free_stack() -> KernelStack {
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
    s.free_slots.reserve(MAX_THREADS); // M2 T0:退出槽复用
                                       // idle 线程:id=0,最低优先级,永不阻塞。
                                       // V4(外部审计 LOW,文档说明):idle 实际运行在**引导栈**
                                       // (kernel_main 的 idle 循环),此处分配的 16 KiB 线程栈与
                                       // `ctx.ra=idle_entry` 仅为 M3 回退占位 —— 一旦 idle 首次参与
                                       // 协作切换,其 ctx 会被真实引导栈上下文覆盖,idle_entry 永不可达。
                                       // 该占位栈在内核生命周期内不回收(微小浪费,换取结构统一)。
    let idle_id = 0;
    let stack = alloc_free_stack();
    let sp = (stack.ptr as usize + THREAD_STACK_SIZE) & !0xF;
    let mut frame = [0usize; FRAME_WORDS];
    frame[FRAME_SEPC] = idle_entry as *const () as usize;
    frame[FRAME_SSTATUS] = (1 << 5) | (1 << 8); // SPIE | SPP
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
    drop(s);
    arch::irq_restore(irq);
    id
}

/// M2 T1:新建用户态线程(`entry_pc` 为用户可执行虚拟地址,`stack_top`
/// 为用户栈顶;二者须已按 U 权限映射到当前内核页表)。
pub fn spawn_user(entry_pc: usize, stack_top: usize, prio: u8) -> usize {
    let irq = arch::irq_save();
    let id = {
        let mut s = SCHED.lock();
        s.spawn_user(entry_pc, stack_top, prio)
    };
    arch::irq_restore(irq);
    id
}

/// 协作让出 CPU。
pub fn yield_() {
    let irq = arch::irq_save();
    // 单锁作用域:状态变更 + pick + 切换目标提取一次完成。
    let target = {
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
        switch_target(&mut s, cur, next)
    };
    // 锁外切换(中断关闭保证原子性;选中者恢复机制见 switch_target:
    // ctx 或帧二选一,均新鲜)。
    do_switch(&target);
    arch::irq_restore(irq);
}

/// 阻塞当前线程(须由调度器外的同步原语唤醒)。
///
/// 丢失唤醒防护(HIGH-2):若唤醒已在阻塞前发生(线程已被置 Ready
/// 并入队),则撤销入队并**继续运行**,不再阻塞 —— 否则唤醒被
/// 吞掉,线程永久挂起。
pub fn block_current() {
    let irq = arch::irq_save();
    let target = {
        let mut s = SCHED.lock();
        let cur = s.current;
        if s.threads[cur].state == ThreadState::Ready {
            // 已被唤醒(队列中):撤销本次入队,继续运行。
            s.remove_from_ready(cur);
            s.threads[cur].state = ThreadState::Running;
            // 自审(审计 17 轮续):同 woken 分支一样消费唤醒标志 ——
            // 避免下次 block_current 把本次唤醒再消费一次(虚假继续)。
            s.threads[cur].woken = false;
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
        switch_target(&mut s, c, n)
    };
    // 锁外切换(中断关闭保证原子性;恢复机制见 switch_target)。
    do_switch(&target);
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

/// 回收已退出线程的栈(LOW-3/审计 17 轮):idle 循环调用,配合
/// yield_ 内的回收,保证无 yield 的纯 wfi 周期也能回收。
///
/// 只允许在**不会退出/正在使用被回收栈**的上下文调用 ——
/// idle 线程永不退出,且在自身栈上释放他人栈是安全的(C2)。
pub fn drain_reaper() {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    while let Some(stack) = s.reaper.pop_front() {
        drop(stack);
    }
    drop(s);
    arch::irq_restore(irq);
}

/// 线程退出(入口函数返回后调用,不返回)。
pub fn exit() -> ! {
    let irq = arch::irq_save();
    let target = {
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
        // M2 T0(V4 自审):释放 TCB 槽供后续 spawn 复用。
        // 安全性:woken 残留由 wait 循环吸收;退出线程必不在就绪/等待
        // 队列(pick 已弹出、wake 已消费),复用即安全。
        s.free_slots.push_back(cur);
        s.ticks_run = 0;
        let next = s.pick_next(true);
        s.current = next;
        switch_target(&mut s, cur, next)
    };
    // 锁外切换(中断关闭保证原子性;恢复机制见 switch_target)。
    do_switch(&target);
    // exit_self 切走,不会到达这里;防御性停机。
    arch::irq_restore(irq);
    arch::halt()
}

/// 用户线程从 **trap 上下文**(ecall)退出,永不返回。
///
/// 与 `exit()`(从线程自身内核栈调用)不同:本函数在
/// `trap_vector → trap_handler → ecall 分支` 中执行,CPU 位于
/// **陷阱栈**上。此时若复用 `exit()` 的 `context_switch` 切到目标
/// 线程,`sscratch` 会残留为本次 trap 的帧底(只有汇编的
/// `frame_restore`/trap 恢复路径才重置 sscratch)——目标线程下一次
/// trap 的嵌套检测(H2,`riscv64.S` label `4`)把残留帧底误判为
/// "第二层 trap"而停机(实测:用户线程 exit 后 timer 中断一进来即死,
/// uptime/抢占全停,系统停在 label `4` 的 wfi 死循环)。
///
/// 正确做法:把 target(当前 `ctx_valid`)的协作上下文**展开为陷阱帧**,
/// 返回帧指针交给 trap/恢复路径 `sret`(恢复路径执行
/// `csrw sscratch, _trap_stack_top` 重置),等价完成切换且不留残留。
///
/// # 线程语义
/// - 当前线程(cur)标记 `Exited`、槽位入 `free_slots`、栈移交 reaper;
/// - `next` 若 `ctx_valid`,其 ctx.ra/sp/s0-11 展开为帧
///   (sepc/sp/帧寄存器区),`split ctx_valid=false, frame_valid=true`;
/// - 之后由 do_switch 经 `frame_restore` sret 进入 next。
pub fn exit_from_trap() -> ! {
    let irq = arch::irq_save();
    let target = {
        let mut s = SCHED.lock();
        let cur = s.current;
        // C2(审计 15 轮):不得释放自身正在使用的栈 —— 交给 idle reaper。
        // 用户线程 stack=None,下方 take 返回 None,安全无操作。
        if let Some(stack) = s.threads[cur].stack.take() {
            s.reaper.push_back(stack);
        }
        s.threads[cur].state = ThreadState::Exited;
        s.threads[cur].frame_valid = false;
        s.threads[cur].ctx_valid = true;
        // M2 T0:退出槽入 free_slots 供后续 spawn 复用。
        s.free_slots.push_back(cur);
        s.ticks_run = 0;
        let next = s.pick_next(true);
        s.current = next;
        // 从 trap 上下文 exit:本函数在 `trap_handler(ecall)` 的栈帧上
        // 运行,sscratch 仍是本次 trap 入栈后的"帧底"(trap_vector 写入)。
        // 直接 context_switch 回目标线程,其 sscratch 会残留该陈旧值,
        // 导致目标线程下一次 trap 的嵌套检测(H2,riscv64.S)误判为
        // "第二层 trap"而停机(lablat `4`)。故切换前先把 sscratch
        // 重置为陷阱栈顶,由下一次 trap 入口正常压帧。
        crate::arch::set_sscratch_trap_top();
        switch_target(&mut s, cur, next)
    };
    // 锁外切换(context_switch:目标线程的协作上下文恢复)。
    do_switch(&target);
    // 切换成功后不会返回此处;防御性停机。
    arch::irq_restore(irq);
    arch::halt()
}

/// 切换目标:锁内提取,锁外执行。
///
/// **裸指针生命周期保证(自审加深注释)**:`old_ctx`/`new_ctx`/`new_frame`
/// 指向 `SCHED.threads` 的 Vec 后备存储。锁守卫(s)在 `switch_target`
/// 返回后、`do_switch` 前已 drop,但 Vec 是 `static SCHED` 的一部分
/// (非局部堆分配),后备存储在内核全程有效,不会因锁释放而失效。
/// 单核下 switch→do_switch 间无其它线程能修改 Vec;M2 多核调度若做
/// 线程迁移/回收 TCB,必须先把目标从 Vec 摘出再切换。
struct SwitchTarget {
    old_ctx: *mut Context,
    new_ctx: *const Context,
    new_frame: *mut usize,
    use_frame: bool,
}

/// 锁内构造切换目标(审计 17 轮):恢复机制按选中者的有效数据选择
/// —— ctx_valid → context_switch;仅帧有效(被抢占后未再 yield)
/// → frame_restore(从帧 sret,不返回)。选中者由 pick_next 保证
/// 至少一种有效。
fn switch_target(s: &mut Scheduler, cur: usize, next: usize) -> SwitchTarget {
    let use_frame = !s.threads[next].ctx_valid;
    let old_ctx = (&mut s.threads[cur].ctx) as *mut Context;
    let new_ctx = (&s.threads[next].ctx) as *const Context;
    let new_frame = s.threads[next].frame.as_mut_ptr();
    SwitchTarget {
        old_ctx,
        new_ctx,
        new_frame,
        use_frame,
    }
}

/// 锁外切换(中断关闭保证原子性)。
fn do_switch(t: &SwitchTarget) {
    if t.use_frame {
        // C2:frame_restore 内部先保存 old_ctx 再帧恢复,防上下文丢失。
        unsafe { arch::frame_restore(t.new_frame, t.old_ctx) }
    } else {
        unsafe { arch::context_switch(t.old_ctx, t.new_ctx) }
    }
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
    // 3) 忙循环线程退出 + 协作恢复:test_busy 不 yield 运行 15 tick
    //    (150ms),期间主线程(S 级轮转)可运行 —— 时间片到期时
    //    on_tick 若无其他 frame-valid 线程则不清抢占(has_other 为假),
    //    故本项实际验证"忙循环线程的正常运行与退出",抢占效果由
    //    第 4 项(优先级抢占)覆盖。V3 审计 #7 更新说明。
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
    // 4) MED-2(审计 17 轮)回归:优先级抢占 —— LOW 忙循环第 5 tick
    //    spawn 高优先级线程,HIGH 必须在 1 tick 内首启(不允许等满
    //    时间片);HIGH 先退出后,仅帧有效的 LOW 须经帧恢复继续
    //    (审计 17 轮:仅帧有效线程的协作恢复,防滞留)。
    TEST_PRIO_LOW_DONE.store(false, Ordering::Relaxed);
    TEST_PRIO_HIGH_DONE.store(false, Ordering::Relaxed);
    TEST_PRIO_LOW_START.store(0, Ordering::Relaxed);
    TEST_PRIO_HIGH_START.store(0, Ordering::Relaxed);
    spawn(test_prio_low, PRIO_LOW);
    guard = 0;
    while (!TEST_PRIO_LOW_DONE.load(Ordering::Acquire)
        || !TEST_PRIO_HIGH_DONE.load(Ordering::Acquire))
        && guard < 500_000
    {
        yield_();
        guard += 1;
    }
    if !TEST_PRIO_LOW_DONE.load(Ordering::Acquire) || !TEST_PRIO_HIGH_DONE.load(Ordering::Acquire) {
        return Err("priority preemption timeout (thread stranded?)");
    }
    let delay = TEST_PRIO_HIGH_START
        .load(Ordering::Relaxed)
        .wrapping_sub(TEST_PRIO_LOW_START.load(Ordering::Relaxed))
        .wrapping_sub(5);
    if delay > 3 {
        return Err("priority preemption not immediate");
    }

    // 5) 调度压力:16 线程 × 1000 次递增+yield,验证大规模协作。
    TEST_STRESS_DONE.store(0, Ordering::Relaxed);
    for _ in 0..16 {
        spawn(test_stress, PRIO_HIGH);
    }
    guard = 0;
    while TEST_STRESS_DONE.load(Ordering::Relaxed) < 16 * 1000 && guard < 500_000 {
        yield_();
        guard += 1;
    }
    if TEST_STRESS_DONE.load(Ordering::Relaxed) != 16 * 1000 {
        return Err("scheduler stress test timeout");
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

/// MED-2 回归:低优先级忙循环 —— 第 5 tick spawn 高优先级线程,
/// 共运行 25 tick。期间被 HIGH 抢占(帧捕获),HIGH 退出后须经
/// 帧恢复继续(仅帧有效线程的协作恢复路径)。
fn test_prio_low() {
    let s0 = crate::logger::tick();
    TEST_PRIO_LOW_START.store(s0 as usize, Ordering::Relaxed);
    let mut spawned = false;
    while crate::logger::tick().wrapping_sub(s0) < 25 {
        if !spawned && crate::logger::tick().wrapping_sub(s0) >= 5 {
            spawn(test_prio_high, PRIO_HIGH);
            spawned = true;
        }
        TEST_PRIO_LOW.fetch_add(1, Ordering::Relaxed);
    }
    TEST_PRIO_LOW_DONE.store(true, Ordering::Relaxed);
}

/// MED-2 回归:高优先级忙循环 —— 记录首启 tick,运行 3 tick 后
/// 退出。首启应发生在 spawn 后的下一个 tick(即时优先级抢占)。
fn test_prio_high() {
    let s0 = crate::logger::tick();
    TEST_PRIO_HIGH_START.store(s0 as usize, Ordering::Relaxed);
    while crate::logger::tick().wrapping_sub(s0) < 3 {
        TEST_PRIO_HIGH.fetch_add(1, Ordering::Relaxed);
    }
    TEST_PRIO_HIGH_DONE.store(true, Ordering::Relaxed);
}

/// 调度压力线程:1000 次递增 + yield,测试大规模协作。
fn test_stress() {
    for _ in 0..1000 {
        TEST_STRESS_DONE.fetch_add(1, Ordering::Relaxed);
        yield_();
    }
}

static TEST_CTR: AtomicUsize = AtomicUsize::new(0);
static TEST_DONE: AtomicBool = AtomicBool::new(false);
static TEST_BUSY: AtomicUsize = AtomicUsize::new(0);
static TEST_BUSY_DONE: AtomicBool = AtomicBool::new(false);
static TEST_PRIO_LOW: AtomicUsize = AtomicUsize::new(0);
static TEST_PRIO_HIGH: AtomicUsize = AtomicUsize::new(0);
static TEST_PRIO_LOW_DONE: AtomicBool = AtomicBool::new(false);
static TEST_PRIO_HIGH_DONE: AtomicBool = AtomicBool::new(false);
static TEST_PRIO_LOW_START: AtomicUsize = AtomicUsize::new(0);
static TEST_PRIO_HIGH_START: AtomicUsize = AtomicUsize::new(0);
static TEST_STRESS_DONE: AtomicUsize = AtomicUsize::new(0);

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
    // V4(外部审计 LOW):用运行时 timebase 频率换算 ns。
    let freq = crate::board::timer_freq();
    let ns_per_tick = 1_000_000_000u64 / freq as u64;
    let ns_per_switch = (dt as u64).saturating_mul(ns_per_tick) / (2 * BENCH_N) as u64;
    info!("bench: context switch ≈ {ns_per_switch} ns/op (yield path)");
}
