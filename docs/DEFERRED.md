# 延迟项跟踪(Deferred Items Registry)

集中登记审计/自审中**确认但不立即实施**的项目。每项含:来源、状态、
触发条件。**"已记录就不算遗忘"** —— 新发现一律先登记再讨论。

## 当前登记

| # | 项目 | 来源 | 触发条件 | 状态 |
|---|---|---|---|---|
| D1 | 中断快速路径(仅保存调用者保存寄存器) | 自审优化报告 | M2 调度器之前 | 待办 |
| D2 | 页权限拆分(代码 RX / 数据 RW,现整体 RWX) | 多次审计 | M1.5 | **已实现**(审计 18 轮):mmu.rs 内核镜像按段拆分(代码 RX / 只读数据 R / 可写数据 RW),堆/栈区域 RW(无 X) |
| D3 | 分配器加锁 | 审计 HIGH-1(12 轮):SpinLock 已包装分配器;审计 17 轮 MED-3 完成 **IRQ 安全变体**(加锁保存/恢复 SIE);剩余 = ISR 分配策略(由 D11 容量预留兜底) | 调度器里程碑 | **已实现** |
| D4 | 栈守护页(boot/trap 栈) | 审计多轮 | M1.5(MMU 已就绪) | **已实现**(审计 18 轮):linker.ld 插入 4KB guard 页,mmu::init 中 unmap,栈溢出→页故障 |
| D5 | FDT 解析(RAM/UART/时钟频率/保留区实际大小) | 审计 M4/多轮 | M1.5 | **已实现**(审计 18 轮):kernel/src/fdt.rs 最小解析器,提取 RAM/定时器频率/UART/保留区;board.rs 运行时参数化 |
| D6 | 板级常量 FDT 化(board.rs 为唯一落点) | 审计 11 轮 M4 | 随 D5 | **已实现**(审计 18 轮):board.rs 由编译期常量改为运行时函数,回退默认值 |
| D7 | per-hart 陷阱栈数组(现全局单栈) | 审计 11 轮 H1 | 多核唤醒前(M2) | 待办:陷阱栈从全局单栈改为 per-hart 数组(按 hartid/tp 索引),sscratch 指向 per-hart 栈顶 |
| D8 | 副核唤醒与多核 bring-up | 自审/ROADMAP | M2+ | 待办:引导者(仲裁赢家)在初始化完成后通过 SBI IPI 或 HSM 扩展唤醒副核;副核从 park 进入内核(初始化 per-hart 陷阱栈后加入调度) |
| D9 | 控制台输出锁(多核防交错) | uart.rs 注释 | 多核唤醒前(M2) | 待办:Wrap uart::putc in SpinLock,防止多核同时输出在串口上交错 |
| D19 | 多核调度器支持 | 审计 18 轮 | M2+ | 待办:per-CPU 空闲线程/idle 循环、per-CPU 就绪队列(或全局锁+迁移)、线程亲和性;当前 SCHED 全局锁在单核下正确,多核下可工作但不缩放 |
| D10 | 用户页交接前清零(防信息泄漏) | 审计 M4(mem.rs) | M2 用户态 | 待办 |
| D11 | ISR 内分配安全(与 D3 配套,防死锁) | 优化报告遗留风险 | 调度器里程碑 | **已实现**(审计 16 轮):就绪队列/reaper/线程 Vec 容量预留(MAX_THREADS=64),ISR 路径零分配;调度器临界区 irq_save/restore;ISR 内仍禁止主动分配(新需求走 D3 IRQ 安全锁) |
| D12 | 陷阱异常恢复路径(现为诊断后停机) | 多轮审计 | M2 用户态(需 per-hart 应急栈) | 待办 |
| D15 | mmu 接口下沉 arch 层(现顶层 mmu.rs) | DESIGN 契约 | x86_64 移植(阶段 5) | 待办 |
| D16 | RVA23 支持计划(见 docs/RVA23.md):P1 编译目标扩展+验证基线(M1.5)/ P2 Zicboz+Svpbmt+Zacas+Sstc(M2)/ P3 Svinval+Zicbom+V 上下文 | 用户提问 | P1=M1.5,P2=M2 | **P1 已实现**(审计 18 轮):Makefile test-rva23 目标 + CI rva23 job;cpu.rs 模块(ISA 诊断输出);扩展编译+`-cpu max` 冒烟通过 |
| D17 | 无 SSTC 平台检测与 SBI 定时器回退(读 FDT riscv,isa;当前无条件用 stimecmp,写读回断言可给出明确诊断) | 审计 14 轮 HIGH-2 | M1.5(FDT 解析已落地,可读取 riscv,isa) | 部分完成(FDT 解析就绪,读取 riscv,isa 与 SBI 回退待实现) |
| D18 | early_trap 最小诊断输出(真机 bring-up 期 UART 未就绪时静默停机,审计 17 轮 INFO-4;文档化为 bring-up 风险,真机适配时落实) | 审计 17 轮 INFO-4 | 真机 bring-up | 待办 |

## 已关闭(移动到这里)

| # | 项目 | 关闭原因 |
|---|---|---|
| D-旧 | boot 定时器失败 warn 改 panic | 审计 11 轮 M1 已修 |
| D-旧 | self_test 固定数组改链表 | 审计 11 轮 M2 已修 |
| D-旧 | SBI a2-a7 clobber 声明 | 审计 9 轮 M1 已修 |
| D13 | 内核堆(slab)—— 使 `Vec`/`Box` 可用 | M1 完成(slab 8 档 + 页路径 + global_allocator) |
| D14 | 上下文切换 + 调度器 + 同步原语 | M1 完成(协作+抢占调度 / Mutex / Condvar);公平性与统计留待 M2 |

## 纪律

- 新延迟项先登记后讨论;拒绝"口头已知"。
- 每轮审计核对本表:已具备触发条件的项目必须启动。
