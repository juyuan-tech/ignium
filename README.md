# Ignium 炬元微内核

从零开发、开源的 **Rust 微内核**,面向 **RISC-V 优先**、兼容 **OpenHarmony 生态**。

- 项目:商丘炬元科技有限公司
- 许可证:Apache-2.0
- 开发环境:WSL2 (Ubuntu 22.04) + QEMU,无需物理机
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
rustup target add riscv64gc-unknown-none-elf
rustup component add llvm-tools

make qemu    # 编译并启动到 QEMU
make test    # CI 同样的启动冒烟测试
make gdb     # QEMU + GDB (gdb-multiarch, 端口 1234)
```

预期输出:

```
[000000] [INFO ] Ignium 炬元微内核 v0.1.0 booting
[000000] [INFO ] M0: boot ok - arch: riscv64, machine: qemu-virt
[000000] [WARN ] timer not yet enabled; tick stays at 0 until M1
```

## 日志系统

分级日志(`[tick] [级别] 消息`,tick 由定时器递增,M1 后启用):

- `error!` / `warn!` / `info!`(默认输出)/ `debug!` / `trace!`(默认隐藏,`logger::set_level` 可调)
- panic 时自动输出:位置、消息、CPU 状态 dump(寄存器 + 关键 CSR),并停机关中断

## 仓库结构

```
├── src/
│   ├── main.rs          # 入口 + 启动日志
│   ├── entry.S          # _start,清零 bss,跳转 kernel_main
│   ├── logger.rs        # 分级日志系统(error/warn/info/debug/trace + tick)
│   ├── panic.rs         # panic 处理:位置/消息/CPU 状态 dump
│   ├── uart.rs          # NS16550 串口驱动(初始化 + println! + 日志输出)
│   └── arch/            # 架构隔离层(riscv64 / 未来 x86_64)
├── linker.ld            # 链接脚本(QEMU virt 0x80200000)
├── Makefile             # build / qemu / gdb / test / clippy / fmt
├── docs/DESIGN.md       # 架构设计原则
└── ROADMAP.md           # 12 个月串行路线
```

## 路线

见 [ROADMAP.md](ROADMAP.md)。当前进度:**M0(可启动 + 串口输出)**,下一步为 trap 处理。

## 里程碑

| 里程碑 | 内容 |
|---|---|
| M0 ✓ | QEMU 启动 + UART 打印 |
| M1 | trap/定时器/内存管理/调度 |
| M2 | 用户进程 + IPC + 能力 |
| M3 | 用户态服务 + shell |
| M4 | 健壮性/测试 + OpenHarmony 组件移植 |
| M5 | x86_64 移植 |
