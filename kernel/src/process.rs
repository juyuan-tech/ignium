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
//! - **能力表(M2 T2a / T3c)**:每进程 `MAX_CAPS` 个能力槽;槽 → `Cap`
//!   枚举值(空槽 = 未授权)。`Cap::Proc(pid)` = 对目标进程 IPC 的许可;
//!   `Cap::Shm(id)` = 共享页所有权(T3c,`mmap_share` 改授,revoke 时
//!   销毁整页)。`grant_cap`/`cap_target` 为 IPC 提供目标解析与授权
//!   校验(未授权 → `CapError`)。销毁/页回收已实现(D12
//!   `process::destroy`/`mmu::destroy_root`,共享页 cap 先 revoke 防
//!   double-free);多核 Running 线程栈/槽不回收为已知局限(见
//!   docs/DESIGN.md 已知限制)。
//! - **锁序契约**:能力表(TABLE)先于 IPC 锁获取,绝不逆序;调用方
//!   (syscall/引导测试)经 `cap_target` 读表后释放,再取 IPC 锁。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// 最大存活进程数(纯容量预留:进程表为 `Vec<Process>`,无固定数组/
/// 位域/ABI 依赖,上限只作资源护栏)。`create` 容量按**存活**进程计,
/// 已销毁槽位经 `free` FIFO 复用(见 `create`)。
///
/// 取值依据:M2 引导冒烟套件本身持有 16 个存活进程(每测试一对/多对
/// 进程,销毁机制 D12 才落地,遗留进程未清理);M3 T2 跨核停杀/射杀
/// 测试需再建 3 个。16 + 3 已逼近旧值 16,故提至 32 为后续 M3 服务
/// (syscall 服务进程)预留余量。
pub const MAX_PROCESSES: usize = 32;

/// 每进程能力槽数(简化能力表,M2 T2a)。
///
/// 数组定长以保持 `Process` Copy(槽位 → `Cap` 值)。槽 0 为
/// 测试/服务约定入口,其余槽由授权方自行约定。
pub const MAX_CAPS: usize = 8;

/// 能力值(M2 T2a/T3c)。
///
/// - `Proc(pid)`:对目标进程发起 IPC(send/recv)的许可(T2a,经
///   `grant_cap` 授予);`ipc.rs` 只接受本变体。
/// - `Shm(id)`:共享页所有权(T3c,经 `grant_shm_cap` 由 `mmap_share`
///   改授);持有者可经 revoke 销毁整页(M2-DESIGN"能力即所有权")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Proc(usize),
    Shm(usize),
}

/// 进程:独立地址空间根表 + 简化能力表。
///
/// `id` 与槽索引一致(pid = 槽位);能力表为 `Cap` 数组
/// (槽 → 能力值;空槽 = 未授权)。T2 销毁/页回收将依赖本字段定位。
struct Process {
    id: usize,
    root: usize,
    /// 能力槽数组:槽位 → `Cap`;None = 未授权(空槽)。
    caps: [Option<Cap>; MAX_CAPS],
}

/// 进程表(IRQ 安全 SpinLock;进程表不进入 ISR 路径)。
struct ProcessTable {
    slots: Vec<Process>,
    /// 已销毁进程的槽位(FIFO 复用;`create` 容量按存活进程计 =
    /// `slots.len() - free.len()`,见 `create` 注释)。
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
    // 容量按**存活**进程计:slots.len() 含已销毁但槽位待复用(在 free
    // 队列)的进程 —— 用 len - free.len() 判断,否则销毁后槽位永远无法
    // 复用(M3 T2 实测:boot 测试 16 次 create + 2 次 destroy 后,free 有
    // 2 个空槽,旧守卫 len>=16 却拒绝后续 create)。
    let live = t.slots.len().saturating_sub(t.free.len());
    if live >= MAX_PROCESSES {
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

// ===== M2 T2a/T3c:简化能力表 =====

/// 能力错误(映射到系统调用 `-errno`,见 M2-DESIGN §4.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// 槽位越界(`slot >= MAX_CAPS`)→ `-EINVAL`。
    InvalidSlot,
    /// 空槽(未授权)或进程不存在 → `-EACCES`。
    NotFound,
    /// 槽内是另一类能力(如期望 `Cap::Proc` 却拿到 `Cap::Shm`)→ `-EINVAL`。
    WrongType,
    /// 能力指向的共享页已不存在(已被 revoke)→ `-ENOENT`。
    ShmNotFound,
}

/// 能力错误 → 负 errno(usize 编码,与 L1 ABI 一致)。
///
/// M2 性能:IPC 错误路径热调用(每次能力校验失败都查),#[inline] 配合
/// release fat-LTO 跨模块内联。
#[inline]
pub fn cap_errno(err: CapError) -> usize {
    match err {
        CapError::InvalidSlot | CapError::WrongType => crate::syscall::SYS_ERR_EINVAL,
        CapError::NotFound => crate::syscall::SYS_ERR_EACCES,
        CapError::ShmNotFound => crate::syscall::SYS_ERR_ENOENT,
    }
}

/// 授权进程能力:把 `Cap::Proc(target_pid)` 写入进程 `pid` 的 `slot` 槽。
///
/// M2 T2a 简化语义:能力 = 对目标进程发起 IPC(send/recv)的许可;
/// 覆盖写入允许重复授权(幂等)。槽越界 → `InvalidSlot`;进程不存在
/// → `NotFound`(进程 id 无效属编程/状态错误,不 panic、返回错误)。
pub fn grant_cap(pid: usize, slot: usize, target_pid: usize) -> Result<(), CapError> {
    grant_typed_cap(pid, slot, Cap::Proc(target_pid))
}

/// 授权任意能力值到槽(内部实现;`grant_cap`/`grant_shm_cap` 的公共底层)。
pub fn grant_typed_cap(pid: usize, slot: usize, cap: Cap) -> Result<(), CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = TABLE.lock();
        match t.slots.get_mut(pid).filter(|p| p.id == pid) {
            Some(p) => {
                p.caps[slot] = Some(cap);
                Ok(())
            }
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}

/// 授权共享页能力:`Cap::Shm(shm_id)` 写入进程 `pid` 的 `slot` 槽。
///
/// T3c 由 `shm::mmap_share` 调用(双槽改授 = M2-DESIGN"能力即所有权"):
/// 覆盖写入(被覆盖的 `Cap::Proc` 槽成为共享页所有权,旧 IPC 许可消失)。
pub fn grant_shm_cap(pid: usize, slot: usize, shm_id: usize) -> Result<(), CapError> {
    grant_typed_cap(pid, slot, Cap::Shm(shm_id))
}

/// 解析能力:返回进程 `pid` 的 `slot` 槽的 `Cap` 值。
///
/// 供 IPC 目标解析(须 `Cap::Proc`)与共享页校验(`Cap::Shm`)。槽越界 →
/// `InvalidSlot`;空槽/进程不存在 → `NotFound`。**不 panic**(syscall 路径
/// 的用户传入 pid/slot 经本函数非 panic 校验)。
///
/// M2 性能:IPC syscall 热路径(每次 send/recv 的目标解析都查),#[inline]
/// 配合 release fat-LTO 跨模块内联。
#[inline]
pub fn cap_target(pid: usize, slot: usize) -> Result<Cap, CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let t = TABLE.lock();
        match t.slots.get(pid).filter(|p| p.id == pid) {
            Some(p) => match p.caps[slot] {
                Some(cap) => Ok(cap),
                None => Err(CapError::NotFound),
            },
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}

/// 复制能力:把进程 `pid` 的 `from` 槽值复制到 `to` 槽。
///
/// T3c(syscall 7 `CAP_DUP`):能力可复制(共享所有权不变,槽位增引用)。
/// 源槽空 → `NotFound`;任一端越界 → `InvalidSlot`;进程不存在 → `NotFound`。
pub fn cap_duplicate(pid: usize, from: usize, to: usize) -> Result<(), CapError> {
    if from >= MAX_CAPS || to >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = TABLE.lock();
        match t.slots.get_mut(pid).filter(|p| p.id == pid) {
            Some(p) => match p.caps[from] {
                Some(cap) => {
                    p.caps[to] = Some(cap);
                    Ok(())
                }
                None => Err(CapError::NotFound),
            },
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}

/// 撤销能力:清空进程 `pid` 的 `slot` 槽。
///
/// T3c(syscall 6 `CAP_REVOKE`)语义按能力类型分派:
/// - `Cap::Shm(id)` → **整页撤销**(`shm::shm_revoke`:撤双方映射、回收
///   物理页、清双方槽、注册表出列);共享页销毁,能力即失效;
/// - `Cap::Proc(_)` → 仅清本进程该槽(IPC 许可撤销)。
///
/// 空槽 → `NotFound`;槽越界 → `InvalidSlot`;进程不存在 → `NotFound`。
pub fn cap_revoke(pid: usize, slot: usize) -> Result<(), CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    // 先解析槽内类型(TABLE 锁已释放)再分派 —— 锁不重叠。
    match cap_target(pid, slot)? {
        Cap::Shm(id) => crate::shm::shm_revoke(id).map_err(|_| CapError::ShmNotFound),
        Cap::Proc(_) => clear_cap(pid, slot),
    }
}

/// 清空能力槽(不解析类型)。`cap_revoke` 的 Proc 分支与 `shm_revoke`
/// 撤双方槽内部用;空槽幂等成功(revoke 后再次 revoke 无害)。
pub fn clear_cap(pid: usize, slot: usize) -> Result<(), CapError> {
    if slot >= MAX_CAPS {
        return Err(CapError::InvalidSlot);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = TABLE.lock();
        match t.slots.get_mut(pid).filter(|p| p.id == pid) {
            Some(p) => {
                p.caps[slot] = None;
                Ok(())
            }
            None => Err(CapError::NotFound),
        }
    };
    crate::arch::irq_restore(irq);
    result
}

/// 进程地址空间根表(**非 panic** 版)。
///
/// syscall 路径的用户传入 pid(如 `mmap_share` 的对端)必须经本函数校验
/// 存在性,再取根表 —— 防止非法 pid 触发 `root()` panic(fail-loudly 只
/// 留给内核自身编程错误)。进程不存在 → `None`。
pub fn pid_root(pid: usize) -> Option<usize> {
    let irq = crate::arch::irq_save();
    let r = {
        let t = TABLE.lock();
        t.slots.get(pid).filter(|p| p.id == pid).map(|p| p.root)
    }; // t 在此 drop(TABLE 锁释放)
    crate::arch::irq_restore(irq);
    r
}

/// 销毁进程(M2 D12):revoke 全部 Shm cap → 原子失效槽 → 回收地址空间。
///
/// 顺序:
/// 1. **先 revoke 本进程全部 `Cap::Shm`**(TABLE 锁内收集、锁外逐个经
///    `shm_revoke`:撤双方映射、清双方槽、释放共享页、注册表出列)—— 必须
///    在地址空间释放之前完成:若共享页仍 U 映射,`destroy_root` 会对其
///    double-free;
/// 2. **锁内"捕获 root + 原子失效槽"**(`id = usize::MAX` + 入 `free` 池):
///    并发 destroy(多核自愈路径)在此串行化 —— 先到者失效,后到者
///    `filter(id == pid)` 失败即返回,杜绝同一根表双释放;
/// 3. 锁外 `mmu::destroy_root(root)` 回收用户页/各级表页/根页。
///
/// 失效后 `pid_root`/`cap_target` 对已销毁进程返回 NotFound,槽位可复用。
///
/// # 前提
/// 调用方须已 `switch_root(kernel_root())`(见 `mmu::destroy_root` 前提)。
/// 重复调用(pid 已销毁)→ 无操作(幂等)。
pub fn destroy(pid: usize) {
    // 1) 收集并 revoke 全部 Shm cap。TABLE 锁内收集、锁外逐个 revoke:
    //    shm_revoke 内部取 SHM 与 TABLE 锁,不得与本锁重叠。
    let shm_ids: Vec<usize> = {
        let irq = crate::arch::irq_save();
        let ids = {
            let t = TABLE.lock();
            match t.slots.get(pid).filter(|p| p.id == pid) {
                Some(p) => p
                    .caps
                    .iter()
                    .filter_map(|c| match c {
                        Some(Cap::Shm(id)) => Some(*id),
                        _ => None,
                    })
                    .collect(),
                None => return, // 已销毁/不存在:幂等无操作
            }
        }; // TABLE 锁在此释放
        crate::arch::irq_restore(irq);
        ids
    };
    for id in shm_ids {
        // 重复 revoke(并发 destroy / 用户已 revoke)幂等返回 Err,无害。
        let _ = crate::shm::shm_revoke(id);
    }
    // 2) 捕获 root + 原子失效槽(并发 destroy 在此串行化,防双释放)。
    let root = {
        let irq = crate::arch::irq_save();
        let r = {
            let mut t = TABLE.lock();
            match t.slots.get_mut(pid).filter(|p| p.id == pid) {
                Some(p) => {
                    let root = p.root;
                    p.id = usize::MAX;
                    p.caps = [None; MAX_CAPS];
                    // M3-2:进程销毁时清设备 claim(设备页生命周期随进程)。
                    // 锁序 TABLE → DEVICES(与 device::map 一致,不逆序);
                    // 映射不在此解除,随 destroy_root(非分配器页只 unmap)。
                    crate::device::release_all(pid);
                    t.free.push_back(pid);
                    Some(root)
                }
                None => None,
            }
        }; // TABLE 锁在此释放
        crate::arch::irq_restore(irq);
        r
    };
    let Some(root) = root else { return }; // 已被并发 destroy 处理
                                           // 3) 锁外回收地址空间页(用户页/各级表页/根页)。
    crate::mmu::destroy_root(root);
}
