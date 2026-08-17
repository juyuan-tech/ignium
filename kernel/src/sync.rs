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

use alloc::collections::VecDeque;

use crate::arch;
use crate::sched;

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

/// 阻塞式互斥锁(基于调度器的 block/wake)。
///
/// # 语义
/// - 获取失败时把当前线程登记为等待者并阻塞(而非自旋),
///   释放时唤醒队首等待者。
/// - **必须**在调度器初始化后使用(idle 线程上下文)。
/// - 不保证公平性:被唤醒线程若再次竞争失败会重新排队(可能饥饿,
///   M1 可接受,调度器成熟后引入 ticket/公平队列)。
pub struct Mutex<T: ?Sized> {
    locked: AtomicBool,
    waiters: SpinLock<VecDeque<usize>>,
    value: UnsafeCell<T>,
}

unsafe impl<T: ?Sized> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// 构造。
    pub const fn new(value: T) -> Self {
        Mutex {
            locked: AtomicBool::new(false),
            waiters: SpinLock::new(VecDeque::new()),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// 获取锁(必要时阻塞当前线程)。
    pub fn lock(&self) -> MutexGuard<'_, T> {
        loop {
            let irq = arch::irq_save();
            // 1) 快速路径:未被占用 → 直接获得。
            if !self.locked.swap(true, Ordering::Acquire) {
                arch::irq_restore(irq);
                return MutexGuard { m: self };
            }
            // 2) 占用:登记等待(防错过唤醒),再尝试一次。
            self.waiters.lock().push_back(sched::current_id());
            if !self.locked.swap(true, Ordering::Acquire) {
                // 登记后竞争成功:撤销登记,直接持有。
                self.waiters.lock().pop_back();
                arch::irq_restore(irq);
                return MutexGuard { m: self };
            }
            arch::irq_restore(irq);
            // 3) 阻塞,等待唤醒后重试。
            sched::block_current();
        }
    }

    /// 释放(唤醒队首等待者)。内部供守卫/条件变量调用。
    fn unlock(&self) {
        let irq = arch::irq_save();
        self.locked.store(false, Ordering::Release);
        if let Some(w) = self.waiters.lock().pop_front() {
            sched::wake(w);
        }
        arch::irq_restore(irq);
    }

    fn value(&self) -> *mut T {
        self.value.get()
    }
}

/// 互斥锁守卫:Drop 时释放。
pub struct MutexGuard<'a, T: ?Sized> {
    m: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.m.value() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.m.value() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.m.unlock();
    }
}

/// 条件变量(基于互斥锁 + 阻塞)。
///
/// # 语义
/// - `wait`:登记等待 → 释放互斥锁 → 阻塞;唤醒后重新获取锁。
/// - `notify_one` / `notify_all`:唤醒等待者。
/// - 经典"释放锁 + 阻塞"原子性由关中断保证(先关中断、再登记、
///   然后释放锁并阻塞 —— 期间无人能插入 notify 而丢失唤醒)。
pub struct Condvar {
    waiters: SpinLock<VecDeque<usize>>,
}

impl Condvar {
    /// 构造。
    pub const fn new() -> Self {
        Condvar {
            waiters: SpinLock::new(VecDeque::new()),
        }
    }

    /// 释放 `guard` 并阻塞,直到被通知后重新加锁返回。
    pub fn wait<'a, T: ?Sized>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let m = guard.m;
        let irq = arch::irq_save();
        // 原子地:登记 → 释放互斥(不唤醒其等待者)→ 准备阻塞。
        self.waiters.lock().push_back(sched::current_id());
        m.locked.store(false, Ordering::Release);
        drop(guard);
        arch::irq_restore(irq);
        sched::block_current();
        // 醒来:重新获取互斥锁。
        m.lock()
    }

    /// 唤醒一个等待者。
    pub fn notify_one(&self) {
        let irq = arch::irq_save();
        if let Some(w) = self.waiters.lock().pop_front() {
            sched::wake(w);
        }
        arch::irq_restore(irq);
    }

    /// 唤醒全部等待者(M2 广播场景使用;当前仅 notify_one 被自检覆盖)。
    #[allow(dead_code)]
    pub fn notify_all(&self) {
        let irq = arch::irq_save();
        let mut ws = self.waiters.lock();
        while let Some(w) = ws.pop_front() {
            sched::wake(w);
        }
        arch::irq_restore(irq);
    }
}

// ===== 自检(Mutex/Condvar 阻塞语义) =====

static TEST_MUTEX: Mutex<u32> = Mutex::new(0);
static TEST_MUTEX_DONE: AtomicBool = AtomicBool::new(false);
static TEST_MUTEX_DONE2: AtomicBool = AtomicBool::new(false);

/// 互斥自检线程:在锁内递增 1000 次,完成置位。
fn mutex_inc() {
    for _ in 0..1000 {
        let mut g = TEST_MUTEX.lock();
        *g += 1;
    }
    TEST_MUTEX_DONE.store(true, Ordering::Release);
}

fn mutex_inc2() {
    for _ in 0..1000 {
        let mut g = TEST_MUTEX.lock();
        *g += 1;
    }
    TEST_MUTEX_DONE2.store(true, Ordering::Release);
}

static TEST_DATA: Mutex<u32> = Mutex::new(0);
static TEST_COND: Condvar = Condvar::new();
static TEST_COND_DONE: AtomicBool = AtomicBool::new(false);

/// 生产者:置数据并通知。
fn cond_producer() {
    {
        let mut g = TEST_DATA.lock();
        *g = 42;
    }
    TEST_COND.notify_one();
}

/// 消费者:等待数据就绪后校验。
fn cond_consumer() {
    let mut g = TEST_DATA.lock();
    while *g != 42 {
        g = TEST_COND.wait(g);
    }
    if *g == 42 {
        TEST_COND_DONE.store(true, Ordering::Release);
    }
}

/// 同步原语自检(须在调度器初始化后、作为线程运行)。
pub fn self_test() -> Result<(), &'static str> {
    sched::spawn(mutex_inc, sched::PRIO_HIGH);
    sched::spawn(mutex_inc2, sched::PRIO_HIGH);
    let mut guard = 0;
    while (!TEST_MUTEX_DONE.load(Ordering::Acquire) || !TEST_MUTEX_DONE2.load(Ordering::Acquire))
        && guard < 100_000
    {
        sched::yield_();
        guard += 1;
    }
    if *TEST_MUTEX.lock() != 2000 {
        return Err("mutex counter mismatch");
    }
    if !TEST_MUTEX_DONE.load(Ordering::Acquire) || !TEST_MUTEX_DONE2.load(Ordering::Acquire) {
        return Err("mutex thread not done");
    }

    // 条件变量:消费者阻塞等待生产者通知。
    sched::spawn(cond_consumer, sched::PRIO_HIGH);
    sched::spawn(cond_producer, sched::PRIO_HIGH);
    guard = 0;
    while !TEST_COND_DONE.load(Ordering::Acquire) && guard < 100_000 {
        sched::yield_();
        guard += 1;
    }
    if !TEST_COND_DONE.load(Ordering::Acquire) {
        return Err("condvar handshake failed");
    }
    Ok(())
}
