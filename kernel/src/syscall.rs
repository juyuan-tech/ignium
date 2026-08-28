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
        _ => {
            // 未知 syscall:-ENOSYS,返回到用户。
            unsafe { *frame.add(GPR_A0) = usize::MAX };
            unsafe { *frame.add(FRAME_SEPC) += 4 };
            false
        }
    }
}
