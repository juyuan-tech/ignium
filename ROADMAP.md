# Ignium 炬元微内核 — 路线图

串行推进,每步验收通过才进下一步(`make test` 全绿 + 里程碑 tag + Release)。预估每周 8~10 小时,总跨度约 12 个月。

## 兼容层级定义(决定工作量与卖点)

- **L1 syscall ABI 级**:系统调用号/ABI 对齐 OpenHarmony 轻量系统 —— 阶段 2 完成
- **L2 libc 级**:移植 musl,OpenHarmony 组件 C 代码可直接编译运行 —— 阶段 3 完成
- **L3 组件级**:跑通 lwIP 与 OpenHarmony 轻量系统样例应用 —— v0.2 里程碑

## 阶段 0:环境与仓库(第 1~2 周)✓ 进行中

| 任务 | 产出/验收 |
|---|---|
| WSL2:rustup + riscv64gc-unknown-none-elf + qemu-system-riscv64 + gdb-multiarch + llvm-tools | 工具链可用 |
| 仓库 + Apache-2.0 LICENSE + README + CI(编译 + QEMU 启动冒烟) | CI 绿 |
| 调研 LiteOS-A 的 POSIX 子集/系统调用清单,确定 L1 兼容基线 | 文档 docs/compat-baseline.md |
| 最小内核:启动 + 串口输出 | M0 ✓ |

## 阶段 1:内核核心原语(第 3~8 周)

| 任务 | 产出/验收 |
|---|---|
| trap/异常处理(stvec,中断/异常区分,寄存器 dump) | ✅ 完成:陷阱栈 + 可重入 + 完整帧 + sret 恢复 |
| 定时器中断(SBI set_timer)+ 时钟计数 | ✅ 完成:10ms 节拍,uptime 每秒 +100 tick 实测 |
| 物理内存 buddy allocator | ✅ 完成:4KB 页 / order 0-12,自检(分配/释放/合并/对齐/双释放)通过 |
| Sv39 页表 + 内核自身映射 | arch_mmu_map 接口 |
| 内核堆(slab)+ alloc 稳定 | 内核内 Vec 可用 |
| 上下文切换 arch_thread_switch | 多线程交替打印无错 |
| 调度器:优先级抢占 + 时间片 + idle | 并发调度稳定 |
| 同步原语:mutex/condvar | 并发计数最终值正确 |

**M1 完成**,tag + Release。

## 阶段 2:微内核骨架(第 9~16 周)★ 最难阶段

| 任务 | 产出/验收 |
|---|---|
| U/S 特权级 + 每进程地址空间 | 用户代码执行 ecall |
| 系统调用机制 + **L1 ABI 定义(对齐 LiteOS-A 风格)** | sys_read/write/open 占位 |
| 进程管理:创建/退出/wait | spawn+exit+wait 全链路 |
| ELF 加载器(RISC-V) | 独立编译程序可运行 |
| **IPC 设计**:同步 IPC + 注册发送 + 阻塞/唤醒 + 优先级继承 | A→B 消息往返正确 |
| IPC 性能:寄存器小消息 + 共享内存大消息 | 延迟可测并记录 |
| 能力模型简化版:未授权 IPC 被拒 | 拒绝测试用例 |

**M2 完成**(分水岭,务必在此处停下验收)。

## 阶段 3:用户态服务 + L2 兼容(第 5~8 个月)

| 任务 | 产出/验收 |
|---|---|
| uart_server 进程独占 UART,打印走 IPC | 内核不再直碰 UART |
| 内存服务:cap 发页 + IPC 申请/释放 | 用户进程可申请页 |
| ramfs 文件系统服务(open/read/write/close) | IPC 客户端可读写删文件 |
| virtio-blk 驱动服务 + 持久文件系统 | 重启数据仍在 |
| spawn 服务化 + init 进程 + shell | shell 跑通 echo/cat 重定向 |
| **musl 移植 + busybox 跑通(L2)** | busybox 常用命令可用 |
| 服务崩溃恢复:杀 FS 服务,系统存活可重启 | 故障注入测试通过 |

**M3 完成** —— 此刻可发布 v0.1。

## 阶段 4:健壮性 + L3 兼容(第 9~10 个月)

| 任务 | 产出/验收 |
|---|---|
| 集成/压力测试(万次 IPC、万次 fork)+ CI 全跑 | 全绿 |
| 崩溃注入测试全覆盖 | 全部通过 |
| **L3:lwIP 移植 + OpenHarmony 轻量系统样例应用跑通** | 样例可用 |
| 性能基线:IPC 延迟/上下文切换/内存分配 benchmark | 文档记录 |
| 文档:DESIGN/SYSCALLS/CONTRIBUTING | 文档与代码同步 |

## 阶段 5:x86_64 移植(第 11~12 个月)

| 任务 | 产出/验收 |
|---|---|
| arch 抽象审计(全部 #[cfg] 隔离) | x86_64 空实现可编译 |
| x86 启动(long mode + GDT/IDT)+ APIC + syscall/sysret | Hello |
| x86 四级页表 arch_mmu 移植 | 用户进程跑通 |
| 复用调度/IPC(架构无关) | 同一套 shell 双架构运行 |
| CI 双架构矩阵 | 双绿 |

## 发布策略

- GitHub + Gitee 双平台(Gitee 面向鸿蒙社区)
- 定位文档:"与 LiteOS-A 对比"(接口兼容、代码自研)
- 每里程碑 Release + 进度博客
- v0.1:阶段 3 完成(约 8 个月),主线 = 微内核 + L2

## 红线

1. 接口对齐 LiteOS-A,**代码不抄 LiteOS-A**(同为 Apache-2.0,但坚持自研)
2. 兼容代码**永不进内核**;内核只认 IPC 一种原语
