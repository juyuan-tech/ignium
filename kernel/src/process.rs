//! 进程与每进程独立地址空间(M2 T1.5)。
//!
//! # 设计
//! - **进程** = 一组线程 + 独立 Sv39 地址空间(独立 satp 根表)。
//!   内核线程(调度器普通 `spawn`)不属任何进程,运行于内核根表;
//!   用户线程经 `sched::spawn_user(pid, ...)` 挂到本进程。
//! - **地址空间**:进程根表复制内核驻留区(S 权限,U=0,见
//!   `mmu::create_user_root`);用户页由调用方按 U 权限映射到用户
//!   VA 区(L2 索引 1,0x4000_0000 段)。
//! - **切换**:调度器在切换线程时按线程所属进程切换 satp(见 sched.rs
//!   的 `do_switch`/`on_tick` 调用 `mmu::switch_root`)。
//! - **能力表(M2 T2a)**:每进程 `MAX_CAPS` 个能力槽,槽 → 目标进程 pid;
//!   空槽 = 未授权。`grant_cap`/`cap_target` 为 IPC 提供目标解析与授权
//!   校验(未授权 → `CapError`)。销毁/页回收仍留待后续里程碑。
//! - **锁序契约**:能力表(TABLE)先于 IPC 锁获取,绝不逆序;调用方
//!   (syscall/引导测试)经 `cap_target` 读表后释放,再取 IPC 锁。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// 最大进程数(容量预留,与 `MAX_THREADS` 对齐的纪律)。
pub const MAX_PROCESSES: usize = 16;

/// 每进程能力槽数(简化能力表,M2 T2a)。
///
/// 数组定长以保持 `Process` Copy(槽位 → 目标进程 pid)。槽 0 为
/// 测试/服务约定入口,其余槽由授权方自行约定。
pub const MAX_CAPS: usize = 8;

/// 进程:独立地址空间根表 + 简化能力表。
///
/// `id` 与槽索引一致(pid = 槽位);能力表为**目标进程 pid** 数组
/// (槽 → 目标进程;空槽 = 未授权)。T2 销毁/页回收将依赖本字段定位。
struct Process {
    id: usize,
    root: usize,
    /// 能力槽数组:槽位 → 目标进程 pid;None = 未授权(空槽)。
    caps: [Option<usize>; MAX_CAPS],
}

/// 进程表(IRQ 安全 SpinLock;进程表不进入 ISR 路径)。
struct ProcessTable {
    slots: Vec<Process>,
    /// 已销毁进程的槽位(FIFO 复用;本里程碑无销毁,恒空)。
    free: VecDeque<usize>,
}

/// 进程表单例。
static TABLE: SpinLock<ProcessTable> = SpinLock::new(ProcessTable {
    slots: Vec::new(),
    free: VecDeque::new(),
});

/// 创建新进程:分配独立 Sv39 根表(含内核区映射),返回进程 id。
///
/// 建表含页表分配/清零/写页表,调用期间保持中断关闭(与调度器
/// 公开 API 同一纪律);失败返回 Err(地址空间分配失败/表满)。
pub fn create() -> Result<usize, ()> {
    let irq = crate::arch::irq_save();
    let mut t = TABLE.lock();
    if t.slots.len() >= MAX_PROCESSES {
        drop(t);
        crate::arch::irq_restore(irq);
        return Err(());
    }
    // 建独立地址空间根表(含内核区映射)。失败则不落表。
    let root = match crate::mmu::create_user_root() {
        Ok(r) => r,
        Err(_) => {
            drop(t);
            crate::arch::irq_restore(irq);
            return Err(());
        }
    };
    // M2 T1.5:pid = 槽索引;free 复用(本里程碑恒空)。
    // M2 T2a:新进程能力表全空(未授权,须显式 grant_cap)。
    let pid = match t.free.pop_front() {
        Some(idx) => {
            t.slots[idx] = Process {
                id: idx,
                root,
                caps: [None; MAX_CAPS],
            };
            idx
        }
        None => {
            let idx = t.slots.len();
            t.slots.push(Process {
                id: idx,
                root,
                caps: [None; MAX_CAPS],
            });
            idx
        }
    };
    drop(t);
    crate::arch::irq_restore(irq);
    Ok(pid)
}

/// 进程地址空间根表物理地址(调度器切换 satp 用)。
///
/// PID 越界/不存在 → panic(fail-loudly:调用方传错进程是编程错误,
/// 不应静默退回内核根表掩盖问题)。锁内只读,ISR 内不调用(调度器
/// 经 `Thread.root` 缓存读取,无需查表)。
pub fn root(pid: usize) -> usize {
    let irq = crate::arch::irq_save();
    let t = TABLE.lock();
    let r = t.slots.get(pid).filter(|p| p.id == pid).map(|p| p.root);
    drop(t);
    crate::arch::irq_restore(irq);
    r.expect("process::root: invalid pid")
}

// ===== M2 T2a:简化能力表 =====

/// 能力错误(映射到 IPC 系统调用 `-errno`,见 M2-DESIGN §4.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// 槽位越界(`slot >= MAX_CAPS`)→ `-EINVAL`。
    InvalidSlot,
    /// 空槽(未授权)或进程不存在 → `-EACCES`。
    NotFound,
}

/// 授权能力:把 `target_pid` 写入进程 `pid` 的 `slot` 槽。
///
/// M2 T2a 简化语义:能力 = 对目标进程发起 IPC(send/recv)的许可;
/// 覆盖写入允许重复授权(幂等)。槽越界 → `InvalidSlot`;进程不存在
/// → `NotFound`(进程 id 无效属编程/状态错误,不 panic、返回错误)。
pub fn grant_cap(pid: usize, slot: usize, target_pid: usize) -> Result<(), CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = TABLE.lock();
        match t.slots.get_mut(pid).filter(|p| p.id == pid) {
            Some(p) => {
                p.caps[slot] = Some(target_pid);
                Ok(())
            }
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}

/// 解析能力:返回进程 `pid` 的 `slot` 槽指向的目标进程 pid。
///
/// 供 IPC 的 send/recv 目标解析(经 cap 才能与对方配对)。槽越界 →
/// `InvalidSlot`;空槽/进程不存在 → `NotFound`。
pub fn cap_target(pid: usize, slot: usize) -> Result<usize, CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let t = TABLE.lock();
        match t.slots.get(pid).filter(|p| p.id == pid) {
            Some(p) => match p.caps[slot] {
                Some(target) => Ok(target),
                None => Err(CapError::NotFound),
            },
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}
