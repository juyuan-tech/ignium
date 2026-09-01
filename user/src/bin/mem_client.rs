//! M3-3:内存服务客户端 —— 经 mem_server 申请物理页、映射读写、归还(往返)。
//!
//! # 流程(纯服务授权:M3-DESIGN §11.3)
//! 1. `sys_service_connect(SERVICE_MEMORY, CLIENT_IPC_SLOT=2, SERVER_ACCEPT_SLOT=0)`
//!    —— 双向授予 `Cap::Proc`(内核注册表定位 mem_server);
//! 2. 申请页:send `[OP_ALLOC, 收页槽=3]` → recv `[OK]` —— 服务端经
//!    `mem_grant` 把 `Cap::Page` 移入本进程槽 3;
//! 3. 映射:`sys_mem_map(3, MEM_VA=0x7000_0000)` → 写 MAGIC + 读回比对
//!    (证明 U RW 物理页真实可访问,非影子/非故障页);
//! 4. 归还:send `[OP_FREE, 归还页槽=3]` → recv `[OK, recv_slot=r]` →
//!    `sys_mem_grant(3, peer=2, r)` 把页送回服务端槽 r(池位回填可复用);
//! 5. 完成信号:写 `0xC0DE_0000 | argc` 到 marker 页(内核测试轮询;
//!    marker VA 0x4000_2000 须与 kernel tests.rs 一致)→ `sys_exit`。

#![no_std]
#![no_main]

use ignium_user::*;

/// 共享标记页地址(与 kernel tests.rs 的 ELF_MARKER_VA 一致;完成往返后写入)。
const MARKER_VA: usize = 0x4000_2000;
/// 页校验魔数(写 MEM_VA + 读回比对,证明映射页真实可读写)。
const MAGIC: usize = 0xFEED_FACE;

/// 用户入口:内核加载器建初始帧(a0=argc 用于 marker, a1=argv 忽略)。
#[no_mangle]
pub extern "C" fn _start(argc: usize, _argv: usize) -> ! {
    // 1) 连接服务(双向授予 Cap::Proc;未注册 → -ENOENT,静默退出)。
    if sys_service_connect(MEMORY_SERVICE_ID, CLIENT_IPC_SLOT, SERVER_ACCEPT_SLOT) != 0 {
        sys_exit();
    }
    // 2) 申请页:send [OP_ALLOC, client 收页槽=3] → recv [OK](status=0)。
    if sys_ipc_send(CLIENT_IPC_SLOT, &[OP_ALLOC, CLIENT_PAGE_SLOT, 0, 0, 0]) != 0 {
        sys_exit();
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (OP_ALLOC | OP_REPLY_FLAG) || r[2] != 0 {
        sys_exit();
    }
    // 3) 映射到 MEM_VA,写/读校验(证明 U RW 物理页真实可访问)。
    if sys_mem_map(CLIENT_PAGE_SLOT, MEM_VA) != 0 {
        sys_exit();
    }
    // SAFETY:MEM_VA 页已经 sys_mem_map 建立 U RW 映射(本进程根表)。
    unsafe { core::ptr::write_volatile(MEM_VA as *mut usize, MAGIC) };
    let readback = unsafe { core::ptr::read_volatile(MEM_VA as *const usize) };
    if readback != MAGIC {
        sys_exit();
    }
    // 4) 归还:send [OP_FREE, 归还页槽=3] → recv [OK, recv_slot=r] →
    //    mem_grant(3, peer=2, r) 把页送回服务端(池位回填;归还协议见 D34)。
    if sys_ipc_send(CLIENT_IPC_SLOT, &[OP_FREE, CLIENT_PAGE_SLOT, 0, 0, 0]) != 0 {
        sys_exit();
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (OP_FREE | OP_REPLY_FLAG) || r[2] != 0 {
        sys_exit();
    }
    let recv_slot = r[3];
    if sys_mem_grant(CLIENT_PAGE_SLOT, CLIENT_IPC_SLOT, recv_slot) != 0 {
        sys_exit();
    }
    // 5) 全流程完成信号(内核测试轮询;证明申请→映射→读写→归还往返)。
    // SAFETY:marker 页由内核测试在 spawn 前映射进本进程根表(同 boot_elf_test)。
    unsafe { core::ptr::write_volatile(MARKER_VA as *mut usize, 0xC0DE_0000 | argc) };
    sys_exit();
}
