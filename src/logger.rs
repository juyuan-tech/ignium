use core::fmt::Arguments;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    const fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static TICK: AtomicU64 = AtomicU64::new(0);

pub fn set_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn level() -> Level {
    match LOG_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

pub fn tick() -> u64 {
    TICK.load(Ordering::Relaxed)
}

#[allow(dead_code)]
pub fn tick_up() {
    TICK.fetch_add(1, Ordering::Relaxed);
}

pub fn log(lvl: Level, args: Arguments) {
    if lvl > level() {
        return;
    }
    let _ = writeln!(
        crate::uart::Writer,
        "[{:06}] [{}] {}",
        TICK.load(Ordering::Relaxed),
        lvl.tag(),
        args
    );
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Error, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Warn, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Info, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Debug, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Trace, format_args!($($arg)*))
    };
}
