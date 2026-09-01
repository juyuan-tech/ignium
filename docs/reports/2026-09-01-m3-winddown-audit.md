# M3 收尾全面审查 + 修复(2026-09-01)

日期:2026-09-01
阶段:M3 入口收官后的全量自审(多 agent 分维审查 + 人工精读 + 全部修复过五门禁)

## 1. 摘要

对 M3 入口(tag 前)的代码做一次**全面自审**:分 4 维并行审查(bugs / 性能 /
安全 / 文档/代码卫生),逐项核实后落地修复。本轮目标:任何问题都不放过,
只做**安全**的性能改进,文档体系规整不杂乱。

产出:

- **5 个隐藏 bug 修复**:B1(IPC 跨核 wake-before-block,潜在死锁)、
  B2(sys_write 共享页 TOCTOU,一次 syscall 可停机内核)、B3(unmap_4k 静默
  分配页表页)、B4(ELF 段上界溢出 → panic)、sys_write 缓冲回绕越界。
- **1 项安全性能优化**:P2(单地址 `sfence.vma` 取代全量 TLB 冲刷)。
- **1 项评估后延后**:P1(IPC 路径 3 次顺序 SCHED 锁获取,非 bug,收益≈0,
  延后 M4,登记 D28)。
- **2 项历史遗留落实**:D21(UART 分频按 FDT clock-frequency)、清除
  arch/mod.rs 过期 `TIMER_INTERVAL` 引用与 mem.rs 过期 `#[allow(dead_code)]`。
- **文档体系规整**:修复坏链/错误码/过期"已知局限",刷新 README/AGENTS/
  ROADMAP/benchmarks,统一报告命名,新增双索引(总结各个文档)。

全部修复通过五门禁(clippy `--release -D warnings` / fmt / test / test-smp /
test-rva23),dev + release 双 profile 可编,三 log 零字面 `KERNEL PANIC|TRAP:`。

## 2. 审查方法

- **并行 agent 分维审查**:bugs(逻辑/并发/边界)、perf(锁/冗余计算/热路径)、
  security(用户可达停机面/泄漏)、docs+hygiene(过期引用/文档漂移/注释完备)。
  每维返回结构化发现,人工逐一核实(读代码确认可行路径与可达性)后定级。
- **人工精读**:对 agent 指出的高风险路径(sys_write 拷贝、IPC 唤醒协议、
  unmap/ELF 解析)逐行复核,确认触发条件与修复正确性。

## 3. 发现明细

### F1(HIGH,B1)IPC 跨核 wake-before-block 竞态 —— 唤醒丢失死锁

- **位置**:`kernel/src/sched.rs`(`ipc_wake_with_msg`/`ipc_wake_with_err`/
  `block_current`/`block_user_from_trap`)+ `kernel/src/ipc.rs`(send/recv)。
- **触发**:单核不可达(M2 T2a 单核 + trap 上下文 SIE=0,配对双方在 pending
  队列保证下必已阻塞);**跨核 IPC 落地(M3-2)后可达**:核 A 的配对方先
  `ipc_wake_with_msg` 命中目标,而目标线程在核 B **尚未执行到
  `block_user_from_trap`**(syscall 已返回 NoPeer 但帧复制/阻塞未完成)。
  原实现只处理 `state == Blocked`:目标未阻塞时**唤醒被静默丢弃** →
  目标照常阻塞且永远无人再唤醒 → **死锁**。
- **根因**:唤醒协议假设"NoPeer 登记后无调度点、目标必已阻塞";该假设
  单核成立,跨核不成立(wake 与 block 分属两核,无全局原子窗口)。

### F2(HIGH,B2)sys_write 共享页 TOCTOU —— 一次 syscall 可停机内核

- **位置**:`kernel/src/syscall.rs`(`sys_write` 拷贝段)+ `kernel/src/shm.rs`。
- **触发**:用户缓冲指向**共享页**(M2 T3c);对端进程在另一核并发
  `cap_revoke → shm_revoke`。revoke 撤双方 `SHM_VA` 映射并**释放物理页**。
  若 revoke 恰在「逐页校验之后、SUM=1 拷贝完成之前」执行 → S 模式直读
  已释放页 → `scause=0xd` S 模式页故障 → **内核停机**(用户一次 syscall
  DoS,不受 D12 保护)。
- **根因**:校验与拷贝非原子;共享页的生命周期由**对端核**的 revoke 控制,
  本核拷贝路径无任何锁阻断该撤映射。

### F3(MED,B3)unmap_4k 对未映射地址静默分配页表页

- **位置**:`kernel/src/mmu.rs`(`unmap_4k`)。
- **触发**:对**从未映射**的地址调用 unmap(如 shm_revoke 对已销毁/复用槽的
  根表、elf::rollback 部分失败回退)时,`ensure_table` 会**新建 1-2 个清零
  页表页**并留在根表中(直到 destroy_root 才回收)。
- **影响**:掩蔽调用方错误(原本应报 Err 的情况"成功");属预期外分配
  (D11 容量预留外的页表页),多核下叠加页表页泄漏。
- **根因**:`leaf_pte` 只读走查叶子,未区分"中间级缺失"与"叶子=0"。

### F4(MED,B4)ELF 段上界未校验 —— 恶意 ELF 可令内核 panic 停机

- **位置**:`kernel/src/elf.rs`(parse 段校验)。
- **触发**:仅校验 `p_vaddr < USER_VA_LIMIT`(下界)。恶意 ELF 可令
  `p_vaddr + p_memsz` 逼近 `usize::MAX`:
  - 段重叠检查 `a.vaddr + a.memsz`(未 checked)溢出 → overflow-checks 下
    **内核 panic 停机**(引导期加载不可恢复);
  - `map_segment` 的 `align_up(x)`(`x + 4095`)同样溢出。
- **根因**:段上界校验缺失,下游两处未检算术暴露溢出。

### F5(MED)sys_write 缓冲地址回绕 —— 绕过逐页校验后 S 模式越权读

- **位置**:`kernel/src/syscall.rs`(`sys_write`)。
- **触发**:`buf + len` 在 `buf` 近 `usize::MAX` 时回绕,`last` 回绕到小地址
  → `while va <= last` 整体跳过 → **逐页校验被绕过**,SUM=1 拷贝直读非规范
  地址 → S 模式 load 页故障 → 内核停机。
- **根因**:未用 `checked_add`;`USER_VA_LIMIT` 守卫只覆盖"正常大地址",
  不覆盖回绕后的小地址。

### F6(LOW,P2 评估)全量 TLB 冲刷浪费

- **位置**:`kernel/src/mmu.rs`(`map_user_page`/`unmap_4k`)。
- **观察**:映射/解映射单地址用 `sfence.vma zero, zero`(全量冲刷),高频路径
  (建进程/ELF 映射/mmap_share/IPC 的守卫页解映射)反复清空整条 TLB。
- **安全收益**:单地址 `sfence.vma {vaddr}, zero` 即可(两函数均只影响该 VA:
  map 拒绝覆盖已有 PTE;unmap 只清该地址)。**执行**。

### F7(LOW,P1 评估)IPC 路径 SCHED 锁 3 次顺序获取

- **位置**:`kernel/src/ipc.rs`(send/recv)+ `kernel/src/sched.rs`。
- **观察**:NoPeer 路径 `current_id()` → `donate_on_block()` →
  `block_user_from_trap()` 各取一次 SCHED 锁,共 3 次。
- **核实**:三次均**顺序、不嵌套**(trap 上下文 SIE=0 无重入),**无死锁/无
  正确性问题**;纯性能微项,SCHED 锁在 syscall 路径无竞争,收益 ≈ 0;合并需
  改 `block_user_from_trap` 签名与 donation/IPC-wake API 契约(触碰 F1 刚改的
  最关键路径),风险 >> 收益。**延后**,登记 DEFERRED D28。

### F8(信息)D21 历史遗留落地核验 + 陈旧代码引用

- D21(UART 分频按 FDT clock-frequency)此前已实现,本轮全门禁复验通过。
- `kernel/src/arch/mod.rs` 残留过期 `TIMER_INTERVAL`(应为 `timer_interval`)。
- `kernel/src/mem.rs` `alloc_pages_zeroed` 残留过期 `#[allow(dead_code)]`。

### F9(信息)文档漂移与坏链(详见 §6)

- 唯一坏链 `docs/ROADMAP.md`(M2-DESIGN:190 / M3-DESIGN:234 误用 `docs/` 前缀)。
- M3-DESIGN §5 `sys_write` 错误码错写(实为 `-EINVAL`,非 `-EFAULT`)。
- M3-DESIGN §6 行号漂移(sched.rs:1180/1208-1211、riscv64.rs:534)。
- SECURITY.md「已知局限」残留 M3 已消项(内核栈守护页/跨核 shootdown)。
- README 文档树/内核树缺 M3 新文件(elf.rs / M3-DESIGN / SYSCALLS)。
- ROADMAP:17 `docs/compat-baseline` 坏引用。

## 4. 修复明细

### R1(对 F1,B1)`ipc_wake: Option<IpcWake>` 待消费唤醒协议

- `kernel/src/sched.rs`:新增 `enum IpcWake { Msg([usize; MSG_WORDS]), Err(usize) }`
  + Thread 新字段 `ipc_wake: Option<IpcWake>`(3 个线程 init 点置 None)。
- `ipc_wake_with_msg`/`ipc_wake_with_err`:目标 `state != Blocked` 时**不写帧/
  不入队**,存 `ipc_wake` + `ipc_msg` 后返回(避免"写入的帧被阻塞点覆盖"与
  "Running+就绪队列双调度窗口")。
- `block_current`:阻塞前 `ipc_wake.take()` 命中 → 消费唤醒、保持 Running、
  撤销捐赠、**跳过阻塞**(内核线程随后 `take_ipc_msg` 读消息)。
- `block_user_from_trap`(签名改 `-> bool`):命中待消费唤醒 → 把 Msg/Err
  **写到活帧**(a0/a1..a5/sepc+4)并返回 `false` → 调用方 sret 回用户即达
  (不切走);否则照常阻塞。syscall.rs 两处 NoPeer 分支适配返回值。
- 与 `woken` 协议同构但 IPC 专用:不污染 woken 的互斥/条件语义;IPC 靠
  pending 队列保证配对(有意不用 woken),本字段只补"阻塞前配对"窗口。
- **可达性**:单核当前测试不可达(SIE=0 无调度点);M3-2 跨核 IPC 落地后生效。

### R2(对 F2,B2)sys_write 拷贝期间持 SHM 锁守卫

- `kernel/src/shm.rs`:新增公开包装守卫 `ShmCopyGuard`(隐藏私有 `ShmTable`,
  避免 clippy private-type-leak)+ `pub fn lock_guard()`。
- `kernel/src/syscall.rs`:校验通过后、置 SUM 拷贝前取 `lock_guard()`,拷贝
  完成后释放。`shm_revoke`(取同一 SHM 锁)在拷贝期间无法撤映射/释放页 →
  TOCTOU 窗口闭合。
- 锁序安全:SHM 锁为独立叶子锁(契约:不与 IPC/SCHED 同持),sys_write 拷贝
  路径不持任何其它锁,SIE=0 无抢占;无锁序问题。

### R3(对 F3,B3)unmap_4k 先读叶子,无映射直接 Ok

- `leaf_pte(root, vaddr) == 0` → 中间级缺失或叶子=0,无映射可撤 → 直接
  `Ok(())`,**不调用 ensure_table**(不再静默分配页表页)。超页叶子 → 非零,
  仍走 ensure_table 报 Err(需先拆分,语义不变)。

### R4(对 F4,B4)ELF 段上界 checked_add + USER_VA_LIMIT 双守卫

- `kernel/src/elf.rs`:在 `p_vaddr >= USER_VA_LIMIT` 下界检查后新增
  `p_vaddr.checked_add(p_memsz).is_none_or(|end| end > USER_VA_LIMIT)` →
  `ElfError::AddressTooHigh`,一次性消除段重叠检查与 `align_up` 两处未检
  算术的溢出面(此前 overflow-checks 下均为内核 panic 停机)。

### R5(对 F5)sys_write 缓冲回绕守卫

- `kernel/src/syscall.rs`:`buf.checked_add(len)` 回绕 → `-EFAULT`;
  `end > USER_VA_LIMIT` → `-EFAULT`(与逐页校验失败同码)。

### R6(对 F6,P2)单地址 sfence

- `kernel/src/mmu.rs`:`map_user_page`/`unmap_4k` 的 TLB 冲刷改为
  `sfence.vma {vaddr}, zero`(rs1=vaddr)。正确性:map 拒绝覆盖已有 PTE、
  unmap 只影响该 VA,单地址冲刷保证正确;守卫页解映射等高频路径不再清空
  整条 TLB。

### R7(对 F8)D21 落实 + 陈旧引用清除

- `kernel/src/fdt.rs`:`BoardParams.uart_clock` + `clock-frequency` 解析 arm;
  `kernel/src/board.rs`:`DEFAULT_UART_CLOCK = 3_686_400`、校验 1MHz..100MHz、
  `pub fn uart_clock()`;`kernel/src/uart.rs`:`BAUD_RATE` + `uart_divisor()`
  (clk/(16×BAUD),clamp [1,0xFFFF],异常回退 0x0C),`init_hw` 写 DLL/DLM。
- `kernel/src/arch/mod.rs`:过期 `TIMER_INTERVAL` → `timer_interval`。
- `kernel/src/mem.rs`:删除过期 `#[allow(dead_code)]`。

## 5. 验证结果

- 五门禁全绿:clippy `--release -D warnings` / `fmt --check` / `make test` /
  `make test-smp` / `make test-rva23`(exit=0);dev + release 双 profile 可编。
- 三 log(单核 / smp / rva23)均 **0** 条字面 `KERNEL PANIC|TRAP:`(门禁断言
  通过 + 独立 grep 复核 exit=1)。
- 既有 banner 全部回归:M3 T1/T2/T3 与 M2 全套断言齐全(6 处 grep 一致)。
- B1/B2/B3/B4/P2/D21 修复后无回归(全部既有测试通过)。

## 6. 文档体系规整(2026-09-01)

- **坏链修复**:M2-DESIGN:190、M3-DESIGN:234 `docs/ROADMAP.md` → `ROADMAP.md`
  (全仓库唯一坏链,已复核);ROADMAP:17 `docs/compat-baseline` → DESIGN.md §铁律。
- **错误码对齐**:M3-DESIGN §5 `sys_write len>4096` → `-EINVAL`(现错写
  `-EFAULT`;`-EFAULT` 仅保留给"任一页未映射/缓冲不可访问"),与 SYSCALLS.md
  及 syscall.rs 常量一致。
- **行号刷新 + 实现状态**:M3-DESIGN §6.1 标注"M3 T2 已解决"(sched.rs:
  1180/1208-1211 → 1511,riscv64.rs:534 → 555)。
- **SECURITY.md「已知局限」**:删除 M3 已消项(内核线程栈守护页、跨核
  shootdown),改写为 M3 收官安全模型(含 ELF 加载器新增攻击面)。
- **README.md**:文档树补 M3-DESIGN/SYSCALLS;内核树补 elf.rs;sched/syscall/
  mmu/tests 描述刷新到 M3 状态。
- **AGENTS.md**:环境段标注双环境(WSL2 + Arch 主机 + docker 容器;容器门禁
  命令),工具链锁定 1.97.1 不变。
- **benchmarks.md**:D1 优化方向同步 M3 评估结论(评估后不做,见 D1)。
- **命名规整**:`audit16-proactive.md` → `audit-16-proactive.md`(git mv,与
  audit-10..18 系列编号一致)。
- **新增索引**(总结各个文档):`docs/reports/README.md`(全部报告一句话摘要
  索引,分里程碑/阶段/审计/自审/加固五组)+ `docs/audit-reports/README.md`
  (20 份外部 AI 审计归档说明;历史保留不删)。
- **DEFERRED.md**:D21 标记已实现(2026-09-01);新增 D28(P1 评估后延后)。

## 7. 遗留风险

- **B1 跨核 IPC 唤醒路径未被单核测试覆盖**:当前 QEMU 测试全部单核 trap
  上下文(SIE=0),B1 的跨核分支为"逻辑修复 + 门禁防回归",**需 M3-2 跨核
  IPC 落地后实测**。修复不改变现有单核行为(阻塞目标路径与原实现等价)。
- **P1(SCHED 锁 3 次获取)延后 M4**(D28):纯性能微项,无正确性影响。
- **D25(SCHED 全局锁拆分)/ D1(中断快速路径)/ D26(slab 水位扫描)**:M3
  评估后延后,理由见 DEFERRED.md。
- **D27(带 proc 内核线程的栈守卫)**:内核线程栈守护页仅在 kernel_root 生效,
  当前内核线程均 proc=None;引入带 proc 内核线程时落实跨根表传播。
- **M3 用户态服务**(uart_server → 内存服务 → ramfs → spawn 服务化)为阶段 3
  下一步工作(ROADMAP.md 阶段 3);内核侧已交付 `spawn_elf` 等原语。

## 8. 门禁与提交

- 本轮改动(18 文件,+313/−48)随提交入库,五门禁全绿后推送
  (`git config http.postBuffer 524288000`;非里程碑不打 tag)。
- 提交切分:代码修复(B1-B4 + P2 + D21 + 清除)与文档规整分提交,每提交过门禁。
