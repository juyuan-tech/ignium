//! 基础同步原语(M1)。
//!
//! # SpinLock 使用约束
//! MED-3(审计 17 轮):自旋锁为 **IRQ 安全锁** —— 加锁时保存 SIE
//! 并关中断,释放时恢复。持锁临界区不再被抢占(消除堆/分配器锁
//! convoy)。约束:
//! - 定时器 ISR 仍**零分配、零日志**:不会竞争堆/分配器锁;
//! - 嵌套加锁安全(irq_save/restore 幂等,与 sched 的显式
//!   irq_save 配合无损)。

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::collections::VecDeque;

use crate::arch;
use crate::sched;

/// 自旋锁:IRQ 安全(加锁保存 SIE + 关中断,释放恢复)。
/// 临界区持锁期间不会被定时器抢占,因此不会出现
/// "持锁线程被切走、他人自旋"的 convoy。
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// 内核单地址空间、无线程迁移,锁保证互斥访问 —— 声明 Sync 安全。
// HIGH-8(审计 15 轮):约束 T: Send(锁内值须可安全跨上下文传递;
// 无约束的裸 unsafe impl 在 T 含非 Send 类型时是错误的安全声明)。
unsafe impl<T: Send> Sync for SpinLock<T> {}

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
    /// IRQ 安全:加锁关中断、守卫释放恢复。ISR 内仍应避免持锁
    /// (ISR 零分配约定);对已被主上下文持有的锁,ISR 自旋等待
    /// 也会在中断恢复后正常完成,不再死锁。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let saved = arch::irq_save();
        while self.locked.swap(true, Ordering::Acquire) {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard {
            lock: self,
            saved_irq: saved,
        }
    }
}

/// 锁守卫:释放时恢复中断并解锁。
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    saved_irq: bool,
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
        arch::irq_restore(self.saved_irq);
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

unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

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
        // 原子地:登记 → 释放互斥(与 unlock 相同语义,含唤醒队首
        // 互斥等待者 —— HIGH-1/审计 17 轮:不唤醒会使互斥等待者
        // 永久阻塞,因为锁已释放却无人 pop)→ 准备阻塞。
        self.waiters.lock().push_back(sched::current_id());
        m.locked.store(false, Ordering::Release);
        if let Some(w) = m.waiters.lock().pop_front() {
            sched::wake(w);
        }
        // HIGH-1(审计 15 轮):必须**消费**守卫 —— 直接 drop 会再次
        // unlock,唤醒互斥等待者并破坏其队列(双重解锁)。
        core::mem::forget(guard);
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
// MED-4(审计 17 轮):混合路径用**完成计数**而非布尔 ——
// 布尔在任一完成线程置位后主线程即可退出等待,可能观测到
// 部分计数(非确定性误报)。
static TEST_MUTEX_MIXED_DONE: AtomicUsize = AtomicUsize::new(0);

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

// HIGH-1(审计 17 轮)回归:竞争释放 —— 消费者先持锁并 wait,
// 生产者随后阻塞在互斥上。wait 释放互斥时若不同时唤醒互斥
// 等待者,则无人再能置数据,全系统死锁(原自检掩盖:消费先
// 入队,互斥队空)。
static TEST_DATA2: Mutex<u32> = Mutex::new(0);
static TEST_COND2: Condvar = Condvar::new();
static TEST_COND2_HAS_LOCK: AtomicBool = AtomicBool::new(false);
static TEST_COND2_DONE: AtomicBool = AtomicBool::new(false);

fn cond_consumer2() {
    let mut g = TEST_DATA2.lock();
    TEST_COND2_HAS_LOCK.store(true, Ordering::Release);
    while *g != 42 {
        g = TEST_COND2.wait(g);
    }
    if *g == 42 {
        TEST_COND2_DONE.store(true, Ordering::Release);
    }
}

fn cond_producer2() {
    {
        let mut g = TEST_DATA2.lock();
        *g = 42;
    }
    TEST_COND2.notify_one();
}

/// 互斥自检线程:在锁内递增 500 次(8 线程混合切换版本,覆盖
/// 抢占-协作交错的 ctx_valid/frame_valid 协议)。
fn mutex_mixed() {
    for _ in 0..500 {
        let mut g = TEST_MUTEX.lock();
        *g += 1;
        // 偶次迭代让出:制造协作切换与抢占交错的混合路径。
        if (*g).is_multiple_of(2) {
            sched::yield_();
        }
    }
    TEST_MUTEX_MIXED_DONE.fetch_add(1, Ordering::Release);
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

    // HIGH-1(审计 17 轮)回归:消费者先持锁并 wait,生产者随后
    // 阻塞在互斥上 —— 若 wait 释放互斥不唤醒互斥等待者,死锁。
    *TEST_DATA2.lock() = 0;
    TEST_COND2_HAS_LOCK.store(false, Ordering::Relaxed);
    TEST_COND2_DONE.store(false, Ordering::Relaxed);
    sched::spawn(cond_consumer2, sched::PRIO_HIGH);
    guard = 0;
    while !TEST_COND2_HAS_LOCK.load(Ordering::Acquire) && guard < 100_000 {
        sched::yield_();
        guard += 1;
    }
    if !TEST_COND2_HAS_LOCK.load(Ordering::Acquire) {
        return Err("cond2: consumer never acquired lock");
    }
    // 此刻消费者持有 TEST_DATA2,生产者 spawn 后阻塞在互斥上。
    sched::spawn(cond_producer2, sched::PRIO_HIGH);
    guard = 0;
    while !TEST_COND2_DONE.load(Ordering::Acquire) && guard < 500_000 {
        sched::yield_();
        guard += 1;
    }
    if !TEST_COND2_DONE.load(Ordering::Acquire) {
        return Err("condvar contended release deadlock");
    }

    // 混合路径压力:8 线程 × 500 次互斥递增 + 偶次 yield
    // (抢占-协作交错,覆盖 ctx_valid/frame_valid 协议回归)。
    *TEST_MUTEX.lock() = 0;
    TEST_MUTEX_MIXED_DONE.store(0, Ordering::Relaxed);
    for _ in 0..8 {
        sched::spawn(mutex_mixed, sched::PRIO_HIGH);
    }
    guard = 0;
    while TEST_MUTEX_MIXED_DONE.load(Ordering::Acquire) != 8 && guard < 500_000 {
        sched::yield_();
        guard += 1;
    }
    if TEST_MUTEX_MIXED_DONE.load(Ordering::Acquire) != 8 {
        return Err("mixed-path mutex timeout");
    }
    if *TEST_MUTEX.lock() != 8 * 500 {
        return Err("mixed-path mutex counter mismatch");
    }
    Ok(())
}
