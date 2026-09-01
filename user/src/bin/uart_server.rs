//! M3-2:UART 服务进程 —— 独占 UART MMIO,为客户端提供打印 / 读取。
//!
//! # 定位
//! 内核 ELF 加载器加载运行,测试指定亲和;先于客户端 spawn,靠引导序独占
//! UART(排他 claim,见 kernel device.rs)。打印 / 读取经 IPC + SHM 服务化
//! (微内核"内核直碰 UART"过渡占位已移除,M3-DESIGN §4)。
//!
//! # 主流程
//! 1. `sys_map_device(0, UART_MMIO_VA)` —— 白名单 UART 页 U-映射排他;
//! 2. `sys_service_register(UART_SERVICE_ID)` —— 内核服务注册表自报;
//! 3. 服务循环:`sys_ipc_recv(SERVER_ACCEPT_SLOT)`(阻塞配对)→ 按 op 处理
//!    (SHM_VA 数据区)→ `sys_ipc_send(SERVER_ACCEPT_SLOT, 回复)`。
//!
//! # 协议
//! 请求 `[op, arg1, 0, 0, 0]`:
//! - WRITE(0x01):arg1=len,数据在 SHM_VA[0..len],TX 逐字节(`\n→\r\n`
//!   复刻旧 sys_write 语义);
//! - READ(0x02):arg1=max_len,轮询 LSR.DR 读 RBR → 写 SHM_VA[0..n],n 为
//!   已读数(无数据 → 0,EOF 语义);
//! - PING(0x03):连通性。
//! 回复 `[op|0x80, status, len, 0, 0]`(status=0 成功;未知 op → PROTO_ERR)。
//!
//! # 并发局限
//! accept 单槽 + SHM_VA 单页 → 本轮 1 并发 client(登记 DEFERRED,多 client
//! 需多 SHM VA + 槽池)。

#![no_std]
#![no_main]

use ignium_user::*;

/// NS16550 UART 寄存器偏移(UART_MMIO_VA 基址;RBR / THR 共用偏移 0)。
const RBR_THR: usize = 0x0;
const LSR: usize = 0x5;
/// LSR 位:THRE(发送保持寄存器空)= 可写下一字节;DR(数据就绪)= 可读。
const LSR_THRE: u8 = 0x20;
const LSR_DR: u8 = 0x01;

/// 用户入口:内核加载器建初始帧(a0=argc、a1=argv,本服务忽略)。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // 1) 独占 UART MMIO(白名单 dev_id=0 → board::uart_base())。
    if sys_map_device(0, UART_MMIO_VA) != 0 {
        sys_exit();
    }
    // 2) 自报注册(客户端经 service_connect 定位本进程)。
    if sys_service_register(UART_SERVICE_ID) != 0 {
        sys_exit();
    }
    // 3) 服务循环:阻塞配对 recv → 处理 → 回复。正常路径单客户端 ping-pong。
    loop {
        let r = sys_ipc_recv(SERVER_ACCEPT_SLOT);
        if r[0] != 0 {
            continue; // IPC errno(防御;正常配对路径不达)
        }
        let op = r[1];
        let arg1 = r[2];
        let (status, len) = match op {
            OP_WRITE => {
                let n = arg1.min(SHM_LEN);
                for i in 0..n {
                    let b = unsafe { core::ptr::read_volatile((SHM_VA + i) as *const u8) };
                    uart_tx(b);
                }
                (0usize, n)
            }
            OP_READ => {
                let max = arg1.min(SHM_LEN);
                let mut n = 0usize;
                while n < max && uart_rx_ready() {
                    let b = uart_rx();
                    unsafe { core::ptr::write_volatile((SHM_VA + n) as *mut u8, b) };
                    n += 1;
                }
                (0usize, n)
            }
            OP_PING => (0usize, 0usize),
            _ => (PROTO_ERR, 0usize),
        };
        let _ = sys_ipc_send(SERVER_ACCEPT_SLOT, &[op | OP_REPLY_FLAG, status, len, 0, 0]);
    }
}

/// 发送单字节到 UART(轮询 THRE;`\n→\r\n` 复刻旧 sys_write 语义)。
fn uart_tx(b: u8) {
    if b == b'\n' {
        uart_tx_raw(b'\r');
    }
    uart_tx_raw(b);
}

/// 原始 TX 单字节(轮询 LSR.THRE 后写 THR)。
fn uart_tx_raw(b: u8) {
    loop {
        // SAFETY:UART_MMIO_VA 已 sys_map_device 映射(本服务独占 claim)。
        let lsr = unsafe { core::ptr::read_volatile((UART_MMIO_VA + LSR) as *const u8) };
        if lsr & LSR_THRE != 0 {
            break;
        }
    }
    // SAFETY:同上;THR(offset 0)可写。
    unsafe { core::ptr::write_volatile((UART_MMIO_VA + RBR_THR) as *mut u8, b) };
}

/// RX 数据就绪(LSR.DR)?
fn uart_rx_ready() -> bool {
    // SAFETY:UART_MMIO_VA 已映射(本服务独占)。
    let lsr = unsafe { core::ptr::read_volatile((UART_MMIO_VA + LSR) as *const u8) };
    lsr & LSR_DR != 0
}

/// 读一字节 RX(RBR;调用前须 `uart_rx_ready` 为真)。
fn uart_rx() -> u8 {
    // SAFETY:同上;RBR(offset 0)可读。
    unsafe { core::ptr::read_volatile((UART_MMIO_VA + RBR_THR) as *const u8) }
}
