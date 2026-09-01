//! M3-4:ramfs 文件系统服务 —— 纯用户态,一切皆能力。
//!
//! # 定位
//! 用户态文件系统服务:文件/目录/名字全是本服务内资源(内核**零新 syscall**、
//! **零新 Cap** 变体);文件句柄**绑定连接**(专用连接槽 0,accept-any 变特定
//! 接收,无全局 fd 命名空间)。数据面 = 共享内存窗口(`shm_map`,Cap::Shm),
//! 存储面 = mem_server 服务链(Cap::Page 经 IPC 申请,见 M3-DESIGN §12)。
//!
//! # 槽位(MAX_CAPS=8)
//! - `0` 连接槽:收发 IPC,持 `Cap::Proc(client)`(客户端 service_connect 授入;
//!   启动时为空槽 = accept-any);
//! - `1` SHM 窗:持 `Cap::Shm`(数据载荷窗,客户端 shm_map 授入);
//! - `2` mem_server:持 `Cap::Proc(mem_server)`(OP_ALLOC/FREE 控制面 +
//!   mem_grant 归还 peer 槽);
//! - `3..=6` 文件页槽:`Cap::Page`,**fd = 槽-3**;
//! - `7` 备用。
//!
//! # 文件表(MAX_FILES=4,每文件单页,D37)
//! `state: Free|Open|Closed` + `name[24]` + `size`。**create-or-reopen**:
//! open 不存在名 → 建(经 mem_server 分配页 + mem_map 至 RAMFS_VA);存在且
//! Closed → 重开(同 fd);Open → EEXIST。**close 保槽**(Open→Closed,页保留),
//! **unlink 按 fd**(经 mem_grant 归还页 + 清表)。未 unlink 的页由内核
//! `process::destroy` 钩子回收(revoke-before-destroy,无泄漏无 double-free)。
//!
//! # 五 op 协议(M3-DESIGN §12.4)
//! 请求 `[op,arg1,arg2,arg3,arg4]`,回复首字 `op|0x80`、次字 status(0/负 errno):
//! OPEN(0x06) name_len/name×3 字 → fd;READ(0x07) fd/off/len → bytes_read;
//! WRITE(0x08) fd/off/len → bytes_written(载荷经 SHM_VA);CLOSE(0x09) fd;
//! UNLINK(0x0A) fd。errno 语义见 `USER_ERR_*` 常量与 docs/SYSCALLS.md。

#![no_std]
#![no_main]

use ignium_user::*;

/// 文件槽状态。
#[derive(Clone, Copy, PartialEq)]
enum FileState {
    /// 槽未用(可被 open 建新文件)。
    Free,
    /// 已打开(重复 open → EEXIST)。
    Open,
    /// 已 close(保槽:页保留,可被 open 重开同 fd / 被 unlink 释放,D40)。
    Closed,
}

/// 文件表项:一个文件 = 一个文件页槽(fd = 槽-3)。
#[derive(Clone, Copy)]
struct FileEntry {
    state: FileState,
    name: [u8; NAME_MAX],
    name_len: usize,
    size: usize,
}

impl FileEntry {
    /// 全新空项(名字零、size 0)。
    const fn free() -> Self {
        FileEntry {
            state: FileState::Free,
            name: [0; NAME_MAX],
            name_len: 0,
            size: 0,
        }
    }
}

/// 用户入口:内核加载器建初始帧(a0=argc、a1=argv,本服务忽略)。
#[no_mangle]
pub extern "C" fn _start(_argc: usize, _argv: usize) -> ! {
    // 1) 自报注册(客户端经 service_connect 定位本服务)。
    if sys_service_register(RAMFS_SERVICE_ID) != 0 {
        sys_exit();
    }
    // 2) 连接 mem_server(存储面服务链):槽 2 持 Cap::Proc(mem_server) ——
    //    OP_ALLOC/FREE 控制面 + mem_grant 归还 peer 槽(见 M3-DESIGN §12.2)。
    if sys_service_connect(MEMORY_SERVICE_ID, CLIENT_IPC_SLOT, SERVER_ACCEPT_SLOT) != 0 {
        sys_exit();
    }
    // 3) 文件表 4 项全 Free。
    let mut table = [FileEntry::free(); MAX_FILES];
    // 4) 服务循环:阻塞配对 recv(槽 0)→ 按 op 处理 → 回复。
    loop {
        let r = sys_ipc_recv(SERVER_ACCEPT_SLOT);
        if r[0] != 0 {
            continue; // IPC errno(防御;正常配对路径不达)
        }
        let op = r[1];
        let reply = match op {
            OP_FS_OPEN => fs_open(&mut table, r[2], r[3], r[4], r[5]),
            OP_FS_READ => fs_read(&mut table, r[2], r[3], r[4]),
            OP_FS_WRITE => fs_write(&mut table, r[2], r[3], r[4]),
            OP_FS_CLOSE => fs_close(&mut table, r[2]),
            OP_FS_UNLINK => fs_unlink(&mut table, r[2]),
            _ => [op | OP_REPLY_FLAG, PROTO_ERR, 0, 0, 0],
        };
        let _ = sys_ipc_send(SERVER_ACCEPT_SLOT, &reply);
    }
}

/// OP_FS_OPEN:按名开/建文件。返回回复 `[op|0x80, status, fd, 0, 0]`。
///
/// name_len 非法(0/>24)→ EINVAL;同名 Open → EEXIST;同名 Closed → 重开
/// (同 fd,页保留);不存在 → 建新文件(经 mem_server 分配页 + mem_map);
/// 表满 → ENOMEM。
fn fs_open(
    table: &mut [FileEntry; MAX_FILES],
    name_len: usize,
    w0: usize,
    w1: usize,
    w2: usize,
) -> [usize; 5] {
    // 1) name_len 合法域 [1, NAME_MAX](0/超长 → EINVAL)。
    if name_len == 0 || name_len > NAME_MAX {
        return [OP_FS_OPEN | OP_REPLY_FLAG, USER_ERR_EINVAL, 0, 0, 0];
    }
    let name = name_from_words(&[w0, w1, w2]);
    // 2) 同名查找:Open → EEXIST;Closed → 重开(同 fd,页保留)。Free 项名字
    //    恒空(name_len=0),name_len ≥ 1 不可能命中(防御返回 PROTO_ERR)。
    for fd in 0..MAX_FILES {
        if table[fd].name_len == name_len && table[fd].name[..name_len] == name[..name_len] {
            match table[fd].state {
                FileState::Open => return [OP_FS_OPEN | OP_REPLY_FLAG, USER_ERR_EEXIST, 0, 0, 0],
                FileState::Closed => {
                    table[fd].state = FileState::Open;
                    return [OP_FS_OPEN | OP_REPLY_FLAG, 0, fd, 0, 0];
                }
                FileState::Free => return [OP_FS_OPEN | OP_REPLY_FLAG, PROTO_ERR, 0, 0, 0],
            }
        }
    }
    // 3) 未找到 → 建新文件:找一个 Free 槽(fd = 槽-3),经 mem_server 分配页。
    for fd in 0..MAX_FILES {
        if table[fd].state == FileState::Free {
            return match alloc_file_page(fd) {
                Ok(()) => {
                    table[fd] = FileEntry {
                        state: FileState::Open,
                        name,
                        name_len,
                        size: 0,
                    };
                    [OP_FS_OPEN | OP_REPLY_FLAG, 0, fd, 0, 0]
                }
                Err(e) => [OP_FS_OPEN | OP_REPLY_FLAG, e, 0, 0, 0],
            };
        }
    }
    // 4) 表满(4 项全被不同名占用)→ ENOMEM。
    [OP_FS_OPEN | OP_REPLY_FLAG, ERR_ENOMEM, 0, 0, 0]
}

/// 经 mem_server 服务链为文件 fd 分配一页:OP_ALLOC(收页槽 = 3+fd)→ 收
/// `Cap::Page` 进文件页槽 → mem_map 至 `RAMFS_VA + fd*4096`(本服务数据页)。
/// 任一环节失败 → 原样返回协议 errno(池空 = ERR_ENOMEM 等)。
fn alloc_file_page(fd: usize) -> Result<(), usize> {
    let page_slot = RAMFS_FILE_PAGE_START + fd;
    if sys_ipc_send(CLIENT_IPC_SLOT, &[OP_ALLOC, page_slot, 0, 0, 0]) != 0 {
        return Err(ERR_ENOMEM); // 防御:IPC 控制面失败
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (OP_ALLOC | OP_REPLY_FLAG) || r[2] != 0 {
        return Err(ERR_ENOMEM);
    }
    if sys_mem_map(page_slot, RAMFS_VA + fd * 4096) != 0 {
        return Err(ERR_ENOMEM);
    }
    Ok(())
}

/// OP_FS_READ:fd 偏移读。EOF(offset ≥ size)→ 回 0 字节(非错误);否则
/// `n = min(len, size-offset)` 截断,数据经 SHM_VA[0..n] 拷回。
fn fs_read(
    table: &mut [FileEntry; MAX_FILES],
    fd: usize,
    offset: usize,
    len: usize,
) -> [usize; 5] {
    let entry = match table.get_mut(fd) {
        Some(e) if e.state == FileState::Open => e,
        _ => return [OP_FS_READ | OP_REPLY_FLAG, USER_ERR_EBADF, 0, 0, 0],
    };
    // EOF:offset ≥ size → 0 字节(非错误;空文件 offset=0 亦命中)。
    if offset >= entry.size {
        return [OP_FS_READ | OP_REPLY_FLAG, 0, 0, 0, 0];
    }
    let n = core::cmp::min(len, entry.size - offset);
    let page_va = RAMFS_VA + fd * 4096;
    // SAFETY:文件页已 mem_map(open 时,本服务根表);SHM 窗已由客户端
    // shm_map 授入(槽 1,同页已映射)。两端 VA 均在 [0, 4096) 内。
    for i in 0..n {
        let b = unsafe { core::ptr::read_volatile((page_va + offset + i) as *const u8) };
        unsafe { core::ptr::write_volatile((SHM_VA + i) as *mut u8, b) };
    }
    [OP_FS_READ | OP_REPLY_FLAG, 0, n, 0, 0]
}

/// OP_FS_WRITE:fd 偏移写。载荷经 SHM_VA[0..len] 拷入文件页;size 更新为
/// `max(size, offset+len)`。offset 越界(≥4KB 或 off+len>4KB)→ EINVAL。
fn fs_write(
    table: &mut [FileEntry; MAX_FILES],
    fd: usize,
    offset: usize,
    len: usize,
) -> [usize; 5] {
    let entry = match table.get_mut(fd) {
        Some(e) if e.state == FileState::Open => e,
        _ => return [OP_FS_WRITE | OP_REPLY_FLAG, USER_ERR_EBADF, 0, 0, 0],
    };
    // offset 越界(≥4KB 或 off+len > 4KB)→ EINVAL。
    let end = match offset.checked_add(len) {
        Some(e) if offset < 4096 && e <= 4096 => e,
        _ => return [OP_FS_WRITE | OP_REPLY_FLAG, USER_ERR_EINVAL, 0, 0, 0],
    };
    let page_va = RAMFS_VA + fd * 4096;
    // SAFETY:文件页已 mem_map(open 时,本服务根表);载荷在 SHM_VA[0..len]
    // (客户端写入,同页已映射)。两端 VA 均在 [0, 4096) 内。
    for i in 0..len {
        let b = unsafe { core::ptr::read_volatile((SHM_VA + i) as *const u8) };
        unsafe { core::ptr::write_volatile((page_va + offset + i) as *mut u8, b) };
    }
    entry.size = entry.size.max(end);
    [OP_FS_WRITE | OP_REPLY_FLAG, 0, len, 0, 0]
}

/// OP_FS_CLOSE:fd 关闭。Open → Closed(保槽:页保留,可被 open 重开 / unlink
/// 释放,见 D40)。
fn fs_close(table: &mut [FileEntry; MAX_FILES], fd: usize) -> [usize; 5] {
    let entry = match table.get_mut(fd) {
        Some(e) if e.state == FileState::Open => e,
        _ => return [OP_FS_CLOSE | OP_REPLY_FLAG, USER_ERR_EBADF, 0, 0, 0],
    };
    entry.state = FileState::Closed;
    [OP_FS_CLOSE | OP_REPLY_FLAG, 0, 0, 0, 0]
}

/// OP_FS_UNLINK:按 fd 删除文件。释放页(OP_FREE → recv_slot → mem_grant 归还
/// mem_server,移交隐含解除文件页映射,D34)+ 清表。Open/Closed 均可 unlink;
/// Free(未用槽)→ EBADF。
fn fs_unlink(table: &mut [FileEntry; MAX_FILES], fd: usize) -> [usize; 5] {
    let entry = match table.get_mut(fd) {
        Some(e) if e.state != FileState::Free => e,
        _ => return [OP_FS_UNLINK | OP_REPLY_FLAG, USER_ERR_EBADF, 0, 0, 0],
    };
    // 归还文件页:OP_FREE(归还页槽 = 3+fd)→ 收 recv_slot → mem_grant 送回。
    let page_slot = RAMFS_FILE_PAGE_START + fd;
    if sys_ipc_send(CLIENT_IPC_SLOT, &[OP_FREE, page_slot, 0, 0, 0]) != 0 {
        return [OP_FS_UNLINK | OP_REPLY_FLAG, ERR_ENOMEM, 0, 0, 0];
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 || r[1] != (OP_FREE | OP_REPLY_FLAG) || r[2] != 0 {
        return [OP_FS_UNLINK | OP_REPLY_FLAG, ERR_ENOMEM, 0, 0, 0];
    }
    let recv_slot = r[3];
    if sys_mem_grant(page_slot, CLIENT_IPC_SLOT, recv_slot) != 0 {
        return [OP_FS_UNLINK | OP_REPLY_FLAG, ERR_ENOMEM, 0, 0, 0];
    }
    // 清表(页槽已空;mem_grant 移交隐含解除文件页映射,D34)。
    *entry = FileEntry::free();
    [OP_FS_UNLINK | OP_REPLY_FLAG, 0, 0, 0, 0]
}
