//! 分级日志系统。
//!
//! 格式:`[{tick:06}] [{级别}] 消息`,tick 由定时器递增(M1 启用,
//! 当前恒为 0),用于把日志与中断/时间轴对应。
//!
//! # 级别语义
//! - `error!`:不可恢复或严重错误(panic 路径使用)
//! - `warn!`:异常但可继续
//! - `info!`:默认级别,里程碑/启动信息
//! - `debug!` / `trace!`:详细调试信息,默认过滤
//!
//! # 并发/重入约束(团队注意)
//! 当前**无锁**。M0 现状:中断关闭,输出为轮询,异常路径
//! (trap_handler)可安全调用 —— 嵌套风险已由陷阱栈吸收(写入被
//! 限制在陷阱栈区)。**M1 使能中断前必须引入 ISR 安全输出**
//! (自旋锁或 ISR 专用缓冲),届时更新本约定;UART 写入交错会
//! 破坏诊断可读性(pro 审计 #8)。
//!
//! # 性能
//! 级别过滤发生在 `format_args!` 展开之前,被过滤的调用零格式化开销。

use core::fmt::Arguments;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// 日志级别。声明顺序即优先级顺序(Error 最低,允许输出最多)。
/// 过滤规则:`级别 > 当前配置级别` 的消息被丢弃。
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
    /// 5 字符定宽级别标签,保证日志对齐。
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

/// 当前全局过滤级别(默认 Info)。用原子保存:
/// 为 M1 的"运行时动态调整"预留,当前只在启动时设置一次。
static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// 单调递增 tick(毫秒级节拍,由 M1 定时器中断调用 `tick_up`)。
/// 单调性保证:只能通过 `tick_up` 递增,任何路径不得回写。
static TICK: AtomicU64 = AtomicU64::new(0);

/// 设置全局日志级别(通常只在启动时调用一次)。
pub fn set_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// 读取当前过滤级别。
pub fn level() -> Level {
    // 显式匹配 + 兜底到最宽松级别:
    // 若原子值被内存错误破坏,宁可多输出也不吞诊断信息。
    match LOG_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

/// 当前 tick 值(日志时间戳)。
pub fn tick() -> u64 {
    TICK.load(Ordering::Relaxed)
}

/// tick 递增(M1 定时器中断回调;仅此处可修改 TICK)。
#[allow(dead_code)]
pub fn tick_up() {
    TICK.fetch_add(1, Ordering::Relaxed);
}

/// 记录一条日志(宏展开目标,勿直接调用)。
///
/// # 并发约束
/// 见模块头注释:无锁,当前仅允许在"中断关闭"的上下文中调用。
pub fn log(lvl: Level, args: Arguments) {
    if lvl > level() {
        return;
    }
    // writeln! 通过 uart::Writer 输出,自动处理 \r\n 与忙等超时。
    let _ = writeln!(
        crate::uart::Writer,
        "[{:06}] [{}] {}",
        TICK.load(Ordering::Relaxed),
        lvl.tag(),
        args
    );
}

// 以下宏把日志调用包装为 `$crate::logger::log(...)`:
// 使用 `$crate` 保证在被其他 crate 引用时路径解析正确(宏可导出)。

/// 记录 ERROR 级别日志(过滤级别 ≤ Error 时输出)。
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Error, format_args!($($arg)*))
    };
}

/// 记录 WARN 级别日志(过滤级别 ≤ Warn 时输出)。
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Warn, format_args!($($arg)*))
    };
}

/// 记录 INFO 级别日志(默认过滤级别,启动与里程碑信息用)。
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Info, format_args!($($arg)*))
    };
}

/// 记录 DEBUG 级别日志(需 set_level(Debug) 或更低才输出)。
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Debug, format_args!($($arg)*))
    };
}

/// 记录 TRACE 级别日志(最详细,需 set_level(Trace) 才输出)。
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::Level::Trace, format_args!($($arg)*))
    };
}
