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
//! - **T2**:本模块将挂接每进程能力表与销毁/页回收;本里程碑仅提供
//!   地址空间(id → root),pid 以槽索引编址、`free` 队列留待销毁复用。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// 最大进程数(容量预留,与 `MAX_THREADS` 对齐的纪律)。
pub const MAX_PROCESSES: usize = 16;

/// 进程:独立地址空间根表。
///
/// `id` 与槽索引一致(pid = 槽位);T2 能力表/销毁将依赖本字段定位。
struct Process {
    id: usize,
    root: usize,
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
    let pid = match t.free.pop_front() {
        Some(idx) => {
            t.slots[idx] = Process { id: idx, root };
            idx
        }
        None => {
            let idx = t.slots.len();
            t.slots.push(Process { id: idx, root });
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
