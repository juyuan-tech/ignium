# M2 D12 报告:用户态异常恢复 + 进程销毁/页回收 + 安全性能 + 自审

- 日期:2026-08-29
- 阶段:M2(微内核骨架)主线 D12(用户态异常恢复,ROADMAP 遗留项)+ 进程
  销毁/页回收 + IPC 延迟基准 + release 安全性能
- 提交:`feat: M2 D12 - ...` / `perf: M2 - ...` / `docs: M2 收官 - ...`
  (分三提交,见 §5 与 git log)
- 前置:M2 T3a/T3b/T3c 收官(上一阶段,报告 `2026-08-29-m2-t3c-sharedmem-cap.md`)
- M2 验收(本阶段对应部分):**用户态故障只杀本进程、系统存活;进程退出后
  地址空间页全回收;IPC 延迟可测并记录;release 构建纯编译期提速**

## 1. 摘要

M2 收官阶段处理三件遗留 + 一次自审:

1. **D12 用户态异常恢复**(ROADMAP 主线遗留项):此前 `trap_handler` 对任何非
   ecall 同步异常一律 `dump + halt` 整机停机 —— 用户进程一次访存错误即拖垮
   全系统。本次改为 **用户态故障只杀本进程**(SPP=0 判据),完整实现"杀进程"
   路径:清理 IPC 挂起 → 标记进程全部线程退出 → 撤销陈旧捐赠 → 切回内核根表
   → 销毁进程地址空间。
2. **进程销毁 / 页回收**(已知泄漏):此前进程退出/被杀后地址空间页从未回收。
   新增 `mmu::destroy_root`(递归释放进程自有页)+ `process::destroy`(Shm revoke
   → 槽失效 → 根表回收),配合 kill 路径与最后线程退出钩子,页数精确回收。
3. **IPC 延迟基准 + release 安全性能**:新增 IPC ping-pong 延迟基准(每往返
   ~4 µs,已入 benchmarks.md);`[profile.release]` 加 `lto="fat"` +
   `codegen-units=1`(纯编译期优化,不触碰 panic/溢出策略),热路径补
   `#[inline]`。
4. **自审**(外部 AI 审计不运行,按用户决策仅自审):发现并修复一个
   **PIP 捐赠表 donor 方向陈旧项泄漏**(见 §2.4)、一个**测试辅助进程的守护页
   永久泄漏**(§2.5)、一个 **LTO 后 slab 基准虚报 0 ns/op**(§2.6,LLVM
   malloc-elimination);逐条核对机器码、锁序、释放顺序。

**验证**:五门禁(clippy/fmt/test/test-smp/test-rva23)全绿;单核/SMP/RVA23
日志均见 D12 杀进程诊断 + 恢复 banner + IPC 延迟 banner,无 `KERNEL PANIC|TRAP:`。
详见 §4。

## 2. 发现明细

### 2.1 D12 缺失:用户态故障整机停机(阻塞级缺陷,主线遗留)

- **级别**:阻塞级(M2 ROADMAP 主线项未落地)。
- **位置**:`arch/riscv64.rs` `trap_handler`(旧实现:非 ecall 同步异常 → dump + halt)。
- **触发条件**:任何用户线程在 U 模式执行产生同步异常(页故障 `scause=0xd`、
  非法指令、未对齐访问等)之一。
- **影响**:单个用户进程故障 → `dump + halt` 整机停机;多进程隔离形同虚设,
  M2"进程故障不拖垮系统"的验收目标不达标。

### 2.2 进程退出/被杀后地址空间页从不回收(泄漏,主线遗留)

- **级别**:内存泄漏(M2 T1.5 引入进程后即存在)。
- **位置**:进程根表(root + 各级表页 + 用户页)无任何释放路径;测试进程退出后
  页数单调下降。
- **影响**:每进程约 10 页(7 表页 + 3 用户页)泄漏;进程反复建/销 → buddy 耗尽。

### 2.3 IPC 延迟无基准(验收缺口)

- **级别**:验收缺口(ROADMAP 要求"延迟可测并记录",benchmarks.md 无 IPC 数据)。
- **位置**:`docs/benchmarks.md`("阶段 4 扩充"空挂)。

### 2.4 PIP 捐赠表 donor 方向陈旧项泄漏(自审新发现,已在本次修复)

- **级别**:审计发现(自审发现,非既有 bug 报告;未修复前为泄漏)。
- **位置**:`sched.rs` kill 路径(旧:`kill_current_process` 只调
  `revoke_donations_for_proc`,只清理 `peer_proc == pid` —— 指向被杀的)。
- **触发条件**:进程 A 的一个线程阻塞于 `ipc::send(A → B)`(登记 A→B 捐赠),
  随后 A 被用户态故障杀掉。
- **影响**:
  1. **永久优先级抬升**:A→B 的捐赠永不撤销,B 的全部线程永远被按 A 的捐赠
     优先级调度(即使 A 已不存在),优先级继承语义被陈旧数据污染;
  2. **捐赠表槽泄漏**:`donations` 表为定长 `MAX_DONATIONS`,陈旧项占死槽位,
     后续合法捐赠无法登记(静默丢弃)。
- **根因**:`purge_process` 只唤醒**存活的配对方**并投递错误;被杀进程自己的
  线程(含阻塞 donor)被标记退出、**不再有线程自身触发 `revoke_donations` 的
  机会**,donor 方向的捐赠无人清理。

### 2.5 测试辅助进程守护页永久泄漏(自审发现,先于本次修复存在)

- **级别**:审计发现(测试辅助函数自身缺陷)。
- **位置**:`tests.rs` `map_iso_proc`:`let _stack_lo = alloc_pages_zeroed(0)`
  —— 分配了一页**从未映射、从未释放**(注释称守护页,实为死分配)。
- **影响**:每建一个测试进程泄漏 1 页;D12 回收断言(`free_after >= free_before`)
  精确失败(实测 `free 28480 < 28482`,差 2 页 —— 两个进程各 1 页)。
  守护语义由"0x4000_2000 处不映射的洞"天然提供,该分配纯属多余。

### 2.6 LTO 后 slab 基准虚报 0 ns/op(自审发现)

- **级别**:审计发现(测量卫生问题,不属产品缺陷)。
- **位置**:`heap.rs` `bench()`;`profile.release` 开启 `lto="fat"` +
  `codegen-units=1` 后,LLVM 对"无逃逸的 alloc→dealloc 对"整体消除
  (malloc-elimination),基准恒报 0 ns/op。
- **影响**:LTO 后的性能数字失真,无法纵向对比。

### 2.7 其它(低危/文档)

- `sched.rs` `kill_current_process` 的嵌套 if(clippy collapsible_if,`-D warnings`
  门禁红灯)。
- DEFERRED.md 中 D7/D8/D9/D19 已实现仍标"待办"(状态过期,见 §5 文档更新)。

## 3. 修复明细

### 3.1 杀进程路径(`sched.rs`)

新增 `kill_current_process(scause, sepc, stval) -> !`(trap 上下文调用,永不返回):

1. 诊断:`error!("D12: user fault scause=... sepc=... stval=...; killing process")`
   —— **刻意不含字面 `TRAP:`**(门禁 `grep -qE "KERNEL PANIC|TRAP:"` 误判纪律);
2. `ipc::purge_process(pid)`:清理 IPC 挂起(见 3.2),唤醒存活配对方;
3. SCHED 锁内:进程全部线程(除当前)置 `Exited`、恢复数据失效、栈入 reaper、
   槽入 free_slots;就绪队列摘除被杀线程;`revoke_donations_for_proc`(指向被杀)+
   **`revoke_donations_of(killed, skip_proc)`(被杀线程发出的,§2.4 修复)**;
4. `mmu::switch_root(kernel_root)` —— **必须先切走再释放**(当前 satp 仍指向
   进程根表,直接 free 会使取指/访存立即故障);
5. `process::destroy(pid)`(见 3.3);
6. `sched::exit_from_trap()` 切到下一线程(do_switch 内按线程根表 switch_root)。

多核 Running 线程局限:被杀进程若某线程正 Running 于其它核,该核调度器拥有其
TCB,本路径跳过(不回收栈/槽)—— 地址空间已销毁,该线程下一次用户访存会再次
走本路径自愈(文档化,§5 遗留风险)。

### 3.2 IPC 挂起清理(`ipc.rs`)

新增 `purge_process(pid)`:移除 pending 队列所有 `sender_pid==pid ||
recver_pid==pid` 项;对**存活配对方**(等 pid 发消息的 recver / 等 pid 收消息的
sender)经 `ipc_wake_with_err(tid, -ENOENT)` 唤醒投错,防止永久挂起。锁序
IPC → SCHED 不重叠。

### 3.3 进程销毁与页回收(`process.rs` + `mmu.rs`)

- `mmu::destroy_root(root)`(新):递归走查 Sv39 根表,只释放**进程自有**页
  —— U=1 叶子物理页 + 各级表页 + 根页;U=0(内核区)叶子/超页跳过
  (不 double-free 内核共享页)。`map_super` 写 2MB 叶在 L1 层,故 L1/L2 非叶
  表项可安全下钻。验证:destroy 精确释放 10 页(7 表 + 3 用户)。
- `process::destroy(pid)`(新):
  1. 先 revoke 本进程全部 `Cap::Shm`(TABLE 锁内收集、锁外逐个
     `shm_revoke`:撤双方映射 + 清双方槽 + 释放物理页 + 出表)—— **必须在
     地址空间释放前完成**,否则共享页仍 U 映射会 double-free;
  2. 锁内"捕获 root + 原子失效槽"(`id=usize::MAX` + 入 free 池):并发 destroy
     在此串行化,杜绝同一根表双释放;`pid_root`/`cap_target` 随后返回 NotFound;
  3. 锁外 `destroy_root(root)`。
- 最后线程退出 → 销毁进程:进程用户线程经 `exit()` 正常退出时,若进程已无线程
  存活则触发 `process::destroy`(与 kill 路径共用 3.1/3.3)。

### 3.4 trap 分派(`arch/riscv64.rs`)

非 ecall 同步异常按来源分派:`frame[CS_SSTATUS] & (1<<8)` == 0(SPP=0,用户态)
→ `kill_current_process`;== 1(内核态)→ 仍属内核 bug,保持 dump + halt。

### 3.5 PIP 捐赠表双向清理(`sched.rs`,§2.4 修复)

新增 `revoke_donations_of(&mut self, tids: &[usize], skip_proc: Option<usize>)`:
撤销 `donor_tid ∈ tids` 的全部捐赠,受影响 peer 按自然优先级回落重排;
`skip_proc` = 被杀进程自身(其线程已标记退出,重排会复活,不重排)。
`kill_current_process` 收集 victims + 当前线程(由 exit_from_trap 标记退出)后调用
—— 与 `revoke_donations_for_proc`(peer 方向)互补,双向无陈旧项。

### 3.6 守护页泄漏修复(§2.5)

`map_iso_proc` 删除死分配 `let _stack_lo = alloc_pages_zeroed(0)`;守护语义
(0x4000_2000 不映射的洞)不变。修复后 D12 回收断言 `free_after >= free_before`
精确成立。

### 3.7 基准卫生(§2.6)

`heap::bench` 在 dealloc 前加 `core::hint::black_box(p)` 防止 malloc-elimination;
计时换算用运行时 `board::timer_freq()`(V4 审计项,不硬编码 10 MHz)。

### 3.8 安全性能(`Cargo.toml` + 热路径)

- `[profile.release]`:`lto = "fat"`、`codegen-units = 1`(保留
  `panic="abort"`、`overflow-checks=true`)—— 纯编译期优化,零运行时语义变化;
- `#[inline]`:`sched::current_id/current_proc`、`process::cap_target/cap_errno`,
  配合 fat-LTO 跨模块内联 syscall/调度热路径;
- clippy collapsible_if 合并(§2.7)。

### 3.9 测试(`tests.rs`)

`boot_fault_recovery_test`(D12 回归):建"受害者 F"与"健康进程 H";F 授权
`cap(F,0,H)`;F 的 donor 内核线程先 `send(F→H)` → NoPeer → 阻塞并登记 F→H 捐赠
(§2.4 触发路径);F 用户线程执行解码核对的 `lw t0, 0(x0)`(0x0000_2283,页故障)
→ 杀进程;H 线程写 marker 正常退出。断言:kill 计数==1;`pid_root(F)==None`;
`pid_root(H)==Some(root_h)`;**`donation_count()==0`(§2.4 修复的直接验证)**;
显式 `drain_reaper()` 后 `free_after >= free_before`(§2.5 修复验证)。banner:
`M2: user fault recovery ok (process killed, system alive)`。

`boot_ipc_latency_bench`:T2b 进程对 + cap 授权,sender 用 `arch::get_time()` 对
N=1000 次 send/recv 配对往返计时,`ticks/N` 折算 µs。banner:
`M2: IPC latency ok (reg-msg ~{ticks} ticks ~{us} us/roundtrip)`。

两条新 banner 同步进 Makefile `test/test-smp/test-rva23` 与 ci.yml
`build/smp/rva23` 共 6 处 grep 断言(门禁纪律)。

## 4. 验证结果

### 4.1 五门禁

| 门禁 | 结果 |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo clippy --release -- -D warnings` | PASS(0 警告) |
| `make test` | TEST PASS |
| `make test-smp` | SMP TEST PASS |
| `make test-rva23`(-cpu max,zba/zbb/zbs/zicond) | RVA23 TEST PASS |

### 4.2 单核日志(关键行)

```
[000126] [ERROR] D12: user fault scause=0xd sepc=0x40000000 stval=0x0; killing process
[000126] [INFO ] M2: user fault recovery ok (process killed, system alive)
[000127] [INFO ] M2: IPC latency ok (reg-msg ~44188 ticks ~4 us/roundtrip)
[000129] [INFO ] bench: slab 64B alloc+dealloc ≈ 179 ns/op
[000129] [INFO ] bench: context switch ≈ 261 ns/op (yield path)
```

### 4.3 SMP 日志(关键行)

```
[000127] [ERROR] D12: user fault scause=0xd sepc=0x40000000 stval=0x0; killing process
[000128] [INFO ] M2: user fault recovery ok (process killed, system alive)
[000128] [INFO ] M2: IPC latency ok (reg-msg ~44954 ticks ~4 us/roundtrip)
[000133] [INFO ] bench: slab 64B alloc+dealloc ≈ 191 ns/op
[000136] [INFO ] bench: context switch ≈ 284 ns/op (yield path)
```

### 4.4 性能对比

| 指标 | M1.5 基线* | 本阶段(Phase 0 捕获) | 本阶段(优化后) | 说明 |
|---|---|---|---|---|
| 上下文切换(yield) | 200–284 ns/op | ~346 ns/op | **261 ns/op(单核)** | −25%(LTO + inline);QEMU 抖动 ±20% |
| slab 64B alloc+dealloc | 19–24 ns/op* | (未记录诚实值) | **179 ns/op** | black_box 后诚实值 |
| IPC 往返(reg-msg) | — | — | **~4 µs**(44188 ticks) | 含阻塞/上下文切换语义 |

\* M1.5 基线为 WSL2 环境、非 black_box 版本,与容器/QEMU 非同一测量环境,
绝对值**不可直接对照**;本阶段结论只取同一环境同一方法的纵向对比(Phase 0
捕获 → 优化后)。slab 的 M1.5 数字疑含部分 malloc-elimination,不作回归判定。

### 4.5 自审核对点

- **释放正确性**:`destroy_root` 只放 U=1 叶子 + 表页 + 根页;内核区 U=0 跳过;
  Shm cap 先行 revoke(否则 double-free);回收断言精确成立(10 页/进程)。
- **锁序**:TABLE → IPC → SCHED,未逆序;purge_process 释放 IPC 锁后再取 SCHED。
- **trap 切换顺序**:先 `switch_root(kernel_root)` 再 `process::destroy`(防 UAF)。
- **机器码**:`lw t0,0(x0)` = `0x0000_2283` 逐条解码核对(I 型,lw 主操作码
  `0x03`、funct3 `0b010`、rs1=x0、rd=t0、imm=0)。
- **门禁纪律**:D12 诊断无字面 `TRAP:`;新 banner 6 处 grep 断言同步。

## 5. 遗留风险 / 后续

- **多核 Running 线程**(已文档化):被杀进程线程正 Running 于其它核时不回收其
  栈/槽(该核调度器拥有 TCB);其下次用户访存再次走杀进程路径自愈。M3 跨核
  IPI 停核后彻底解决。
- **slab 空页懒回收**:已在本轮完成(2026-08-30,`docs/reports/2026-08-31-m2-slab-return-cleanup.md`):
  全空**非 head** slab 页在下次任一档 grow 时摘链归还 buddy;head 页保留作
  快复用缓存,热路径逐指令不变。残余:空页在下次 grow 前暂留(有界)。
- **D1 中断快速路径 / ELF 加载器 / RVA23 P2 / D24 多 bank**:按用户决策文档化
  延后(见 DEFERRED.md 刷新),与本次"安全"目标冲突项(M3 评估)。
- **IPC 延迟绝对值**:QEMU 虚拟时钟抖动 ±20%,~4 µs 为量级参考;真机 bring-up
  后重新标定。
- **性能绝对值对照**:M1.5 与当前非同一测量环境,benchmarks.md 已注明;
  后续所有对比统一"同一环境同方法"基线。

## 6. 提交

1. `feat: M2 D12 - 用户态异常恢复(进程故障→杀进程)+ 进程销毁/页回收`
   (kill/purge/destroy/destroy_root/revoke_donations_of/trap 分派/测试/门禁)
2. `perf: M2 - IPC 延迟基准 + release LTO/codegen-units 安全性能`
   (bench + black_box + #[inline] + Cargo.toml/门禁)
3. `docs: M2 收官 - 全量文档同步 + 自审报告`
   (本报告 + README/ROADMAP/DEFERRED/DESIGN/M2-DESIGN/RVA23/benchmarks/SECURITY)

tag:`v0.1.0-M2`,随代码提交同步推送。
