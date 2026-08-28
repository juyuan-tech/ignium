# M2 T3a 报告:多核 bring-up(D7 per-hart 陷阱栈 / D8 副核唤醒 / D9 控制台锁)

- 日期:2026-08-28
- 阶段:M2(微内核骨架)子任务 T3a(多核前置)
- 提交:`feat: M2 T3a - multi-core bring-up (D7 per-hart trap stacks / D8 wake / D9 console lock)`
- 前置:T2b(优先级继承 + IPC 压力)→ 本阶段按 T3 计划拆 T3a → T3b → T3c
- M2 验收(本阶段对应部分):**4 核 QEMU 上所有 hart 进入 idle,副核各进
  idle 停等,既有全部测试在 boot hart 语义不变**

## 1. 摘要

M2 T3 按计划拆为 T3a(多核 bring-up)→ T3b(per-CPU 调度)→ T3c(共享内存
+ 能力扩展)。本次完成 **T3a**,三个子项按红线 8 要求齐上:

- **D9 控制台锁**:`sync.rs` 增 `SpinLock::try_lock`;`uart.rs` 增
  `CONSOLE_LOCK`(输出先 try_lock,失败回退无锁裸写,防 panic 死锁)。
  附带 `PANIC_OUTPUT` 标志(panic 处理器置位后所有输出不加锁)。
- **D7 per-hart 陷阱栈**:linker.ld 把陷阱栈/idle 栈改为 **per-hart 数组**
  (stride 32K = 16K 守护 + 16K 栈,`MAX_HARTS` 槽);`sscratch` 协议改版
  —— 陷阱外 sscratch = 本 hart 槽顶,陷阱期间 = 帧基址,`hartid =
  (sscratch − base) >> 15` 任意时刻可靠(不依赖 tp);`arch/riscv64.S`
  陷阱向量 4 处 per-hart 化;`TIMER_DEADLINE` 改 per-hart 数组。
- **D8 副核唤醒**:核心发现 —— **QEMU 8.2 + 自带 OpenSBI 不会自动释放
  副核给载荷**:副核 warm boot 后停在其 HSM 状态机的 `sbi_hsm_hart_wait`
  (M-mode wfi 循环,等 START_PENDING=2),载荷必须显式调用 **SBI HSM
  `hart_start`** 逐个启动副核(与 Linux 多核引导协议一致)。boot hart 发布
  `BOOT_SATP`/`BOOT_RELEASE`(.data)后对每个非自身副核 `hsm_hart_start`;
  副核进入 `_start` → 输掉 BOOT_LOCK 仲裁 → park 轮询 BOOT_RELEASE →
  载入 satp 进入同一 Sv39 身份映射 → per-hart 栈/陷阱栈 → `secondary_main`
  打印 `hart N online` → idle 停等。boot hart 有界自旋等全部副核上线 →
  banner `M2 T3a: multi-core boot ok (N harts online)`。
- **T3a 实测补充(控制台交错)**:三个副核上线与 banner 同时打印,
  `write_str` 的 D9 try_lock 在争用下立即回退裸写 → 逐字符交错 →
  test-smp "3 条 online" 断言偶发失配。新增 `uart::locked_line`(**阻塞**
  拿 CONSOLE_LOCK 后执行;boot 期 SIE 全关、写入者有界 → 阻塞安全),
  `secondary_main` 经它打印 online 行,且 `mark_online` 移到打印之后
  (boot hart 等全部副核打印完归 idle 才打 banner → banner 行原子)。

**验证**:六门禁(clippy/fmt/test/build/test-smp/test-rva23)全绿;test-smp
10 连跑 `online=3 / banner(4 harts)=1 / M0=1 / panic=0` 全确定;
boot 日志干净无交错(见 §4)。CI 双 job 断言四处同步加 T3a banner。

**不做的**(记入遗留,见 §5):SBI IPI 跨核唤醒(T3b 重新验证)、真实跨核
TLB shootdown、多核调度/共享内存(T3b/T3c)。

## 2. 发现明细

### 2.1 副核从不进入内核 `_start`(阻塞级缺陷 —— 本次核心根因)

- **级别**:阻塞级功能缺陷(T3a 主线,曾致多核 bring-up 完全不通)。
- **位置**:QEMU virt + OpenSBI 引导链路。
- **现象**:UART 直写标记显示副核永远不执行内核 `_start` 第一行;
  `-d in_asm` 合并追踪里副核踪迹全是 **M 模式固件地址**(Priv:3)的 wfi
  块,不见任何 S 模式内核地址。
- **根因**:反汇编 `/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.elf`
  定位副核停在 **`sbi_hsm_hart_wait`** —— OpenSBI HSM 状态机的 M-mode
  wfi 循环(`wfi; ld per-hart 值; bne ≠ 2, wfi`,前置 `csrrs zero, mie,
  0x808` = MSIE|MEIE),等待 `SBI_HSM_STATE_START_PENDING`(值 2)。QEMU
  8.2 + 自带 OpenSBI 在 warm boot 后**把副核留在 STOPPED** 状态,由
  载荷自己用 SBI HSM `hart_start` 释放(与 Linux 的 `cpu_ops`/SBI 启动
  协议一致)。此前按计划草案假设"QEMU 所有 hart 同址同时启动"不成立。
- **影响**:无本修复则 T3a 永远不通;副核不是"没被唤醒"而是"根本没
  被启动"。

### 2.2 SBI IPI 不是可靠唤醒手段(设计偏差,改为 HSM)

- **级别**:设计偏差(计划草案假设 IPI 可唤醒,实测否决)。
- **位置**:`sbi.rs` `SBI_EXT_IPI`(EID 0x0073_5049)。
- **现象**:`send_ipi(hart_mask)` 返回 SBI_SUCCESS,但副核仍停 HSM wait,
  收不到 m_software 委托的 S 软中断 —— 因为副核根本不在内核地址空间,
  停在 OpenSBI M 模式 wfi。
- **决定**:副核唤醒改用 SBI HSM `hart_start`;`send_ipi` 保留
  `#[allow(dead_code)]` 供 **T3b 跨核调度唤醒**(把线程放到目标核时唤醒
  该核 idle),T3b 报告需重新验证其可用性。

### 2.3 boot hart 不一定是 hart 0(设计事实,影响唤醒循环)

- **级别**:引导事实(实测 4 核时 boot hart 在 0/1/2/3 间变化)。
- **现象**:M0 日志 `hartid=N` 每次不同;首次实现 `for h in 1..expected`
  在 boot hart=1 时既重复启动自身(HSM 返回 SBI_ERR_INVALID_STATE
  rc=-6)又漏启 hart 0 → "timed out (got 3)"。
- **决定**:唤醒循环遍历 `0..expected`,`if h == boot_hartid { continue; }`
  (含 hart 0;跳过自身)。

### 2.4 D9 控制台字符交错(实测缺陷,本次补充修复)

- **级别**:功能/测试可靠性缺陷(非死锁;日志行偶发错位)。
- **位置**:`uart.rs` `write_str`(D9 try_lock + 裸写回退)。
- **现象**:三个副核同时 `info!("hart N online")` 时,争用失败者立即回退
  无锁裸写 → 逐字符交错(QEMU `-nographic` 串口逐字符慢写放大窗口);
  test-smp 曾只数到 2 条 online、banner 与 online 行互相夹杂。
- **根因**:D9 的 try_lock 在**争用**下也立即回退(其本意是防 panic
  死锁:panic=abort 下守卫不 Drop,阻塞锁会让等待核挂死)。曾试"有界
  重试"方案 —— 实测更糟(慢串口下持锁者写一行期间,竞争者重试预算
  耗尽全回退 → 4 路交错),证明**只有阻塞锁才能保证整行原子**。
- **决定**:不全局改 D9(保留 panic 安全契约);新增 **boot 期专用**
  `uart::locked_line` 阻塞打印 —— boot 窗口 SIE 全关、无 ISR/无重入、
  写入者有界(每核一行),阻塞拿锁安全;`mark_online` 移到打印之后 →
  boot hart 的 banner 在全部副核归 idle 后才打,独占输出原子。

### 2.5 调试设施(开发实证)

- **可用**:`-d in_asm -D file`(合并 per-hart 追踪,以 Priv:3 M 模式块
  识别 OpenSBI 停驻)、UART 直接 MMIO 标记(`'A'+hartid` 写 THR,先于
  UART 初始化可用)、DTB 反汇编、llvm-objdump 反汇编 OpenSBI ELF。
- **不可用**:gdb-multiarch 缺失;QEMU 8.2 移除 QMP `query-cpus`;HMP
  `info cpus` 仅 thread_id;gdbstub 对 GDB 远程协议包完全静默(已弃用)。

## 3. 修复明细

### 3.1 sbi.rs:HSM hart_start + IPI 保留

- `SBI_EXT_HSM = 0x0048_534D`,`hsm_hart_start(hartid, start_addr, spriv,
  sarg1)`:裸 ecall(a7=HSM,a6=0,a0=hartid,a1=_start,a2=0(S 模式),
  a3=hartid → 副核 a0 收到 hartid),`clobber_abi("C")`,返回 a0 错误码。
- `SBI_EXT_IPI = 0x0073_5049`(EID 六位宽补零:0x0073_5049 满足 clippy
  `unusual_byte_groupings`)+ `send_ipi` 保留(dead_code,注释指向 T3b)。

### 3.2 board.rs:FDT cpu 数 + 内核入口地址

- `BOARD_CPU_COUNT`(原子)+ `cpu_count()`(0/未解析回退 1)。
- `kernel_start_addr()` = `_kernel_start` 地址(HSM 启动跳转目标)。

### 3.3 entry.S:副核引导协议(HSM → park → boot_done)

- `park`:`stvec=park`(轮询期异常回到 park 重试);`park_poll` 纯轮询
  `BOOT_RELEASE`(.data 初值 0,防 BSS 未清时误读垃圾值);`tp>=MAX_HARTS`
  者永久停 park(per-hart 数组无槽)。
- `boot_done`:清 sie/sstatus.SIE/sip → 载入 `BOOT_SATP` → `csrw satp` +
  `sfence.vma` + `fence.i` → 恢复 gp → `sp = _idle_stack_base +
  (hartid+1)<<15`(32K 槽顶)→ `sscratch = _trap_stack_base + (hartid+1)<<15`
  → `stvec = trap_vector` → `a0=hartid` → `call secondary_main`。
- BSS 清零终点 `_trap_stack_top` → `_alloc_start`(覆盖 per-hart 数组)。

### 3.4 main.rs:发布协议 + 等待 + 副核主函数

- `wake_secondaries(boot_hartid)`:`expected = cpu_count().min(MAX_HARTS)`;
  单核直接打 banner 返回。多核:发布 `BOOT_SATP`(mmu::satp)→
  `fence rw,rw` → `BOOT_RELEASE=1` → `fence rw,rw` → 对每个非自身副核
  `hsm_hart_start(h, _start, 0, h)`(失败仅 warn,副核仍轮询发布标志)→
  有界自旋(50M 次,超时 panic)等 `harts_online()+1 == expected` →
  `info!("M2 T3a: multi-core boot ok ({} harts online)")`。
- `secondary_main(hartid)`:irq_disable → sanitize_csr → `init_traps(hartid)`
  → `locked_line(|| info!("hart {hartid} online"))` → `mark_online()` →
  idle 停等(SIE 关、无定时器 → wfi 永不被唤醒,等效停等)。
- T3a 副核**无定时器、无调度器**:共享调度器 `on_tick` 会把帧写进
  `s.current`(boot hart 线程)TCB,破坏调度状态,故副核绝不 enable_timer、
  绝不进共享调度器路径(T3b 加 per-CPU 调度后解除)。

### 3.5 uart.rs:locked_line(整行原子,仅 boot 期)

```rust
pub fn locked_line(f: impl FnOnce()) {
    if PANIC_OUTPUT.load(Ordering::Relaxed) { f(); return; }
    let _guard = CONSOLE_LOCK.lock();   // 阻塞自旋
    f();
}
```
- 安全论证:调用方限定 boot 期(各核 SIE 全关,无 ISR、无重入);写入者
  有界(每核一行,持锁者必然释放 → 等待者必然拿到);panic 短接
  (PANIC_OUTPUT 下不取锁)。**不能**用于 panic 之后的通用输出(阻塞锁
  在 panic=abort 守卫不 Drop 时会挂死等待核)—— 通用多核日志仍走 D9
  try_lock + best-effort 回退。

### 3.6 Makefile / ci.yml:banner 断言四处同步 + test-smp 强化

- `test` / `test-rva23` / ci build / ci rva23:新增 `grep -q "M2 T3a:
  multi-core boot ok"`(部分匹配,兼容单核 `(1 harts online)` 与多核
  `(4 harts online)`)。
- `test-smp`(Makefile + ci):`M0 count == 1`(原断言)+ **`hart [0-9]
  online` count == 3**(locked_line 后行原子,可靠)+ `M2 T3a: multi-core
  boot ok` + 无 panic/trap。

## 4. 验证结果

六门禁全绿(dev+release 由 docker-make.sh 统一执行):

```
=== GATE: clippy === PASS     === GATE: fmt === PASS
=== GATE: test === PASS       === GATE: build === PASS
=== GATE: test-smp === PASS   === GATE: test-rva23 === PASS
```

test-smp(-smp 4)10 连跑全确定:

```
run1:  online=3 banner_full=1 m0=1 panic=0   ...  run10: 同
```

boot 日志(干净无交错,`-smp 4`,boot hart=3):

```
[000000] [INFO ] M0: boot ok - arch: riscv64, machine: qemu-virt, hartid=3, fdt=0x87e00000
[000126] [INFO ] hart 0 online
[000126] [INFO ] hart 2 online
[000126] [INFO ] hart 1 online
[000126] [INFO ] M2 T3a: multi-core boot ok (4 harts online)
```

单核 test(-smp 1)回归:既有 12 banner + T3a banner(1 harts)+ uptime≥2 +
无 panic 全过;RVA23(-cpu max)同断言全过。

## 5. 遗留风险

- **SBI IPI 可靠性未定**:T3a 实测不能唤醒 HSM 停驻副核;T3b 作为跨核
  调度唤醒手段必须重新验证(目标核已在内核 idle,与 boot 场景不同)。
- **通用多核日志可能交错**:D9 try_lock 的 best-effort 回退在争用下仍会
  字符交错(设计如此,防 panic 死锁)。T3b/T3c 若出现多核同时日志,断言
  必须避开对行原子的依赖,或用 `locked_line`(仅限 boot 窗口)。
- **无真实跨核 TLB shootdown**:revoke(T3c)以"satp 切换全刷 + 本核
  sfence"简化(M2 单页共享、revoke 即销毁页,无残留映射复用)。
- **副核无定时器/无调度器**:T3b 落地 per-CPU 就绪队列 + per-hart
  deadline 前,副核仅 idle 停等;共享调度器路径对副核是禁区(见 §3.4)。
- **MAX_HARTS=4 硬上限**:超出 QEMU -smp 4 的 hart 永久停 park(per-hart
  数组无槽);在线核数取 FDT cpu 数 ∩ MAX_HARTS。
- **多核压力未压**:只做功能验证(banner/上线/idle);多核竞争规模压测
  归 T3b/T4。

## 附:T3a 与 T3 计划的偏差说明

计划草案原以 SBI IPI + "副核同址同时启动"为 D8 机制;实测 QEMU+OpenSBI
**不会自动释放副核**,改为 SBI HSM `hart_start` 显式启动(计划风险节已
预留"若异常回退:副核纯轮询 BOOT_RELEASE"为兜底;最终机制 = HSM 启动 +
park 轮询 BOOT_RELEASE 放行,二者配合)。banner/断言/报告按计划落地。
