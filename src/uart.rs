use core::fmt;

const UART_BASE: usize = 0x1000_0000;
const UART_IER: usize = UART_BASE + 0x01;
const UART_FCR: usize = UART_BASE + 0x02;
const UART_LCR: usize = UART_BASE + 0x03;
const UART_MCR: usize = UART_BASE + 0x04;
const UART_LSR: usize = UART_BASE + 0x05;

pub struct Writer;

#[inline]
fn read_u8(addr: usize) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}

#[inline]
fn write_u8(addr: usize, val: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, val) }
}

#[inline]
fn is_transmit_empty() -> bool {
    read_u8(UART_LSR) & 0x20 != 0
}

pub fn init() {
    write_u8(UART_LCR, 0x80);
    write_u8(UART_BASE, 0x0C);
    write_u8(UART_IER, 0x00);
    write_u8(UART_LCR, 0x03);
    write_u8(UART_FCR, 0x07);
    write_u8(UART_MCR, 0x03);
}

pub fn putc(c: u8) {
    while !is_transmit_empty() {}
    write_u8(UART_BASE, c);
}

pub fn write_str(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::uart::Writer, $($arg)*);
    }};
}
