use core::fmt;

const UART_BASE: usize = 0x1000_0000;
const UART_THR: usize = UART_BASE + 0x00;
const UART_LSR: usize = UART_BASE + 0x05;

pub struct Writer;

#[inline]
fn is_transmit_empty() -> bool {
    unsafe { core::ptr::read_volatile(UART_LSR as *const u8) & 0x20 != 0 }
}

pub fn init() {}

pub fn putc(c: u8) {
    while !is_transmit_empty() {}
    unsafe { core::ptr::write_volatile(UART_THR as *mut u8, c) }
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
