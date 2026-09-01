//! 内核服务注册表(M3-2):用户态服务自报注册 + 客户端连接。
//!
//! # 设计
//! - **服务注册**(syscall 10 `service_register`):服务进程自报注册,id 合法域
//!   `1..MAX_SERVICES`(id 0 保留)。id 越界 → `-EINVAL`;已占用 → `-EEXIST`。
//!   原子性:进程存活校验与注册表插入经 **TABLE 锁串行化**(见
//!   `process::register_service` 持 TABLE 调 `register_locked`)—— 杜绝
//!   "注册于已亡进程 + pid 复用"竞态。
//! - **服务连接**(syscall 11 `service_connect`):客户端按 id 查注册表得
//!   server_pid → **双向授予** `Cap::Proc`(client 槽得 server,server 槽得
//!   client)—— 服务端须持 `Cap::Proc(client)` 才能 `ipc_recv`(ipc.rs 契约:
//!   recv 要求本进程槽持 Cap::Proc(src))。失败完整回滚(不留半授予)。
//! - **生命周期**:服务进程销毁(含被杀)时经 `process::destroy` 钩子自动
//!   注销(`unregister_all_locked`,TABLE 锁内调用)。
//!
//! # 锁序契约
//! `TABLE → SERVICES`(destroy / register_service 持 TABLE 调本模块 *_locked
//! 函数),**不逆序**;`connect` 查表后先释放 SERVICES 再 grant(TABLE),无
//! 嵌套。SERVICES 为独立叶子锁,不与 IPC/SCHED 锁同持。

use crate::sync::SpinLock;

/// 服务 id 合法域上界(槽式:索引 0 保留,合法 id = `1..MAX_SERVICES`)。
pub const MAX_SERVICES: usize = 8;

/// 服务注册表槽:索引 = 服务 id(1 基);None = 未注册。
///
/// `id` 字段与槽索引一致(冗余,`lookup` 作防御性失效标记校验,防槽位
/// 复用后读到陈旧条目 —— 与 shm.rs `SharedPage.id` 同风格)。
#[derive(Clone, Copy)]
struct ServiceEntry {
    id: usize,
    pid: usize,
}

/// 服务注册表(槽式:id 0 恒保留,实际索引 [1, MAX_SERVICES))。
struct ServiceTable {
    slots: [Option<ServiceEntry>; MAX_SERVICES],
}

/// 注册表单例(独立 SpinLock;ISR 路径不访问)。
static SERVICES: SpinLock<ServiceTable> = SpinLock::new(ServiceTable {
    slots: [None; MAX_SERVICES],
});

/// 在服务注册表插入条目(`process::register_service` 持 TABLE 锁调用)。
///
/// 锁序:TABLE → SERVICES(本函数取 SERVICES,调用方持 TABLE,不逆序)。
/// id 越界 → `-EINVAL`;已占用 → `-EEXIST`;成功 `Ok(())`。
pub fn register_locked(pid: usize, id: usize) -> Result<(), usize> {
    if id == 0 || id >= MAX_SERVICES {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = SERVICES.lock();
        match t.slots[id] {
            Some(_) => Err(crate::syscall::SYS_ERR_EEXIST),
            None => {
                t.slots[id] = Some(ServiceEntry { id, pid });
                Ok(())
            }
        }
    }; // SERVICES 锁在此释放
    crate::arch::irq_restore(irq);
    result
}

/// 注销进程注册的全部服务(`process::destroy` 钩子,TABLE 锁内调用)。
///
/// 仅清 SERVICES 槽(服务 id 释放,可被新服务复用);幂等:无该进程注册时
/// 无操作。锁序 TABLE → SERVICES(调用方持 TABLE,本函数取 SERVICES)。
pub fn unregister_all_locked(pid: usize) {
    let irq = crate::arch::irq_save();
    {
        let mut t = SERVICES.lock();
        for slot in t.slots.iter_mut() {
            if let Some(entry) = slot {
                if entry.pid == pid {
                    *slot = None;
                }
            }
        }
    } // SERVICES 锁在此释放
    crate::arch::irq_restore(irq);
}

/// 按 id 查注册表,返回服务进程 pid(未注册 → None)。供 connect。
fn lookup(id: usize) -> Option<usize> {
    let irq = crate::arch::irq_save();
    let r = {
        let t = SERVICES.lock();
        // 防御性 `entry.id == id`(槽位失效/复用后不应有陈旧条目)。
        t.slots.get(id).and_then(|s| match s {
            Some(entry) if entry.id == id => Some(entry.pid),
            _ => None,
        })
    }; // SERVICES 锁在此释放
    crate::arch::irq_restore(irq);
    r
}

/// 客户端连接服务(syscall 11 `service_connect` 的调用方)。
///
/// - `caller`:调用方进程 id(`sched::current_proc()`,恒有效);
/// - `id`:服务 id;
/// - `client_slot`:caller 的能力槽(成功后持 `Cap::Proc(server)`);
/// - `server_slot`:服务进程的能力槽(成功后持 `Cap::Proc(caller)`)。
///
/// **双向授予**(互认介绍):client 槽得 `Cap::Proc(server)`,server 槽得
/// `Cap::Proc(caller)` —— 服务端无 Cap::Proc(client) 则 `ipc_recv` 被拒。
/// 失败完整回滚:server 槽授予失败时清空已写入的 client 槽(不留半授予)。
///
/// 错误:id 未注册 → `-ENOENT`;server == caller → `-EACCES`;槽越界 →
/// `-EINVAL`;进程已亡(grant 阶段)→ `-EACCES`。
pub fn connect(
    caller: usize,
    id: usize,
    client_slot: usize,
    server_slot: usize,
) -> Result<(), usize> {
    // 1) 服务注册表查 id → server_pid(无 → -ENOENT)。SERVICES 锁在此释放,
    //    之后的 grant(TABLE 锁)与 SERVICES 无嵌套。
    let server_pid = match lookup(id) {
        Some(p) => p,
        None => return Err(crate::syscall::SYS_ERR_ENOENT),
    };
    // 2) 服务不得连接自身;槽越界 → -EINVAL(提前校验,防双向授予后才发现
    //    非法槽留半授予)。
    if server_pid == caller {
        return Err(crate::syscall::SYS_ERR_EACCES);
    }
    if client_slot >= crate::process::MAX_CAPS || server_slot >= crate::process::MAX_CAPS {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 3) 双向授予(先 client 槽再 server 槽;server 槽失败回滚 client 槽)。
    if let Err(e) = crate::process::grant_cap(caller, client_slot, server_pid) {
        return Err(crate::process::cap_errno(e));
    }
    if let Err(e) = crate::process::grant_cap(server_pid, server_slot, caller) {
        let _ = crate::process::clear_cap(caller, client_slot);
        return Err(crate::process::cap_errno(e));
    }
    Ok(())
}
