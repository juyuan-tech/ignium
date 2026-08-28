# M2 T3b 报告:per-CPU 调度器(D19)

- 日期:2026-08-28
- 阶段:M2(微内核骨架)子任务 T3b(D19 多核调度)
- 提交:`feat: M2 T3b - per-CPU scheduler (D19)`
- 前置:T3a(多核 bring-up,D7/D8/D9)→ 本阶段按 T3 计划拆 T3a → T3b → T3c
- M2 验收(本阶段对应部分):**4 核 QEMU 上调度器可把线程分配到各核,每核
  独立 idle/定时器/IPI 唤醒;既有全部测试在 boot hart 语义不变**

## 1. 摘要

M2 T3 第二阶段完成 **D19 per-CPU 调度器**,把单队列共享调度器改为
per-CPU 就绪队列 + 线程亲和性,并落地跨核唤醒:

- **数据结构 per-CPU 化**:`Scheduler.ready: [[VecDeque; 3]; MAX_HARTS]`、
  `current/idle/ticks_run: [..; MAX_HARTS]`;`Thread` 增 `hart`(亲和性,
  默认 = 创建时当前核)。**全局 SCHED 锁保留** → 无数据竞争,只改语义:
  线程恒在 `ready[threads[id].hart]`,只在亲和核上运行。
- **7 处 ready 触点 per-hart 化**(按**线程的** hart 索引,非运行核):
  `enqueue`/`pick_next(hart)`/`remove_from_ready`/`requeue_proc_threads`/
  `on_tick(hart)`/spawn 系/yield-block-wake。`on_tick` 由 trap_handler 传入
  当前核,抢占只作用本核 current 与本核队列(每核独立时间片)。
- **`secondary_idle(hart)`**:副核 idle 循环 —— `pick_next(hart)` 有就绪 →
  `do_switch`,否则 `wfi`。idle **不入就绪队列**(与 boot hart 的 yield 式
  idle 不同),回退恒为 `idle[hart]`。副核首次进入即开启每核定时器。
- **跨核唤醒 = SBI IPI + SSIP**:`wake(id)` 入线程亲和核队列;若目标核
  idle 且非本核 → `send_ipi(1<<tgt)`(判定与发 IPI 同在 SCHED 锁临界区,
  与目标核"查空→wfi"互斥 → 无丢失唤醒窗口)。SBI IPI 送达 S 模式软中断
  (scause=1),trap_handler 清 `sip.SSIP` 后返回原帧,idle 循环重查队列。
  IPI 失败仅降级为 ≤1 tick 定时器唤醒,正确性不受影响。
- **`set_affinity(id, hart)`**:迁移非运行线程(Ready → 跨核队列迁移;
  Blocked → 只改 hart),供"把线程分配到各核"的验收与测试。
- **测试 `tests::smp_sched_test()`**(boot hart、irq_enable 后调用):阶段 1
  对每个在线核 spawn 内核线程 + `set_affinity(t, h)`,线程把运行核写入每核
  atomic 槽并计数,boot hart yield 轮询,断言每槽 == 亲和核、计数 == 在线
  核数;阶段 2 **确定性覆盖 IPI 链路**(等线程真正 Blocked 后跨核 wake)。
  banner `M2 T3b: per-CPU sched ok (N harts)`(N=1 单核/4 多核均过)。

**验证**:五门禁(clippy/fmt/test/test-smp/test-rva23)全绿 + dev profile
clippy/build 绿;test-smp 5 连跑全确定;IPI 链路无告警、banner 在
irq_enable 后 ~4 tick(确定性走 IPI)。详见 §4。

**本次实测修复的关键 bug**(见 §2.1):`CAUSE_SUPERVISOR_SOFTWARE` 误写为
3(RISC-V 规范中 3 是 **M** 模式软中断;S 模式软中断是 **1**),IPI 到达时
trap_handler 落入 unhandled 分支直接停机副核 —— 首轮 test-smp 因测试竞态
"假通过",手动跑暴露后修复。

## 2. 发现明细

### 2.1 S 软中断 cause 常量错误(阻塞级缺陷 —— 本次核心根因)

- **级别**:阻塞级缺陷(跨核 IPI 唤醒完全不通)。
- **位置**:`arch/riscv64.rs` `CAUSE_SUPERVISOR_SOFTWARE`。
- **现象**:`smp_sched_test` 阶段 2(跨核 wake)后副核收到 SSIP,日志出现
  `TRAP: unhandled interrupt cause=1, sepc=0x80204fa2`,随后 boot hart 因
  "cross-hart wake timeout" panic(tests.rs:582)。
- **根因**:设计时误以为 S 模式软中断 scause=3。RISC-V 特权规范:
  中断 cause **1** = Supervisor software interrupt(SSI),**3** = Machine
  software interrupt(MSI)。trap_handler 的 `match scause & !INTERRUPT_BIT`
  无 1 分支 → 落入 `other =>` unhandled → `dump_trap_frame` + `halt()`
  停机副核 → 唤醒线程永不恢复 → boot hart 超时 panic。
- **影响**:无本修复则跨核调度唤醒死路;副核一旦被 IPI 唤醒即停机。
- **附带教训**:sie/sip 的 SSIP 位 = bit 1 一直是正确的(enable_timer 加
  `csrs sie, 2`、handler `csrc sip, 2` 均对),仅 scause 编号常量错。

### 2.2 阶段 2 测试竞态(假通过/真失败,暴露 2.1 的窗口)

- **级别**:测试可靠性缺陷(掩盖上述阻塞缺陷)。
- **位置**:`tests.rs` `smp_sched_test` 阶段 2。
- **现象**:首轮 `docker-make test-smp` 报 PASS;手动重跑(同二进制)失败
  panic。即同一代码一次过一次挂。
- **根因**:原实现用"线程置 BLOCKED 标志"通知 boot hart。该标志在
  `block_current()` **之前**置位 —— boot hart 的 `wake()` 可能先于
  `block_current()` 到达:此时线程 state 仍为 Running,`wake` 只置 woken
  标志不入队不发 IPI;随后 `block_current` 消费 woken 直接继续 → 测试
  绕过 IPI 路径通过(不触发 2.1)。若 `block_current` 先完成 → IPI 发出 →
  SSIP 停机 → 失败。两次跑分岔 = 调度时序差异。
- **决定**:新增 `sched::is_blocked(id)`(SCHED 锁内读 state),boot hart
  先轮询等线程**真正** Blocked 再 `wake` —— wake 必走
  `Blocked → enqueue + IPI` 路径,IPI 链路被确定性覆盖。

### 2.3 MAX_HARTS 作用域(clippy 编译错误,开发期修正)

- **级别**:编译错误(非缺陷)。
- **位置**:`sched.rs`。`use crate::arch::{self, Context}` 未引入
  `MAX_HARTS`,而 per-CPU 数组用了裸 `MAX_HARTS`。补 `use ..., MAX_HARTS`。

### 2.4 副核退出线程后回 `secondary_idle` 的两条路径(设计验证,非缺陷)

- **机制**:副核 idle 从不入就绪队列,`idle_entry` 仅是 init 占位。
  真实线程退出/阻塞后,pick 回退 `idle[hart]`,恢复机制按 idle 的
  数据有效性二选一:
  - **timer 抢占路径**:idle 曾被 `on_tick` 捕获(帧有效、ctx 失效)→
    `frame_restore` sret 回 `secondary_idle` 的 wfi 恢复点;
  - **协作切换路径**:idle 曾经 `context_switch` 切走(ctx 保存了
    `secondary_idle` 内 do_switch 之后的恢复点)→ `context_switch` 回此处。
  两条路径实测均正确回到 idle 循环(见 §4 日志:多轮线程在副核上运行、
  退出后系统持续 uptime)。

## 3. 修复明细

### 3.1 sched.rs:D19 per-CPU 化(核心)

- `Thread` 增 `hart: usize`(亲和性,spawn/spawn_user 默认 `arch::hartid()`)。
- `Scheduler`:`ready` 增维 `[[VecDeque; 3]; MAX_HARTS]`;`current/idle/
  ticks_run` 数组化。SCHED 静态初值用内联 const 重复
  (`[const { [const { VecDeque::new() }; PRIO_LEVELS] }; MAX_HARTS]`,
  非 Copy 元素合法,Rust 1.97)。
- `pick_next(hart, need_ctx)`:`ready[hart][level]`;回退用
  `current[hart]`/`idle[hart]`。`enqueue`:`ready[threads[id].hart][prio]`。
  `remove_from_ready` 只扫线程亲和核队列。`requeue_proc_threads` 展平
  `ready.iter_mut().flatten()`(可跨核散布)。
- `on_tick(frame, hart)`:按 hart 索引 ticks_run/current/ready;`on_tick`
  公开包装收 hart 参数(trap_handler 已推导)。
- `yield_`/`block_current`/`exit`/`exit_from_trap`/`block_user_from_trap`/
  `current_id`/`current_proc`/`take_ipc_msg`/`thread_entry`:全部
  `s.current[arch::hartid()]`。
- `wake(id)`:入亲和核队列;`if tgt != my_hart && s.current[tgt] ==
  s.idle[tgt] { send_ipi(1u64 << tgt, 0) }`(锁内判定 + 发 IPI;失败仅 warn
  一次,降级定时器唤醒)。
- `init()`:为每核建 idle 线程(id 0..3,harts 0..3),`current[h] =
  idle[h]`;全部就绪队列/reaper/threads 容量一次性预留(ISR 零分配不变)。
- 新增 `set_affinity(id, hart)`(Ready 跨核迁移 / Blocked 改 hart /
  Running/Exited panic fail-loudly)、`secondary_idle(hart) -> !`(idle
  循环)、`is_blocked(id)`(测试用)。

### 3.2 arch/riscv64.rs:SSIP 使能 + 软中断处理

- `enable_timer()` 增 `csrs sie, 2`(sie.SSIP) —— 每核开中断前使能,
  否则 IPI 无法唤醒 wfi 中的 idle。
- 修正 `CAUSE_SUPERVISOR_SOFTWARE = 1`(附规范注释 + 实测教训)。
- `trap_handler` 增 `CAUSE_SUPERVISOR_SOFTWARE` 分支:`csrc sip, 2`
  (清挂起位,防 wfi 忙转)+ 返回原帧(idle 循环重查队列,不在 ISR 调度)。
- 定时器分支改传 `h` 给 `on_tick(frame, h)`。

### 3.3 sbi.rs / main.rs

- `sbi.rs`:移除 `send_ipi` 与 `SBI_EXT_IPI` 的 `#[allow(dead_code)]`
  (现被 wake 使用)。
- `main.rs`:`secondary_main` 从"SIE 关 + 无定时器 idle 停等"改为
  `enable_timer` + `irq_enable` + `crate::sched::secondary_idle(hartid)`;
  `kernel_main` 在 irq_enable 后调 `tests::smp_sched_test()`。

### 3.4 tests.rs:smp_sched_test(两阶段)

- 阶段 1(分配):对每个在线核 spawn + `set_affinity(t, h)`;线程读
  `current_id()`/`arch::hartid()`,写入每核 atomic 槽(错核写
  `usize::MAX`)、计数;boot hart yield 轮询(500k 上限);断言每槽 == 亲和
  核、计数 == 在线核数。
- 阶段 2(IPI):线程置亲和核 1(单核退化 0)→ `block_current`;boot hart
  `is_blocked` 轮询等真阻塞 → `wake(wtid)`(跨核发 IPI)→ 等 RAN。
- banner `M2 T3b: per-CPU sched ok ({n} harts)`。

### 3.5 Makefile / ci.yml:banner 断言四处同步

- `test`/`test-rva23`/ci build/ci rva23:新增 `grep -q "M2 T3b: per-CPU
  sched ok"`(部分匹配,兼容 N=1/N=4)。
- `test-smp`(Makefile + ci):追加同 banner 断言,保 `M0==1 / online==3 /
  T3a / 无 panic` 原断言。

## 4. 验证结果

五门禁全绿(dev+release 由 docker-make.sh 统一执行):

```
=== GATE: clippy === PASS     === GATE: fmt === PASS
=== GATE: test === PASS       === GATE: test-smp === PASS
=== GATE: test-rva23 === PASS (+ dev profile clippy/build PASS)
```

test-smp(-smp 4)5 连跑全确定(修复竞态后):

```
run1: SMP TEST PASS  ...  run5: SMP TEST PASS
```

SMP 日志(boot hart=0,阶段 2 确定性走 IPI —— 无 send_ipi 失败/无
unhandled/无 panic):

```
[000127] [INFO ] M2 T3a: multi-core boot ok (4 harts online)
[000127] [INFO ] M1: timer enabled (10000us interval), interrupts on
[000131] [INFO ] M2 T3b: per-CPU sched ok (4 harts)   <- irq_enable 后 ~4 tick
[000136] [INFO ] uptime: 136 ticks (1360 ms)
... uptime 持续到 2136 ticks(全 10s 运行)
```

- 3 条 `hart N online`(本次 boot hart=0 → 副核 1/2/3);M0 恰 1 条。
- 单核 test(-smp 1):既有 banner + `M2 T3b: per-CPU sched ok (1 harts)` +
  uptime≥2 + 无 panic 全过。
- RVA23(-cpu max):同断言全过。

## 5. 遗留风险

- **spawn / set_affinity 不触发 IPI**:新线程/迁移线程放入目标核队列后,
  该核最多等下一 tick(10ms)由定时器取到(仅 `wake` 对 idle 核发 IPI)。
  测试与验收场景可容忍;低延迟跨核投递(T4)需在 spawn 路径也发 IPI。
- **IPI 降级路径未被测试压到**:`send_ipi` 失败 → 定时器唤醒的降级路径
  只靠设计论证,无注入失败测试(需可注入的 SBI 错误)。
- **无真实跨核 TLB shootdown**:revoke(T3c)以"satp 切换全刷 + 本核
  sfence"简化(单页共享、revoke 即销毁页,无残留映射复用)。
- **无负载均衡/work stealing**:spawn 默认落到当前核;多核间线程分布
  靠显式 `set_affinity`,无自动均衡(T4)。
- **MAX_HARTS=4 硬上限**:超出 QEMU -smp 4 的 hart 永久停 park;在线核数
  取 FDT cpu 数 ∩ MAX_HARTS。
- **通用多核日志可能交错**:D9 try_lock best-effort 回退在争用下字符
  交错(既有设计,防 panic 死锁);后续多核日志断言须避开对行原子的依赖。
- **多核竞争规模压测未做**:只做功能验证(banner/分配/IPI 链路);多核
  竞争压测归 T4。

## 附:T3b 与 T3 计划的偏差说明

计划为 `wake` 设计了 IPI 机制但未在测试中确定性覆盖;实测发现 IPI 链路
(SSIP cause 常量)缺陷且首轮测试竞态掩盖它,故补 `is_blocked` 让阶段 2
确定性走 IPI 路径 —— 这同时满足 sbi.rs 注释"T3b 报告需重新验证 IPI 在
新机制的可用性"。banner/断言/报告其余按计划落地。
