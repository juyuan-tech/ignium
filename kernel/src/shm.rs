//! 共享内存(M2 T3c):单页物理内存映射进两个进程地址空间。
//!
//! # 设计
//! - **`mmap_share`**(syscall 5 `SHM_MAP`):调用方进程须在 `a_slot` 持
//!   `Cap::Proc(peer)`(定对端 + 授权)。分配一页清零物理页 → 在固定
//!   `SHM_VA = 0x5000_0000`(与既有用户测试 0x4000_0000 段不重叠)分别
//!   `map_user_page` 进调用方与对端根表(U 权限,读写) → 注册表入列 →
//!   **双槽改授 `Cap::Shm(id)`**(`a_slot` 覆盖原 Proc cap = M2-DESIGN
//!   "能力即所有权")。返回 shm_id。
//! - **`shm_revoke`**(经 `process::cap_revoke` 对 `Cap::Shm` 分派):
//!   撤双方 `SHM_VA` 映射、释放物理页、清双方槽、注册表出列。
//!   跨核 TLB shootdown 以"satp 切换全刷 + 本核 sfence"简化(M2 单页
//!   共享、revoke 即销毁页,无残留映射复用 —— 见 M2-DESIGN 遗留风险)。
//! - **注册表**:槽式(索引 = id 稳定)+ free 池复用;`revoke` 把 id 置
//!   `usize::MAX` 失效后入池。访问仅在用户 trap 上下文(syscall)与
//!   引导测试(非 ISR),表满 fail-loudly 返回 `-ENOMEM`。
//!
//! # 锁序契约
//! `cap_target`(TABLE)→ `pid_root`(TABLE)→ 本模块表锁,均**不重叠**
//! (逐个获取释放);`mmap_share` 内 SHM 表锁与 grant_shm_cap(TABLE 锁)
//! 亦不重叠。不与 IPC 锁、SCHED 锁同持。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// 共享页固定虚拟地址(两个进程地址空间的同一 VA,映射同一物理页)。
/// 与既有用户测试 VA 0x4000_0000 段(Sv39 L2=1)同 L2 不同 L1,不重叠。
pub const SHM_VA: usize = 0x5000_0000;
/// 单页共享长度(mmep_share 仅接受 4096;多页共享留待后续里程碑)。
pub const SHM_LEN: usize = 4096;
/// 注册表容量上限(槽式复用;超出返回 -ENOMEM)。
const MAX_SHMS: usize = 16;

/// 共享页:一个物理页 + 双所有者(owner pid + 持有能力槽)。
///
/// `owners` 恰两元(A/B);`revoke` 撤双方映射、清双方槽。
#[derive(Clone, Copy)]
struct SharedPage {
    /// 注册表槽索引(= 返回给调用方的 shm_id);revoke 后置 `usize::MAX` 失效。
    id: usize,
    /// 共享物理页地址(分配器页,身份映射下可直接读写)。
    paddr: usize,
    /// 所有者(pid, 能力槽)对;双方均持 `Cap::Shm(id)`。
    owners: [(usize, usize); 2],
}

/// 共享页注册表(槽式:id = 索引,稳定;free 池复用)。
struct ShmTable {
    shms: Vec<SharedPage>,
    free: VecDeque<usize>,
}

/// 注册表单例。
static SHM: SpinLock<ShmTable> = SpinLock::new(ShmTable {
    shms: Vec::new(),
    free: VecDeque::new(),
});

/// 注册表容量预留(boot 期调用;非 ISR 分配,D11 只约束 ISR)。
///
/// 由 `kernel_main` 在 `sched::init` 之后、`tests::boot_tests` 之前调用。
pub fn init() {
    let irq = crate::arch::irq_save();
    {
        let mut t = SHM.lock();
        let cap = t.shms.capacity();
        if cap < MAX_SHMS {
            t.shms.reserve(MAX_SHMS - cap);
        }
    }
    crate::arch::irq_restore(irq);
}

/// 建立共享内存(syscall 5 `SHM_MAP` 的调用方)。
///
/// - `caller`:调用方进程 id(`sched::current_proc()`,恒有效);
/// - `a_slot`:调用方能力槽,**须持 `Cap::Proc(peer)`**(定对端 + 授权);
///   成功后本槽被覆盖为 `Cap::Shm(id)`(IPC 许可消失);
/// - `b_slot`:对端进程的能力槽,成功后授予 `Cap::Shm(id)`(覆盖写入);
/// - `len`:必须 `== SHM_LEN`(单页),否则 `-EINVAL`。
///
/// 返回 `Ok(shm_id)` 或负 errno(usize 编码)。失败路径完整回滚
/// (unmap + free,不泄漏页、不留半映射)。
pub fn mmap_share(caller: usize, a_slot: usize, b_slot: usize, len: usize) -> Result<usize, usize> {
    if len != SHM_LEN {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 1) 调用方 a_slot 须持 Cap::Proc(peer)(定对端 + 授权;经非 panic
    //    cap_target,用户传入的槽号不会触发 panic)。
    let peer = match crate::process::cap_target(caller, a_slot) {
        Ok(crate::process::Cap::Proc(p)) => p,
        Ok(crate::process::Cap::Shm(_)) => return Err(crate::syscall::SYS_ERR_EINVAL),
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    if b_slot >= crate::process::MAX_CAPS {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 2) 对端进程必须存在(pid_root 非 panic;缺则 -EACCES,防 root() panic)。
    let root_b = match crate::process::pid_root(peer) {
        Some(r) => r,
        None => return Err(crate::syscall::SYS_ERR_EACCES),
    };
    let root_a = match crate::process::pid_root(caller) {
        Some(r) => r,
        None => return Err(crate::syscall::SYS_ERR_EACCES),
    };
    // 3) 分配共享页并清零(D10:交接用户态前防信息泄漏)。
    let paddr = match crate::mem::alloc_pages_zeroed(0) {
        Some(p) => p,
        None => return Err(crate::syscall::SYS_ERR_ENOMEM),
    };
    // 4) 映射进两进程根表(SHM_VA 固定;任一失败完整回滚)。
    const MAP_FLAGS: u64 = 0xC7; // V|R|W|A|D,map_user_page 自动加 U
    if crate::mmu::map_user_page(root_a, SHM_VA, paddr, MAP_FLAGS).is_err() {
        crate::mem::free_pages(paddr).ok();
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    if crate::mmu::map_user_page(root_b, SHM_VA, paddr, MAP_FLAGS).is_err() {
        crate::mmu::unmap_4k(root_a, SHM_VA).ok();
        crate::mem::free_pages(paddr).ok();
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 5) 入表(SHM 锁)。表满回滚映射 + 释放。
    let irq = crate::arch::irq_save();
    let id = {
        let mut t = SHM.lock();
        if t.shms.len() >= MAX_SHMS {
            drop(t);
            crate::arch::irq_restore(irq);
            crate::mmu::unmap_4k(root_a, SHM_VA).ok();
            crate::mmu::unmap_4k(root_b, SHM_VA).ok();
            crate::mem::free_pages(paddr).ok();
            return Err(crate::syscall::SYS_ERR_ENOMEM);
        }
        match t.free.pop_front() {
            Some(idx) => {
                t.shms[idx] = SharedPage {
                    id: idx,
                    paddr,
                    owners: [(caller, a_slot), (peer, b_slot)],
                };
                idx
            }
            None => {
                let idx = t.shms.len();
                t.shms.push(SharedPage {
                    id: idx,
                    paddr,
                    owners: [(caller, a_slot), (peer, b_slot)],
                });
                idx
            }
        }
    }; // SHM 锁在此释放
    crate::arch::irq_restore(irq);
    // 6) 双槽改授 Cap::Shm(id)(覆盖原 Cap::Proc —— 能力即所有权)。
    //    前置校验(cap_target/pid_root/b_slot 界)已保证 grant 只可能成功;
    //    万一失败(防御,维持 syscall 路径不 panic 纪律)→ `shm_revoke`
    //    完整回滚(撤双映射 + 清双槽 + 释放页 + 出表)后返回 -ENOMEM。
    if crate::process::grant_shm_cap(caller, a_slot, id).is_err()
        || crate::process::grant_shm_cap(peer, b_slot, id).is_err()
    {
        shm_revoke(id).ok();
        return Err(crate::syscall::SYS_ERR_ENOMEM);
    }
    Ok(id)
}

/// 整页撤销共享内存(经 `process::cap_revoke` 对 `Cap::Shm` 分派)。
///
/// 撤双方 `SHM_VA` 映射、清双方槽、释放物理页、注册表出列(槽 id 置
/// `usize::MAX` 失效 + 入 free 池)。id 不存在/已撤销 → `Err(())`。
/// 任一内部步骤失败(如物理页 double-free)→ `Err(())`(调用方映射为
/// `CapError::ShmNotFound`)。
pub fn shm_revoke(id: usize) -> Result<(), ()> {
    // 1) 出表(复制出物理页与所有者;槽失效入池)。SHM 锁内快照。
    let sp = {
        let irq = crate::arch::irq_save();
        let r = {
            let mut t = SHM.lock();
            match t.shms.get(id) {
                Some(s) if s.id == id => {
                    let s = *s;
                    t.shms[id].id = usize::MAX; // 失效标记(防重复 revoke)
                    t.free.push_back(id);
                    Some(s)
                }
                _ => None,
            }
        }; // SHM 锁在此释放
        crate::arch::irq_restore(irq);
        r
    };
    let sp = match sp {
        Some(s) => s,
        None => return Err(()),
    };
    // 2) 撤双方映射 + 清双方槽(unmap 失败忽略:根表可能已不存在 ——
    //    进程销毁在后续里程碑;清槽是幂等强语义,必成功)。
    for (owner_pid, owner_slot) in sp.owners {
        if let Some(root) = crate::process::pid_root(owner_pid) {
            crate::mmu::unmap_4k(root, SHM_VA).ok();
        }
        let _ = crate::process::clear_cap(owner_pid, owner_slot);
    }
    // 3) 释放物理页(失败即 Err,调用方映射 ShmNotFound —— 页泄漏须暴露)。
    crate::mem::free_pages(sp.paddr).map_err(|_| ())?;
    // 4) TLB 冲刷:当前核立即刷;**其它在线核经 REMOTE_REQ(TLB_FLUSH)+
    //    IPI 投递并等待完成**(M3 T2 修复:M2 只刷本核,其它核陈旧 TLB
    //    项会让其继续读到已释放物理页 —— 泄漏或数据错乱)。远端 SSIP
    //    handler 仅 sfence,不取 SHM/表锁,无锁序反转。
    crate::mmu::tlb_flush();
    crate::sched::tlb_shootdown_remote();
    Ok(())
}

/// 查询共享页物理地址(引导测试用;revoke 后返回 None)。
pub fn shm_paddr(id: usize) -> Option<usize> {
    let irq = crate::arch::irq_save();
    let r = {
        let t = SHM.lock();
        t.shms.get(id).filter(|s| s.id == id).map(|s| s.paddr)
    }; // SHM 锁在此释放
    crate::arch::irq_restore(irq);
    r
}
