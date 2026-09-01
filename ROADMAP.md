# Ignium 炬元微内核 — 路线图

串行推进,每步验收通过才进下一步(`make test` 全绿 + 里程碑 tag + Release)。预估每周 8~10 小时,总跨度约 12 个月。

## 兼容层级定义(决定工作量与卖点)

- **L1 syscall ABI 级**:系统调用号/ABI 对齐 OpenHarmony 轻量系统 —— 阶段 2 完成
- **L2 libc 级**:移植 musl,OpenHarmony 组件 C 代码可直接编译运行 —— 阶段 3 完成
- **L3 组件级**:跑通 lwIP 与 OpenHarmony 轻量系统样例应用 —— v0.2 里程碑

## 阶段 0:环境与仓库(第 1~2 周)✓ 完成

| 任务 | 产出/验收 |
|---|---|
| WSL2:rustup + riscv64gc-unknown-none-elf + qemu-system-riscv64 + gdb-multiarch + llvm-tools | ✅ 工具链可用 |
| 仓库 + Apache-2.0 LICENSE + README + CI(编译 + QEMU 启动冒烟) | ✅ CI 绿 |
| 调研 LiteOS-A 的 POSIX 子集/系统调用清单,确定 L1 兼容基线 | ✅ 见 docs/DESIGN.md §铁律(接口对齐 LiteOS-A,兼容代码零进内核,内核只认 IPC) |
| 最小内核:启动 + 串口输出 | ✅ M0 ✓ |

## 阶段 1:内核核心原语(第 3~8 周)✅ 全部完成(M1)

| 任务 | 产出/验收 |
|---|---|
| trap/异常处理(stvec,中断/异常区分,寄存器 dump) | ✅ 完成:陷阱栈 + 可重入 + 完整帧 + sret 恢复 |
| 定时器中断(SBI set_timer)+ 时钟计数 | ✅ 完成:10ms 节拍,uptime 每秒 +100 tick 实测 |
| 物理内存 buddy allocator | ✅ 完成:4KB 页 / order 0-12,自检(分配/释放/合并/对齐/双释放)通过 |
| Sv39 页表 + 内核自身映射 | ✅ 完成:身份映射(2MB 超页 RAM + UART 4KB),satp 切换 + sfence,自检通过 |
| 内核堆(slab)+ alloc 稳定 | ✅ 完成:8 档 slab(16B..2KB)+ buddy 页路径,`#[global_allocator]`,Vec/Box 可用,自检通过 |
| 上下文切换 arch_thread_switch | ✅ 完成:协作切换(调用者保存寄存器)+ 抢占切换(全量帧) |
| 调度器:优先级抢占 + 时间片 + idle | ✅ 完成:2 级优先级 + 轮转 + 100ms 时间片抢占 + idle,自检通过 |
| 同步原语:mutex/condvar | ✅ 完成:阻塞式互斥(2000 计数校验)+ 条件变量握手,自检通过 |

**M1 完成**,tag + Release。

## 阶段 1.5:稳定化与真机就绪(M1.5,全部在 QEMU 内完成)✅ 全部完成

承接 DEFERRED 中触发条件已到的各项。**定位:QEMU 内完成全部硬化,
产出"真机就绪"内核;真机烧录验证推迟到 M2 之后**(届时架构定型、
有用户态可演示,一次到位)。

| 任务 | 来源 | 产出/验收 |
|---|---|---|
| FDT 解析:RAM 大小 / UART 基址与时钟 / 定时器频率 / 保留区实际大小 | D5 | board.rs 由编译期常量改为引导期数据(回退默认值);**FDT 细节不渗入通用代码** | ✅ 完成:kernel/src/fdt.rs + board.rs 运行时参数化 |
| 页权限拆分:代码 RX / 数据 RW / MMIO RW(去除整体 RWX) | D2 | 越权访问(写代码段)触发页故障而非静默成功 | ✅ 完成:内核镜像按段拆分(代码 RX/只读 R/数据 RW),堆栈 RW(无 X) |
| 栈守护页:boot/trap 栈下方不可映射页(MMU 已就绪) | D4 | 栈溢出 → 页故障停机(而非静默损坏) | ✅ 完成:linker.ld 插入 4KB guard,mmu::init unmap |
| RVA23 P1:编译目标 Zba/Zbb/Zbs+Zicond + CI `-cpu max` 矩阵 + ISA 探测 | D16 | 双 CPU 基线 CI 全绿;启动输出 ISA 能力表 | ✅ 完成:make test-rva23 + CI rva23 job;cpu.rs 模块;扩展编译+`-cpu max` 冒烟通过 |
| 页表接口补全:`arch_mmu_unmap` / TLB flush 封装(接口定稳,x86 移植受益) | 前瞻(M2 前置) | 为 M2 用户态映射打底 | ✅ 完成:mmu::unmap_4k (pub) + mmu::tlb_flush,模块头文档化公开接口 |
| 压力自检:调度/互斥高频竞争(万级锁争用、多线程交错) | 审计 13 轮建议 | 10 万次以上锁操作无丢失/重复 | ✅ 完成:sched 16线程×1000=yield 压力测试;sync 8线程×2000=互斥 16000 次操作(含偶次 yield),万级压力通过 |
| **里程碑验收** | — | 以上全绿 → tag `v0.1.0-M1.5` | ✅ 完成 |

**真机 bring-up(不属 M1.5,移至 M2 之后)**:选板(如 Milk-V Duo /
VisionFive 2)→ 烧录 → 串口/调试器 bring-up → 按 board.rs 抽象换数据源。

## 阶段 2:微内核骨架(M2)★ 最难阶段 —— ✅ 完成(v0.1.0-M2)

> **当前进度:M2 收官 ✓** —— T0 ✓ / T1 ✓(含每进程地址空间与 D20)/
> T2a ✓(同步 IPC 核心 + 简化能力表 + D22 woken 抢占)/ T2b ✓(优先级继承
> PIP + IPC 压力测试)/ **T3a ✓ / T3b ✓ / T3c ✓(多核 bring-up + per-CPU
> 调度 + 共享内存大消息 + 能力 revoke/dup)** / **D12 ✓(用户态异常恢复:
> 用户进程故障 → 杀进程,系统存活)** / **IPC 延迟基准 ✓(~4 µs/往返)**。
> 下一步 **M3(用户态服务 + L2 兼容)**。
> 实现顺序见 `docs/M2-DESIGN.md` §9(T0 地基 → T1 U/S → T2 IPC → T3 完善)。
> 设计先行,已落地前置:用户页清零 D10、页表用户映射契约、
> 线程 TCB 槽复用;D22 woken 抢占已随 M2 调度器落地(T2a)。

| 任务 | 产出/验收 |
|---|---|---|
| U/S 特权级 + 每进程地址空间 | ✅ 完成:U/S 特权级(T1:用户线程 U 模式取指、ecall 往返、sys_get_ticks/sys_exit);**每进程独立地址空间(T1.5:独立 satp 根表 + 切换 + 双进程同 VA 隔离)** + **用户栈守护页(D20)** |
| 系统调用机制 + **L1 ABI 定义(对齐 LiteOS-A 风格)** | ✅ a7 传 syscall 号、a0 返回值(L1 占位:sys_get_ticks/sys_exit),sys_write/open 待实现 |
| 进程管理:创建/退出/wait | ✅ spawn_user(用户态线程创建)+ exit_from_trap(用户线程退出,正确帧切换);**进程销毁/页回收(M2 D12)** |
| ELF 加载器(RISC-V) | 独立编译程序可运行 | ✅ **M3 T1 完成**:独立 `user/` crate 编译真 ELF,内核 `elf.rs` 解析/校验/逐段映射(无任意物理地址),U 模式运行回写结果;`sys_write(fd=1)` 占位 + `sys_read` 保留。见 docs/M3-DESIGN.md §3 与 docs/SYSCALLS.md |
| **IPC 设计**:同步 IPC + 注册发送 + 阻塞/唤醒 + 优先级继承(依赖 D22 woken 抢占,见 M2-DESIGN §5.1) | A→B 消息往返正确 | ✅ **T2a ✓**(同步 IPC 核心:寄存器消息 + 阻塞配对 + 简化能力表 + D22 woken 抢占,引导期往返测试通过);**T2b ✓**(优先级继承 PIP:按进程捐赠表 + 有效优先级,on_tick 用有效优先级判抢占;IPC 压力测试:内核线程 send/recv 环 1000 次,无丢失无损坏) |
| IPC 性能:寄存器小消息 + 共享内存大消息 | 延迟可测并记录 | ✅ 寄存器小消息(T2a)+ **共享内存大消息(T3c)**;延迟基准 ~4 µs/往返(见 benchmarks.md 与报告) |
| 能力模型简化版:未授权 IPC 被拒 | 拒绝测试用例 | ✅ T2a:未授权 cap → `-EACCES` 拒绝测试通过;T3c:cap dup/revoke + Shm 能力 |
| **用户态异常恢复**:用户进程故障 → 杀进程而非整机停机(D12,需 per-hart 应急栈) | 用户进程触发 page fault/非法指令,系统存活、其余进程不受影响 | ✅ **完成**:trap 按 SPP 分派,用户态故障(U 模式)杀进程、系统存活;内核态故障仍停机。杀进程 = 清 IPC 挂起 + 标记线程退出 + 撤捐赠(双向)+ 切内核根表 + 销毁地址空间页。见报告 `2026-08-29-m2-d12-recovery-perf.md` |
| 多核前置:per-hart 陷阱栈(D7)、控制台锁(D9)、副核唤醒(D8)、多核调度器(D19) | 4 核 QEMU 上所有 hart 进入 idle,调度器可分配线程到各核 | ✅ **完成**(T3a:副核唤醒 D8 + per-hart 陷阱栈 D7 + 控制台锁 D9;T3b:per-CPU 调度 D19,线程亲和) |

**M2 完成**(分水岭,务必在此处停下验收)。

## 阶段 3:用户态服务 + L2 兼容(第 5~8 个月)

> **M3 入口(2026-09-01)✓** —— 设计先行(`docs/M3-DESIGN.md` + `docs/SYSCALLS.md`)→
> **T1 ELF 加载器 ✓** / **T2 跨核 IPI 停核 + Running 线程回收 + 跨核 TLB
> shootdown ✓** / **T3 内核线程栈守护页 ✓**。M2 两条已知限制(守护页、跨核
> shootdown)已消项(见 docs/DESIGN.md);SCHED 锁拆分 / D1 快速路径 / slab
> 水位扫描评估后延后(见 docs/DEFERRED.md)。
> **M3-2(2026-09-01)✓** —— uart_server 服务化落地(设计见 docs/M3-DESIGN.md §10,
> 报告见 docs/reports/2026-09-01-m3-2-uart-server.md):设备页授予(`map_device` 号 12)+
> 内核服务注册表(`service_register/connect` 号 10/11)+ `sys_write/read` 移除(打印/读取
> 走 IPC)+ 跨核 IPC IPI 实测(T1/T2 banner)。
> **M3-3(2026-09-01)✓** —— 内存服务落地(设计见 docs/M3-DESIGN.md §11,报告见
> docs/reports/2026-09-01-m3-3-memory-service.md):`Cap::Page` 能力 + 页注册表 +
> `mem_grant/mem_map`(号 13/14)+ mem_server 用户态服务(**纯服务授权**:内核不暴露
> 分配 syscall)+ T1/T2 测试。
> **M3-4(2026-09-01)✓** —— ramfs 文件系统服务落地(设计见 docs/M3-DESIGN.md §12,
> 报告见 docs/reports/2026-09-01-m3-4-ramfs.md):**一切皆能力** —— 内核**零新
> syscall/零新 Cap**(保持 Proc/Shm/Page);数据面 = 客户端自建 SHM 窗(Cap::Shm),
> 存储面 = mem_server 服务链(文件页经 IPC 申请 `Cap::Page`);fd 绑定连接,无全局
> 命名空间;open/read/write/close/unlink 全链 + 跨核往返(T1/T2 banner)。
> **下一步:virtio-blk 驱动服务 + 持久文件系统**(内存服务 → ramfs → 持久化)。

| 任务 | 产出/验收 |
|---|---|
| ~~uart_server 进程独占 UART,打印走 IPC~~ | **✓(M3-2)**:uart_server 独占 UART,内核不再直碰(设备页 U 映射 + IPC WRITE/READ);读取经用户态库 `uart_read` |
| ~~内存服务:cap 发页 + IPC 申请/释放~~ | **✓(M3-3)**:用户进程可经 mem_server 服务申请/映射/归还物理页(`Cap::Page`,mem_grant/mem_map 号 13/14;纯服务授权) |
| ~~ramfs 文件系统服务(open/read/write/close)~~ | **✓(M3-4)**:纯用户态 ramfs_server(open/read/write/close/unlink),客户端经 SHM 窗数据面 + mem_server 服务链存储页;内核零新 syscall/零新 Cap,fd 绑定连接 |
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

## 兼容性与发布基线

- **RVA23**:当前不符(工具链 RV64GC 子集);分阶段支持计划见
  `docs/RVA23.md`(P1 编译目标扩展 + CI `-cpu max` 基线 → M1.5;
  P2 Zicboz/Svpbmt/Zacas/Sstc → M2;P3 完整性 → M2+)。

## 发布策略

- GitHub + Gitee 双平台(Gitee 面向鸿蒙社区)
- 定位文档:"与 LiteOS-A 对比"(接口兼容、代码自研)
- 每里程碑 Release + 进度博客
- v0.1:阶段 3 完成(约 8 个月),主线 = 微内核 + L2

## 红线

1. 接口对齐 LiteOS-A,**代码不抄 LiteOS-A**(同为 Apache-2.0,但坚持自研)
2. 兼容代码**永不进内核**;内核只认 IPC 一种原语
