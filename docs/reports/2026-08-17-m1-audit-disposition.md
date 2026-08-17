# 报告:M1 完成后的全量外部审计处置(2026-08-17)

## 1. 摘要

M1 里程碑完成后按约定引入外部 max 审计(`docs/audit-reports/20260817-114758-deepseek-v4-pro.md`):
**4 CRITICAL / 4 HIGH / 6 MEDIUM / 2 LOW**,全部核实处置。
这是迄今发现问题最密集的一轮 —— 调度器/同步原语是并发正确性的重灾区。

## 2. 修复明细

### CRITICAL

| # | 发现 | 修复 |
|---|---|---|
| C1 | slab 满页时**同一槽分配两次**(新页 free_list 未弹出) | new_slab 后显式弹出首个槽 |
| C2 | **BOOT_LOCK 落在 .bss**,但仲裁发生在 BSS 清零前 | `#[unsafe(link_section = ".data")]` 强制入 .data |
| C3 | **抢占恢复用过期帧**:yield/block 后的线程帧失效(sepc=thread_entry),被抢占恢复会从头重跑 | **frame_valid 生命周期**:yield/block/exit/thread_entry 置 false,on_tick 捕获时置 true,抢占只选帧有效线程 |
| C4 | `8 < align <= 页` 时 block+8 未对齐 | 统一 `size+8+align-1` 过量分配 + align_up;顺带拒绝 >16MB 超量(原静默截断) |

### HIGH

| # | 发现 | 修复 |
|---|---|---|
| H1 | `Condvar::wait` **双重解锁**(drop(guard) 触发第二次 unlock,破坏互斥等待队列) | `core::mem::forget(guard)` 消费守卫 |
| H2 | `Mutex::lock` 阻塞前丢失唤醒 | `block_current` 增加"已唤醒则撤销入队并继续"协议;醒来后撤销 wake 入队(防双调度) |
| H3 | 抢占自检未真正测抢占(忙线程首片内完成) | 忙线程改 **tick 驱动**(跑满 150ms > 100ms 片,必被抢占) |
| H4 | 线程栈 sp 仅 8 对齐 | `sp & !0xF` 16 对齐 |

### MEDIUM

| # | 发现 | 修复 |
|---|---|---|
| M1 | FDT 头按**本机字节序**读(大端数据,小端机 magic 校验恒失败,大小解析从未生效) | 逐字节手工组装大端 u32 |
| M2 | slab 判别表无界检查(坏指针 → 下溢越界) | dealloc 增加分配区边界校验 |
| M3 | page_alloc 静默截断 order(>16MB 欠分配) | 明确 panic |
| M4 | trap 入口先写后验 sscratch | 维持(已文档化:sscratch 仅栈顶/帧底两种值) |
| M5 | RAM 整体 RWX | 已登记 D2(M1.5 权限拆分) |
| M6 | 未清零页泄漏元数据 | 已登记(M2 用户交接前清零) |

### 修复过程中又抓到 2 个关联缺陷

| # | 现象 | 根因 | 修复 |
|---|---|---|---|
| X1 | 唤醒线程**双调度**:醒来后仍在就绪队列 | wake 入队与恢复未联动 | block_current 恢复后 remove_from_ready |
| X2 | 无限自旋(静默挂起) | **派发时未置 Running**:唤醒线程以 Ready 进 block_current 误判"已唤醒" | pick_next 派发即置 Running;取消路径同步置 Running |

## 3. 验证结果

```
[000015] [INFO ] M1: scheduler selftest ok (cooperative + preemptive)
[000015] [INFO ] M1: sync primitives selftest ok (mutex + condvar)
[000100] [INFO ] uptime: 100 ticks (1000 ms)
```

- 门禁:dev+release / clippy / fmt / make test / make test-smp 全绿。

## 4. 遗留

- M4/M5/M6 按注册表维持(有触发条件);LOW 项已文档化。
- 调度器/同步的并发语义经此轮显著加固;仍建议在 M2 前再跑一轮
  压力自检(多线程竞争互斥的高频场景)以覆盖更多交错。

## 5. 附录

- 审计报告:`docs/audit-reports/20260817-114758-deepseek-v4-pro.md`
- 累计 13 轮外部审计。
