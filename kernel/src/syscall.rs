//! 系统调用(M2 T1/T2a):用户态 `ecall` 分发。
//!
//! 用户线程在 U 模式执行 `ecall`,陷入 `trap_handler`(scause=8,
//! SPP=0)。本模块按 **a7**(syscall 号)分发(M2-DESIGN §4.1),
//! 参数在 a0-a5,结果写回帧 a0(a1-a5 可作消息),随后 `sret` 回用户态。
//!
//! # ABI 常量
//! 帧 GPR 索引单一来源为 `arch::gpr`(ABI 契约,汇编保存顺序一致);
//! 本模块的 GPR_A* 常量是其别名,禁止手写魔数。

pub const SYSCALL_EXIT: usize = 1;
pub const SYSCALL_GET_TICKS: usize = 2;
pub const SYSCALL_IPC_SEND: usize = 3;
pub const SYSCALL_IPC_RECV: usize = 4;
pub const SYSCALL_SHM_MAP: usize = 5;
pub const SYSCALL_CAP_REVOKE: usize = 6;
pub const SYSCALL_CAP_DUP: usize = 7;
/// M3 T1:`sys_write`(fd=1 → UART 过渡占位,M3-2 uart_server 落地后删除)。
pub const SYSCALL_WRITE: usize = 8;
/// M3 T1:`sys_read` 占位(本轮返回 -ENOSYS,号保留,见 SYSCALLS.md)。
pub const SYSCALL_READ: usize = 9;

/// 负 errno 的 usize 编码(与 L1 ABI 一致,见 M2-DESIGN §4.1)。
/// 统一存放本模块:ipc.rs 的 `IPC_ERR_*` 与其别名,process::cap_errno
/// 亦引用 —— 单一事实来源。
pub const SYS_ERR_EINVAL: usize = usize::MAX;
pub const SYS_ERR_EACCES: usize = usize::MAX - 1;
pub const SYS_ERR_ENOENT: usize = usize::MAX - 2;
pub const SYS_ERR_ENOMEM: usize = usize::MAX - 3;
/// M3 T1:非法 fd(随 sys_write;SYSCALLS.md §错误码)。
pub const SYS_ERR_EBADF: usize = usize::MAX - 4;
/// M3 T1:缓冲越界/不可访问(随 sys_write;SYSCALLS.md §错误码)。
pub const SYS_ERR_EFAULT: usize = usize::MAX - 5;
/// 未实现/未知号(与 -EINVAL 同编码 usize::MAX,语义靠上下文区分)。
pub const SYS_ERR_ENOSYS: usize = usize::MAX;

/// `sys_write` 单次写入长度上限(4KB,与内核栈缓冲/逐页校验匹配)。
const MAX_WRITE_LEN: usize = 4096;

/// 帧 GPR 槽位(arch::gpr 索引别名;x(n+1),如 A0=x10)。
const GPR_A0: usize = crate::arch::gpr::X_A0; // x10
const GPR_A1: usize = crate::arch::gpr::X_A1; // x11
const GPR_A2: usize = crate::arch::gpr::X_A2; // x12
const GPR_A3: usize = crate::arch::gpr::X_A3; // x13
const GPR_A4: usize = crate::arch::gpr::X_A4; // x14
const GPR_A5: usize = crate::arch::gpr::X_A5; // x15
const GPR_A7: usize = crate::arch::gpr::X_A7; // x17
/// 帧 sepc 槽(与 riscv64.rs CS_SEPC 一致)。
const FRAME_SEPC: usize = 33;

/// 分发用户 ecall。
///
/// # Safety
/// `frame` 必须指向有效用户陷阱帧(由 trap_handler 校验),且当前确为
/// U 模式(scause=8)。返回 `true` = 线程请求退出(调用方走 sched::exit,
/// 不再 sret);`false` = 已写入结果并前移 sepc,应 sret 回用户。
///
/// M2 T2a:IPC SEND/RECV 无配对时经 `block_user_from_trap(frame)` **阻塞**
/// (该函数不返回,配对方投递后经帧恢复继续),能力校验失败返回负 errno。
pub unsafe fn handle(frame: *mut usize) -> bool {
    let a7 = unsafe { *frame.add(GPR_A7) };
    match a7 {
        SYSCALL_EXIT => true,
        SYSCALL_GET_TICKS => {
            let ticks = crate::logger::tick();
            unsafe { *frame.add(GPR_A0) = ticks as usize };
            // sepc 指向 ecall 指令本身;sret 前 +4 跳到下一条。
            unsafe { *frame.add(FRAME_SEPC) += 4 };
            false
        }
        SYSCALL_IPC_SEND => {
            let slot = unsafe { *frame.add(GPR_A0) };
            let msg = [
                unsafe { *frame.add(GPR_A1) },
                unsafe { *frame.add(GPR_A2) },
                unsafe { *frame.add(GPR_A3) },
                unsafe { *frame.add(GPR_A4) },
                unsafe { *frame.add(GPR_A5) },
            ];
            match crate::ipc::send(crate::sched::current_proc(), slot, msg) {
                Ok(crate::ipc::SendBlock::Done) => {
                    unsafe { *frame.add(GPR_A0) = 0 };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
                Ok(crate::ipc::SendBlock::NoPeer) => {
                    // 已登记 pending,立即阻塞(无调度点,配对不丢)。
                    // 不返回:配对方写入 a0/a1-a5/sepc+4 后经帧恢复回本点。
                    unsafe { crate::sched::block_user_from_trap(frame) };
                }
                Err(code) => {
                    unsafe { *frame.add(GPR_A0) = code };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
            }
        }
        SYSCALL_IPC_RECV => {
            let slot = unsafe { *frame.add(GPR_A0) };
            match crate::ipc::recv(crate::sched::current_proc(), slot) {
                Ok(crate::ipc::RecvBlock::Done(msg)) => {
                    unsafe { *frame.add(GPR_A0) = 0 };
                    for (i, w) in msg.iter().enumerate() {
                        unsafe { *frame.add(GPR_A1 + i) = *w };
                    }
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
                Ok(crate::ipc::RecvBlock::NoPeer) => {
                    unsafe { crate::sched::block_user_from_trap(frame) };
                }
                Err(code) => {
                    unsafe { *frame.add(GPR_A0) = code };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
            }
        }
        SYSCALL_SHM_MAP => {
            // a0=本槽, a1=对端槽, a2=len;成功返回 a0=shm_id,失败负 errno。
            let a_slot = unsafe { *frame.add(GPR_A0) };
            let b_slot = unsafe { *frame.add(GPR_A1) };
            let len = unsafe { *frame.add(GPR_A2) };
            match crate::shm::mmap_share(crate::sched::current_proc(), a_slot, b_slot, len) {
                Ok(id) => {
                    unsafe { *frame.add(GPR_A0) = id };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
                Err(code) => {
                    unsafe { *frame.add(GPR_A0) = code };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
            }
        }
        SYSCALL_CAP_REVOKE => {
            // a0=槽;成功 a0=0;Cap::Shm → 整页撤销,Cap::Proc → 清槽。
            let slot = unsafe { *frame.add(GPR_A0) };
            match crate::process::cap_revoke(crate::sched::current_proc(), slot) {
                Ok(()) => {
                    unsafe { *frame.add(GPR_A0) = 0 };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
                Err(e) => {
                    unsafe { *frame.add(GPR_A0) = crate::process::cap_errno(e) };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
            }
        }
        SYSCALL_CAP_DUP => {
            // a0=源槽, a1=目标槽;成功 a0=0。
            let from = unsafe { *frame.add(GPR_A0) };
            let to = unsafe { *frame.add(GPR_A1) };
            match crate::process::cap_duplicate(crate::sched::current_proc(), from, to) {
                Ok(()) => {
                    unsafe { *frame.add(GPR_A0) = 0 };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
                Err(e) => {
                    unsafe { *frame.add(GPR_A0) = crate::process::cap_errno(e) };
                    unsafe { *frame.add(FRAME_SEPC) += 4 };
                    false
                }
            }
        }
        SYSCALL_WRITE => {
            sys_write(frame);
            false
        }
        SYSCALL_READ => {
            // 本轮占位:返回 -ENOSYS(SYSCALLS.md 登记;9 号 READ 保留)。
            unsafe { *frame.add(GPR_A0) = SYS_ERR_ENOSYS };
            unsafe { *frame.add(FRAME_SEPC) += 4 };
            false
        }
        _ => {
            // 未知 syscall:-ENOSYS,返回到用户。
            unsafe { *frame.add(GPR_A0) = SYS_ERR_ENOSYS };
            unsafe { *frame.add(FRAME_SEPC) += 4 };
            false
        }
    }
}

/// M3 T1:`sys_write`(号 8)。语义见 SYSCALLS.md §sys_write(唯一来源)。
///
/// a0=fd, a1=buf, a2=len;成功 a0=写入字节数,失败负 errno。结果写回帧,
/// sepc 前移 4。当前地址空间即进程根表(syscall 上下文 satp 未切),
/// 逐页 `mmu::is_user_mapped` 校验(U 页),拷贝置 SUM 后直读用户缓冲。
fn sys_write(frame: *mut usize) {
    let fd = unsafe { *frame.add(GPR_A0) };
    let buf = unsafe { *frame.add(GPR_A1) };
    let len = unsafe { *frame.add(GPR_A2) };
    // 过渡占位(M3-1):仅 fd=1(stdout)→ UART;M3-2 uart_server 落地后
    // 删除(见 M3-DESIGN §4,微内核"内核直碰 UART"临时例外)。
    if fd != 1 {
        unsafe { *frame.add(GPR_A0) = SYS_ERR_EBADF };
        unsafe { *frame.add(FRAME_SEPC) += 4 };
        return;
    }
    if len > MAX_WRITE_LEN {
        unsafe { *frame.add(GPR_A0) = SYS_ERR_EINVAL };
        unsafe { *frame.add(FRAME_SEPC) += 4 };
        return;
    }
    if len > 0 {
        // 逐页校验 buf 在当前进程根表映射为**用户页**(防跨页越界/未映射
        // 缓冲 → -EFAULT;限定 U 页,防 S 模式拷贝放行内核区页泄漏内核内存)。
        let root = match crate::process::pid_root(crate::sched::current_proc()) {
            Some(r) => r,
            // 进程销毁竞态:地址空间已失效,按不可访问处理。
            None => {
                unsafe { *frame.add(GPR_A0) = SYS_ERR_EFAULT };
                unsafe { *frame.add(FRAME_SEPC) += 4 };
                return;
            }
        };
        let first = buf & !0xfff;
        let last = buf + len - 1;
        let mut va = first;
        while va <= last {
            if !crate::mmu::is_user_mapped(root, va) {
                unsafe { *frame.add(GPR_A0) = SYS_ERR_EFAULT };
                unsafe { *frame.add(FRAME_SEPC) += 4 };
                return;
            }
            va += 0x1000;
        }
    }
    // 拷入内核栈缓冲(≤ 4096;当前 satp = 进程根表)。**S 模式读 U 页须置
    // SUM**(RISC-V:无 SUM 时 S 模式对 U 页访问立即故障,实测 scause=0xd);
    // trap 恢复路径写回入口保存的 sstatus,临时置位不泄漏;SIE=0 无抢占。
    let mut kbuf = [0u8; MAX_WRITE_LEN];
    crate::arch::set_sum();
    for (i, slot) in kbuf.iter_mut().take(len).enumerate() {
        // SAFETY:buf 已逐页校验为当前进程根表的 U 页,且 SUM=1 使 S 模式
        // 可读;len ≤ MAX_WRITE_LEN 防栈缓冲越界。
        *slot = unsafe { core::ptr::read_volatile((buf + i) as *const u8) };
    }
    crate::arch::clear_sum();
    crate::uart::write_bytes(&kbuf[..len]);
    unsafe { *frame.add(GPR_A0) = len };
    unsafe { *frame.add(FRAME_SEPC) += 4 };
}
