//! Ignium 用户态共享库(M3-2):syscall helper / 常量 / panic handler / uart client。
//!
//! 被两枚 bin(hello / uart_server)共享。syscall 号与 `kernel/src/syscall.rs`
//! 一致(注释"须与 kernel 一致",禁止复制定义 —— docs/SYSCALLS.md 登记纪律);
//! 槽位编排 / 消息协议 / 固定 VA 见 M3-DESIGN §10.4 / §10.8。

#![no_std]

use core::arch::asm;

// ===== syscall 号(与 kernel/src/syscall.rs 一致,禁止复制定义)=====
/// 线程退出(kernel EXIT,不返回)。
pub const SYS_EXIT: usize = 1;
/// IPC 发送(a0=槽, a1-a5=5 字消息)。
pub const SYS_IPC_SEND: usize = 3;
/// IPC 接收(a0=槽;返回 a0 + a1-a5 消息)。
pub const SYS_IPC_RECV: usize = 4;
/// 共享内存映射(a0=本槽, a1=对端槽, a2=len)。
pub const SYS_SHM_MAP: usize = 5;
/// 能力槽复制(a0=源槽, a1=目标槽)。
pub const SYS_CAP_DUP: usize = 7;
/// 服务注册(a0=服务 id)。
pub const SYS_SERVICE_REGISTER: usize = 10;
/// 服务连接(a0=id, a1=client 槽, a2=server 槽)。
pub const SYS_SERVICE_CONNECT: usize = 11;
/// 设备页映射(a0=dev_id, a1=va;白名单见 kernel device.rs)。
pub const SYS_MAP_DEVICE: usize = 12;

// ===== M3-2 固定布局(与 M3-DESIGN §10.4 一致;禁止复制定义)=====
/// 服务端 accept 槽(持 Cap::Proc(client),收发请求/回复)。
pub const SERVER_ACCEPT_SLOT: usize = 0;
/// 服务端 SHM 槽(经 shm_map 收 Cap::Shm(id))。
pub const SERVER_SHM_SLOT: usize = 1;
/// 客户端 IPC 槽(经 service_connect 持 Cap::Proc(server))。
pub const CLIENT_IPC_SLOT: usize = 2;
/// 客户端 SHM 槽(经 cap_dup + shm_map 变 Cap::Shm(id))。
pub const CLIENT_SHM_SLOT: usize = 3;
/// UART 设备窗口 VA(避开 ELF 0x4000_0000 段 / SHM 0x5000_0000)。
pub const UART_MMIO_VA: usize = 0x6000_0000;
/// UART 服务 id(kernel services.rs 注册域 [1, MAX_SERVICES))。
pub const UART_SERVICE_ID: usize = 1;
/// 共享页固定 VA / 长度(与 kernel shm.rs SHM_VA / SHM_LEN 一致)。
pub const SHM_VA: usize = 0x5000_0000;
pub const SHM_LEN: usize = 4096;

// ===== 消息协议(M3-DESIGN §10.8)=====
/// WRITE 请求:arg1 = 数据长度,数据在 SHM_VA[0..len]。
pub const OP_WRITE: usize = 0x01;
/// READ 请求:arg1 = max_len,服务端读 RBR → 写 SHM_VA[0..n]。
pub const OP_READ: usize = 0x02;
/// PING 请求:连通性测试。
pub const OP_PING: usize = 0x03;
/// 回复标记:回复首字 = op | 0x80。
pub const OP_REPLY_FLAG: usize = 0x80;
/// 协议级错误状态(未知 op;数值与内核 -EINVAL 编码一致)。
pub const PROTO_ERR: usize = usize::MAX;

/// panic handler:无输出通道(打印须走 IPC 到 uart_server;服务未就绪时
/// 静默)。内核测试以 marker / 超时断言暴露 panic;spin_loop 缓解 empty_loop。
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// 系统调用 helper(a7=号, a0-a5=参数,返回 a0;错误为负 errno 编码)。
///
/// `clobber_abi("C")`:内核 syscall ABI 允许 clobber 调用者保存寄存器。
#[inline]
pub fn syscall(
    num: usize,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> usize {
    let ret: usize;
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
            in("a5") a5,
            in("a7") num,
            clobber_abi("C"),
            options(nostack)
        );
    }
    ret
}

/// 线程退出(内核 EXIT 不返回;防御性兜底不可达)。
pub fn sys_exit() -> ! {
    syscall(SYS_EXIT, 0, 0, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// IPC send:slot 须持 Cap::Proc(peer);返回 a0(0 = 已投递,负 = errno)。
pub fn sys_ipc_send(slot: usize, msg: &[usize; 5]) -> usize {
    syscall(
        SYS_IPC_SEND,
        slot,
        msg[0],
        msg[1],
        msg[2],
        msg[3],
        msg[4],
    )
}

/// IPC recv:slot 须持 Cap::Proc(src);返回 `[a0, a1..a5]`(a0=0 成功,消息
/// 在 a1-a5;负 = errno)。
pub fn sys_ipc_recv(slot: usize) -> [usize; 6] {
    let (a0, a1, a2, a3, a4, a5): (usize, usize, usize, usize, usize, usize);
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") slot => a0,
            out("a1") a1,
            out("a2") a2,
            out("a3") a3,
            out("a4") a4,
            out("a5") a5,
            in("a7") SYS_IPC_RECV,
            clobber_abi("C"),
            options(nostack)
        );
    }
    [a0, a1, a2, a3, a4, a5]
}

/// shm_map:a_slot 持 Cap::Proc(peer);成功返回 shm_id,失败负 errno。
pub fn sys_shm_map(a_slot: usize, b_slot: usize, len: usize) -> usize {
    syscall(SYS_SHM_MAP, a_slot, b_slot, len, 0, 0, 0)
}

/// cap_dup:复制 from 槽到 to 槽(共享页授予前保 IPC 槽)。
pub fn sys_cap_dup(from: usize, to: usize) -> usize {
    syscall(SYS_CAP_DUP, from, to, 0, 0, 0, 0)
}

/// service_register:服务进程自报注册(重复 → -EEXIST)。
pub fn sys_service_register(id: usize) -> usize {
    syscall(SYS_SERVICE_REGISTER, id, 0, 0, 0, 0, 0)
}

/// service_connect:客户端连接服务(双向授予 Cap::Proc;未注册 → -ENOENT)。
pub fn sys_service_connect(id: usize, client_slot: usize, server_slot: usize) -> usize {
    syscall(SYS_SERVICE_CONNECT, id, client_slot, server_slot, 0, 0, 0)
}

/// map_device:白名单设备 MMIO 页映射进本进程(排他 claim)。
pub fn sys_map_device(dev_id: usize, va: usize) -> usize {
    syscall(SYS_MAP_DEVICE, dev_id, va, 0, 0, 0, 0)
}

// ===== uart client 库(打印/读取经 IPC + SHM 到 uart_server)=====

/// 连接 UART 服务并建立 SHM 通道(hello 打印前调用)。
///
/// 返回 0 = 就绪;非 0 = errno(服务未注册 → -ENOENT,调用方静默跳过打印)。
/// 步骤:connect 双向授予 → cap_dup 保 IPC 槽 → shm_map 双槽变 Cap::Shm。
pub fn uart_init() -> usize {
    let rc = sys_service_connect(UART_SERVICE_ID, CLIENT_IPC_SLOT, SERVER_ACCEPT_SLOT);
    if rc != 0 {
        return rc;
    }
    let rc = sys_cap_dup(CLIENT_IPC_SLOT, CLIENT_SHM_SLOT);
    if rc != 0 {
        return rc;
    }
    sys_shm_map(CLIENT_SHM_SLOT, SERVER_SHM_SLOT, SHM_LEN)
}

/// uart client:写字节到 UART(阻塞;返回输出字节数或负 errno)。
pub fn uart_write(bytes: &[u8]) -> isize {
    let n = bytes.len().min(SHM_LEN);
    for (i, b) in bytes[..n].iter().enumerate() {
        // SAFETY:SHM_VA 页已经 uart_init 建立映射(共享通道)。
        unsafe { core::ptr::write_volatile((SHM_VA + i) as *mut u8, *b) };
    }
    let req = [OP_WRITE, n, 0, 0, 0];
    if sys_ipc_send(CLIENT_IPC_SLOT, &req) != 0 {
        return -1;
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 {
        return r[0] as isize; // IPC errno
    }
    let status = r[2] as isize;
    if status != 0 {
        return status; // 协议状态(未知 op 等)
    }
    r[3] as isize // 已输出字节数
}

/// uart client:读 UART 至 buf(非阻塞;返回读取字节数,无数据 0,错误负)。
pub fn uart_read(buf: &mut [u8]) -> isize {
    let max = buf.len().min(SHM_LEN);
    let req = [OP_READ, max, 0, 0, 0];
    if sys_ipc_send(CLIENT_IPC_SLOT, &req) != 0 {
        return -1;
    }
    let r = sys_ipc_recv(CLIENT_IPC_SLOT);
    if r[0] != 0 {
        return r[0] as isize;
    }
    let status = r[2] as isize;
    if status != 0 {
        return status;
    }
    let n = r[3].min(buf.len());
    for (i, slot) in buf.iter_mut().take(n).enumerate() {
        // SAFETY:服务端已把字节写入 SHM_VA[0..n](同页已映射)。
        *slot = unsafe { core::ptr::read_volatile((SHM_VA + i) as *const u8) };
    }
    n as isize
}
