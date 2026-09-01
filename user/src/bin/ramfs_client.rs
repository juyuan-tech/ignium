//! M3-4:ramfs 客户端 —— 经 ramfs_server 开/写/读/关/删文件(全链路)。
//!
//! # 流程(M3-DESIGN §12.5;能力链:客户端自建 Shm 数据窗 + 存储页经服务链)
//! 1. `sys_service_connect(RAMFS_SERVICE_ID, 2, 0)` —— 双向授予 Cap::Proc;
//! 2. 建 SHM 数据窗:`cap_dup(2→3)` + `shm_map(3, server_slot=1, 4096)`;
//! 3. OPEN "test.txt"(create-or-reopen:不存在 → 建,收 fd);
//! 4. **内联负面**:空文件读 EOF(0 字节)→ 重复 open(EEXIST)→ 坏 fd=99
//!    (EBADF)→ 越界写 off=4095 len=2(EINVAL);
//! 5. WRITE "hello ramfs"(11B 经 SHM_VA,校验 written=11)→ READ 回读比对;
//! 6. CLOSE → UNLINK(close 保槽后仍可 unlink,D40)→ 写 marker
//!    `0xC0DE_0000|argc` → sys_exit。
//!
//! 任一断言失败 → 提前 sys_exit,marker 不到,内核测试侧 guard 超时 → 断言
//! 失败(同 mem_client 模式)。

#![no_std]
#![no_main]

use ignium_user::*;

/// 共享标记页地址(与 kernel tests.rs 的 ELF_MARKER_VA 一致;完成往返后写入)。
const MARKER_VA: usize = 0x4000_2000;
/// 写/读回读校验载荷(11B)。
const PAYLOAD: &[u8] = b"hello ramfs";

/// 用户入口:内核加载器建初始帧(a0=argc 用于 marker, a1=argv 忽略)。
#[no_mangle]
pub extern "C" fn _start(argc: usize, _argv: usize) -> ! {
    // 1) 连接 ramfs 服务(槽 2 持 Cap::Proc(ramfs_server);未注册 → -ENOENT)。
    if sys_service_connect(RAMFS_SERVICE_ID, CLIENT_IPC_SLOT, SERVER_ACCEPT_SLOT) != 0 {
        sys_exit();
    }
    // 2) 建 SHM 数据窗:cap_dup 保 IPC 槽 → shm_map 双槽变 Cap::Shm。
    if sys_cap_dup(CLIENT_IPC_SLOT, CLIENT_SHM_SLOT) != 0 {
        sys_exit();
    }
    if sys_shm_map(CLIENT_SHM_SLOT, SERVER_SHM_SLOT, SHM_LEN) != 0 {
        sys_exit();
    }
    // 3) OPEN "test.txt"(不存在 → 建,收 fd)。
    let name = b"test.txt";
    let (w, name_len) = name_to_words(name);
    let open_reply = fs_result(OP_FS_OPEN, &[name_len, w[0], w[1], w[2]]);
    if open_reply[1] != 0 {
        sys_exit();
    }
    let fd = open_reply[2];
    // 4) 内联负面用例(任一失败 → 提前 exit,marker 不到)。谓词帧:
    //    reply = [op|0x80, status, result, 0, 0],故 r[1]=status、r[2]=result。
    //    a) EOF:空文件 offset=0 read → 0 字节(非错误)。
    if fs_request(OP_FS_READ, &[fd, 0, 16, 0], |r| r[1] == 0 && r[2] == 0).is_err() {
        sys_exit();
    }
    //    b) 重复 open → EEXIST(文件处于 Open,create-or-reopen 拒绝)。
    if fs_request(OP_FS_OPEN, &[name_len, w[0], w[1], w[2]], |r| {
        r[1] == USER_ERR_EEXIST
    })
    .is_err()
    {
        sys_exit();
    }
    //    c) 坏 fd=99(表外)→ EBADF(用户协议层复活,D39)。
    if fs_request(OP_FS_READ, &[99, 0, 1, 0], |r| r[1] == USER_ERR_EBADF).is_err() {
        sys_exit();
    }
    //    d) 越界写(off=4095, len=2 → off+len=4097 > 4096)→ EINVAL。
    if fs_request(OP_FS_WRITE, &[fd, 4095, 2, 0], |r| r[1] == USER_ERR_EINVAL).is_err() {
        sys_exit();
    }
    // 5) WRITE 载荷(11B 经 SHM_VA),校验 written=11。
    // SAFETY:SHM 窗已建立(shm_map 成功,同页已映射)。
    for (i, b) in PAYLOAD.iter().enumerate() {
        unsafe { core::ptr::write_volatile((SHM_VA + i) as *mut u8, *b) };
    }
    if fs_request(OP_FS_WRITE, &[fd, 0, PAYLOAD.len(), 0], |r| {
        r[1] == 0 && r[2] == PAYLOAD.len()
    })
    .is_err()
    {
        sys_exit();
    }
    // 6) READ 回读,与载荷逐字节比对(证明 SHM 窗数据面 + 文件页存储链)。
    if fs_request(OP_FS_READ, &[fd, 0, PAYLOAD.len(), 0], |r| {
        r[1] == 0 && r[2] == PAYLOAD.len()
    })
    .is_err()
    {
        sys_exit();
    }
    for (i, b) in PAYLOAD.iter().enumerate() {
        // SAFETY:服务端已把文件字节写入 SHM_VA[0..n](同页已映射)。
        let got = unsafe { core::ptr::read_volatile((SHM_VA + i) as *const u8) };
        if got != *b {
            sys_exit();
        }
    }
    // 7) CLOSE → UNLINK(close 保槽后仍可 unlink,D40)。
    if fs_request(OP_FS_CLOSE, &[fd, 0, 0, 0], |r| r[1] == 0).is_err() {
        sys_exit();
    }
    if fs_request(OP_FS_UNLINK, &[fd, 0, 0, 0], |r| r[1] == 0).is_err() {
        sys_exit();
    }
    // 8) 全流程完成信号(内核测试轮询;证明 open→write→read→close→unlink
    //    全链路往返)。
    // SAFETY:marker 页由内核测试在 spawn 前映射进本进程根表(同 boot_elf_test)。
    unsafe { core::ptr::write_volatile(MARKER_VA as *mut usize, 0xC0DE_0000 | argc) };
    sys_exit();
}

/// 发请求 + 收回复,校验回复帧 `[op|0x80, status, result, 0, 0]` 满足谓词。
/// send/recv 失败或 op 帧不符 → Err。谓词收**完整回复数组**(reply[1]=status,
/// reply[2]=result)。
fn fs_request(
    op: usize,
    args: &[usize; 4],
    ok: impl FnOnce(&[usize; 5]) -> bool,
) -> Result<(), ()> {
    if sys_ipc_send(CLIENT_IPC_SLOT, &[op, args[0], args[1], args[2], args[3]]) != 0 {
        return Err(());
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (op | OP_REPLY_FLAG) {
        return Err(());
    }
    let reply = [r[1], r[2], r[3], r[4], r[5]];
    if ok(&reply) {
        Ok(())
    } else {
        Err(())
    }
}

/// 发请求 + 收回复,返回回复数组(调用方自行断言;帧校验同 `fs_request`)。
fn fs_result(op: usize, args: &[usize; 4]) -> [usize; 5] {
    if sys_ipc_send(CLIENT_IPC_SLOT, &[op, args[0], args[1], args[2], args[3]]) != 0 {
        sys_exit();
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (op | OP_REPLY_FLAG) {
        sys_exit();
    }
    [r[1], r[2], r[3], r[4], r[5]]
}
