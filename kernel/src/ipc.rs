//! 同步 IPC(M2 T2a/T2b):寄存器消息 + 阻塞配对 + 简化能力表授权 +
//! 优先级继承(PIP)。
//!
//! # ABI(M2-DESIGN §4.1 / L1)
//! - `syscall 3 = ipc_send(a0=cap slot, a1..a5=消息 5 字)`;成功返回 a0=0;
//!   无配对则**阻塞**(配对方送达后由 sched 写回结果,见 §原子性)。
//! - `syscall 4 = ipc_recv(a0=cap slot)`;成功返回 a0=0、a1..a5=消息;
//!   无配对则阻塞。
//! - 错误以负 errno 返回(不阻塞):`-EINVAL`(槽越界)、`-EACCES`(未授权/
//!   空槽 **send**)。成功状态统一 `a0=0`,阻塞线程醒来后 sepc 已被配对方前移。
//!
//! # 配对语义
//! `PendingSend`/`PendingRecv` 两个队列配对匹配条件:
//! - send(pid, slot, msg) 查 recvs: `recver_pid == dst && src_pid == pid`,
//!   **M3-2 增 accept-any 兜底**:`recver_pid == dst && src_pid == IPC_ACCEPT_ANY`
//!   (specific 优先,send 侧只作唯一入口);
//! - recv(pid, slot) 查 sends: `sender_pid == src && dst_pid == pid`。
//!
//! # M3-2 accept-any(空槽 recv = 监听)
//! `recv` 空槽由 -EACCES 改为 **接受任何能向本进程 send 的进程**(阻塞
//! 监听):服务端在客户端 `service_connect` 之前即可挂起,客户端 connect
//! 双向授予后 send,经 send 侧兜底命中。安全性:能 send 者必持
//! `Cap::Proc(本进程)`(仅 connect 或引导期授予可得)→ 接收方只收自己
//! 已授权的发送方,能力模型不破。具体见 `IPC_ACCEPT_ANY` 与 `recv` 注释。
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
//!
//! # 优先级继承(PIP,M2 T2b)
//! NoPeer 分支阻塞前经 `sched::donate_on_block` 登记捐赠:发送方把期望
//! 的接收方进程抬到自身有效优先级(接收方对称)。被捐赠进程的线程得以
//! 抢占中间优先级忙循环、完成配对;配对完成(wake)时 `ipc_wake_with_msg`
//! 撤销捐赠。锁序不变:捐赠注册在 IPC 锁释放后、阻塞前(无调度点)。

use alloc::collections::VecDeque;

use crate::process::Cap;
use crate::sync::SpinLock;

/// 寄存器消息字数(经 a1..a5 传输)。
pub const MSG_WORDS: usize = 5;

/// `-EACCES`(未授权/空槽)以 usize 表示(single source:syscall.rs)。
pub const IPC_ERR_EACCES: usize = crate::syscall::SYS_ERR_EACCES;
// T3c:`-EINVAL`(槽越界 / Cap::Shm 类型错误)由 `process::cap_errno`
// (InvalidSlot/WrongType → SYS_ERR_EINVAL)统一编码,不再单列别名。

/// M3-2 accept-any 哨兵:`PendingRecv.src_pid = IPC_ACCEPT_ANY` 表示"接受
/// 任何能向我 `send` 的进程"(空槽 `ipc_recv` 的监听语义,见 `recv`)。
///
/// 值取 `usize::MAX`:真实进程 id 从 1 起(0 保留),永不与之碰撞,且
/// `purge_process` 的 `r.src_pid == pid` 对哨兵恒假 → accept-any 挂起不随
/// 某个客户端消亡被清(服务端继续等待下一个)。
pub const IPC_ACCEPT_ANY: usize = usize::MAX;

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

/// 发送消息到 `slot` 指向的目标进程。
///
/// 成功(配对)返回 `Done`;暂无匹配 recv 返回 `NoPeer`(已登记,调用方须
/// 立即阻塞 —— 见模块头「原子性」);能力校验失败返回负 errno(不阻塞)。
/// 目标解析要求 `Cap::Proc`;`Cap::Shm`(共享页能力)对 IPC 是类型错误 →
/// `-EINVAL`(M2 T3c:cap_target 现返回 `Cap` 枚举)。
pub fn send(pid: usize, slot: usize, msg: [usize; MSG_WORDS]) -> Result<SendBlock, usize> {
    // 1) 能力解析(TABLE 锁,已释放)。失败即返回,不登记、不阻塞。
    let dst = match crate::process::cap_target(pid, slot) {
        Ok(Cap::Proc(d)) => d,
        // Cap::Shm 对 IPC 是类型错误 → -EINVAL(经 cap_errno(WrongType) 编码)。
        Ok(Cap::Shm(_)) => {
            return Err(crate::process::cap_errno(
                crate::process::CapError::WrongType,
            ))
        }
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    // 2) 取当前线程 id(SCHED 锁短暂获取、已释放)再取 IPC 锁 ——
    //    保持 SCHED → IPC 顺序,不与「IPC → SCHED」唤醒路径交叉重叠。
    let tid = crate::sched::current_id();
    let mut ipc = IPC.lock();
    // 3) 查已挂起的 recv:recver 的目标进程 == 本进程,期望发送方 == 本进程
    //    (M3-2:再查 accept-any 挂起 `src_pid == IPC_ACCEPT_ANY` —— 服务端
    //    空槽监听,接受任何能向我 send 的进程)。**specific 优先**:同名目标
    //    下先满足明确指定 src 的 recv,accept-any 仅作兜底,防"通配抢配
    //    特定接收者消息"(accept-any 收到的 send 方必持 `Cap::Proc(dst)`,
    //    即已被接收方授权,故不破能力模型)。
    let pos = ipc
        .recvs
        .iter()
        .position(|r| r.recver_pid == dst && r.src_pid == pid)
        .or_else(|| {
            ipc.recvs
                .iter()
                .position(|r| r.recver_pid == dst && r.src_pid == IPC_ACCEPT_ANY)
        });
    if let Some(pos) = pos {
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
    // M2 T2b(PIP):阻塞前登记捐赠 —— 把期望的接收方进程 `dst` 的所有
    // 线程抬到本发送方有效优先级,使其能抢占中间优先级忙循环来 recv
    // 配对(否则低优先接收方被饿死 → 本发送方永久阻塞,优先级反转)。
    // 在 IPC 锁释放后、调用方阻塞前调用:无调度点,原子性保持。
    crate::sched::donate_on_block(tid, dst);
    Ok(SendBlock::NoPeer)
}

/// M2 D12:进程被杀时清理 IPC 状态。
///
/// 移除 pending 队列中所有 `sender_pid == pid || recver_pid == pid` 项
/// (被杀进程自己的挂起 send/recv —— 其线程已被标记退出,不再唤醒);对
/// **存活配对方**唤醒并投递"对端已亡"错误(`-ENOENT`,目标进程不存在),
/// 防止其永久挂起:
/// - `dst_pid == pid` 的 pending send 之 sender 线程(等 pid 收消息);
/// - `src_pid == pid` 的 pending recv 之 recver 线程(等 pid 发消息)。
///
/// 锁序:本函数持 IPC 锁收集,释放后经 `sched::ipc_wake_with_err` 取 SCHED
/// 锁(IPC → SCHED,不逆序)。
pub fn purge_process(pid: usize) {
    let mut wake_err: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    let mut ipc = IPC.lock();
    // 1) 等 pid 发消息的存活 recver:唤醒并报"对端已亡"。
    ipc.recvs.retain(|r| {
        if r.src_pid == pid {
            if r.recver_pid != pid {
                wake_err.push((r.recver_tid, crate::syscall::SYS_ERR_ENOENT));
            }
            false
        } else {
            true
        }
    });
    // 2) 等 pid 收消息的存活 sender:唤醒并报错。
    ipc.sends.retain(|s| {
        if s.dst_pid == pid {
            if s.sender_pid != pid {
                wake_err.push((s.sender_tid, crate::syscall::SYS_ERR_ENOENT));
            }
            false
        } else {
            true
        }
    });
    // 3) 杀掉进程自己的挂起 send(发给其它进程但尚未配对)。
    ipc.sends.retain(|s| s.sender_pid != pid);
    // M3-2:杀掉进程自己的挂起 recv(线程已亡,配对永不可能)—— 含空槽
    // accept-any 监听。否则 pid 复用后陈旧 recv 会错误命中新进程的 send
    // (消息投给死线程、发送方误报 Done;recver 即被杀进程,无需唤醒)。
    ipc.recvs.retain(|r| r.recver_pid != pid);
    drop(ipc); // 释放 IPC 锁后再唤醒(IPC → SCHED 不重叠)。
    for (tid, code) in wake_err {
        crate::sched::ipc_wake_with_err(tid, code);
    }
}

/// 从 `slot` 指向的目标进程接收消息。
///
/// 成功(配对)返回 `Done(msg)`;暂无匹配 send 返回 `NoPeer`(已登记,调用方
/// 须立即阻塞);能力校验失败返回负 errno(不阻塞)。
pub fn recv(pid: usize, slot: usize) -> Result<RecvBlock, usize> {
    // 1) 能力解析(TABLE 锁,已释放);要求 Cap::Proc(共享页能力对 IPC
    //    是类型错误 → -EINVAL)。
    let src = match crate::process::cap_target(pid, slot) {
        Ok(Cap::Proc(s)) => s,
        // Cap::Shm 对 IPC 是类型错误 → -EINVAL(经 cap_errno(WrongType) 编码)。
        Ok(Cap::Shm(_)) => {
            return Err(crate::process::cap_errno(
                crate::process::CapError::WrongType,
            ))
        }
        // M3-2 accept-any:**空槽 recv = 监听/accept**(而非 -EACCES)——
        // 服务端在客户端 `service_connect` 之前即可阻塞等待,消息到达由
        // `send` 的 accept-any 兜底匹配(`src_pid == IPC_ACCEPT_ANY`)。
        // 安全性:能向本进程 send 者必持 `Cap::Proc(本进程)`(connect 或
        // 引导期授予) —— 接收方只收"自己已授权"的发送方,能力模型不破。
        // 槽越界仍 `InvalidSlot → -EINVAL`,不吞。
        Err(crate::process::CapError::NotFound) => IPC_ACCEPT_ANY,
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    // 2) 同 send:先取 tid 再取 IPC 锁。
    let tid = crate::sched::current_id();
    let mut ipc = IPC.lock();
    // 3) 查已挂起的 send:发送方进程 == 本进程期望的 src,目标进程 == 本进程。
    //    (accept-any `src == IPC_ACCEPT_ANY` 与任何真实 sender_pid 都不等 →
    //    永不消费既有特定 send,只登记监听,由 `send` 侧兜底命中。)
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
    // M2 T2b(PIP):对称捐赠 —— 把期望的发送方进程 `src` 的线程抬到本
    // 接收方有效优先级,使其能抢占中间优先级忙循环来 send 配对。
    // M3-2:accept-any 无特定期望发送方(捐赠目标未知),跳过 —— 客户端
    // connect 双向授予后再由**特定 recv** 生效捐赠。
    if src != IPC_ACCEPT_ANY {
        crate::sched::donate_on_block(tid, src);
    }
    Ok(RecvBlock::NoPeer)
}
