# 性能基准(Ignium M1.5 基线)

> 记录 M1.5 里程碑的启动性能基线,供后续里程碑(尤其 M2 调度/IPC、
> 阶段 4 性能优化)对比。测量环境:QEMU virt,`-m 128M`,-cpu 默认
> (rv64gc),release 构建,WSL2。计时源:`time` CSR(10 MHz → 100 ns/tick)。

## 基线数据(M1.5)

| 指标 | 值 | 说明 |
|---|---|---|
| 启动到 M0 boot ok | tick 0 | 串口输出后 |
| buddy 自检 | ~0 tick | 128 MiB(114688 KiB)管理 |
| heap 自检 | ~0 tick | slab 8 档 + 页路径 |
| scheduler 自检 | ~41 tick | 协作 + 抢占跑完 |
| sync 自检 | ~43 tick | Mutex/Condvar 语义 |
| **slab 64B alloc+dealloc** | **~19–24 ns/op** | 10 万次,含堆锁 |
| **上下文切换(yield 路径)** | **~200–284 ns/op** | 2000 次乒乓 ≈ 4000 次切换 |

> 注:数值随 QEMU 虚拟时钟略有波动(±20%);基准为量级参考,非精确标定。

## 说明

- 计时器节拍 10 ms(10 MHz / 100);task 日志 tick 即约 10 ms。
- 上下文切换成本 = (get_time 差 × 100ns) / (2×BENCH_N);`get_time()`
  读 `time` CSR,10 MHz。
- heap bench = (get_time 差 × 100ns) / 100_000。
- 方法见 `kernel/src/heap.rs::bench`、`kernel/src/sched.rs::bench`。

## 优化方向登记

- **D1**:中断快速路径(仅保存调用者保存寄存器)—— 定时器 ISR 全帧
  保存(~288 B)是主成本;M2 调度器前实施。
- **定时器**:SSTC 直写已避开每 tick SBI ecall(D17);context switch
  成本主要来自 SCHED 锁(pick_next/对列扫描)。

## 阶段 4 扩充

阶段 4 将补充:IPC 延迟、大消息、锁吞吐、页表操作等基线
(见 docs/DESIGN.md「验证策略」)。