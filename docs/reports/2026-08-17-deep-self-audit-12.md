# 报告:深度自审(第 12 轮)——多核引导仲裁 bug + 文档漂移清理(2026-08-17)

## 1. 摘要

逐文件通读 + 行为验证测试(不同内存/核数/时长/子目录构建),抓到:
**1 个真实启动 bug(多核引导仲裁)** 与 7 处文档漂移/一致性瑕疵。

## 2. 发现与处置

### 2.1 真实 Bug(行为测试暴露)

| # | 级别 | 发现 | 修复 |
|---|---|---|---|
| B1 | **HIGH** | **`-smp 4` 启动失败**:OpenSBI 的 boot hart 不一定是 hart 0(实测 boot hart = 3)。原 `bnez a0, park` 把**引导 hart 也送去停车** → 全员停车、无输出 | **引导权原子仲裁**:`.data` 中 `BOOT_LOCK`(Rust static,镜像加载即就绪),entry.S 用 `amoswap.w.aq` 竞争,赢家引导、输家停车;`fence rw,rw` 保证后续 BSS 清零可见 |
| B2 | 关联 | `amoswap` 被集成汇编器拒绝(`Zaamo` 子扩展未启用) | entry.S 就地 `.option arch, +zaamo`(不依赖 rustc 配置) |
| — | 验证 | -smp 4 / -smp 2 / 单核全部正常引导(仅 1 个 hart 执行引导) | 实测 ✓ |

### 2.2 文档漂移/一致性(7 处)

| # | 文件 | 漂移 | 修复 |
|---|---|---|---|
| D1 | sbi.rs | "内核当前不校验返回值"、"ISR 内可忽略" —— 实际 boot/ISR 均已 assert | 改为"调用方必须检查,失败即 panic" |
| D2 | logger.rs | "tick 当前恒为 0"(已启用);"M0 现状…M1 使能中断前必须引入" | 更新为 M1 实际契约(ISR 零日志 / 异常路径可输出) |
| D3 | main.rs | init 顺序文档缺 sanitize_csr / mem / mmu(共 6 步,实际 9 步) | 补全 9 步 |
| D4 | entry.S | 头部启动顺序注释与实现不符 | 同步(含仲裁步骤) |
| D5 | panic.rs | "取地址"注释仍写旧语法 `&_stack_bottom as usize` | 改为 `(&raw const X).addr()` |
| D6 | riscv64.rs | `enable_timer` 的 `csrs sie` 带 `nomem`(与 irq_* 屏障语义不一致) | 移除,统一为编译器内存屏障 |
| D7 | .cargo/config.toml | 上一轮遗留的 rustflags 清理说明 | 简化注释 |

### 2.3 行为验证矩阵(本轮新增回归保障)

| 场景 | 结果 |
|---|---|
| `-m 64M` / `-m 256M` | 正常引导 + 定时器心跳 ✓ |
| `-smp 2` / `-smp 4` | 正常引导,仅 1 hart 执行引导 ✓(修复后) |
| 单核长跑 | uptime 持续 ✓ |
| `cd kernel && cargo build`(子目录) | ✓(build.rs 绝对路径) |
| `make bin` | ✓(裸二进制生成) |

## 3. 验证结果

- 门禁:dev+release / clippy / fmt / make test 全绿。
- 回归:`-smp 4` 从"完全无输出"恢复为正常引导。

## 4. 遗留风险

- 副 hart 停车由 `wfi` 承担,M2 唤醒需 IPI + 内存标志(登记 D8)。
- 多核引导仲裁假设 RAM 原子操作可用(QEMU 天然一致;真机需平台
  确认原子性,登记备注)。

## 5. 建议

将"多核引导仲裁"列入 CI 冒烟:增加 `-smp 4` 启动断言(防 boot hart
假设回归)。
