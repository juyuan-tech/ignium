# 性能基准(Ignium)

> 记录各里程碑的启动/运行时性能基线,供后续里程碑对比。
>
> **测量环境注意**:M1.5 基线在 WSL2、非 `black_box` 版本、硬编码 10 MHz
> timebase 下测得;**M2 起在容器 QEMU(`ignium-dev`)、运行时
> `board::timer_freq()` timebase 下测得** —— 跨环境绝对值**不可直接对照**。
> 同环境同方法的纵向对比才有效。数值随 QEMU 虚拟时钟波动(±20%),基准为
> 量级参考,非精确标定。

## 基线数据(M1.5,WSL2 环境)

| 指标 | 值 | 说明 |
|---|---|---|
| 启动到 M0 boot ok | tick 0 | 串口输出后 |
| buddy 自检 | ~0 tick | 128 MiB(114688 KiB)管理 |
| heap 自检 | ~0 tick | slab 8 档 + 页路径 |
| scheduler 自检 | ~41 tick | 协作 + 抢占跑完 |
| sync 自检 | ~43 tick | Mutex/Condvar 语义 |
| **slab 64B alloc+dealloc** | **~19–24 ns/op** | 10 万次,含堆锁;⚠️ 非 `black_box` 版本,疑含部分 malloc-elimination,不作强断言 |
| **上下文切换(yield 路径)** | **~200–284 ns/op** | 2000 次乒乓 ≈ 4000 次切换 |

## M2 数据(容器 QEMU,release fat-LTO + codegen-units=1)

### 2026-08-29(收官,单核 `make test` / SMP `make test-smp`)

| 指标 | 单核 | SMP(4 核) | 说明 |
|---|---|---|---|
| **IPC 往返(reg-msg)** | **~44188 ticks ≈ 4 µs** | **~44954 ticks ≈ 4 µs** | N=1000 次 send/recv 配对往返,含阻塞/唤醒/上下文切换语义 |
| **slab 64B alloc+dealloc** | **≈ 179 ns/op** | ≈ 191 ns/op | 10 万次,`black_box` 防 LLVM malloc-elimination |
| **上下文切换(yield 路径)** | **≈ 261 ns/op** | ≈ 284 ns/op | Phase 0(优化前)捕获 ≈ 346 ns/op → 优化后 −25% |

### 对比与说明

- **release 优化**:`lto="fat"` + `codegen-units=1`(纯编译期)+ 热路径
  `#[inline]`(current_id/proc、cap_target/errno)。上下文切换 346 → 261
  ns/op(−25%,单核);不触碰 panic/溢出策略,零运行时语义变化。
- **slab 基线卫生(V4/自审)**:LTO 后 LLVM 对无逃逸 alloc→dealloc 对整体消除,
  基准曾虚报 0 ns/op;加 `core::hint::black_box(p)` 后取诚实值。M1.5 的
  ~19–24 ns/op 为不同环境/版本,不作回归判定。
- **IPC 延迟语义**:含阻塞配对 + 唤醒 + 上下文切换的完整往返成本;QEMU
  虚拟时钟抖动 ±20%,真机 bring-up 后重新标定。

## 说明

- 计时器节拍 10 ms;task 日志 tick 即约 10 ms。
- 换算用运行时 `board::timer_freq()`,不硬编码 10 MHz(V4 审计)。
- 方法见 `kernel/src/heap.rs::bench`、`kernel/src/sched.rs::bench`、
  `kernel/src/tests.rs::boot_ipc_latency_bench`。

## 优化方向登记

- **D1**:中断快速路径(仅保存调用者保存寄存器)—— 定时器 ISR 全帧
  保存(~288 B)是主成本;**M3-1 评估后不做**(见 docs/DEFERRED.md D1):asm ABI
  重构(仅存 caller-saved)需在切换点把 s0-s11 从陷阱栈搬进 TCB(on_tick/block/
  force_kill 全改),TRAP_FRAME 索引是跨 4 文件单一事实来源;收益 <5%(SSTC 已
  移除每 tick SBI ecall)。
- **定时器**:SSTC 直写已避开每 tick SBI ecall(D17);context switch
  成本主要来自 SCHED 锁(pick_next/队列扫描)。
- **阶段 4 扩充**:剩余基线(大消息、锁吞吐、页表操作)在阶段 4 补测。