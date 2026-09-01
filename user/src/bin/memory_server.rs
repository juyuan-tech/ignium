//! M3-3:内存服务进程 —— 用户态申请/释放物理页的唯一入口(纯服务授权)。
//!
//! # 定位
//! 内核 ELF 加载器加载运行。**页池由引导编排注入**(当前 = 内核测试 T1/T2
//! 在 spawn 后经 `pages::alloc` + `grant_typed_cap` 写入本进程槽 1..=4,
//! 见 DEFERRED D33);内核**不暴露**通用分配 syscall(避免 ambient 授权,
//! M3-DESIGN §11.3)。
//!
//! # 主流程
//! 1. `sys_service_register(MEMORY_SERVICE_ID)` —— 内核服务注册表自报;
//! 2. 服务循环:`sys_ipc_recv(SERVER_ACCEPT_SLOT)`(阻塞配对)→ 按 op 处理
//!    → `sys_ipc_send(SERVER_ACCEPT_SLOT, 回复)`。
//!
//! # 协议(M3-DESIGN §11.8)
//! 请求 `[op, arg1, 0, 0, 0]`:
//! - ALLOC(0x04):arg1 = 客户端收页槽;选池内满槽经 `mem_grant(i, peer=0,
//!   dst)` 移交页给客户端,池位清空;
//! - FREE(0x05):arg1 = 客户端归还页槽;选一个空池槽 r → 回复 `[OK, recv_slot
//!   =r]`,客户端随后 `mem_grant(归还页槽, peer=2, r)` 送回,池位回填。
//! 回复 `[op|0x80, status, arg2, 0, 0]`(status=0 成功;池空 → ERR_ENOMEM;
//! 未知 op → PROTO_ERR)。
//!
//! # 池位状态
//! 用户态单线程进程,池位满/空在本地 `[bool; 4]` 维护(经 `&mut` 传递,无需
//! 原子)。归还槽采用**乐观置位**(D35):回复 FREE 即标满,客户端归还崩溃
//! 前 mem_grant 失败可优雅降级 ERR_ENOMEM。

#![no_std]
#![no_main]

use ignium_user::*;

/// 用户入口:内核加载器建初始帧(a0=argc、a1=argv,本服务忽略)。
#[no_mangle]
pub extern "C" fn _start(_argc: usize, _argv: usize) -> ! {
    // 1) 自报注册(客户端经 service_connect 定位本进程)。
    if sys_service_register(MEMORY_SERVICE_ID) != 0 {
        sys_exit();
    }
    // 池初始全满:4 页由内核测试注入槽 1..=4(引导编排,D33)。
    let mut pool_full = [true; 4];
    // 2) 服务循环:阻塞配对 recv → 按 op 处理 → 回复。
    loop {
        let r = sys_ipc_recv(SERVER_ACCEPT_SLOT);
        if r[0] != 0 {
            continue; // IPC errno(防御;正常配对路径不达)
        }
        let op = r[1];
        let arg1 = r[2];
        let reply = match op {
            OP_ALLOC => match alloc_page(&mut pool_full, arg1) {
                Ok(()) => [op | OP_REPLY_FLAG, 0, 0, 0, 0],
                Err(e) => [op | OP_REPLY_FLAG, e, 0, 0, 0],
            },
            OP_FREE => match free_slot(&mut pool_full) {
                Some(recv_slot) => [op | OP_REPLY_FLAG, 0, recv_slot, 0, 0],
                None => [op | OP_REPLY_FLAG, ERR_ENOMEM, 0, 0, 0],
            },
            _ => [op | OP_REPLY_FLAG, PROTO_ERR, 0, 0, 0],
        };
        let _ = sys_ipc_send(SERVER_ACCEPT_SLOT, &reply);
    }
}

/// 从池中取一满槽,经 `mem_grant` 把 `Cap::Page` 移交给客户端(dst =
/// 客户端收页槽)。成功清池位;无满槽(池空)→ `Err(ERR_ENOMEM)`。mem_grant
/// 失败(防御:乐观池位误判,见 D35)→ 原样返回内核 errno。
fn alloc_page(pool: &mut [bool; 4], client_dst: usize) -> Result<(), usize> {
    for i in SERVER_POOL_START..=SERVER_POOL_END {
        if pool[i - SERVER_POOL_START] {
            // peer 槽 = 服务端 accept 槽(经 service_connect 持 Cap::Proc(client))。
            let rc = sys_mem_grant(i, SERVER_ACCEPT_SLOT, client_dst);
            if rc != 0 {
                return Err(rc);
            }
            pool[i - SERVER_POOL_START] = false;
            return Ok(());
        }
    }
    Err(ERR_ENOMEM)
}

/// 归还页:选一个**空**池槽 r(乐观置位为满,D35)返回给服务循环 —— 回复
/// `[OK, recv_slot=r]` 后,客户端经 `mem_grant` 把页送回本槽,池位回填。
/// 无空槽(池已满,不应发生)→ `None`。
fn free_slot(pool: &mut [bool; 4]) -> Option<usize> {
    for i in SERVER_POOL_START..=SERVER_POOL_END {
        if !pool[i - SERVER_POOL_START] {
            pool[i - SERVER_POOL_START] = true; // 乐观置位(等待归还落位)
            return Some(i);
        }
    }
    None
}
