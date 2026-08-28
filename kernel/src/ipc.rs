//! 同步 IPC(M2 T2a):寄存器消息 + 阻塞配对 + 简化能力表授权。
//!
//! # ABI(M2-DESIGN §4.1 / L1)
//! - `syscall 3 = ipc_send(a0=cap slot, a1..a5=消息 5 字)`;成功返回 a0=0;
//!   无配对则**阻塞**(配对方送达后由 sched 写回结果,见 §原子性)。
//! - `syscall 4 = ipc_recv(a0=cap slot)`;成功返回 a0=0、a1..a5=消息;
//!   无配对则阻塞。
//! - 错误以负 errno 返回(不阻塞):`-EINVAL`(槽越界)、`-EACCES`(未授权/
//!   空槽)。成功状态统一 `a0=0`,阻塞线程醒来后 sepc 已被配对方前移。
//!
//! # 配对语义
//! `PendingSend`/`PendingRecv` 两个队列配对匹配条件:
//! - send(pid, slot, msg) 查 recvs: `recver_pid == dst && src_pid == pid`;
//! - recv(pid, slot) 查 sends: `sender_pid == src && dst_pid == pid`。
//!
//! 双方都不就绪时各自挂起;先到者入队,后到者命中即出队并唤醒对方。
//! 每进程单线程(T2a)下自配对不会发生 —— 自发自收会阻塞(见报告遗留风险)。
//!
//! # 锁序契约(TABLE → IPC → SCHED,全程不逆序)
//! - `process::cap_target`(TABLE)在进入本模块前已释放;
//! - 本模块只持有 IPC 锁;命中配对时**先释放 IPC 锁再取 SCHED 锁**
//!   (`sched::ipc_wake_with_msg`),锁不重叠;
//! - IPC 锁只在 trap/syscall 或引导期上下文获取,**绝不在 ISR 获取**。
//!
//! # 原子性
//! 返回 `NoPeer`(已登记 pending)与调用方(`syscall::handle`)经
//! `block_user_from_trap` 阻塞之间**无调度点**:单核、trap 上下文中断关闭,
//! 期间无其他线程运行,登记不丢、配对不丢。

use alloc::collections::VecDeque;

use crate::process::CapError;
use crate::sync::SpinLock;

/// 寄存器消息字数(经 a1..a5 传输)。
pub const MSG_WORDS: usize = 5;

/// `-EINVAL`(槽越界)以 usize 表示。
pub const IPC_ERR_EINVAL: usize = usize::MAX;
/// `-EACCES`(未授权/空槽)以 usize 表示。
pub const IPC_ERR_EACCES: usize = usize::MAX - 1;

/// send 结果:配对成功 / 暂无可配 recv(调用方阻塞)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendBlock {
    /// 已与匹配的 recv 配对,消息已投递。
    Done,
    /// 暂无匹配 recv,已登记 pending —— 调用方应阻塞等待配对。
    NoPeer,
}

/// recv 结果:配对成功(带回消息)/ 暂无可配 send(调用方阻塞)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvBlock {
    /// 已与匹配的 send 配对,携带来消息。
    Done([usize; MSG_WORDS]),
    /// 暂无匹配 send,已登记 pending —— 调用方应阻塞等待配对。
    NoPeer,
}

/// 挂起的 send(等配对的 recv)。
struct PendingSend {
    /// 发送线程(sched 唤醒目标)。
    sender_tid: usize,
    /// 发送者所属进程。
    sender_pid: usize,
    /// 目标进程(recv 方 cap 指向的进程)。
    dst_pid: usize,
    /// 消息负载(5 字)。
    msg: [usize; MSG_WORDS],
}

/// 挂起的 recv(等配对的 send)。
struct PendingRecv {
    /// 接收线程(sched 唤醒目标)。
    recver_tid: usize,
    /// 接收者所属进程。
    recver_pid: usize,
    /// 期望的发送方进程(send 方 cap 指向的进程)。
    src_pid: usize,
}

/// IPC 状态:两个 pending 队列(FIFO,先到先配)。
struct IpcState {
    sends: VecDeque<PendingSend>,
    recvs: VecDeque<PendingRecv>,
}

/// IPC 表单例(锁序:TABLE → IPC → SCHED,见模块头)。
static IPC: SpinLock<IpcState> = SpinLock::new(IpcState {
    sends: VecDeque::new(),
    recvs: VecDeque::new(),
});

/// 能力错误 → 负 errno(usize 编码,与 ABI 一致)。
fn cap_err_code(err: CapError) -> usize {
    match err {
        CapError::InvalidSlot => IPC_ERR_EINVAL,
        CapError::NotFound => IPC_ERR_EACCES,
    }
}

/// 发送消息到 `slot` 指向的目标进程。
///
/// 成功(配对)返回 `Done`;暂无匹配 recv 返回 `NoPeer`(已登记,调用方须
/// 立即阻塞 —— 见模块头「原子性」);能力校验失败返回负 errno(不阻塞)。
pub fn send(pid: usize, slot: usize, msg: [usize; MSG_WORDS]) -> Result<SendBlock, usize> {
    // 1) 能力解析(TABLE 锁,已释放)。失败即返回,不登记、不阻塞。
    let dst = match crate::process::cap_target(pid, slot) {
        Ok(d) => d,
        Err(e) => return Err(cap_err_code(e)),
    };
    // 2) 取当前线程 id(SCHED 锁短暂获取、已释放)再取 IPC 锁 ——
    //    保持 SCHED → IPC 顺序,不与「IPC → SCHED」唤醒路径交叉重叠。
    let tid = crate::sched::current_id();
    let mut ipc = IPC.lock();
    // 3) 查已挂起的 recv:recver 的目标进程 == 本进程,期望发送方 == 本进程。
    if let Some(pos) = ipc
        .recvs
        .iter()
        .position(|r| r.recver_pid == dst && r.src_pid == pid)
    {
        let r = ipc.recvs.remove(pos).expect("position 已确认");
        drop(ipc); // 先释放 IPC,再取 SCHED 唤醒(锁不重叠)。
        crate::sched::ipc_wake_with_msg(r.recver_tid, Some(&msg));
        return Ok(SendBlock::Done);
    }
    // 4) 无匹配:登记 pending send,返回 NoPeer(调用方随即阻塞)。
    ipc.sends.push_back(PendingSend {
        sender_tid: tid,
        sender_pid: pid,
        dst_pid: dst,
        msg,
    });
    drop(ipc);
    Ok(SendBlock::NoPeer)
}

/// 从 `slot` 指向的目标进程接收消息。
///
/// 成功(配对)返回 `Done(msg)`;暂无匹配 send 返回 `NoPeer`(已登记,调用方
/// 须立即阻塞);能力校验失败返回负 errno(不阻塞)。
pub fn recv(pid: usize, slot: usize) -> Result<RecvBlock, usize> {
    // 1) 能力解析(TABLE 锁,已释放)。
    let src = match crate::process::cap_target(pid, slot) {
        Ok(s) => s,
        Err(e) => return Err(cap_err_code(e)),
    };
    // 2) 同 send:先取 tid 再取 IPC 锁。
    let tid = crate::sched::current_id();
    let mut ipc = IPC.lock();
    // 3) 查已挂起的 send:发送方进程 == 本进程期望的 src,目标进程 == 本进程。
    if let Some(pos) = ipc
        .sends
        .iter()
        .position(|s| s.sender_pid == src && s.dst_pid == pid)
    {
        let s = ipc.sends.remove(pos).expect("position 已确认");
        drop(ipc); // 先释放 IPC,再取 SCHED 唤醒(锁不重叠)。
                   // 仅 ack 唤醒发送方(消息已被本函数带走给调用方)。
        crate::sched::ipc_wake_with_msg(s.sender_tid, None);
        return Ok(RecvBlock::Done(s.msg));
    }
    // 4) 无匹配:登记 pending recv,返回 NoPeer。
    ipc.recvs.push_back(PendingRecv {
        recver_tid: tid,
        recver_pid: pid,
        src_pid: src,
    });
    drop(ipc);
    Ok(RecvBlock::NoPeer)
}
