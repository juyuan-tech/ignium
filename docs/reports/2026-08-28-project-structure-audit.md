# 2026-08-28 项目结构优化与全量自我审计

## 1. 摘要

对全项目做一次**结构盘点 + 自我审计**(首次全量),目标是:
1. 检查项目结构(模块组织、职责边界)是否合理;
2. 找出所有过期/遗漏/不一致的文档、门禁、注释;
3. 优化更详细的注释;
4. 补强门禁并防止"本地过、CI 挂"类漂移。

结论:代码质量整体很高(模块边界清晰、ABI 契约有编译期锁定、注释密度足够)。
审计发现 **1 处门禁遗漏(CI rva23 job 缺 M2 banner 断言)**、多轮文档过时、
3 处陈旧 `#[allow(dead_code)]` 标注,以及 main.rs 承载测试逻辑的结构耦合。
全部已修复,5 门禁全绿。

## 2. 发现明细

| # | 级别 | 位置 | 触发条件 | 影响 |
|---|---|---|---|---|
| F1 | **HIGH** | `.github/workflows/ci.yml` rva23 job | 三处 grep 同步纪律 | **rva23 冒烟缺 `M2: per-process address space ok` 断言**(Makefile test-rva23 与 ci.yml build job 均有,唯独 CI rva23 job 漏)。若未来该 banner 回归,CI rva23 不拦、本地 test-rva23 拦,语义不一致 |
| F2 | MED | `.github/workflows/ci.yml` rva23 job | clippy 门禁 | `cargo clippy -- -D warnings` 缺 `--release`,且跑标准 target/ —— 与 Makefile `cargo clippy --release` 及 build job 不一致 |
| F3 | MED | `Makefile` test/test-rva23、`ci.yml` build job | 门禁完备性 | 只断言 `M2: per-process address space ok`,未显式断言 `M2 T1: user-mode thread ecall ok`(依赖"addrspace 测试在 T1 之后跑"的隐式顺序) |
| F4 | MED | `docs/DESIGN.md`「已知限制」 | 文档过时 | 仍写"截至 M1.5、无用户态"—— 与已落地的 M2 T1(用户线程 U 模式)/T1.5(每进程地址空间)矛盾 |
| F5 | LOW | `ROADMAP.md` 阶段 1 表格 | 文档冗余 | 残留一行重复行 `Sv39 页表 + 内核自身映射 | arch_mmu_map 接口`(产出列不完整) |
| F6 | LOW | `AGENTS.md` 门禁/结构速览 | 文档过时 | "三条全绿"但实际 5 门禁(缺 test-smp);结构速览缺 process.rs |
| F7 | LOW | `kernel/src/mmu.rs` 三处 `#[allow(dead_code)]` | 标注过时 | `PTE_U` / `ensure_table_user` / `map_user_page` 自 M2 T1.5 已被调用,注释"未调用"错误(误导读者);`tlb_flush` 的 allow 注释未澄清契约属性 |
| F8 | LOW | `kernel/src/mmu.rs` 模块头 | 文档遗漏 | 「公开接口」未列出 M2 T1.5 新 API(`create_user_root`/`switch_root`/`is_mapped`/`kernel_root`) |
| F9 | LOW | `kernel/src/main.rs` | 结构耦合 | boot 测试三函数(约 200 行)与"入口聚焦初始化顺序"职责耦合,main.rs 378 行 |

**未发现问题的核查项**(确认无遗漏):ABI 常量双端同步(TRAP_FRAME 36/288 有编译期断言、gpr 索引与汇编保存顺序一致)、syscall 帧索引与 arch 一致、TODO/FIXME 标记(无)、旧函数残留引用(无)、.gitignore 覆盖 target-rva23、rust-toolchain 1.97.1 与 CI 同步、tag v0.1.0-M1.5 存在。

## 3. 修复明细

| # | 修复 | 方案 |
|---|---|---|
| F1 | ci.yml rva23 job 补 `grep -q "M2: per-process address space ok"` | 与 Makefile/build job 三处对齐 |
| F2 | ci.yml rva23 clippy 改 `--release` | 与 Makefile 及 build job 对齐;注释说明 |
| F3 | Makefile test/test-rva23 + ci.yml build job 各补 `grep -q "M2 T1: user-mode thread ecall ok"` | 显式断言每个里程碑 banner,不再依赖隐式顺序 |
| F4 | DESIGN.md 已知限制改「截至 M2 T1.5」,用户态改为"受限"(无 IPC/无销毁/无 D12) | 与现状一致 |
| F5 | 删除 ROADMAP 重复行 | — |
| F6 | AGENTS.md 改 5 门禁 + 补 test-smp;结构速览补 process.rs/tests.rs;加"三处 grep 同步"纪律条目 | 门禁清单与实际一致 |
| F7 | 移除 mmu.rs 三处陈旧 allow(PTE_U/ensure_table_user/map_user_page);tlb_flush 注释澄清为 arch 契约 API(保留 allow) | 标注与事实一致 |
| F8 | mmu.rs 模块头补 M2 T1.5 公开 API 清单 | — |
| F9 | boot 测试下沉 `kernel/src/tests.rs`(新增 `mod tests;`,main.rs 调用 `tests::boot_tests()`);main.rs 378→165 行,聚焦初始化顺序 | 职责解耦 |

## 4. 验证结果(全部门禁在 Docker 容器 `ignium-dev:1.97.1` 内通过)

```
$ make clippy     → cargo clippy --release -- -D warnings  零警告
$ make fmt        → 通过(cargo fmt 重排 tests.rs assert 块)
$ make test       → TEST PASS
$ make test-smp   → SMP TEST PASS(恰好 1 条 M0)
$ make test-rva23 → RVA23 TEST PASS
$ cargo build     → dev profile 编译通过
```

重构后 QEMU 启动日志(关键 banner 完好):

```
[000000] [INFO ] M0: boot ok - arch: riscv64, machine: qemu-virt, hartid=0, fdt=0x87e00000
[000000] [INFO ] M1: Sv39 paging ok (identity map, satp root=0x8000000000086000)
[000042] [INFO ] M1: sync primitives selftest ok (mutex + condvar)
[000042] [INFO ] M2 T1: user-mode thread ecall ok (user tick=42)
[000042] [INFO ] M2: per-process address space ok (2 proc @ same VA, satp switch, guard page)
[000100] [INFO ] uptime: 100 ticks (1000 ms)
... (持续到 10s 冒烟结束)
```

回归确认:用户线程 ecall、每进程地址空间隔离、guard 页结构校验在测试下沉后
行为不变;上下文切换/调度未受影响。

## 5. 遗留风险

- **`tlb_flush` 无调用方**:保留为 arch 层契约 API(`#[allow(dead_code)]`),供
  T2 页回收/批量解除映射使用 —— 非问题,但读者应知它未被热路径使用。
- **`sstc_available` 无调用方**:保留供 M2 多核探测,同类。
- **内核线程栈守护页仍未落地**:D20 仅覆盖用户栈(见 DEFERRED.md 已实现条目
  与 `docs/reports/2026-08-28-m2-t15-addrspace.md` 遗留风险)。
- **历史报告快照不追溯更新**:如 2026-08-28-security-perf-hardening.md 中
  "三门禁"是当时快照,保持时间线真实,不因本次门禁变 5 条而改写。
- **Wiki 无需再同步**:本次为结构/门禁/注释维护性改动,里程碑状态(T2 待办)
  未变;wiki 已同步至 `7c47c0b`(M2 T1.5)。
- **审计深度**:本轮为"初步全量自审"(结构与一致性),未做穷举式 bug 狩猎
  (每轮外部 AI 审计覆盖);计划进入 M2 T2(IPC)时按里程碑纪律再全量。
