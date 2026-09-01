//! 物理页能力注册表(M3-3 `Cap::Page`):跟踪"作为能力被持有"的物理页。
//!
//! # 设计
//! - **1 cap = 1 个物理页(4KB),单引用**:`Cap::Page(id)` 是唯一指向该页的
//!   能力(禁 dup,见 `process::cap_duplicate`);页生命周期 = 能力生命周期。
//!   `mem_grant`(号 13)以 **move** 语义把页移交给已连接进程(清源槽,移交
//!   隐含解除发送方映射),`cap_revoke`(号 6)对 `Cap::Page` 分派 = **释放**
//!   (先 unmap 再 `free_pages` 归还 buddy)。详见 M3-DESIGN §11.2/11.4/11.5。
//! - **注册表**(仿 shm.rs):槽式(`Vec<PageRecord>`,索引 = id 稳定)+ `free`
//!   池复用;`revoke` 把 `id` 置 `usize::MAX` 失效后入池。`map_va` 记录单
//!   映射 VA(revoke/移交时定位 unmap);同一页只允许一个映射(单映射不变量)。
//! - **纯服务授权**:本模块的 `alloc` 仅由**引导编排**(测试 T1/T2)在
//!   mem_server spawn 后注入页池调用 —— 内核**不暴露**通用分配 syscall
//!   (避免 ambient 授权;见 M3-DESIGN §11.3)。正式 spawn/init 服务落地后
//!   改为引导期自动授予(登记 DEFERRED D33)。
//!
//! # 锁序契约
//! 本模块 PAGES 锁与 TABLE 锁**不重叠持有**(逐个获取释放)。`mem_grant`/
//! `mem_map` 入口先经 `cap_target`/`pid_root`(TABLE,取后即放)再取 PAGES;
//! `revoke`/`grant` 锁外再经 `pid_root`(TABLE)与 `mmu` 解除映射 —— 全程
//! `TABLE → PAGES` 顺序保持,不逆序,不重叠。调用方(`process::destroy` /
//! `cap_revoke`)同理:先取 TABLE 释放后再调本模块。与 IPC/SCHED 锁无同持。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// 注册表容量上限(槽式复用;超出返回 -ENOMEM)。
const MAX_PAGES: usize = 64;

/// 页记录:一个被能力持有的物理页。
#[derive(Clone, Copy)]
struct PageRecord {
    /// 槽索引(= `Cap::Page` 的 id);revoke 后置 `usize::MAX` 失效(防槽位
    /// 复用后陈旧记录被误命中,仿 `SharedPage`)。
    id: usize,
    /// 物理页地址(分配器页,身份映射下可直接读写)。
    paddr: usize,
    /// 当前持有者进程(防御性不变量:与 `Cap::Page` 唯一持有者一致)。
    owner: usize,
    /// 单映射 VA:页映射进持有者根表的地址(revoke/移交时定位 unmap);
    /// None = 未映射(可移交)。
    map_va: Option<usize>,
}

/// 页注册表(槽式:id = 索引,稳定;free 池复用)。
struct PageTable {
    pages: Vec<PageRecord>,
    free: VecDeque<usize>,
}

/// 注册表单例。
static PAGES: SpinLock<PageTable> = SpinLock::new(PageTable {
    pages: Vec::new(),
    free: VecDeque::new(),
});

/// 注册表容量预留(boot 期调用;非 ISR 分配,D11 只约束 ISR)。
///
/// 由 `kernel_main` 在 `shm::init` 之后、`tests::boot_tests` 之前调用
/// (引导期非 ISR 分配,仿 shm::init)。
pub fn init() {
    let irq = crate::arch::irq_save();
    {
        let mut t = PAGES.lock();
        let cap = t.pages.capacity();
        if cap < MAX_PAGES {
            t.pages.reserve(MAX_PAGES - cap);
        }
    }
    crate::arch::irq_restore(irq);
}

/// 撤销页(经 `process::cap_revoke` 对 `Cap::Page` 分派;以及 `destroy` 钩子)。
///
/// 若页已映射(map_va Some)→ 先从持有者根表 unmap;释放物理页归还 buddy;
/// 注册表出列(id 置 `usize::MAX` 失效 + 入 free 池)。幂等:已撤销/不存在 →
/// `Err(())`(调用方映射 `PageNotFound → -ENOENT`)。
///
/// 不修改任何能力槽 —— 槽的清理由调用方负责:`cap_revoke` 在返回前经
/// `clear_cap` 清本槽;`destroy` 步骤 2 原子失效槽时清空全部 cap。
pub fn revoke(id: usize) -> Result<(), ()> {
    // 1) 出表快照 + 失效入池。PAGES 锁内复制出记录(校验 id == 槽索引,防
    //    槽位复用后陈旧记录被误命中),锁外释放物理页/解除映射(不重叠持锁)。
    let rec = {
        let irq = crate::arch::irq_save();
        let r = {
            let mut t = PAGES.lock();
            match t.pages.get(id) {
                Some(r) if r.id == id => {
                    let rec = *r;
                    t.pages[id].id = usize::MAX; // 失效标记(防重复 revoke)
                    t.pages[id].map_va = None;
                    t.free.push_back(id);
                    Some(rec)
                }
                _ => None,
            }
        }; // PAGES 锁在此释放
        crate::arch::irq_restore(irq);
        r
    };
    let rec = match rec {
        Some(r) => r,
        None => return Err(()),
    };
    // 2) 若已映射:从持有者根表解除映射(unmap 失败忽略:根表可能已不存在
    //    —— 防御;syscall 路径调用方存活、destroy 路径在 root 捕获前调用,
    //    根表均有效)。
    if let Some(va) = rec.map_va {
        if let Some(root) = crate::process::pid_root(rec.owner) {
            crate::mmu::unmap_4k(root, va).ok();
        }
    }
    // 3) 释放物理页(失败即 Err,调用方映射 PageNotFound —— 页泄漏须暴露)。
    crate::mem::free_pages(rec.paddr).map_err(|_| ())?;
    // 4) TLB 冲刷:当前核立即刷;**其它在线核经 REMOTE_REQ(TLB_FLUSH)+
    //    IPI 投递并等待完成**(仿 shm_revoke;boot 期单核时退化为纯本地
    //    sfence,零开销零副作用)。远端 SSIP handler 仅 sfence,不取
    //    PAGES/表锁,无锁序反转。
    crate::mmu::tlb_flush();
    crate::sched::tlb_shootdown_remote();
    Ok(())
}

/// 移交页所有权(mem_grant 号 13 的内核侧;registry 级,**move** 语义)。
///
/// - 若页已映射(map_va Some)→ 先从**旧持有者**根表 unmap —— 移交隐含
///   解除发送方映射。这由归还协议必然性决定(客户端归还给 mem_server 时
///   页已映射,而系统无 unmap-without-free syscall,D34);`mem_map`/`grant`
///   为页映射状态唯一变更点,状态机封闭。
/// - 更新 owner 为新持有者;map_va 清 None(新持有者自行 `mem_map`)。
///
/// **不修改任何能力槽**(槽的移动由调用方 `mem_grant`:先 `grant_typed_cap`
/// 写对端、后 `clear_cap` 清源槽)。id 不存在/已撤销 → `Err(())`。
pub fn grant(id: usize, to_pid: usize) -> Result<(), ()> {
    // 1) 快照(旧 owner + 映射 VA)并更新 owner + 清映射。PAGES 锁内。
    let mv = {
        let irq = crate::arch::irq_save();
        let r = {
            let mut t = PAGES.lock();
            match t.pages.get_mut(id) {
                Some(r) if r.id == id => {
                    let old_owner = r.owner;
                    // 移交隐含解除发送方映射(map_va.take() 同时清单映射记录)。
                    let va = r.map_va.take();
                    r.owner = to_pid;
                    Some((old_owner, va))
                }
                _ => None,
            }
        }; // PAGES 锁在此释放
        crate::arch::irq_restore(irq);
        r
    };
    let (old_owner, va) = match mv {
        Some(x) => x,
        None => return Err(()),
    };
    // 2) 若旧持有者已映射:从其根表解除映射(unmap 失败忽略:根表可能已不
    //    存在 —— 防御;syscall 路径发送方存活)。旧持有者即 mem_grant 调用方
    //    (src_slot 持本页),单地址 sfence 覆盖其当前核;页单映射不变量保证
    //    无跨核映射残留。
    if let Some(va) = va {
        if let Some(root) = crate::process::pid_root(old_owner) {
            crate::mmu::unmap_4k(root, va).ok();
        }
    }
    Ok(())
}

/// 受控跨进程页移交(syscall 13 `mem_grant` 的内核侧;M3-DESIGN §11.4)。
///
/// **move** `Cap::Page`:调用方 `src_slot` → peer 的 `dst_slot`,并清调用方
/// `src_slot`(单引用,防双持)。门禁:
/// - 调用方 `src_slot` 持 `Cap::Page(id)`(空槽 → `-EACCES`;非 Page → `-EINVAL`);
/// - 调用方 `peer_slot` 持 `Cap::Proc(peer)`(空槽 → `-EACCES`;非 Proc →
///   `-EINVAL`)—— **只能移交给已连接(持 Cap::Proc)的进程**,无 ambient 移交;
/// - peer 存活(→ `-EACCES`);`dst_slot` 越界(→ `-EINVAL`)/非空(→ `-EEXIST`,
///   防静默丢 cap)。
///
/// 页已映射时由 `pages::grant` **自动解除发送方映射**(移交隐含 unmap ——
/// 归还协议:客户端归还给 mem_server 时页已映射;系统无 unmap-without-free
/// syscall,见 DEFERRED D34)。返回 `Ok(())` 或负 errno(usize 编码)。
pub fn mem_grant(
    caller: usize,
    src_slot: usize,
    peer_slot: usize,
    dst_slot: usize,
) -> Result<(), usize> {
    // 1) 调用方 src_slot 须持 Cap::Page(id)。
    let id = match crate::process::cap_target(caller, src_slot) {
        Ok(crate::process::Cap::Page(id)) => id,
        Ok(_) => {
            return Err(crate::process::cap_errno(
                crate::process::CapError::WrongType,
            ))
        }
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    // 2) 调用方 peer_slot 须持 Cap::Proc(peer_pid)(定对端 + 授权)。
    let peer = match crate::process::cap_target(caller, peer_slot) {
        Ok(crate::process::Cap::Proc(p)) => p,
        Ok(_) => {
            return Err(crate::process::cap_errno(
                crate::process::CapError::WrongType,
            ))
        }
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    // 3) peer 存活 + dst_slot 越界/占用预检(防静默丢 cap)。
    if crate::process::pid_root(peer).is_none() {
        return Err(crate::syscall::SYS_ERR_EACCES);
    }
    if dst_slot >= crate::process::MAX_CAPS {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    match crate::process::cap_target(peer, dst_slot) {
        Ok(_) => return Err(crate::syscall::SYS_ERR_EEXIST),
        Err(crate::process::CapError::InvalidSlot) => return Err(crate::syscall::SYS_ERR_EINVAL),
        Err(_) => {} // NotFound(空槽)= 可授予
    }
    // 4) 注册表 owner 移交(隐含解除发送方映射;失败仅防御 —— 已 revoke)。
    grant(id, peer).map_err(|_| crate::syscall::SYS_ERR_ENOENT)?;
    // 5) 对端收槽写入 Cap::Page(id)(原子;失败回滚 owner,不落半状态)。
    if crate::process::grant_typed_cap(peer, dst_slot, crate::process::Cap::Page(id)).is_err() {
        let _ = grant(id, caller); // 回滚 owner(页映射已清,不会再 unmap)
        return Err(crate::syscall::SYS_ERR_EEXIST);
    }
    // 6) 清调用方源槽(单引用 move)。
    let _ = crate::process::clear_cap(caller, src_slot);
    Ok(())
}

/// 页是否已映射(map_va Some;`mem_map` 预检)。
pub fn is_mapped(id: usize) -> bool {
    let irq = crate::arch::irq_save();
    let r = {
        let t = PAGES.lock();
        t.pages
            .get(id)
            .filter(|r| r.id == id)
            .is_some_and(|r| r.map_va.is_some())
    }; // PAGES 锁在此释放
    crate::arch::irq_restore(irq);
    r
}

/// 查询页物理地址(`mem_map` 用;已撤销/不存在 → None)。
pub fn paddr(id: usize) -> Option<usize> {
    let irq = crate::arch::irq_save();
    let r = {
        let t = PAGES.lock();
        t.pages.get(id).filter(|r| r.id == id).map(|r| r.paddr)
    }; // PAGES 锁在此释放
    crate::arch::irq_restore(irq);
    r
}

/// 记录页映射(`mem_map` 号 14 在 `map_user_page` **成功之后**调用)。
///
/// 已映射(map_va Some)→ `Err(())`(单映射不变量;调用方映射 `-EEXIST`)。
/// 成功后 map_va = Some(va);失败(防御)不修改。映射者即 `PageRecord.owner`
/// (cap 单引用),故无需 pid 入参 —— 记录 owner 为准。
pub fn map(id: usize, va: usize) -> Result<(), ()> {
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = PAGES.lock();
        match t.pages.get_mut(id) {
            Some(r) if r.id == id => {
                if r.map_va.is_some() {
                    Err(())
                } else {
                    r.map_va = Some(va);
                    Ok(())
                }
            }
            _ => Err(()),
        }
    }; // PAGES 锁在此释放
    crate::arch::irq_restore(irq);
    result
}

/// 把 `Cap::Page` 映射进调用进程根表(syscall 14 `mem_map` 的内核侧;
/// M3-DESIGN §11.5)。
///
/// - `va` 页对齐且 `< USER_VA_LIMIT`(否则 `-EINVAL`);槽持 `Cap::Page`
///   (空槽 `-EACCES` / 非 Page `-EINVAL`);
/// - 页已映射(任何 VA)→ `-EEXIST`(单映射不变量);
/// - `mmu::map_user_page(root, va, paddr, 0xC7)`(U RW + 单地址 sfence;
///   `page_in_range` 由分配器页天然满足),成功后 `pages::map` 记 map_va。
///
/// 失败路径完整回滚:map_user_page 失败不落 map_va;`pages::map` 失败(防御)
/// 先 unmap 再返回,不留半映射。返回 `Ok(())` 或负 errno(usize 编码)。
pub fn mem_map(caller: usize, slot: usize, va: usize) -> Result<(), usize> {
    // 1) va 合法性(页对齐 + 用户区上限,与 map_user_page 同校验,提前返回)。
    if !va.is_multiple_of(crate::mem::PAGE_SIZE) || va >= crate::mmu::USER_VA_LIMIT {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 2) 槽持 Cap::Page(id)。
    let id = match crate::process::cap_target(caller, slot) {
        Ok(crate::process::Cap::Page(id)) => id,
        Ok(_) => {
            return Err(crate::process::cap_errno(
                crate::process::CapError::WrongType,
            ))
        }
        Err(e) => return Err(crate::process::cap_errno(e)),
    };
    // 3) 已映射(任何 VA)→ -EEXIST(防同一页映射两次 / 双映射不一致)。
    if is_mapped(id) {
        return Err(crate::syscall::SYS_ERR_EEXIST);
    }
    // 4) 物理地址(注册表查;已撤销 → -ENOENT,防御)。
    let paddr = paddr(id).ok_or(crate::syscall::SYS_ERR_ENOENT)?;
    // 5) 当前进程根表 + 映射(U RW;page_in_range 由分配器页天然满足)。
    let root = match crate::process::pid_root(caller) {
        Some(r) => r,
        None => return Err(crate::syscall::SYS_ERR_EACCES),
    };
    if crate::mmu::map_user_page(root, va, paddr, 0xC7).is_err() {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 6) 记录 map_va(在 map_user_page 成功之后;失败则回滚 unmap,防半映射)。
    if map(id, va).is_err() {
        crate::mmu::unmap_4k(root, va).ok();
        return Err(crate::syscall::SYS_ERR_EEXIST);
    }
    Ok(())
}
