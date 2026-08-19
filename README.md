# Ignium 炬元微内核

从零开发、开源的 **Rust 微内核**,面向 **RISC-V 优先**、兼容 **OpenHarmony 生态**。

- 项目:商丘炬元科技有限公司
- 许可证:Apache-2.0
- 开发环境:WSL2 (Ubuntu 24.04) + QEMU,无需物理机
- 启动链:QEMU OpenSBI 固件 → Ignium 内核 @0x80200000(S 模式)

## 定位与差异化

| 维度 | Ignium |
|---|---|
| 内核架构 | 纯微内核(内核只含 IPC/调度/内存/能力) |
| 兼容策略 | 兼容层全部在用户态(syscall 翻译层 + musl + 服务进程),内核零兼容代码 |
| 生态目标 | OpenHarmony 轻量系统接口(POSIX 子集对齐 LiteOS-A) |
| 架构规划 | RISC-V 优先,x86_64 后期移植 |
| 语言 | Rust,unsafe 限制在最小硬件层 |

参考对比:Asterinas(framekernel + Linux 兼容)、zCore(Zircon 微内核 + Linux 兼容)、Redox(宏内核 + POSIX)——**开源侧没有"Rust 微内核 + 鸿蒙生态兼容"的现成项目,这就是 Ignium 的立足点**。

## 快速开始(WSL2)

```bash
sudo apt install -y qemu-system-misc
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

make test    # 编译(工具链/目标/组件由 rust-toolchain.toml 自动安装)+ QEMU 冒烟
make qemu    # 启动到 QEMU(交互查看)
make gdb     # QEMU + GDB (gdb-multiarch, 端口 1234)
```

预期输出:

```
[000000] [INFO ] Ignium 炬元微内核 v0.1.0 booting
[000000] [INFO ] M0: boot ok - arch: riscv64, machine: qemu-virt, hartid=0, fdt=0x87e00000
[000000] [INFO ] M1: buddy allocator selftest ok (114688 KiB managed)
[000000] [INFO ] M1: Sv39 paging ok (identity map, satp root=0x8000000000081000)
[000000] [INFO ] M1: kernel heap selftest ok (slab 16B..2KB + page path)
[000001] [INFO ] M1: scheduler selftest ok (cooperative + preemptive)
[000001] [INFO ] M1: sync primitives selftest ok (mutex + condvar)
[000100] [INFO ] uptime: 100 ticks (1000 ms)
[000200] [INFO ] uptime: 200 ticks (2000 ms)
...
```

## 日志系统

分级日志(`[tick] [级别] 消息`,tick 由定时器递增,M1 后启用):

- `error!` / `warn!` / `info!`(默认输出)/ `debug!` / `trace!`(默认隐藏,`logger::set_level` 可调)
- panic 时自动输出:位置、消息、CPU 状态 dump(寄存器 + 关键 CSR),并停机关中断

## 独立 AI 审计

防止"自己审自己"的盲区,可用外部 AI(DeepSeek)对源码做独立安全审查:

```bash
IGNIUM_AUDIT_KEY=sk-xxx python3 scripts/ai_audit.py
# 默认模型 deepseek-v4-pro,可切换:IGNIUM_AUDIT_MODEL=deepseek-v4-flash
```

- 密钥只走环境变量/一次性文件,不入库、不发给模型
- 报告保存至 `docs/audit-reports/<时间戳>-<模型>.md`(用法见 scripts/README.md)

## 仓库结构

```
├── kernel/                 # 内核 crate(唯一特权层,workspace 成员)
│   ├── src/
│   │   ├── main.rs         # 入口 + 启动顺序(依赖关系见注释)
│   │   ├── board.rs        # 板级平台参数(FDT 运行时推导,回退默认值)
│   │   ├── cpu.rs          # CPU 能力检测与 ISA 诊断(RVA23 P1,当前为诊断输出,运行时回退待 D17)
│   │   ├── fdt.rs          # FDT 最小解析器(RAM/UART/定时器频率/保留区)
│   │   ├── entry.S         # _start:SIE 清零/gp 初始化/副核停车/清 BSS/设栈/早期 trap stub
│   │   ├── logger.rs       # 分级日志(error/warn/info/debug/trace + tick)
│   │   ├── panic.rs        # panic:位置/消息/CPU dump/栈水位/双 panic 保护
│   │   ├── uart.rs         # NS16550 驱动(DLAB 陷阱注释 + MMIO fence + 有界发送)
│   │   ├── sbi.rs          # SBI 调用封装(ecall:TIME 扩展定时器)
│   │   ├── mem.rs          # buddy 物理内存分配器(order 0-12 + FDT 刻蚀 + 自检)
│   │   ├── mmu.rs          # Sv39 页表 + 内核身份映射(段级权限拆分:代码RX/数据RW/堆栈RW)
│   │   ├── heap.rs         # 内核堆(slab 16B..2KB + buddy 页路径,#[global_allocator])
│   │   ├── sched.rs        # 线程调度器(协作+抢占/时间片/优先级/idle/退出)
│   │   ├── sync.rs         # 同步原语(SpinLock/阻塞式 Mutex/Condvar)
│   │   └── arch/           # 架构隔离层(riscv64.rs + riscv64.S:陷阱向量/sret/context_switch)
│   ├── build.rs            # 链接脚本绝对路径传递(CARGO_MANIFEST_DIR)
│   ├── Cargo.toml
│   └── linker.ld           # 链接脚本(栈独立于镜像,_alloc_start 红线)
├── scripts/                # 工具(ai_audit.py 外部 AI 审计,密钥不入库)
├── docs/
│   ├── DESIGN.md           # 架构设计原则
│   ├── DEFERRED.md         # 延迟项注册表(含触发条件与状态)
│   ├── RVA23.md            # RVA23 兼容性差距与分阶段支持计划
│   ├── reports/            # 详尽报告(每次修复/更新必写)
│   └── audit-reports/      # 外部 AI 审计留档
├── .github/                # CI + Issue/PR 模板
├── AGENTS.md               # AI 协作者与团队执行规范(红线 + 报告规范)
├── CONTRIBUTING.md         # 贡献指南
└── SECURITY.md             # 漏洞报告政策
```

## 团队协作

- 加入团队前必读:`CONTRIBUTING.md`(流程)、`AGENTS.md`(红线)、`docs/DESIGN.md`(架构)
- 质量门禁:clippy 零警告 / fmt / QEMU 冒烟(CI 与本地 `make` 等价)
- 工具链锁定 1.97.1(rust-toolchain.toml 与 CI 双端同步)

## 路线

见 [ROADMAP.md](ROADMAP.md)。当前进度:**M1 ✓ / M1.5 ✓**(FDT 解析/页权限拆分/栈守护页/RVA23 P1/压力自检/页表接口),下一步 **M2(用户进程 + IPC + 能力)**。

## 里程碑

| 里程碑 | 内容 |
|---|---|
| M0 ✓ | QEMU 启动 + UART 打印 |
| M1 ✓ | trap/定时器/内存管理/分页/内核堆/调度/同步原语 |
| M1.5 ✓ | FDT 解析/页权限拆分/栈守护页/RVA23 P1/压力自检/页表接口补全 |
| M2 | 用户进程 + IPC + 能力 |
| M3 | 用户态服务 + shell |
| M4 | 健壮性/测试 + OpenHarmony 组件移植 |
| M5 | x86_64 移植 |
