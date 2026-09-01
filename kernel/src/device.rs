//! 设备页授予(M3-2):白名单设备 MMIO 页以 U 权限映射进用户进程地址空间。
//!
//! # 设计
//! - **白名单**:`dev_id 0` → `board::uart_base()`(UART 16550 MMIO 页,
//!   0x1000_0000);其它 dev_id → `-EINVAL`。禁任意 paddr —— 设备 MMIO 不在
//!   buddy 分配区,`map_user_page` 的 `page_in_range` 会拒绝,须走专用接口
//!   `mmu::map_device_page`(跳过该检查;白名单校验在本模块完成)。
//! - **排他 claim**:同一设备一次只归一个进程。`slots[dev_id] = (pid, va)`;
//!   他进程重复 claim → `-EEXIST`;本进程重复 claim 幂等(va 一致时成功)。
//! - **生命周期随进程**:进程销毁时 `process::destroy` 持 TABLE 锁调用
//!   `release_all(pid)` 清 claim;U 映射随 `mmu::destroy_root` 解除 —— 设备页
//!   为**非分配器** U 叶子,destroy_root 已修复为只 unmap 不 free(见 mmu.rs
//!   `destroy_root` 注释)。
//! - **安全局限**(登记 DEFERRED D31):map_device 无特权校验,任何进程可
//!   claim UART;M3-2 靠引导序(uart_server 先 spawn 先 claim)保证,特权授予
//!   与 `Cap::Dev` 延后。
//!
//! # 锁序契约
//! 本模块表锁(DEVICES)与进程表(TABLE)的序:`TABLE → DEVICES`(destroy 持
//! TABLE 取 DEVICES),**不逆序**。`map` 经 `pid_root` 取 TABLE(调用返回即
//! 释放)再取 DEVICES,无嵌套重叠;`release_all` 仅取 DEVICES。DEVICES 为
//! 独立叶子锁,不与 IPC/SCHED 锁同持。

use crate::sync::SpinLock;

/// 白名单设备数上限(纯资源护栏;槽式稳定索引 = dev_id)。
const MAX_DEVICES: usize = 8;

/// 设备 claim 槽:dev_id → (进程 pid, 映射 VA)。None = 未 claim。
///
/// 定长数组保持表结构 Copy(与 process::Process 同风格);槽位稳定
/// (dev_id = 索引),无 free 池 —— 设备 claim 随进程销毁释放,不扩容。
struct DeviceTable {
    slots: [Option<(usize, usize)>; MAX_DEVICES],
}

/// 设备表单例(独立 SpinLock;ISR 路径不访问)。
static DEVICES: SpinLock<DeviceTable> = SpinLock::new(DeviceTable {
    slots: [None; MAX_DEVICES],
});

/// 设备 MMIO 映射叶子标志(V|R|W|A|D;`map_device_page` 自动加 U)。
/// 与 shm.rs `MAP_FLAGS` 同值(用户 RW 页标志;设备页无需 X)。
const DEVICE_FLAGS: u64 = 0xC7;

/// 映射设备 MMIO 页到调用进程(syscall 12 `map_device` 的调用方)。
///
/// - `caller`:调用方进程 id(`sched::current_proc()`,恒有效);
/// - `dev_id`:白名单设备号(0 = UART → `board::uart_base()`);
/// - `va`:映射目标虚拟地址(页对齐,< USER_VA_LIMIT,未映射)。
///
/// 返回 `Ok(())` 或负 errno(usize 编码):dev_id 未知/va 非法 → `-EINVAL`;
/// 调用进程不存在 → `-EACCES`;设备已被**其它**进程 claim → `-EEXIST`。
pub fn map(caller: usize, dev_id: usize, va: usize) -> Result<(), usize> {
    // 1) 白名单:dev_id 0 → UART;其它一律 -EINVAL(禁任意 paddr)。
    let paddr = match dev_id {
        0 => crate::board::uart_base(),
        _ => return Err(crate::syscall::SYS_ERR_EINVAL),
    };
    // 2) 参数校验:va 页对齐 + 用户区;防御性校验 paddr 页对齐(board 恒页对齐)。
    if va >= crate::mmu::USER_VA_LIMIT || !va.is_multiple_of(4096) || !paddr.is_multiple_of(4096) {
        return Err(crate::syscall::SYS_ERR_EINVAL);
    }
    // 3) 调用方进程必须存在(pid_root 非 panic;缺则 -EACCES,防 root panic)。
    let root = match crate::process::pid_root(caller) {
        Some(r) => r,
        None => return Err(crate::syscall::SYS_ERR_EACCES),
    };
    // 4) DEVICES 锁内:排他 claim + 映射(原子;映射失败不落 claim)。
    let irq = crate::arch::irq_save();
    let result = {
        let mut t = DEVICES.lock();
        match t.slots[dev_id] {
            // 已 claim:他进程 → -EEXIST;本进程 va 不一致 → -EINVAL;一致
            // → 幂等成功(映射已存在)。
            Some((owner, _owner_va)) if owner != caller => Err(crate::syscall::SYS_ERR_EEXIST),
            Some((_, owner_va)) if owner_va != va => Err(crate::syscall::SYS_ERR_EINVAL),
            Some(_) => Ok(()),
            None => {
                // 未 claim:映射设备页(`map_device_page` 跳过 page_in_range,
                // 白名单 paddr 已在本函数校验;拒绝覆盖已有 PTE)。成功才落
                // claim —— 失败不留半 claim(天然回滚)。
                match crate::mmu::map_device_page(root, va, paddr, DEVICE_FLAGS) {
                    Ok(()) => {
                        t.slots[dev_id] = Some((caller, va));
                        Ok(())
                    }
                    Err(_) => Err(crate::syscall::SYS_ERR_EINVAL),
                }
            }
        }
    }; // DEVICES 锁在此释放
    crate::arch::irq_restore(irq);
    result
}

/// 释放进程的全部设备 claim(process::destroy 钩子,TABLE 锁内调用)。
///
/// 仅清 claim 槽,使设备可被他进程重新 claim;U 映射不在此解除 —— 地址
/// 空间回收随 `mmu::destroy_root`(设备页 = 非分配器 U 叶子,只 unmap 不
/// free,见 mmu.rs `destroy_root`)。幂等:无该进程 claim 时无操作。
pub fn release_all(pid: usize) {
    let irq = crate::arch::irq_save();
    {
        let mut t = DEVICES.lock();
        for slot in t.slots.iter_mut() {
            if let Some((owner, _)) = slot {
                if *owner == pid {
                    *slot = None;
                }
            }
        }
    } // DEVICES 锁在此释放
    crate::arch::irq_restore(irq);
}
