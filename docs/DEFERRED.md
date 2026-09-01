# 延迟项跟踪(Deferred Items Registry)

集中登记审计/自审中**确认但不立即实施**的项目。每项含:来源、状态、
触发条件。**"已记录就不算遗忘"** —— 新发现一律先登记再讨论。

状态约定:`待办`(未启动)/ `已实现`(落地,注明位置)/ `已关闭`(被替代或无需再做)。

## 待办(未启动)

| # | 项目 | 来源 | 触发条件 | 状态 |
|---|---|---|---|---|
| D1 | 中断快速路径(仅保存调用者保存寄存器) | 自审优化报告 | M2 调度器之前 | **M3-1 评估后不做**(M3-DESIGN §7):asm ABI 重构(仅存 caller-saved)需在切换点把 s0-s11 从陷阱栈搬进 TCB(on_tick/block/force_kill 全改),TRAP_FRAME 索引是跨 4 文件单一事实来源;收益 <5%(SSTC 已移除每 tick SBI ecall) |
| D15 | mmu 接口下沉 arch 层(现顶层 mmu.rs) | DESIGN 契约 | x86_64 移植(阶段 5) | 待办:接口形态见 mmu.rs 模块头;x86_64 移植时下沉为 arch::mmu |
| D16 | RVA23 支持计划(见 docs/RVA23.md):P1 编译目标扩展+验证基线(M1.5)/ P2 Zicboz+Svpbmt+Zacas+Sstc(M2)/ P3 Svinval+Zicbom+V 上下文 | 用户提问 | P1=M1.5,P2=M2 | **P1 已实现**(审计 18 轮):Makefile test-rva23 目标 + CI rva23 job;cpu.rs 模块(ISA 诊断输出);扩展编译+`-cpu max` 冒烟通过。**P2 按 M2 收官决策延后 M2+**(Zicboz 页清零加速/Svpbmt 真机 MMIO/Zacas 原子,留待真机 bring-up 与后续里程碑评估),P3 仍 M2+ |
| D18 | early_trap 最小诊断输出(真机 bring-up 期 UART 未就绪时静默停机,审计 17 轮 INFO-4;文档化为 bring-up 风险,真机适配时落实) | 审计 17 轮 INFO-4 | 真机 bring-up | 待办 |
| D21 | UART 波特率分频按 FDT clock-frequency 计算 | 审计多轮 pro #6 | 真机 bring-up 前(不属 M1.5:M1.5 全部 QEMU 内) | **已实现(2026-09-01,M3 收尾)**:fdt.rs 解析 uart 节点 `clock-frequency` → BoardParams.uart_clock;board.rs 校验(1MHz..100MHz)并暴露 `uart_clock()`;uart.rs `uart_divisor()` 按 clk/(16×115200) 计算(DLL/DLM),异常回退 0x0C。见 uart.rs |
| D23 | 早期 UART 用硬编码基址(V4 MED):uart::init 在 FDT 解析前写默认 0x10000000,真机若不在该址会误写无关 MMIO | 审计 V4 MED | 真机 bring-up 前 | 待办:真机须先解析 FDT(或经 SBI 调试控制台)再驱动 UART;reinit 对 QEMU 足够 |
| D24 | FDT 多内存 bank / `#address-cells`/`#size-cells` 变体:当前只取首个 reg 对且仅 2-cell/1-cell | 自审(挑剔视角) | 真机 bring-up | 待办:M2 收官决策**延至真机 bring-up**(QEMU 单 bank 无触发条件);多 bank 需 buddy 支持不连续区间;`#cells` 变体(如 3-cell)需按 node 解析 cells 属性 |
| D25 | SCHED 全局锁拆分(per-CPU 锁) | M3-DESIGN §7 | M4 | **M3-1 评估后延后 M4**:现状正确(D19);M3-1 已叠加跨核 kill/shootdown + ELF 两个高敏改动,拆分风险不可控;收益有限(QEMU 4 核 IPC 瓶颈是 current_id 取锁,非整锁带宽);成本高(跨核 enqueue 需按 hart 序取锁防死锁;on_tick/block/exit/yield 全部重审)。可选低风险子项(per-CPU `CURRENT_TID` 原子缓存)单独评估,不并入 M3-1 |
| D26 | slab 空页水位扫描(定时/水位触发后台扫描) | M3-DESIGN §7 | M4 或真实内存压力 | **M3-1 评估后不做**:现状有界(非 head 空页下次任一档 grow 懒回收,head 保留作快复用缓存),内核堆用量几 KB 级 / RAM 128MB,收益 ≈ 0。M4 或真实内存压力出现时再评估 |
| D28 | IPC syscall 路径 SCHED 锁三次顺序获取合并(current_id → donate_on_block → block_user_from_trap) | M3 收尾审查(perf agent) | M4(SCHED 锁缩放评估时一并) | **M3-1 评估后延后**:三次获取均**顺序、不嵌套**(trap 上下文 SIE=0,无重入/死锁风险),是纯性能微项;SCHED 锁在 syscall 路径无竞争,收益 ≈ 0;合并需改 `block_user_from_trap` 签名与 donation/IPC-wake API 契约,触碰 B1 刚修改的最关键路径,风险 >> 收益。留待 M4 与 D25(SCHED 锁缩放)一并评估 |
| D29 | 服务端 client 消亡后的 recv 恢复:陈旧 `Cap::Proc(client)` 令 uart_server 的 recv 永久阻塞(无对端再配对) | M3-2 设计(§10.11) | M3-3(服务崩溃恢复里程碑) | 待办:M3-2 uart_server 面向 1 并发 client 且测试期无 client 消亡场景;`purge_process` 已可对阻塞 recv 投 -ENOENT,预留「cap_revoke + 重连协议」 |
| D30 | 多并发 client 支持:accept 单槽 + SHM_VA 单页映射限制为 1 并发 client | M3-2 设计(§10.11) | M3-3(ramfs/init-shell 多进程交互) | 待办:需多 SHM VA + 服务端槽池;M3-2 明确面向单 client |
| D31 | 设备页能力化:`Cap::Dev` + 手动 revoke + 特权授予(map_device 当前无特权校验,任何进程可 claim UART,靠引导序保证;设备页生命周期随进程不可单独 revoke) | M3-2 设计(§10.11) | M3-3(设备驱动服务化) | 待办:M3-2 靠「uart_server 先 spawn 先 claim」引导序保证;设备页非分配器,`destroy_root` 只 unmap 不 free |
| D32 | uart_server 崩溃即 UART 卡死(无看门狗/服务重启) | M3-2 设计(§10.11) | M3-3+(服务崩溃恢复) | 待办:本轮无看门狗;崩溃恢复里程碑统一设计 |
| D33 | 页池注入依赖引导编排:mem_server 页池由测试(T1/T2)在 spawn 后经内核侧 `pages::alloc` + `grant_typed_cap` 注入;正式 spawn/init 服务落地前无「引导期自动授予」通道 | M3-3 设计(§11.3) | spawn 服务化落地 | 待办:纯服务授权设计使然(无公开分配 syscall);spawn 服务落地后改为引导期授予 |
| D34 | 页无 unmap-without-free / remap:`cap_revoke`(号 6)对 Cap::Page 语义即「释放」(先 unmap 再 free_pages);页重定位需先 revoke 再重分配 | M3-3 设计(§11.5) | 需要页重定位/内存整理 | 待办:内存整理(compact)里程碑统一设计 |
| D35 | OP_FREE 归还乐观置位:memory_server 回填池位前假定客户端必归还;客户端在归还 mem_grant 前崩溃 → 池位空但服务误判可用(mem_grant 失败可优雅降级 -EACCES) | M3-3 设计(§11.7) | 服务崩溃恢复里程碑 | 待办:客户端消亡检测 + 池位重扫 |
| D36 | ramfs 单并发 client:SHM 单窗(4KB,恰两 owner)+ 单连接槽 → 服务端同时只能接待 1 客户端 | M3-4 设计(§12.1) | 多进程交互(init/shell) | 待办:多 SHM VA + 服务端槽池,承接 D30 |
| D37 | ramfs 每文件单页上限:文件数据 ≤ 4096B(一页),无页链表/多页管理 | M3-4 设计(§12.2) | 大文件/虚拟块设备 | 待办:文件页链表或多页编帧 |
| D38 | ramfs 名字内联 IPC ≤ 24B(NAME_MAX=24):长文件名无法表达 | M3-4 设计(§12.4) | 长路径名 | 待办:名字改走 SHM 窗或扩展消息帧 |
| D39 | EBADF 仅用户协议层复活(USER_ERR_EBADF=MAX-4):内核 syscall 级 MAX-4 仍保留空档不复用,编码统一待定 | M3-4 设计(§12.4) | 用户协议扩展 / 内核级 fd 编码统一 | 待办:本轮内核不复用,防未来冲突 |
| D40 | ramfs close 保槽 / unlink 按 fd:close 不释放文件槽(Open→Closed),unlink 按 fd 而非 by-name;多 fd 同名与 POSIX by-name unlink 语义延后 | M3-4 设计(§12.4) | POSIX 语义完整化 | 待办:close-然后-unlink 依赖保槽;by-name 需名字索引 |
| D41 | ramfs 无目录树/seek/权限/持久化:平面名空间 + 带 offset 顺序读写 | M3-4 设计(§12.4) | 树形目录/权限/持久 FS | 待办:后续 FS 里程碑统一设计 |
| D42 | ramfs 依赖 mem_server 存活:存储页经服务链申请,mem_server 崩溃 → ramfs 无法分配/归还 | M3-4 设计(§12.2) | 服务崩溃恢复里程碑 | 待办:承接 D32 看门狗;服务链崩溃传播统一设计 |

## 已实现(落地)

| # | 项目 | 来源 | 触发条件 | 落地位置 |
|---|---|---|---|---|
| D2 | 页权限拆分(代码 RX / 数据 RW,现整体 RWX) | 多次审计 | M1.5 | mmu.rs 内核镜像按段拆分(代码 RX / 只读数据 R / 可写数据 RW),堆/栈区域 RW(无 X) |
| D3 | 分配器加锁 | 审计 HIGH-1(12 轮) | 调度器里程碑 | SpinLock 包装分配器;审计 17 轮 MED-3 完成 **IRQ 安全变体**(加锁保存/恢复 SIE);ISR 分配由 D11 容量预留兜底 |
| D4 | 栈守护页(boot/trap 栈) | 审计多轮 | M1.5(MMU 已就绪) | linker.ld 插入 4KB guard 页,mmu::init 中 unmap,栈溢出→页故障 |
| D5 | FDT 解析(RAM/UART/时钟频率/保留区实际大小) | 审计 M4/多轮 | M1.5 | kernel/src/fdt.rs 最小解析器,提取 RAM/定时器频率/UART/保留区;board.rs 运行时参数化 |
| D6 | 板级常量 FDT 化(board.rs 为唯一落点) | 审计 11 轮 M4 | 随 D5 | board.rs 由编译期常量改为运行时函数,回退默认值 |
| D10 | 用户页交接前清零(防信息泄漏) | 审计 M4(mem.rs) | M2 用户态 | mem::alloc_pages_zeroed(整块清零);用户页交接(T1 boot 测试)已调用 |
| D11 | ISR 内分配安全(与 D3 配套,防死锁) | 优化报告遗留风险 | 调度器里程碑 | 就绪队列/reaper/线程 Vec 容量预留(MAX_THREADS=64),ISR 路径零分配;调度器临界区 irq_save/restore |
| D17 | 无 SSTC 平台检测与 SBI 定时器回退(读 FDT riscv,isa) | 审计 14 轮 HIGH-2 | M1.5(FDT 解析已落地) | riscv64.rs 增 USE_SSTC 标志 + arm_timer 双路径;enable_timer 首次用 SBI(不怕 trap),cpu::init_from_fdt 检测 isa 含 sstc 则切 stimecmp |
| D20 | 线程栈守护页(**用户态 + 内核态**) | 审计 V3 M1 | M2(每进程地址空间) | 每进程地址空间(M2 T1.5):用户栈下 4KB 守护页(分配不映射,`mmu::is_mapped` 结构性校验);**内核线程栈守护页(M3 T3)补齐**:堆栈布局改 20KB(16KB 栈体 + 4KB 守护页),分配后 unmap、归还前先 remap 恢复身份映射再 dealloc;mmu.rs 增 `split_superpage`/`unmap_kernel_4k`/`map_kernel_4k`/`remap_kernel_4k`。见 reports/2026-09-01-m3-entry.md |
| D27 | 带 proc 内核线程的栈守卫(跨根表拆分传播):内核线程栈守护页当前仅在 `kernel_root` unmap;若内核线程带进程根表运行,其内核栈守卫页须在该根表也有效,需 split_superpage 跨根表传播 | M3-DESIGN §9 风险表 | 带 proc 内核线程落地 / M4 | 待办:当前内核线程均 proc=None 运行于 kernel_root,守卫生效;引入带 proc 内核线程时须在 M3-DESIGN 风险表记录的基础上落实跨根表传播 |
| D22 | woken 高优先级线程的抢占(V4 审计 HIGH):on_tick 只认 frame_valid,被唤醒线程(ctx_valid-only)无法被定时器抢占 → 低优忙循环可长时间饿死高优 | 审计 V4 HIGH | M2(IPC 依赖 wake 驱动抢占) | sched.rs(M2 T2a):on_tick/pick_next(false) 候选谓词放宽为 `frame_valid \|\| (ctx_valid && woken)`;被唤醒 ctx 线程经 `expand_ctx_to_frame` 展开为 S 模式帧再 frame_restore(sepc=ctx.ra、SPP=1),同步消费 woken。IPC 阻塞唤醒后即可抢占忙循环;PIP(T2b)依赖此基础 |
| D7 | per-hart 陷阱栈数组(现全局单栈) | 审计 11 轮 H1 | 多核唤醒前(M2) | sched.rs/entry.S(M2 T3a):陷阱栈按 hartid 数组,sscratch 指向 per-hart 栈顶;副核唤醒后各自初始化 |
| D8 | 副核唤醒与多核 bring-up | 自审/ROADMAP | M2+ | entry.S/sched.rs(M2 T3a):引导者仲裁赢家经 SBI IPI/HSM 唤醒副核;副核从 park 进入内核,初始化 per-hart 陷阱栈后加入调度 |
| D9 | 控制台输出锁(多核防交错) | uart.rs 注释 | 多核唤醒前(M2) | uart.rs(M2 T3a):uart::putc 加 SpinLock,多核输出不再交错 |
| D12 | 陷阱异常恢复路径(现为诊断后停机) | 多轮审计 | M2 用户态(需 per-hart 应急栈) | arch/riscv64.rs + sched.rs/process.rs/mmu.rs/ipc.rs(M2 D12):用户态故障(SPP=0)经 `kill_current_process` 杀进程 —— 清 IPC 挂起 + 标记线程退出 + 撤捐赠(双向)+ 切内核根表 + `process::destroy` 回收地址空间页;内核态故障仍停机。详见 `reports/2026-08-29-m2-d12-recovery-perf.md` |
| D19 | 多核调度器支持 | 审计 18 轮 | M2+ | sched.rs(M2 T3b):per-CPU 空闲线程/idle 循环、per-CPU 就绪队列、线程亲和性(hart 亲和,enqueue/唤醒按亲和核归位);SCHED 全局锁保持(正确性优先,缩放留待 M3) |

## 已关闭(被替代/无需再做)

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
- 状态变更须同步更新(落地 → 移入「已实现」;不再需要 → 移入「已关闭」)。
