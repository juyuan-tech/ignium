# M2 设计:微内核骨架(草案)

> 目标阶段:**M2**(用户进程 + IPC + 能力 + 多核前置)。本文为 M2 设计
> 基线,先文档后代码(遵循 DESIGN.md「先读透 seL4/rCore,前 3 个月
> 重文档轻代码」)。**M2 已收官(v0.1.0-M2)**:正文标注 ✅ 的项均落地;
> ⏳ 项(ELF 加载器、wait)按决策延至 M3。

## 1. 目标与验收(对齐 ROADMAP M2)

| 任务 | 验收 |
|---|---|
| U/S 特权级 + 每进程地址空间 | 用户代码执行 ecall |
| 系统调用机制 + **L1 ABI 定义(对齐 LiteOS-A 风格)** | sys_read/write/open 占位 |
| 进程管理:创建/退出/wait | spawn+exit+wait 全链路 | ✅ 创建/退出/销毁(D12:用户线程退出后进程销毁 + 地址空间页回收);wait 属 M3(spawn 服务化) |
| ELF 加载器(RISC-V) | 独立编译程序可运行 | ⏳ 延至 M3(M2 收官决策,见 DEFERRED) |
| **IPC**:同步 + 注册发送 + 阻塞/唤醒 + 优先级继承 | A→B 消息往返正确 | ✅ T2a(核心)+ T2b(PIP) |
| IPC 性能:寄存器小消息 + 共享内存大消息 | 延迟可测并记录 | ✅ 延迟基准 ~4 µs/往返;共享内存大消息 T3c |
| 能力模型简化版:未授权 IPC 被拒 | 拒绝测试用例 | ✅ T2a 未授权拒绝;T3c Cap 枚举 + dup/revoke |
| 多核前置:D7/D8/D9/D19 | 4 核 QEMU 所有 hart idle,调度分配线程到各核 | ✅ T3a(唤醒/陷阱栈/控制台锁)+ T3b(per-CPU 调度) |

## 2. 线程模型演进

- M1.5 已有:内核线程(scheduler)+ 双协议恢复(ctx_valid/frame_valid)。
- M2 加:**进程** = 一个或多个线程 + 独立地址空间 + 能力表。
- 线程从"内核线程"泛化为"可驻留内核或用户态";需要:
  - **用户态上下文**:陷阱帧新增 U/S 位(SPP=0)。
  - **每线程地址空间指针**(spaces 表索引)。
  - **系统调用返回**:ecall → trap → 分发 → sret 回用户态。

## 3. U/S 特权级与每进程地址空间

### 3.1 过渡机制(基于现有 trap 框架)

- 用户态 `ecall` → S 陷阱 → `trap_handler` 进入内核(SPP=0 表示源自 U)。
- 内核保存**用户帧**(含 U 模式 sstatus/sepc/全部 GPR)。
- 分发 syscall:
  - 无需切换地址空间(当前进程 syscall)→ 直接处理 → sret 回用户。
  - 需切换进程 → 换 satp(每进程根目录表)+ `sfence.vma + tlb_flush`。
- 用户进程第一条指令帧:仿照 spawn 双协议,
  初始帧 SPP=0(U 模式),sepc=ELF entry,sp=用户栈顶。

### 3.2 每进程地址空间

- 为每进程建**独立 Sv39 根表**(从 buddy 分配,`mmu::init` 同款)。
- **内核高半区共享**:所有进程的 satp 均把内核区映射为 S 权限
  (U 位=0)——内核驻留区在每进程页表中一致 + 只映射所需。
- 用户区:**U 位=1、SV=相同页表项按用户只可读/写/执行**,禁止
  用户读他人地址空间(页级隔离)。
- 切换:见 3.1;`sfence.vma` 必须执行(现有 `unmap_4k` 已如此)。

### 3.3 页表 API 契约(待固化,D15 方向)

- `map_4k`/`map_region_4k` **公开化**,并在 M2 落实"**拒绝覆盖**"
  语义:M2 用户映射遇已有效 PTE 返回错误而非静默覆盖。
- 现有 `mmu.rs` 内部 init 用同款;新增 `map_user_*` 便捷封装。

### 3.4 用户页清零(D10)

- 交接用户前必须整页清零(防信息泄漏,M4 已登记 D10)。
- 实现:M1.5 现做 `mem::alloc_pages_zeroed(order)`,
  内部 `zero_page` 后返回;页交换给用户前显式调用。

## 4. 系统调用 ABI(L1,对齐 LiteOS-A 风格)

### 4.1 寄存器约定(暂定,L1 IPC ABI 已落地 T2a)

```
a7  = syscall 号(与 LiteOS-A 对齐表,先占 sys_read=.../write/open)
a0-a5 = 参数
返回值: a0 = 结果, a1 = 附加
错误: 负数(类似 -errno)
```

**已落地的 L1 IPC ABI(T2a)**:

| syscall | 号 | 入参 | 返回 |
|---|---|---|---|
| `sys_exit` | 1 | — | 不返回(线程退出) |
| `sys_get_ticks` | 2 | — | a0 = 当前 tick |
| `ipc_send` | 3 | a0 = 目标进程 cap 槽;a1-a5 = 消息 5 字 | 成功 a0=0;配对前阻塞 |
| `ipc_recv` | 4 | a0 = 源进程 cap 槽 | 成功 a0=0、a1-a5 = 消息;配对前阻塞 |

- 错误以负 errno 返回(不阻塞):`-EINVAL`(槽越界)、`-EACCES`(未授权/空槽)。
- 无配对时发送/接收方阻塞;配对后经 sched 唤醒,`sepc+4` 由配对方前移。
- 帧 GPR 索引单一来源:`arch::gpr`(与 riscv64.S 保存顺序一致)。

### 4.2 内核态服务分层

- 内核直接实现:内存(cap 发页)、IPC、进程生命周期。
- 间接实现:文件/网络/uarts 由**用户态服务**(通过 IPC 提供),
  内核为纯微内核(零兼容代码,铁律)。

## 5. IPC 设计

### 5.1 同步 IPC + 寄存器消息

- `ipc_send(dst_cap, msg)` / `ipc_recv(src_cap, buf)`:
  发送方阻塞直至接收方就绪(或反之),二者交换后唤醒。
- **优先级继承**:等待 IPC 的发送发给持锁/持 IPC 接收的高优线程时,
  接收方临时继承发送方优先级(防反转)。→ 需 **D22**(woken 线程
  抢占)落地:on_tick 需能切到 ctx-valid 的高优线程。
- 基于现有 block/wake:`sched::block_current` / `wake`,但唤醒后
  **立即抢占**(M2 调度器增强)。

**T2a 已落地**:同步 IPC 核心 —— 寄存器消息(5 字,a1-a5)+ 阻塞配对
(`PendingSend`/`PendingRecv` FIFO 队列,先到先配)+ 简化能力表授权
(未授权 cap → `-errno` 不阻塞)+ **D22 woken 抢占**(ctx 展开为陷阱帧,
on_tick 候选谓词放宽)。实现见 `ipc.rs`(锁序 TABLE → IPC → SCHED,见模块头)。

**T2b 已落地**:优先级继承(PIP)+ IPC 压力测试。PIP 按**进程**捐赠
(发送方把期望接收方进程抬到自身有效优先级,接收方对称):调度器持
`Donation` 表,`enqueue` 用有效优先级选队,`on_tick` 用有效优先级判
抢占(被抬升者不被中间优先级打断);配对完成撤销。D22 提供抢占基础。
压力测试:内核线程 send/recv 环 1000 次,消息无丢失无损坏,捐赠表
配对后清空。多级捐赠链为近似(单跳 + 自然链)。

**进程销毁/页回收已落地(D12,收官)**:`process::destroy`(Shm cap 先行
revoke → 槽原子失效 → `mmu::destroy_root` 回收进程自有页);用户态故障经
`kill_current_process` 杀进程,捐赠表**双向**清理(`revoke_donations_for_proc`
清指向被杀进程的 + `revoke_donations_of` 清被杀线程发出的),详见
`reports/2026-08-29-m2-d12-recovery-perf.md`。

### 5.2 共享内存大消息

- 大消息经 `mmap_share(src_cap, dst_cap, len)`:把同一物理页映射到
  双方地址空间(U 权限),避免数据拷贝。
- 能力表示共享页的所有权;revoke 时双方页表 unmap + tlb_flush。

### 5.3 能力模型(简化版)

- 每个进程持能力槽数组;IPC 目标必须是已授权 cap。
- 能力操作:grant(授权)/revoke(回收)/duplicate。
- 未授权 IPC → 返回错误,不 panic。

## 6. 进程生命周期

- **spawn**:加载 ELF → 建地址空间 + 首线程 → 初始帧(SPP=0)入队。
  当前为 `sched::spawn_user(pid, ...)`(测试程序以机器码注入地址空间,
  `tests.rs`);ELF 加载器延至 M3。
- **exit**:回收地址空间页(逐页 unmap/free)→ 线程栈回收(现有 reaper)
  → 进程销毁。**已落地(D12)**:用户线程退出后进程无存活线程 → 经
  `process::destroy` 回收(Shm cap revoke → 槽失效 → 根表回收)。
- **wait**:父进程 `wait(pid)` 阻塞直至子退出或信号 —— M3(spawn 服务化)。

## 7. ELF 加载器

- 解析 RISC-V ELF64:`e_entry`、program headers(LOAD 段),
  按段映射(代码 RX / 数据 RW,U 权限),bss 清零。
- 校验:段内无覆盖、无任意物理地址(仅相对基址)。
- 栈:用户栈分配 + 初始 argc/argv(来自 spawn)。

## 8. 多核前置(D7/D8/D9/D19)

进入 IPC 压力前必须先交付(否则每核独立调度/IPC 跨核语义先天缺陷):

- **D7** per-hart 陷阱栈(sscratch 按 hartid 数组)。✅ T3a
- **D8** 副核唤醒(SBI IPI/HSM),per-hart init + 进入调度。✅ T3a
- **D9** 控制台输出锁。✅ T3a
- **D19** per-CPU 就绪队列 / 线程亲和性;SCHED 全局锁先保持
  (正确性优先,缩放后评)。✅ T3b(线程亲和按核归位;全局锁缩放留待 M3)

## 9. 实现顺序建议(里程碑内 T0→T3)

1. **T0 地基**:D10 用户页清零、页表公开 API、`mem::alloc_pages_zeroed`、
   D22 woken 抢占、线程 TCB/ID 复用。
2. **T1 U/S**:每进程地址空间 + ecall 入口 + syscall 分发 + spawn/exit/wait。
3. **T2 IPC**:同步 IPC + 消息寄存器 + 能力表 + 优先级继承(压力测试)。
   - **T2a ✓(已落地)**:同步 IPC 核心(寄存器消息 + 阻塞配对)+ 简化
     能力表(未授权拒绝)+ D22 woken 抢占。
   - **T2b ✓(已落地)**:优先级继承(PIP,按进程捐赠表 + 有效优先级)+
     IPC 压力测试(内核线程 send/recv 环 1000 次)。
4. **T3 完善**:**已落地** —— T3a 多核 bring-up(D7/D8/D9)、T3b per-CPU
   调度(D19)、T3c 共享内存大消息 + 能力 revoke/dup(见
   `reports/2026-08-29-m2-t3c-sharedmem-cap.md`);收官项 **D12** 用户态异常
   恢复 + 进程销毁/页回收(见 `reports/2026-08-29-m2-d12-recovery-perf.md`)。

## 10. 风险与对策

| 风险 | 对策 |
|---|---|
| IPC 性能陷阱 | 同步 IPC + 寄存器消息起步,禁止过早异步/共享内存 |
| TCB 表溢出 | T0 落实线程 ID/槽复用,大于 MAX_THREADS 才允许 |
| 优先级反转 | IPC 收发带 PIP(继承),D22 提供抢占基础 |
| 用户帧越界 | trap_handler 帧边界校验 + U 模式 sepc/stval 白名单 |
| 多核引入竞态 | 每个 per-hart 改造配专门审计(锁序/原子序) |

## 关联登记

- DEFERRED:D1/D7/D8/D9/D10/D12/D19/D20/D22/D23/D24(已实现项状态已同步)。
- docs/ROADMAP.md「阶段 2」(✅ 已完成,v0.1.0-M2)。