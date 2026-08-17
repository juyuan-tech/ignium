//! 基础同步原语(M1)。
//!
//! # SpinLock 使用约束(重要)
//! 当前仅**主上下文**(关中断外、无调度)使用:
//! - 定时器 ISR 零分配、零日志,不会与主上下文竞争;
//! - **ISR 内持锁会与主上下文死锁**(自旋等待)。
//!
//! ISR 安全需要在加锁时保存/恢复 SIE(中断安全锁),
//! 与 IRQ 安全分配一起在调度器里程碑引入(DEFERRED D3/D11)。

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// 自旋锁:无调度器时的最小互斥原语。
/// 单核 + 中断关闭语义下,临界区不应被抢占。
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// 内核单地址空间、无线程迁移,锁保证互斥访问 —— 声明 Sync 安全。
unsafe impl<T> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// 构造(可在静态上下文中使用)。
    pub const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// 获取锁(自旋等待),返回守卫。
    ///
    /// # 并发约束
    /// 见模块头:主上下文专用;ISR/中断上下文禁止调用。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self.locked.swap(true, Ordering::Acquire) {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

/// 锁守卫:释放时自动解锁。
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
