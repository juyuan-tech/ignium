//! 系统调用(M2 T1):用户态 `ecall` 分发。
//!
//! 用户线程在 U 模式执行 `ecall`,陷入 `trap_handler`(scause=8,
//! SPP=0)。本模块按 **a7**(syscall 号)分发(M2-DESIGN §4.1),
//! 参数在 a0-a5,结果写回帧 a0,随后 `sret` 回用户态。

pub const SYSCALL_EXIT: usize = 1;
pub const SYSCALL_GET_TICKS: usize = 2;

/// 帧 GPR 槽位(与 arch/riscv64.rs 的 gpr 索引一致)。
const GPR_A0: usize = 9; // x10
const GPR_A7: usize = 16; // x17
/// 帧 sepc 槽(与 riscv64.rs CS_SEPC 一致)。
const FRAME_SEPC: usize = 33;

/// 分发用户 ecall。
///
/// # Safety
/// `frame` 必须指向有效用户陷阱帧(由 trap_handler 校验),且当前确为
/// U 模式(scause=8)。返回 `true` = 线程请求退出(调用方走 sched::exit,
/// 不再 sret);`false` = 已写入结果并前移 sepc,应 sret 回用户。
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
        _ => {
            // 未知 syscall:-ENOSYS,返回到用户。
            unsafe { *frame.add(GPR_A0) = usize::MAX };
            unsafe { *frame.add(FRAME_SEPC) += 4 };
            false
        }
    }
}
