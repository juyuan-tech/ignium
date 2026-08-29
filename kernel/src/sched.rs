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
//! - **D22(M2 T2a)woken 抢占**:抢占路径的候选从「仅帧有效」放宽为
//!   「帧有效 **或** 被唤醒的 ctx 线程」。被 `wake()` 唤醒(经
//!   `block_current` 阻塞,`ctx_valid` 有效)的线程此前不可被抢占,
//!   可被同/更高优先级忙循环饿死;现把其 ctx 展开为 S 模式陷阱帧
//!   (sepc=ctx.ra, SPP=1)再 frame_restore sret 恢复(见
//!   `expand_ctx_to_frame`)。这是优先级继承(PIP,T2b)的抢占基础。
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
//! - **每进程地址空间(M2 T1.5)**:Thread 携带 `root`(本线程应运行的
//!   satp 根表:内核线程 = 内核根表,用户线程 = 所属进程根表)。
//!   `do_switch`/`on_tick` 在切换前经 `mmu::switch_root` 切换 satp
//!   (与当前相同则 no-op);用户线程恒走 frame_restore(sret),切换点
//!   在 Rust 侧(汇编恢复路径不碰 satp,见 riscv64.S)。
//! - **D19 多核调度(M2 T3b)**:就绪队列/当前/idle/时间片全部 per-CPU
//!   化(数组 `[..; MAX_HARTS]`),`Thread.hart` = 亲和性(默认 = 创建时
//!   当前核)。**全局 SCHED 锁保留** → 无数据竞争,只改语义:线程恒在
//!   `ready[threads[id].hart]` 且只在亲和核上运行。`wake` 把线程放进
//!   目标核队列,若目标核 idle 且非本核 → SBI IPI 唤醒其 wfi(判定与发
//!   IPI 同在 SCHED 锁临界区 → 无丢失唤醒窗口)。副核 idle 循环 =
//!   `pick_next(hart)` 有就绪 → `do_switch`,否则 `wait_for_interrupt`。

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::arch::{self, Context, MAX_HARTS};
use crate::error;
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
const PRIO_LEVELS: usize = 3;
/// 高优先级。
pub const PRIO_HIGH: u8 = 0;
/// 中优先级(M2 T2b:构造确定性优先级反转测试 —— HIGH 阻塞 / MED 饿死 LOW)。
pub const PRIO_MED: u8 = 1;
/// 低优先级。
pub const PRIO_LOW: u8 = 2;

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
    /// M2 T2b:IPC 唤醒时投递的消息(内核线程路径)。
    /// 用户线程经 frame_restore 读 TCB 帧取消息;内核线程经
    /// `block_current` 恢复(ctx,读不到帧)—— 由本字段中转,
    /// `take_ipc_msg()` 读取即清。None = 无待取消息。
    ipc_msg: Option<[usize; crate::ipc::MSG_WORDS]>,
    /// M2 T1.5:所属进程(None = 内核线程,运行于内核根表)。
    /// M2 T2a:IPC 能力查表经 `current_proc()` 读取。
    proc: Option<usize>,
    /// M2 T3b(D19):亲和性 —— 本线程所属的核(其就绪队列 / 运行核)。
    /// 默认 = 创建时当前 hart;`set_affinity` 可迁移(仅非运行线程)。
    /// 不变量:线程恒在 `ready[hart]`,且只在 `hart` 上运行。
    hart: usize,
    /// M2 T1.5:本线程应运行的 satp 根表(内核线程 = kernel_root,
    /// 用户线程 = 所属进程根表)。**缓存到 Thread**(非切换时查进程表),
    /// 使 ISR 路径(on_tick)零分配读根表。
    root: usize,
    /// 线程入口(thread_entry 经 id 查表调用)。
    entry: fn(),
    /// 线程栈(Box:栈内存来自内核堆)。
    #[allow(dead_code)]
    stack: Option<KernelStack>,
}

/// M2 T2b(PIP):优先级捐赠 —— donor 阻塞在 IPC 时,把其**有效优先级**
/// 捐赠给期望的对方进程 `peer_proc` 的所有线程;配对完成(wake)时撤销。
///
/// 语义(经典 PIP):持资源的 LOW 进程线程临时继承等待方(HIGH)优先级,
/// 从而能抢占中间优先级的忙循环、完成配对、唤醒等待方 —— 否则等待方
/// 被中间优先级饿死(优先级反转)。`prio` 存 donor 的有效优先级(可能
/// 已被其他捐赠抬升 → 自然支持近似链式)。每线程至多一处 IPC 阻塞,
/// 故表上限 = 线程上限。
struct Donation {
    donor_tid: usize,
    peer_proc: usize,
    prio: u8,
}

/// 捐赠表容量上限(与线程上限一致;每线程至多一处 IPC 阻塞)。
const MAX_DONATIONS: usize = MAX_THREADS;

/// 调度器。
struct Scheduler {
    threads: Vec<Thread>,
    /// M2 T3b(D19)per-CPU 就绪队列:`ready[hart][prio]`。线程按**其亲和性
    /// hart**(`threads[id].hart`)入队,只在亲和核上运行。全局 SCHED 锁
    /// 保留 → 无数据竞争(只改语义)。
    ready: [[VecDeque<usize>; PRIO_LEVELS]; MAX_HARTS],
    /// M2 T2b(PIP):优先级捐赠表(见 `Donation`)。ISR(on_tick)只读扫描,
    /// 变更仅在外路径(阻塞/唤醒);容量 init 预留,零分配。
    donations: Vec<Donation>,
    /// M2 T3b(D19):每核当前线程 id。
    current: [usize; MAX_HARTS],
    /// M2 T3b(D19):每核 idle 线程 id(最低优先级,永不阻塞)。
    idle: [usize; MAX_HARTS],
    /// M2 T3b(D19):每核当前线程已运行 tick 数。
    ticks_run: [u64; MAX_HARTS],
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
        let id = s.current[arch::hartid()];
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
    /// = false 表示抢占路径(on_tick):选 frame_valid 的线程,或
    /// **被唤醒的 ctx 线程**(D22:`ctx_valid && woken`,on_tick 会把它
    /// 展开为帧再恢复) —— 帧恢复是被抢占线程的唯一合法恢复方式。
    /// 无分配:仅 VecDeque 出/入队(轮转),不满足条件的候选回队尾。
    ///
    /// D19:`hart` = 运行核 —— 只从该核的 ready 队列选(就绪队列中的
    /// 线程按定义恒在其亲和核队列,见 `enqueue`);回退同理用该核的
    /// current/idle。
    fn pick_next(&mut self, hart: usize, need_ctx: bool) -> usize {
        for level in 0..PRIO_LEVELS {
            let q = &mut self.ready[hart][level];
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
                    // M2 T2a(D22):抢占路径额外接受「被唤醒且协作上下文
                    // 有效」的线程(woken 标志由 wake 置位)。on_tick 选中
                    // 后会经 expand_ctx_to_frame 把 ctx 展开为 S 模式帧。
                    self.threads[id].frame_valid
                        || (self.threads[id].ctx_valid && self.threads[id].woken)
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
        if self.threads[self.current[hart]].state == ThreadState::Exited
            || self.threads[self.current[hart]].state != ThreadState::Running
        {
            self.idle[hart]
        } else {
            self.current[hart]
        }
    }

    /// 把线程加入就绪队列。按**有效优先级**选队:捐赠(见 `Donation`)
    /// 可临时抬升所属进程线程的优先级,spawn/spawn_user/requeue 一律
    /// 经本函数 → 捐赠对"晚 spawn 的接收方"也自动生效。
    ///
    /// D19:队列按线程的**亲和性核**索引(`ready[threads[id].hart]`)——
    /// 线程只在亲和核上运行;唤醒/迁移(set_affinity)后由本函数把线程
    /// 放入目标核队列。
    fn enqueue(&mut self, id: usize) {
        let prio = self.eff_prio(id) as usize;
        let h = self.threads[id].hart;
        self.ready[h][prio].push_back(id);
        self.threads[id].state = ThreadState::Ready;
    }

    /// M2 T2b(PIP):线程 `id` 的**有效优先级** = 自然优先级与所有指向其
    /// 所属进程的捐赠取最小(数值小 = 优先级高)。捐赠表为固定小表
    /// (≤ MAX_THREADS),ISR(on_tick)可只读线性扫描,零分配。
    fn eff_prio(&self, id: usize) -> u8 {
        let mut p = self.threads[id].prio;
        if let Some(proc) = self.threads[id].proc {
            for d in &self.donations {
                if d.peer_proc == proc && d.prio < p {
                    p = d.prio;
                }
            }
        }
        p
    }

    /// M2 T2b(PIP):把进程 `proc` 的所有**就绪**线程按有效优先级重排
    /// 队列。捐赠注册(抬升)或撤销(回落)后调用,使队列反映新优先级。
    ///
    /// 不触碰 `woken`:抬升线程的抢占资格由其恢复机制决定(新线程
    /// frame_valid、IPC 配对唤醒的 ctx 线程经协作路径选中),若在此置
    /// woken 会造成**陈旧 woken** —— IPC 的 recv/send 在 `block_current`
    /// 之前已登记 pending,block_current 一旦消费陈旧 woken 而跳过阻塞,
    /// 该 pending 即成孤儿(M6「良性虚假继续」仅适用于互斥/条件循环的
    /// 重查重等,不适用于 IPC 的原子"登记+阻塞")。
    fn requeue_proc_threads(&mut self, proc: usize) {
        // 栈数组收集:本函数只在外路径调用(非 ISR),容量固定,零堆分配。
        // 从**任意核、任意优先**就绪队列移除该进程全部就绪线程(可能
        // 散布于不同核/优先队列),再逐个按有效优先级 enqueue 重排 ——
        // 抬升(注册)与回落(撤销)都正确(D19:enqueue 按线程亲和核归位)。
        let mut buf = [0usize; MAX_THREADS];
        let mut n = 0usize;
        for q in self.ready.iter_mut().flatten() {
            q.retain(|&id| {
                if self.threads[id].proc == Some(proc) {
                    if n < MAX_THREADS {
                        buf[n] = id;
                        n += 1;
                    }
                    false
                } else {
                    true
                }
            });
        }
        for &id in buf.iter().take(n) {
            self.enqueue(id);
        }
    }

    /// M2 T2b(PIP):注册捐赠 —— donor(阻塞于 IPC,有效优先级 `prio`)把
    /// 其优先级捐赠给期望对方进程 `peer_proc` 的所有线程。同 donor
    /// 去重(每线程至多一处 IPC 阻塞)。随后把 peer 进程就绪线程按新
    /// 有效优先级重排。
    fn register_donation(&mut self, donor_tid: usize, peer_proc: usize) {
        let prio = self.eff_prio(donor_tid);
        let mut exists = false;
        for d in self.donations.iter_mut() {
            if d.donor_tid == donor_tid {
                d.peer_proc = peer_proc;
                d.prio = prio;
                exists = true;
                break;
            }
        }
        if !exists && self.donations.len() < MAX_DONATIONS {
            self.donations.push(Donation {
                donor_tid,
                peer_proc,
                prio,
            });
        }
        self.requeue_proc_threads(peer_proc);
    }

    /// M2 T2b(PIP):撤销 donor 的全部捐赠(配对完成 wake 时调用)。受影响
    /// 的 peer 进程按自然优先级回落重排。
    fn revoke_donations(&mut self, donor_tid: usize) {
        let mut affected = [0usize; MAX_THREADS];
        let mut n = 0usize;
        self.donations.retain(|d| {
            if d.donor_tid == donor_tid {
                if n < MAX_THREADS && !affected[..n].contains(&d.peer_proc) {
                    affected[n] = d.peer_proc;
                    n += 1;
                }
                false
            } else {
                true
            }
        });
        for &proc in affected.iter().take(n) {
            self.requeue_proc_threads(proc);
        }
    }

    /// M2 D12:撤销指定线程作为 donor 的全部捐赠(进程被杀时调用),受影响
    /// peer 进程按自然优先级回落重排。与 `revoke_donations_for_proc`(按
    /// 被捐进程定位)互补:后者清 `peer_proc == pid`(指向被杀的),本函数
    /// 清 `donor_tid ∈ 被杀线程`(**被杀进程发出的**)—— `purge_process`
    /// 只唤醒存活的配对方,被杀线程自己的捐赠无人撤销,不清理会永久
    /// 抬升 peer 进程优先级并占死 `MAX_DONATIONS` 槽。
    ///
    /// `skip_proc`:不重排该进程(被杀的进程自身 —— 其线程已标记退出,
    /// 重排会复活;仅当被杀线程曾自捐时才可能命中)。
    fn revoke_donations_of(&mut self, tids: &[usize], skip_proc: Option<usize>) {
        let mut affected = [0usize; MAX_THREADS];
        let mut n = 0usize;
        self.donations.retain(|d| {
            if tids.contains(&d.donor_tid) {
                if n < MAX_THREADS && !affected[..n].contains(&d.peer_proc) {
                    affected[n] = d.peer_proc;
                    n += 1;
                }
                false
            } else {
                true
            }
        });
        for &proc in affected.iter().take(n) {
            if Some(proc) != skip_proc {
                self.requeue_proc_threads(proc);
            }
        }
    }

    /// M2 D12:撤销指向进程 `proc` 的全部捐赠(进程被销毁时调用),受影响
    /// donor 的所属进程按自然优先级回落重排。与 `revoke_donations`(按 donor
    /// 线程定位)不同:本函数按**被捐进程**定位(`peer_proc == proc`)。
    ///
    /// 防御性兜底:常规路径上,阻塞于 IPC 的 donor 已由 `purge_process` →
    /// `ipc_wake_with_err` 逐个唤醒并撤销;此处清理**尚未撤销**的陈旧捐赠
    /// (如多核竞争窗口内新登记的),防捐赠抬升已无线程的无主进程。
    fn revoke_donations_for_proc(&mut self, proc: usize) {
        let mut affected = [0usize; MAX_THREADS];
        let mut n = 0usize;
        self.donations.retain(|d| {
            if d.peer_proc == proc {
                if n < MAX_THREADS && !affected[..n].contains(&d.donor_tid) {
                    affected[n] = d.donor_tid;
                    n += 1;
                }
                false
            } else {
                true
            }
        });
        for &tid in affected.iter().take(n) {
            if let Some(p) = self.threads[tid].proc {
                // 不重排被杀进程自身(其线程已被标记退出,重排会复活)。
                if p != proc {
                    self.requeue_proc_threads(p);
                }
            }
        }
    }

    /// 从就绪队列撤销一个线程(block_current 的"已唤醒则继续"路径)。
    /// D19:线程只在亲和核队列中 → 只扫 `ready[threads[id].hart]`。
    fn remove_from_ready(&mut self, id: usize) {
        let h = self.threads[id].hart;
        for q in self.ready[h].iter_mut() {
            if let Some(pos) = q.iter().position(|&x| x == id) {
                q.remove(pos);
                return;
            }
        }
    }

    /// M2 T2a(D22):抢占候选谓词 —— `frame_valid`(被抢占捕获帧)或
    /// 「被唤醒的 ctx 线程」(woken 标志由 wake 置位,协作保存点新鲜,
    /// 可展开为帧)。仅 on_tick / pick_next(false) 使用;不改变协作
    /// 路径语义。ISR 内纯只读、零分配。
    fn preemptable(&self, id: usize) -> bool {
        self.threads[id].state == ThreadState::Ready
            && (self.threads[id].frame_valid
                || (self.threads[id].ctx_valid && self.threads[id].woken))
    }

    /// 抢占决策(定时器 ISR 内,中断关闭,不可阻塞):
    /// 时间片到期**或更高优先级线程就绪**(MED-2/审计 17 轮:
    /// 优先级抢占应即时,不等时间片)且存在可抢占的其他就绪线程
    /// (D22:帧有效或被唤醒的 ctx 线程)→ 复制当前帧、返回下一线程帧。
    /// 否则返回原帧。
    /// D19:`hart` = 当前运行核(ISR 内由 trap_handler 传入)。抢占只作用
    /// 于本核的 current / 本核就绪队列(每核独立时间片)。
    fn on_tick(&mut self, frame: *mut usize, hart: usize) -> *mut usize {
        // INFO-2(审计 17 轮):wrapping —— overflow-checks 开启下
        // 2^64 tick 后 ISR 内 panic(工程上不可达,与工程约定一致)。
        self.ticks_run[hart] = self.ticks_run[hart].wrapping_add(1);
        if self.ticks_run[hart] < SLICE_TICKS {
            // 时间片未到:仅当更高优先级就绪(可抢占)才抢占。
            // M2 T2b(PIP):用**有效**优先级判断当前线程 —— 被捐赠抬升的
            // 线程(如持资源的 LOW 进程线程)在继承 HIGH 期间,中间优先级
            // 不得抢占它(否则 PIP 失效:抬升了还是被 MED 打断)。
            let cur_prio = self.eff_prio(self.current[hart]) as usize;
            // P2(本轮性能):当前线程已是最高优先级(0)时,更高优先级
            // 扫描范围 `0..cur_prio` 必然为空 → higher 恒 false ——
            // 直接早退,省掉最常见的无抢占路径(tick 内)的就绪队列
            // 扫描。与原始逻辑严格等价(0..0 的 any 恒为 false)。
            if cur_prio == 0 {
                return frame;
            }
            let higher = (0..cur_prio).any(|l| {
                self.ready[hart][l]
                    .iter()
                    .any(|&id| id != self.current[hart] && self.preemptable(id))
            });
            if !higher {
                return frame;
            }
        }
        self.ticks_run[hart] = 0;
        // 是否存在可抢占的就绪线程(非自身)。
        let has_other = (0..PRIO_LEVELS).any(|l| {
            self.ready[hart][l]
                .iter()
                .any(|&id| id != self.current[hart] && self.preemptable(id))
        });
        if !has_other {
            return frame;
        }
        let cur = self.current[hart];
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
        // CRITICAL-1:抢占路径选帧有效线程(本核队列)。
        let next = self.pick_next(hart, false);
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
        self.current[hart] = next;
        // M2 T2a(D22):选中者可能是「被唤醒的 ctx 线程」(仅 ctx_valid)——
        // 帧恢复是其唯一合法恢复方式,先把 ctx 展开为 S 模式陷阱帧
        // (expand_ctx_to_frame 置 frame_valid=true、清 woken)。帧有效者
        // 直接使用既有捕获帧。
        if !self.threads[next].frame_valid {
            self.expand_ctx_to_frame(next);
        }
        // M2 T1.5:抢占切换到 next 线程的 satp 根表(相同则 no-op)。
        // 必须在返回帧指针、汇编 sret 之前完成 —— 恢复路径汇编不碰
        // satp;此处仅 CSR 读写 + sfence,ISR 内零分配、零日志。
        crate::mmu::switch_root(self.threads[next].root);
        // 注意:恢复目标 next 不置 ctx_valid —— 其 ctx 陈旧(自上次
        // yield 后未更新;审计 17 轮自审发现:置有效会让协作路径用
        // 陈旧 ctx 切换它,复现旧程序点)。它经帧恢复后,下次
        // yield/block/exit 会重新保存 ctx。
        // 返回下一线程的帧指针,汇编据此 sret 全量恢复。
        self.threads[next].frame.as_mut_ptr()
    }

    /// M2 T2a(D22):把线程的协作上下文(ctx)展开为 S 模式陷阱帧。
    ///
    /// 用途:抢占路径(on_tick)选中「被唤醒且仅 ctx_valid」的线程时,
    /// frame_restore(sret)是唯一合法恢复机制 —— 该线程此前在协作点
    /// (如 `block_current`)经 context_switch 切走,进度在 ctx。展开后
    /// sepc=ctx.ra(内核恢复点)、sstatus=SPIE|SPP=1(S 模式 sret)、
    /// sp/gp/s0-s11 来自 ctx;frame_restore 会把 sscratch 恒置陷阱栈顶,
    /// 无嵌套误判。展开同时**消费 woken 标志**(本次唤醒已兑现)。
    ///
    /// 调用点要求:已持有 SCHED 锁;ISR 内零分配(栈上数组)。
    fn expand_ctx_to_frame(&mut self, id: usize) {
        let ctx = self.threads[id].ctx;
        let mut frame = [0usize; FRAME_WORDS];
        frame[FRAME_SEPC] = ctx.ra;
        frame[FRAME_SSTATUS] = (1 << 5) | (1 << 8); // SPIE | SPP=1
        frame[crate::arch::gpr::X_SP] = ctx.sp;
        frame[crate::arch::gpr::X_GP] = __global_pointer();
        // s0-s11 → 帧槽 X_S0(7)/X_S1(8)/X_S2..X_S11(17..26)。
        frame[crate::arch::gpr::X_S0] = ctx.s[0];
        frame[crate::arch::gpr::X_S1] = ctx.s[1];
        frame[crate::arch::gpr::X_S2..crate::arch::gpr::X_S2 + 10].copy_from_slice(&ctx.s[2..12]);
        let t = &mut self.threads[id];
        t.frame = frame;
        t.frame_valid = true;
        t.ctx_valid = false;
        t.woken = false;
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
            // 内核线程:不属任何进程,运行于内核根表。
            proc: None,
            // D19:亲和性 = 创建时当前核(spawn 后可用 set_affinity 迁移)。
            hart: arch::hartid(),
            root: crate::mmu::kernel_root(),
            entry,
            stack: Some(stack),
            ipc_msg: None,
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
    /// `proc_id` 为该线程所属进程(决定其地址空间根表);`entry_pc` 必须
    /// 指向该进程根表内用户可执行(U 位)的虚拟地址;`stack_top` 为用户
    /// 栈顶(用户可访问)。该线程**始终经 frame_restore(sret)恢复**:
    /// ctx_valid 恒 false,因此协作选中也走帧恢复 → U 模式保持。
    fn spawn_user(&mut self, proc_id: usize, entry_pc: usize, stack_top: usize, prio: u8) -> usize {
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
        // M2 T1.5:取所属进程的地址空间根表(缓存进 Thread,供
        // do_switch/on_tick 切换 satp 用)。无效 pid 由 process::root
        // panic(fail-loudly)。
        let root = crate::process::root(proc_id);
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
            proc: Some(proc_id),
            // D19:亲和性 = 创建时当前核(用户线程同理,只在亲和核运行)。
            hart: arch::hartid(),
            root,
            // 占位:用户不经过 thread_entry(S 模式包装);进入即错误。
            entry: user_entry_stub,
            // 用户线程无内核栈(其 trap 用全局陷阱栈)。
            stack: None,
            ipc_msg: None,
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
/// D19:就绪队列/当前/idle/时间片均为 per-CPU 数组(MAX_HARTS 槽);
/// 空 VecDeque 用内联 const 重复初始化(非 Copy 元素)。
static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    threads: Vec::new(),
    ready: [const { [const { VecDeque::new() }; PRIO_LEVELS] }; MAX_HARTS],
    donations: Vec::new(),
    current: [0; MAX_HARTS],
    idle: [0; MAX_HARTS],
    ticks_run: [0; MAX_HARTS],
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

/// 初始化调度器(每核一个 idle 线程;须在堆就绪后、irq_enable 前调用)。
pub fn init() {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    // HIGH-5:预留容量 —— 此后 ISR 内的 enqueue/栈移交均零分配。
    // D19:每核每优先队列都预留(全局 ISR 零分配约定不变)。
    for q in s.ready.iter_mut().flatten() {
        q.reserve(MAX_THREADS);
    }
    s.reaper.reserve(MAX_THREADS);
    s.threads.reserve(MAX_THREADS);
    s.free_slots.reserve(MAX_THREADS); // M2 T0:退出槽复用
    s.donations.reserve(MAX_DONATIONS); // M2 T2b(PIP):捐赠表
                                        // D19:每个 hart 一个 idle 线程(每核独立 idle;副核由 entry.S 在
                                        // per-hart idle 栈上引导进入 secondary_main,再入本 idle 循环)。
                                        // V4(外部审计 LOW,文档说明):idle 实际运行在**引导栈 / per-hart
                                        // idle 栈**(boot hart = kernel_main 的 idle 循环),此处分配的
                                        // 16 KiB 线程栈与 `ctx.ra=idle_entry` 仅为 M3 回退占位 —— 一旦 idle
                                        // 首次参与协作切换,其 ctx 会被真实引导栈上下文覆盖,idle_entry
                                        // 永不可达。该占位栈在内核生命周期内不回收(微小浪费,换取结构统一)。
    for h in 0..MAX_HARTS {
        let idle_id = s.threads.len();
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
            // idle 以引导上下文直接运行(非经切换),初始帧从未被使用 →
            // 无效;其帧在真正被抢占时才会捕获。
            frame_valid: false,
            ctx_valid: true,
            woken: false,
            // idle 为内核线程:运行于内核根表。
            proc: None,
            // D19:idle 恒属其核(占位语义;实际运行在引导/per-hart 栈)。
            hart: h,
            root: crate::mmu::kernel_root(),
            entry: idle_entry,
            stack: Some(stack),
            ipc_msg: None,
        });
        s.current[h] = idle_id;
        s.idle[h] = idle_id;
    }
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

/// M2 T1:新建用户态线程(`proc_id` 为所属进程,`entry_pc`/`stack_top`
/// 为用户可执行虚拟地址/用户栈顶;二者须已按 U 权限映射到该进程根表)。
pub fn spawn_user(proc_id: usize, entry_pc: usize, stack_top: usize, prio: u8) -> usize {
    let irq = arch::irq_save();
    let id = {
        let mut s = SCHED.lock();
        s.spawn_user(proc_id, entry_pc, stack_top, prio)
    };
    arch::irq_restore(irq);
    id
}

/// M2 T2b(PIP):新建**内核线程但挂到进程 `proc_id`**(proc=Some,root=
/// kernel_root)。供捐赠目标/PIP 与 IPC 压力测试使用;进程的**用户**
/// 线程仍须经 `spawn_user` 才有用户根表。
pub fn spawn_owned(entry: fn(), prio: u8, proc_id: usize) -> usize {
    let irq = arch::irq_save();
    let id = {
        let mut s = SCHED.lock();
        // 复用 spawn 内核线程路径,随后把所属进程改写为 proc_id。
        let id = s.spawn(entry, prio);
        s.threads[id].proc = Some(proc_id);
        // spawn-later 命中:若已有指向该进程的活跃捐赠,按有效优先级
        // 重排队列(新线程在 s.spawn 内已按自然优先级入队)。
        if s.donations.iter().any(|d| d.peer_proc == proc_id) {
            s.requeue_proc_threads(proc_id);
        }
        id
    };
    arch::irq_restore(irq);
    id
}

/// M2 T2b(PIP):阻塞前登记捐赠 —— donor(阻塞于 IPC,`peer_proc` 为期望
/// 对方进程)把其有效优先级捐赠给对方进程线程。调用点:ipc.rs 的 send/
/// recv NoPeer 分支(IPC 锁已释放后、block 前,无调度点)。
pub fn donate_on_block(donor_tid: usize, peer_proc: usize) {
    let irq = arch::irq_save();
    {
        let mut s = SCHED.lock();
        s.register_donation(donor_tid, peer_proc);
    }
    arch::irq_restore(irq);
}

/// M2 T2b:当前(内核)线程读取 IPC 唤醒时投递的消息,读取即清。
/// 用户线程经 frame_restore 读 TCB 帧取消息,不走本函数。
pub fn take_ipc_msg() -> Option<[usize; crate::ipc::MSG_WORDS]> {
    let irq = arch::irq_save();
    let msg = {
        let mut s = SCHED.lock();
        let id = s.current[arch::hartid()];
        s.threads[id].ipc_msg.take()
    };
    arch::irq_restore(irq);
    msg
}

/// M2 T2b(PIP):当前活跃捐赠数(测试断言用:配对完成应全部撤销)。
pub fn donation_count() -> usize {
    let irq = arch::irq_save();
    let n = {
        let s = SCHED.lock();
        s.donations.len()
    };
    arch::irq_restore(irq);
    n
}

/// 协作让出 CPU。
pub fn yield_() {
    let irq = arch::irq_save();
    // 单锁作用域:状态变更 + pick + 切换目标提取一次完成。
    let target = {
        let mut s = SCHED.lock();
        let h = arch::hartid();
        let cur = s.current[h];
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
        s.ticks_run[h] = 0;
        let next = s.pick_next(h, true);
        s.current[h] = next;
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
        let h = arch::hartid();
        let cur = s.current[h];
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
        let c = s.current[h];
        s.threads[c].state = ThreadState::Blocked;
        s.threads[c].frame_valid = false;
        s.threads[c].ctx_valid = true;
        s.ticks_run[h] = 0;
        let n = s.pick_next(h, true);
        s.current[h] = n;
        switch_target(&mut s, c, n)
    };
    // 锁外切换(中断关闭保证原子性;恢复机制见 switch_target)。
    do_switch(&target);
    // 醒来:撤销 wake 的入队并消费唤醒标志(防双调度/重入)。
    let mut s = SCHED.lock();
    let cur = s.current[arch::hartid()];
    s.remove_from_ready(cur);
    s.threads[cur].woken = false;
    drop(s);
    arch::irq_restore(irq);
}

/// 唤醒指定线程:无条件记录唤醒标志(C5 —— 唤醒可能发生在目标
/// 线程"登记之后、阻塞之前",此时其 state 尚非 Blocked,靠标志
/// 兜底,block_current 会消费它),若已阻塞则入队。
///
/// D19:线程入其**亲和核**的队列;若目标核 idle 且非本核 → 发 SBI IPI
/// 唤醒目标核 wfi 中的 idle(判定与发 IPI 同在 SCHED 锁临界区,与目标
/// 核"查空 → wfi"的临界区互斥 → 无丢失唤醒窗口)。IPI 失败仅降级为
/// 最长一个 tick 的唤醒延迟(定时器仍会唤醒 wfi 并重 pick),不破坏正确性。
pub fn wake(id: usize) {
    let irq = arch::irq_save();
    let my_hart = arch::hartid();
    let mut s = SCHED.lock();
    if id < s.threads.len() {
        s.threads[id].woken = true;
        if s.threads[id].state == ThreadState::Blocked {
            let tgt = s.threads[id].hart;
            s.enqueue(id);
            if tgt != my_hart && s.current[tgt] == s.idle[tgt] {
                static IPI_FAILED_LOGGED: AtomicBool = AtomicBool::new(false);
                let rc = crate::sbi::send_ipi(1u64 << tgt, 0);
                if rc != 0 && !IPI_FAILED_LOGGED.swap(true, Ordering::Relaxed) {
                    warn!("SBI send_ipi(hart {tgt}) failed (rc=0x{rc:x}); wakeup via timer only");
                }
            }
        }
    }
    drop(s);
    arch::irq_restore(irq);
}

/// M2 T2a:唤醒阻塞中的 IPC 线程并写入结果到其 TCB 帧。
///
/// `msg` = Some(m) 写 a1..a5(前 `m.len()` 字,≤ MSG_WORDS);None 仅写状态。
/// 统一写 `a0=0`(成功)并把 `sepc+4`(跳过 ecall)。**不置 woken**:配对
/// 由 IPC pending 队列保证(配对方经 `block_user_from_trap` 已阻塞),避免
/// 陈旧唤醒标志干扰后续 `block_current` 的消费时序。
///
/// 锁序:调用方须已持有 IPC 锁(IPC → SCHED);本函数内再取 SCHED。
pub fn ipc_wake_with_msg(tid: usize, msg: Option<&[usize]>) {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    {
        let t = &mut s.threads[tid];
        debug_assert!(
            t.state == ThreadState::Blocked,
            "ipc_wake_with_msg: tid {tid} not blocked"
        );
        t.frame[crate::arch::gpr::X_A0] = 0;
        if let Some(m) = msg {
            for (i, w) in m.iter().enumerate() {
                t.frame[crate::arch::gpr::X_A1 + i] = *w;
            }
        }
        t.frame[FRAME_SEPC] += 4;
        // M2 T2b:内核线程读不到帧 → 消息经 ipc_msg 中转(take_ipc_msg)。
        t.ipc_msg = msg.map(|m| {
            let mut buf = [0usize; crate::ipc::MSG_WORDS];
            let n = m.len().min(crate::ipc::MSG_WORDS);
            for (i, w) in m.iter().take(n).enumerate() {
                buf[i] = *w;
            }
            buf
        });
    }
    s.enqueue(tid);
    // M2 T2b(PIP):配对完成,撤销本线程此前登记的全部捐赠(peer 回落)。
    s.revoke_donations(tid);
    drop(s);
    arch::irq_restore(irq);
}

/// M2 D12:唤醒阻塞中的 IPC 线程并投递"对端已亡"错误(不配对)。
///
/// 向目标线程帧 a0 写 `code`(负 errno)并前移 sepc(跳过 ecall),使其从
/// recv/send 系统调用带错误返回(而非永久挂起);内核线程经 `ipc_msg`
/// 中转同一错误。随后撤销该线程此前登记的捐赠(其 IPC 配对已不可能完成,
/// 防陈旧捐赠抬升无主进程)。
///
/// 锁序:调用方须已释放 IPC 锁(IPC → SCHED 不重叠);本函数内再取 SCHED。
pub fn ipc_wake_with_err(tid: usize, code: usize) {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    {
        let t = &mut s.threads[tid];
        debug_assert!(
            t.state == ThreadState::Blocked,
            "ipc_wake_with_err: tid {tid} not blocked"
        );
        t.frame[crate::arch::gpr::X_A0] = code;
        t.frame[FRAME_SEPC] += 4;
        // 内核线程读不到帧 → 错误码经 ipc_msg 中转(take_ipc_msg)。
        t.ipc_msg = Some([code; crate::ipc::MSG_WORDS]);
    }
    s.enqueue(tid);
    // M2 T2b(PIP):配对已不可能完成,撤销本线程登记的全部捐赠。
    s.revoke_donations(tid);
    drop(s);
    arch::irq_restore(irq);
}

/// 当前线程 id(D19:本核的 current)。
///
/// M2 性能:IPC syscall 热路径(每次 send/recv 都查),#[inline] 配合
/// release fat-LTO 跨模块内联,省一次调用帧。
#[inline]
pub fn current_id() -> usize {
    let irq = arch::irq_save();
    let s = SCHED.lock();
    let id = s.current[arch::hartid()];
    drop(s);
    arch::irq_restore(irq);
    id
}

/// M2 T2a:当前线程所属进程 id(IPC 能力查表用)。
///
/// 仅在用户线程的 syscall/trap 上下文调用(经 CAUSE_ECALL_FROM_U 到达),
/// 当前必为用户线程;误调(内核线程) → panic(fail-loudly)。
///
/// M2 性能:syscall/trap 热路径(每次用户系统调用/故障都查),#[inline]
/// 配合 release fat-LTO 跨模块内联,省一次调用帧。
#[inline]
pub fn current_proc() -> usize {
    let irq = arch::irq_save();
    let p = {
        let s = SCHED.lock();
        s.threads[s.current[arch::hartid()]].proc
    };
    arch::irq_restore(irq);
    p.expect("current_proc: current thread is not a user thread")
}

// ===== M2 D12:用户态异常恢复(进程故障 → 杀进程) =====

/// 已杀进程数(用户态故障→杀进程;测试断言用)。
static FAULT_KILL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 当前已杀进程数。
pub fn fault_kill_count() -> usize {
    FAULT_KILL_COUNT.load(Ordering::Relaxed)
}

/// M2 D12:用户态故障 → 杀当前进程并切走(永不返回)。
///
/// 在 trap 上下文调用(trap_handler 同步异常分支、SPP=0 判定后)。顺序:
/// 1. 诊断输出(`error!`,同步异常路径允许日志;前缀 `D12:` 而非 `TRAP:` ——
///    门禁把后者当内核故障标志);
/// 2. `ipc::purge_process(pid)` 清理 IPC pending,唤醒存活的配对方并投递
///    "对端已亡"错误(IPC → SCHED 锁序);
/// 3. SCHED 锁内把进程**所有**线程置 `Exited`、恢复数据失效;非当前线程:
///    非 Running 的栈移交 reaper、槽入 free_slots(当前线程由
///    `exit_from_trap` 统一处理);**Running 于其它核的线程跳过** —— 已知
///    局限(其 TCB 归该核调度器所有;地址空间已销毁,其下一次用户访存会
///    再次走本路径自愈);同时清理指向被杀进程的陈旧捐赠;
/// 4. `mmu::switch_root(kernel_root())` —— **必须先切走再释放根表**(当前
///    satp 仍是进程根表,直接释放会使取指/访存立即故障);
/// 5. `process::destroy(pid)` 销毁进程(revoke Shm → 回收地址空间页 → 槽
///    失效);已销毁的进程(自愈路径)幂等无操作;
/// 6. `exit_from_trap()` 切换(切走时 `do_switch` 会 switch_root 到 next
///    线程根表)。
pub fn kill_current_process(scause: usize, sepc: usize, stval: usize) -> ! {
    // 1) 诊断(同步异常路径允许日志;避免字面 "TRAP:" —— 门禁误判)。
    error!("D12: user fault scause={scause:#x} sepc={sepc:#x} stval={stval:#x}; killing process");
    let pid = current_proc();
    FAULT_KILL_COUNT.fetch_add(1, Ordering::Relaxed);
    // 2) IPC 清理 + 唤醒存活配对方(IPC → SCHED 锁序)。
    crate::ipc::purge_process(pid);
    // 3) SCHED 锁内标记全部进程线程。
    {
        let irq = arch::irq_save();
        let mut s = SCHED.lock();
        let h = arch::hartid();
        let cur = s.current[h];
        // 收集进程全部线程(除当前线程 —— 由 exit_from_trap 统一处理)。
        let mut victims = [0usize; MAX_THREADS];
        let mut n = 0usize;
        for id in 0..s.threads.len() {
            if s.threads[id].proc == Some(pid) && id != cur && n < MAX_THREADS {
                victims[n] = id;
                n += 1;
            }
        }
        // 摘除被杀进程线程的就绪队列项(防槽复用后陈旧队列项误调度
        // 新线程)。
        for q in s.ready.iter_mut().flatten() {
            q.retain(|&id| !victims[..n].contains(&id));
        }
        for &id in victims.iter().take(n) {
            // Running 于其它核:该核调度器拥有其 TCB —— 跳过(已知局限)。
            if s.threads[id].state == ThreadState::Running {
                continue;
            }
            // 非当前线程:栈移交 reaper(用户线程 stack=None,安全无操作),
            // 槽入 free_slots 复用;恢复数据失效(线程不会再被选中运行)。
            if let Some(stack) = s.threads[id].stack.take() {
                s.reaper.push_back(stack);
            }
            s.threads[id].state = ThreadState::Exited;
            s.threads[id].frame_valid = false;
            s.threads[id].ctx_valid = false;
            s.free_slots.push_back(id);
        }
        // 清理指向被杀进程的陈旧捐赠(peer_proc == pid,防抬升无主进程)。
        s.revoke_donations_for_proc(pid);
        // 清理被杀进程线程作为 donor 发出的捐赠(防永久抬升其它进程)。
        // victims + 当前线程(由 exit_from_trap 标记退出)一并撤销。
        let mut killed = [0usize; MAX_THREADS];
        let mut kn = 0usize;
        for &id in victims.iter().take(n) {
            killed[kn] = id;
            kn += 1;
        }
        killed[kn] = cur;
        kn += 1;
        s.revoke_donations_of(&killed[..kn], Some(pid));
        drop(s);
        arch::irq_restore(irq);
    }
    // 4) 先切回内核根表,再释放进程根表(当前 satp 仍是进程根表)。
    crate::mmu::switch_root(crate::mmu::kernel_root());
    // 5) 销毁进程(revoke Shm → 回收地址空间页 → 槽失效)。
    crate::process::destroy(pid);
    // 6) exit_from_trap 切走(内部再取 SCHED 锁;当前线程由其统一标记退出)。
    crate::sched::exit_from_trap()
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
        let h = arch::hartid();
        let cur = s.current[h];
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
        s.ticks_run[h] = 0;
        let next = s.pick_next(h, true);
        s.current[h] = next;
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
/// 返回帧指针交给 trap/恢复路径 `sret`(恢复路径 D7 起按 sscratch 回读
/// 帧基址 +TRAP_FRAME_SIZE 重置回**当前 hart 槽顶**,见 riscv64.S),
/// 等价完成切换且不留残留。
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
        let h = arch::hartid();
        let cur = s.current[h];
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
        s.ticks_run[h] = 0;
        let next = s.pick_next(h, true);
        s.current[h] = next;
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

/// M2 T2a:从 trap 上下文阻塞当前**用户**线程(如 IPC 等待配对),永不返回。
///
/// 与 `exit_from_trap` 同源(CPU 在陷阱栈上、sscratch 指向本帧底),不同点:
/// 线程置 `Blocked` 而非退出 —— 当前帧**复制进 TCB**(配对方经
/// `ipc_wake_with_msg` 写入结果、以及抢占恢复都依赖它),随后切走。醒来时
/// 经 frame_restore sret 直接回用户态(结果已写在帧里,sepc 已前移)。
///
/// # Safety
/// `frame` 必须指向当前有效用户陷阱帧(trap_handler 传入、scause=8);
/// 仅在用户线程的 syscall 上下文调用。
pub unsafe fn block_user_from_trap(frame: *mut usize) -> ! {
    let irq = arch::irq_save();
    let target = {
        let mut s = SCHED.lock();
        let h = arch::hartid();
        let cur = s.current[h];
        // 当前帧复制进 TCB(与 on_tick 捕获同款)供恢复与配对写结果。
        s.threads[cur]
            .frame
            .copy_from_slice(unsafe { core::slice::from_raw_parts(frame, FRAME_WORDS) });
        s.threads[cur].frame_valid = true;
        // 进度在帧里,协作 ctx 失效(双机制互斥协议)。
        s.threads[cur].ctx_valid = false;
        // 无条件阻塞:消费陈旧唤醒标志(本次阻塞由 IPC pending 队列
        // 保证配对,不依赖 wake 的 woken 协议)。
        s.threads[cur].woken = false;
        s.threads[cur].state = ThreadState::Blocked;
        s.ticks_run[h] = 0;
        let next = s.pick_next(h, true);
        s.current[h] = next;
        // 与 exit_from_trap 同理:切换前重置 sscratch,防目标线程下次
        // trap 的嵌套检测(H2,riscv64.S label 4)误判为第二层而停机
        // (本帧已入 TCB,陷阱栈可复用)。
        crate::arch::set_sscratch_trap_top();
        switch_target(&mut s, cur, next)
    };
    // 锁外切换(中断关闭保证原子性)。
    do_switch(&target);
    // 醒来经 frame_restore sret 回用户态,不会到达这里;防御性停机。
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
    /// M2 T1.5:目标线程应运行的 satp 根表(do_switch 切换前启用)。
    next_root: usize,
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
        // M2 T1.5:目标线程的 satp 根表(切换前启用)。
        next_root: s.threads[next].root,
    }
}

/// 锁外切换(中断关闭保证原子性)。
fn do_switch(t: &SwitchTarget) {
    // M2 T1.5:切到目标线程的 satp 根表(与当前相同则 no-op)。
    // frame_restore/context_switch 汇编均不碰 satp,必须在此 Rust 侧
    // 切换 —— switch_root 仅 CSR 读写 + sfence,ISR 内零分配。
    crate::mmu::switch_root(t.next_root);
    if t.use_frame {
        // C2:frame_restore 内部先保存 old_ctx 再帧恢复,防上下文丢失。
        unsafe { arch::frame_restore(t.new_frame, t.old_ctx) }
    } else {
        unsafe { arch::context_switch(t.old_ctx, t.new_ctx) }
    }
}

/// 定时器 ISR 回调(中断关闭上下文):本核抢占决策,返回恢复帧。
///
/// # 参数
/// `hart`:当前运行核(trap_handler 已按 sscratch 推导,避免重复推导)。
///
/// # Safety
/// `frame` 必须是当前陷阱帧(见 trap_handler)。
pub unsafe fn on_tick(frame: *mut usize, hart: usize) -> *mut usize {
    let mut s = SCHED.lock();
    s.on_tick(frame, hart)
}

/// M2 T3b(D19):把线程 `id` 的亲和性设为 `hart`(迁移其就绪队列)。
///
/// 仅允许迁移**非运行**线程:Ready(已在队列)→ 从原核队列移除再入
/// 目标核队列;Blocked → 只改 hart(醒来时 wake 按新亲和核入队)。
/// Running/Exited 属内部错误,fail-loudly。本函数在 SCHED 锁内完成
/// 迁移与入队,目标核若 idle 由下一轮 pick/定时器自然取到(测试场景
/// 为"分配线程到各核",无需即时 IPI)。
pub fn set_affinity(id: usize, hart: usize) {
    let irq = arch::irq_save();
    let mut s = SCHED.lock();
    assert!(id < s.threads.len(), "set_affinity: thread {id} OOB");
    let h = hart.min(MAX_HARTS - 1);
    match s.threads[id].state {
        ThreadState::Ready => {
            if s.threads[id].hart != h {
                s.remove_from_ready(id);
                s.threads[id].hart = h;
                s.enqueue(id);
            }
        }
        ThreadState::Blocked => s.threads[id].hart = h,
        other => panic!("set_affinity: thread {id} is {other:?} (not migratable)"),
    }
    drop(s);
    arch::irq_restore(irq);
}

/// M2 T3b(D19):查询线程是否已阻塞(SCHED 锁内读 state)。
///
/// 测试专用:smp_sched_test 用它等副核线程**真正**进入 Blocked 后再
/// `wake` —— 保证 wake 必走 `Blocked → enqueue + IPI` 路径,确定性覆盖
/// 跨核 IPI 唤醒链路(若只等"将阻塞"标志,wake 可能先于 block_current
/// 到达而走 woken 标志路径,绕过 IPI —— T3b 首轮实测定时不定)。
pub fn is_blocked(id: usize) -> bool {
    let irq = arch::irq_save();
    let b = {
        let s = SCHED.lock();
        id < s.threads.len() && s.threads[id].state == ThreadState::Blocked
    };
    arch::irq_restore(irq);
    b
}

/// M2 T3b(D19):副核调度入口(idle 循环,永不返回)。
///
/// 由 `secondary_main` 在 `enable_timer`+`irq_enable` 后调用。循环:
/// 取本核就绪线程 → `do_switch` 运行;无就绪则 `wfi`(定时器或 IPI
/// 唤醒后重查)。**idle 不入就绪队列**(与 boot hart 的 yield 式 idle
/// 不同):`pick_next` 只会选真实就绪线程,回退恒为 `idle[hart]`,因此
/// `current[hart]` 在每次进入本循环时 == `idle[hart]`(仅当被选回时)。
pub fn secondary_idle(hart: usize) -> ! {
    loop {
        let target = {
            let mut s = SCHED.lock();
            let cur = s.current[hart];
            debug_assert!(
                cur == s.idle[hart],
                "secondary idle: current {cur} != idle {}",
                s.idle[hart]
            );
            s.ticks_run[hart] = 0;
            let next = s.pick_next(hart, true);
            if next == cur {
                // 无其他就绪线程:保持 idle,回 wfi(不切换)。
                drop(s);
                None
            } else {
                s.current[hart] = next;
                Some(switch_target(&mut s, cur, next))
            }
        };
        if let Some(t) = target {
            // 锁外切换(中断关闭保证原子性)。切到真实线程;当该线程
            // yield/block/exit 且本核无其他就绪时,pick 回退选回 idle,
            // context_switch 恢复此处 → 继续循环。
            do_switch(&t);
        }
        arch::wait_for_interrupt();
    }
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

    // 6) M2 T2a(D22)回归:被唤醒的 HIGH 线程抢占忙循环的 LOW 线程。
    // 场景:LOW 忙循环不 yield、第 3 tick 起 wake(HIGH 线程 H;H 先前
    // block_current 阻塞,ctx_valid 有效、frame_valid 失效)。D22 生效时
    // on_tick 把 H 的 ctx 展开为帧抢占运行 —— H 的记录 tick < L 的完成
    // tick;否则 H 只可达于协作路径,断言失败。
    // 关键:阶段 2 的等待必须**纯自旋不 yield** —— 若 main 协作 yield,
    // 就绪队列中的 H(仅 ctx_valid)可被协作切换选中提前运行,掩盖 D22
    // 缺陷(假阳性);自旋下 H 只能经 on_tick 抢占路径被选中。
    D22_H_BLOCKED.store(false, Ordering::Relaxed);
    D22_H_RAN.store(false, Ordering::Relaxed);
    D22_LOW_DONE.store(false, Ordering::Relaxed);
    let h_id = spawn(test_d22_high, PRIO_HIGH);
    D22_H_ID.store(h_id, Ordering::Relaxed);
    // 阶段 1:等 H 阻塞。可 yield —— H 尚为 Blocked,协作切换不会误选它。
    guard = 0;
    while !D22_H_BLOCKED.load(Ordering::Relaxed) && guard < 200_000 {
        yield_();
        guard += 1;
    }
    if !D22_H_BLOCKED.load(Ordering::Relaxed) {
        return Err("D22: high thread did not block");
    }
    spawn(test_d22_low, PRIO_LOW);
    // 阶段 2:纯自旋不 yield。**先显式开中断**:本上下文(idle)在自检期
    // SIE 恒为 0(每次 yield 恢复到引导期保存值)—— 不开中断则定时器
    // 永不触发,main 自旋独占 CPU,L 无法运行,死等超时。开中断后:
    // 定时器抢占 main → LOW 运行 → 第 3 tick wake(H)→ 下一 tick
    // on_tick 抢占到 H(expand_ctx_to_frame)。main 全程不 yield,排除
    // 协作路径选中 H 的假阳性(见第 6 项头注释)。
    crate::arch::irq_enable();
    let t6 = crate::logger::tick();
    while (!D22_H_RAN.load(Ordering::Relaxed) || !D22_LOW_DONE.load(Ordering::Relaxed))
        && crate::logger::tick().wrapping_sub(t6) < 100
    {
        core::hint::spin_loop();
    }
    if !D22_H_RAN.load(Ordering::Relaxed) || !D22_LOW_DONE.load(Ordering::Relaxed) {
        return Err("D22: woken thread stranded (no preemption path)");
    }
    // H 的恢复 tick 必须早于 L 的完成 tick(经 on_tick 抢占运行,非协作)。
    let h_start = D22_H_TICK.load(Ordering::Relaxed);
    let l_done = D22_LOW_DONE_TICK.load(Ordering::Relaxed);
    if h_start >= l_done {
        return Err("D22: woken thread did not preempt busy loop");
    }
    info!("M2 T2a: woken-thread preemption ok (D22)");

    // 7) M2 T2b(PIP)回归:优先级继承。场景(确定性反转):
    // - H(spawn_owned→H_proc,HIGH):recv → NoPeer(捐赠 H→L_proc 注册)→
    //   block_current;被 L 唤醒后 take_ipc_msg 校验消息。
    // - L(spawn_owned→L_proc,LOW):忙循环 3 tick 后 send → 配对 H。
    // - M(spawn,MED):忙循环 50 tick —— 制造中间优先级,饿死未抬升的 LOW。
    // PIP 生效:L 的线程临时继承 H 的优先级(HIGH)先于 M 运行,完成配对、
    // 唤醒 H(H 完成 tick < M 完成 tick);否则 L 滞留 LOW 被 M 饿死,H 永不
    // 完成 → 超时失败。阶段 2 纯自旋不 yield(同第 6 项:排除协作路径假阳性)。
    let h_proc = crate::process::create().expect("PIP: create H proc");
    let l_proc = crate::process::create().expect("PIP: create L proc");
    crate::process::grant_cap(h_proc, 0, l_proc).expect("PIP: H cap");
    crate::process::grant_cap(l_proc, 0, h_proc).expect("PIP: L cap");
    PIP_H_PROC.store(h_proc, Ordering::Relaxed);
    PIP_L_PROC.store(l_proc, Ordering::Relaxed);
    PIP_H_BLOCKED.store(false, Ordering::Relaxed);
    PIP_H_MSG_OK.store(false, Ordering::Relaxed);
    PIP_H_DONE.store(false, Ordering::Relaxed);
    PIP_L_DONE.store(false, Ordering::Relaxed);
    PIP_M_DONE.store(false, Ordering::Relaxed);
    spawn_owned(test_pip_high, PRIO_HIGH, h_proc);
    // 阶段 1:等 H 阻塞(捐赠已注册,但 L/M 尚未 spawn)。
    guard = 0;
    while !PIP_H_BLOCKED.load(Ordering::Relaxed) && guard < 200_000 {
        yield_();
        guard += 1;
    }
    if !PIP_H_BLOCKED.load(Ordering::Relaxed) {
        return Err("PIP: high thread did not block");
    }
    spawn_owned(test_pip_low, PRIO_LOW, l_proc);
    spawn(test_pip_med, PRIO_MED);
    // 阶段 2:纯自旋不 yield(机制同第 6 项;本上下文 idle 在自检期 SIE=0,
    // 须显式开中断让定时器推进,否则自旋独占 CPU,L/M 永不运行)。
    crate::arch::irq_enable();
    let t7 = crate::logger::tick();
    while (!PIP_H_DONE.load(Ordering::Relaxed)
        || !PIP_L_DONE.load(Ordering::Relaxed)
        || !PIP_M_DONE.load(Ordering::Relaxed))
        && crate::logger::tick().wrapping_sub(t7) < PIP_TIMEOUT_TICKS
    {
        core::hint::spin_loop();
    }
    if !PIP_H_DONE.load(Ordering::Relaxed)
        || !PIP_L_DONE.load(Ordering::Relaxed)
        || !PIP_M_DONE.load(Ordering::Relaxed)
    {
        return Err("PIP: timeout (priority inversion?)");
    }
    if !PIP_H_MSG_OK.load(Ordering::Relaxed) {
        return Err("PIP: message integrity");
    }
    let pip_h_tick = PIP_H_TICK.load(Ordering::Relaxed);
    let pip_m_done = PIP_M_DONE_TICK.load(Ordering::Relaxed);
    if pip_h_tick >= pip_m_done {
        return Err("PIP: low holder did not run before med (no inheritance)");
    }
    if donation_count() != 0 {
        return Err("PIP: donations not drained after pairing");
    }
    info!("M2 T2b: priority inheritance ok (PIP)");

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

/// M2 T2a(D22)回归:被唤醒的 HIGH 线程。
/// 记录阻塞标记后立即 `block_current`(ctx_valid 保持、frame_valid 失效)。
/// 被 LOW 唤醒后仅当 on_tick 选中它(expand_ctx_to_frame 展开 ctx → sret)
/// 才会运行;恢复点在 block_current 之后,此刻记录恢复 tick 并置 RAN。
fn test_d22_high() {
    D22_H_BLOCKED.store(true, Ordering::Relaxed);
    block_current();
    D22_H_TICK.store(crate::logger::tick() as usize, Ordering::Relaxed);
    D22_H_RAN.store(true, Ordering::Relaxed);
}

/// M2 T2a(D22)回归:LOW 忙循环线程。
/// 忙循环 D22_LOW_TICKS 个 tick(不 yield),第 D22_WAKE_AT 个 tick 起
/// wake(HIGH 线程 H)。完成时记录完成 tick。若 D22 生效,H 在 L 忙循环
/// 期间经 on_tick 抢占运行(H 恢复 tick < L 完成 tick)。
fn test_d22_low() {
    let s0 = crate::logger::tick();
    let mut woke = false;
    while crate::logger::tick().wrapping_sub(s0) < D22_LOW_TICKS {
        if !woke && crate::logger::tick().wrapping_sub(s0) >= D22_WAKE_AT {
            wake(D22_H_ID.load(Ordering::Relaxed));
            woke = true;
        }
        D22_LOW.fetch_add(1, Ordering::Relaxed); // 忙循环计数(防优化掉循环体)
    }
    D22_LOW_DONE_TICK.store(crate::logger::tick() as usize, Ordering::Relaxed);
    D22_LOW_DONE.store(true, Ordering::Relaxed);
}

/// M2 T2b(PIP)回归:HIGH 接收方线程。
/// 先 recv → NoPeer(登记捐赠 H→L_proc)→ 置已阻塞标志 → block_current。
/// 被 L 唤醒后经 `take_ipc_msg` 取消息(内核线程路径,读帧不可见)、校验,
/// 记录完成 tick 与完整性标志。
fn test_pip_high() {
    let pid = PIP_H_PROC.load(Ordering::Relaxed);
    match crate::ipc::recv(pid, 0) {
        Ok(crate::ipc::RecvBlock::NoPeer) => {}
        other => panic!("PIP: H recv unexpected {other:?}"),
    }
    PIP_H_BLOCKED.store(true, Ordering::Relaxed);
    block_current();
    let m = take_ipc_msg().expect("PIP: H no ipc_msg");
    if m[0] == PIP_MAGIC {
        PIP_H_MSG_OK.store(true, Ordering::Relaxed);
    }
    PIP_H_TICK.store(crate::logger::tick() as usize, Ordering::Relaxed);
    PIP_H_DONE.store(true, Ordering::Relaxed);
}

/// M2 T2b(PIP)回归:LOW 持资源方线程(被捐赠抬升到 HIGH)。
/// 忙循环 3 tick(制造被 M 饿死的时间窗)后 send → 配对 H 的 pending
/// recv → Done。send 完即完成(配对即唤醒 H,捐赠在唤醒时撤销)。
fn test_pip_low() {
    let pid = PIP_L_PROC.load(Ordering::Relaxed);
    let s0 = crate::logger::tick();
    while crate::logger::tick().wrapping_sub(s0) < PIP_L_BUSY_TICKS {
        PIP_L_BUSY.fetch_add(1, Ordering::Relaxed); // 忙循环(防优化掉)
    }
    match crate::ipc::send(pid, 0, [PIP_MAGIC, 0, 0, 0, 0]) {
        Ok(crate::ipc::SendBlock::Done) => {}
        other => panic!("PIP: L send unexpected {other:?}"),
    }
    PIP_L_DONE.store(true, Ordering::Relaxed);
}

/// M2 T2b(PIP)回归:MED 忙循环线程 —— 制造优先级反转场景(无 PIP 时
/// 饿死 LOW 持资源方)。运行 PIP_M_TICKS 个 tick 后记录完成 tick。
fn test_pip_med() {
    let s0 = crate::logger::tick();
    PIP_M_START.store(s0 as usize, Ordering::Relaxed);
    while crate::logger::tick().wrapping_sub(s0) < PIP_M_TICKS {
        PIP_M_BUSY.fetch_add(1, Ordering::Relaxed); // 忙循环(防优化掉)
    }
    PIP_M_DONE_TICK.store(crate::logger::tick() as usize, Ordering::Relaxed);
    PIP_M_DONE.store(true, Ordering::Relaxed);
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

// ===== M2 T2a(D22)回归参数与标志 =====

/// LOW 忙循环时长(tick)。
const D22_LOW_TICKS: u64 = 20;
/// LOW 启动后第几个 tick 起唤醒 HIGH。
const D22_WAKE_AT: u64 = 3;
/// 忙循环计数(观察量,防循环体被优化掉)。
static D22_LOW: AtomicUsize = AtomicUsize::new(0);
static D22_H_ID: AtomicUsize = AtomicUsize::new(0);
static D22_H_BLOCKED: AtomicBool = AtomicBool::new(false);
static D22_H_RAN: AtomicBool = AtomicBool::new(false);
static D22_H_TICK: AtomicUsize = AtomicUsize::new(0);
static D22_LOW_DONE: AtomicBool = AtomicBool::new(false);
static D22_LOW_DONE_TICK: AtomicUsize = AtomicUsize::new(0);

// ===== M2 T2b(PIP)回归参数与标志 =====

/// 消息魔数(H 校验收到的消息)。
const PIP_MAGIC: usize = 0x5050_494d; // "PIPM"
/// LOW 持资源方忙循环时长(tick),之后 send 配对。
const PIP_L_BUSY_TICKS: u64 = 3;
/// MED 忙循环时长(tick) —— 制造中间优先级饿死 LOW 的时间窗。
const PIP_M_TICKS: u64 = 50;
/// 阶段 2 纯自旋等待超时(tick;M 跑满 50 + L 3 + H 1 + 调度开销)。
const PIP_TIMEOUT_TICKS: u64 = 300;
static PIP_H_PROC: AtomicUsize = AtomicUsize::new(0);
static PIP_L_PROC: AtomicUsize = AtomicUsize::new(0);
static PIP_H_BLOCKED: AtomicBool = AtomicBool::new(false);
static PIP_H_MSG_OK: AtomicBool = AtomicBool::new(false);
static PIP_H_DONE: AtomicBool = AtomicBool::new(false);
static PIP_H_TICK: AtomicUsize = AtomicUsize::new(0);
static PIP_L_BUSY: AtomicUsize = AtomicUsize::new(0);
static PIP_L_DONE: AtomicBool = AtomicBool::new(false);
static PIP_M_START: AtomicUsize = AtomicUsize::new(0);
static PIP_M_BUSY: AtomicUsize = AtomicUsize::new(0);
static PIP_M_DONE: AtomicBool = AtomicBool::new(false);
static PIP_M_DONE_TICK: AtomicUsize = AtomicUsize::new(0);

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
