//! Ignium M3 T1 用户测试程序(独立 crate,内核 ELF 加载器加载运行)。
//!
//! # 定位
//! 由 `kernel/build.rs` 编译为 `riscv64gc-unknown-none-elf` ELF,拷入
//! `$OUT_DIR/hello.elf`;内核 `include_bytes!` 内嵌,`tests::boot_elf_test`
//! 经 `elf::load` 映射进进程地址空间、U 模式运行。
//!
//! # ABI 约定
//! - 入口 `_start`(extern "C";内核加载器建初始帧 a0=argc、a1=argv,
//!   sp 指向初始栈,16B 对齐 —— RISC-V psABI 进程入口约定);
//! - 系统调用经 `ecall`,分发号 a7、参数 a0-a2、返回 a0;
//! - 输出经 `sys_write(fd=1)` → 内核 UART **过渡占位**(M3-1;M3-2
//!   uart_server 落地后删除,见 M3-DESIGN §4)。

#![no_std]
#![no_main]

use core::arch::asm;

/// 系统调用号(与 `kernel/src/syscall.rs` 一致;须与 kernel 一致,禁止
/// 复制定义 —— docs/SYSCALLS.md 登记纪律)。
const SYS_EXIT: usize = 1;
const SYS_WRITE: usize = 8;

/// 共享标记页地址:须与 `kernel/src/tests.rs` boot_elf_test 的 marker VA
/// (0x4000_2000)一致 —— 内核测试映射该页,用户程序写入后内核轮询断言。
const MARKER_VA: usize = 0x4000_2000;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // panic 后无输出通道(此测试以标记页断言成败);boot_elf_test 超时断言会
    // 暴露 panic,不静默吞掉。spin_loop 缓解 clippy::empty_loop。
    loop {
        core::hint::spin_loop();
    }
}

/// 用户入口:内核加载器 `elf::load` 建初始帧,a0=argc、a1=argv 经 sret 到达。
#[no_mangle]
pub extern "C" fn _start(argc: usize, argv: usize) -> ! {
    // 1) 写共享标记页:证明 ELF 已映射 + U 模式执行 + argc 经 ABI 到达。
    unsafe {
        core::ptr::write_volatile(MARKER_VA as *mut usize, 0xC0DE_0000 | argc);
    }
    // 2) 打印(经 sys_write(fd=1) → 内核 UART 过渡占位)。
    const HELLO: &[u8] = b"hello, ignium!\n";
    let _ = sys_write(1, HELLO.as_ptr() as usize, HELLO.len());
    // 3) argc。
    const ARGC_PREFIX: &[u8] = b"argc=";
    let _ = sys_write(1, ARGC_PREFIX.as_ptr() as usize, ARGC_PREFIX.len());
    write_dec(argc);
    let _ = sys_write(1, b"\n".as_ptr() as usize, 1);
    // 4) argv[0](如有):读指针数组首项 → 逐字节输出其字符串。
    if argv != 0 {
        let s0 = unsafe { core::ptr::read_volatile(argv as *const usize) };
        if s0 != 0 {
            const ARGV_PREFIX: &[u8] = b"argv[0]=";
            let _ = sys_write(1, ARGV_PREFIX.as_ptr() as usize, ARGV_PREFIX.len());
            let mut p = s0 as *const u8;
            loop {
                let b = unsafe { core::ptr::read_volatile(p) };
                if b == 0 {
                    break;
                }
                let ch = [b];
                let _ = sys_write(1, ch.as_ptr() as usize, 1);
                p = unsafe { p.add(1) };
            }
            let _ = sys_write(1, b"\n".as_ptr() as usize, 1);
        }
    }
    sys_exit();
}

/// 十进制无符号打印(经 sys_write,逐字节;测试程序,慢无妨)。
fn write_dec(mut n: usize) {
    let mut buf = [0u8; 20];
    if n == 0 {
        let _ = sys_write(1, b"0".as_ptr() as usize, 1);
        return;
    }
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = sys_write(1, buf.as_ptr() as usize + i, buf.len() - i);
}

/// 系统调用 helper(a7=号,a0-a2=参数,返回 a0;错误为负 errno 编码)。
///
/// `clobber_abi("C")`:内核 syscall ABI 允许 clobber 调用者保存寄存器。
#[inline]
fn syscall(num: usize, a0: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            in("a7") num,
            clobber_abi("C"),
            options(nostack)
        );
    }
    ret
}

#[inline]
fn sys_write(fd: usize, buf: usize, len: usize) -> usize {
    syscall(SYS_WRITE, fd, buf, len)
}

fn sys_exit() -> ! {
    syscall(SYS_EXIT, 0, 0, 0);
    // 内核 EXIT 永不返回;防御性兜底(不可达)。
    loop {
        core::hint::spin_loop();
    }
}
