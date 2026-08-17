# AGENTS.md — 给 AI 协作者与团队成员的执行规范

本文档同时面向人类协作者与 AI 代理(如 opencode)。违反红线可能导致
代码被拒或 CI 失败。

## 环境

- 开发在 **WSL2 (Ubuntu 24.04)** 中执行:编译、QEMU 测试、审计。
- Windows 侧仅做文件编辑与 git;不要尝试在 Windows 直接构建。
- 工具链锁定 Rust **1.97.1**(rust-toolchain.toml 与 CI 同步,改版本两处一起改)。

## 构建与验证(每次改动后必跑)

```bash
make clippy    # cargo clippy --release -- -D warnings(零警告)
make fmt       # cargo fmt --check
make test      # QEMU 启动冒烟
```

三条全绿才能提交。dev 与 release 双 profile 都要能编译:
`cargo build` 与 `cargo build --release`。

## 红线(不可违反)

1. **兼容代码永不进内核**:OpenHarmony/POSIX 兼容全部在用户态
   (docs/DESIGN.md);内核只认 IPC 原语。
2. **初始化顺序** kernel_main:irq_disable → sanitize_csr → uart::init
   → init_traps → enable_timer → set_level → mem::init(fdt)/mmu::init
   →(自检)→ irq_enable。trap 窗口(无 stvec 时异常跳地址 0)不可恢复。
3. **汇编与 Rust 的 ABI 约定**:TRAP_FRAME 布局(riscv64.S 与 riscv64.rs
   必须同步)、CpuState 字段顺序(repr(C))。改一侧必须改另一侧。
4. **链接脚本符号契约**:_kernel_start/_end、_stack_bottom/_top 等
   被 entry.S / panic.rs 引用,改名必须同步修改引用处。
5. **日志无锁**:定时器/ISR 中断上下文**禁止**调用日志宏(无锁输出
   会交错);同步异常路径(trap_handler 的 dump)可调用 —— 嵌套风险
   由陷阱栈吸收。M1 之后若需 ISR 日志,先引入锁或 ISR 缓冲。
6. 工具链/CI 版本两处同步(rust-toolchain.toml + ci.yml)。
7. 新增 Rust 代码必须写注释:模块级文档 + 每个 unsafe 的 Safety 说明。

## 结构速览

- `kernel/` — 内核 crate(唯一特权层);arch 隔离层在 kernel/src/arch/
- `scripts/ai_audit.py` — 外部 AI 独立审计(密钥走环境变量,见 scripts/README.md)
- `docs/` — DESIGN.md(架构铁律)、audit-reports/(外部审计留档)
- 里程碑节奏见 ROADMAP.md;每个里程碑 tag + Release。

## 提交规范

- 提交信息:类型前缀(`feat:` `fix:` `docs:` `refactor:` `ci:`)+ 中文说明。
- 每个里程碑打 tag(`v0.1.0-M1` 等);tag 提交与代码提交同步推送。

## 详尽报告规范(每次修复/更新必写)

每次**修复 bug、审计处置、功能更新**,都必须编写详尽报告并随提交入库:

- 位置:`docs/reports/<YYYY-MM-DD>-<主题>.md`
- 必含章节:
  1. **摘要** —— 本次做了什么、为什么
  2. **发现明细** —— 每个 bug/审计项:级别、位置、触发条件、影响
  3. **修复明细** —— 每个修复:方案、关键代码位置、为何这样修
  4. **验证结果** —— 门禁/自检/实测输出(粘贴关键日志)
  5. **遗留风险** —— 未修项及理由、后续阶段计划
- 同一主题多次迭代可追加到同一文件(保留时间线)。
- 报告与代码同一次提交推送,CI 通过后报告才视为完成。
