//! Ignium M3 T1 用户冒烟程序(M3-2 起打印经 uart_server 服务)。
//!
//! # 定位
//! 内核 ELF 加载器加载运行,`tests::boot_elf_test` 轮询**共享标记页**断言
//! 成功。打印走 M3-2 uart_server 服务:连接服务(未注册 → -ENOENT,静默
//! 跳过打印 —— 测试不依赖 UART 输出)、SHM 写、IPC 请求 / 回复。
//!
//! # ABI 约定
//! 入口 `_start`(extern "C";内核加载器建初始帧 a0=argc、a1=argv,sp 指向
//! 初始栈,16B 对齐 —— RISC-V psABI 进程入口约定)。

#![no_std]
#![no_main]

use ignium_user::*;

/// 共享标记页地址:须与 `kernel/src/tests.rs` boot_elf_test 的 marker VA
/// (0x4000_2000)一致 —— 内核测试映射该页,用户程序写入后内核轮询断言。
const MARKER_VA: usize = 0x4000_2000;

/// 用户入口:内核加载器 `elf::load` 建初始帧,a0=argc、a1=argv 经 sret 到达。
#[no_mangle]
pub extern "C" fn _start(argc: usize, argv: usize) -> ! {
    // 1) 经 uart_server 服务打印(连接失败 = 服务未注册,静默跳过 —— 测试
    //    不依赖 UART 输出)。
    if uart_init() == 0 {
        let _ = uart_write(b"hello, ignium!\n");
        let _ = uart_write(b"argc=");
        write_dec(argc);
        let _ = uart_write(b"\n");
        // argv[0](如有):读指针数组首项 → 逐字节输出其字符串。
        if argv != 0 {
            let s0 = unsafe { core::ptr::read_volatile(argv as *const usize) };
            if s0 != 0 {
                let _ = uart_write(b"argv[0]=");
                let mut p = s0 as *const u8;
                loop {
                    let b = unsafe { core::ptr::read_volatile(p) };
                    if b == 0 {
                        break;
                    }
                    let _ = uart_write(&[b]);
                    p = unsafe { p.add(1) };
                }
                let _ = uart_write(b"\n");
            }
        }
    }
    // 2) 写共享标记页:证明 ELF 已映射 + U 模式执行 + argc 经 ABI 到达。
    //    **置于打印完成后**:M3-2 T1 以它作为"客户端已完成全部服务 IPC
    //    往返(connect/send/recv + uart_server TX)"的完成信号(内核测试
    //    轮询断言;boot_elf_test 在服务未注册时连接 -ENOENT 静默跳过,
    //    marker 仍写,断言不受影响)。
    unsafe {
        core::ptr::write_volatile(MARKER_VA as *mut usize, 0xC0DE_0000 | argc);
    }
    sys_exit();
}

/// 十进制无符号打印(经 uart_write 服务,逐字节;测试程序,慢无妨)。
fn write_dec(mut n: usize) {
    let mut buf = [0u8; 20];
    if n == 0 {
        let _ = uart_write(b"0");
        return;
    }
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = uart_write(&buf[i..]);
}
