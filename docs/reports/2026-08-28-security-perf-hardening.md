# 安全/性能加固报告(2026-08-28)

## 1. 摘要

对内核全部源码进行安全与性能复查(读遍 `kernel/src/*` 与 `docs/DESIGN.md`/`DEFERRED.md`),确认基线健康(18 轮审计后无新增高危缺陷),并对 4 个经证实的薄弱点实施加固/优化:

- **S1(安全)** — `mmu::map_user_page` 现拒绝映射分配器管理区之外的物理页(内核镜像/固件/MMIO/FDT 保留区),杜绝未来调用方失误把内核内存标 `U` 位暴露给用户态。
- **S2(安全)** — `trap_handler` 的 ecall 分支校验 `sstatus.SPP=0`;若 `scause=8` 来自 S 模式(medeleg 被错误配置),拒绝并 fail-loudly,防止内核线程误入用户 syscall(尤其 `EXIT`)破坏调度器状态。
- **P2(性能)** — `sched::on_tick` 在当前线程已是最高优先级(0)时直接早退,省掉每次 tick 的最常见无抢占路径就绪队列扫描(与原始逻辑严格等价:`0..0` 的 `any` 恒为 false)。
- **P4(性能)** — `mmu::unmap_4k` 用单地址 `sfence.vma`(rs1=vaddr)取代全 TLB 冲刷,守卫页解映射等路径不再清空整条 TLB。

## 2. 发现明细

### 2.1 S1 — `map_user_page` 不校验物理地址范围(加固)
- **级别**:MED(当前无触发路径,属面向 M2 进程 API 的加固)
- **位置**:`kernel/src/mmu.rs:map_user_page`
- **触发条件**:调用方传入任意 paddr(如内核数据页、MMIO、固件区),函数无条件在叶子置 `PTE_U` 映射。
- **影响**:一次调用方失误即可让用户态读写内核数据/固件/外设 —— 内核完全失守。当前唯一调用方(boot 用户测试)传的是 buddy 分配页,未触发,但该 API 是 M2 进程的关键路径,需在契约层封死。

### 2.2 S2 — 系统调用未校验中断来源(加固)
- **级别**:LOW(防御纵深)
- **位置**:`kernel/src/arch/riscv64.rs:trap_handler` ecall 分支
- **触发条件**:`scause=8`(ecall)理论上只从 U 模式触发(S 模式 ecall 由 medeleg 保留给 M)。但若 medeleg 配置被改动/错误,scause=8 可来自 S 模式(SPP=1)。
- **影响**:内核线程误入用户 syscall 分发,`SYSCALL_EXIT` 会直接进入 `exit_from_trap` 把**当前内核线程**标记退出并切换 —— 调度器状态破坏、行为不可预期。

### 2.3 P2 — `on_tick` 每次 tick 都扫描就绪队列(性能)
- **级别**:微优化
- **位置**:`kernel/src/sched.rs:on_tick`
- **现状**:时间片未到期时,`(0..cur_prio).any(...)` 每次 tick 都扫描更高优先级队列。最常见的运行场景是当前线程已是最高优先级(0),此时扫描范围必然为空,`higher` 恒 false。

### 2.4 P4 — `unmap_4k` 用全 TLB 冲刷(性能)
- **级别**:微优化
- **位置**:`kernel/src/mmu.rs:unmap_4k`
- **现状**:每次 unmap 都执行 `sfence.vma zero, zero`(清空整条 TLB)。unmap 只影响一个虚拟地址的转换,单地址冲刷即足够。

## 3. 修复明细

### 3.1 S1 修复
- **文件**:`kernel/src/mem.rs` + `kernel/src/mmu.rs`
- `mem.rs` 新增 `pub fn page_in_range(paddr) -> bool`:判断 paddr 是否落在分配器管理区 `[base, base + real_count*PAGE_SIZE)`(锁内单次读取,不加额外锁竞争面)。
- `mmu.rs:map_user_page` 在页对齐检查后追加 `if !crate::mem::page_in_range(paddr) { return Err(()) }`。
- 说明:分配器区内的堆/页表页与用户页同区,属 T2 页所有权问题,此处先封住**非分配器**区域(内核镜像、固件、MMIO、FDT 保留刻蚀区全部低于 base 或在保留区内,现被拒绝)。

### 3.2 S2 修复
- **文件**:`kernel/src/arch/riscv64.rs:trap_handler`
- ecall 分支开头读取帧 `sstatus` 的 SPP 位(bit 8):非零(来自 S 模式)→ `error!` + `dump_trap_frame` + `halt()`(fail-loudly,与其它不可恢复同步异常路径一致)。
- `syscall::handle` 的调用前提("确为 U 模式")由调用方在分发层强制保证。

### 3.3 P2 修复
- **文件**:`kernel/src/sched.rs:on_tick`
- 在计算 `higher` 之前插入 `if cur_prio == 0 { return frame; }`。正确性:`0..0` 范围为空,`.any` 恒 false,与原始 `if !higher { return frame; }` 分支行为完全一致;同时跳过对就绪队列的两次 `iter().any` 扫描。

### 3.4 P4 修复
- **文件**:`kernel/src/mmu.rs:unmap_4k`
- `sfence.vma zero, zero` → `sfence.vma {vaddr}, zero`(rs1=目标虚拟地址,rs2=x0,只使该地址的转换失效)。init 期守卫页解映射在分页启用前执行(Bare 模式下 sfence 为 no-op),行为不变。

## 4. 验证结果

三门禁全绿(`make clippy` / `make fmt` / `make test`):

```
cargo clippy --release -- -D warnings
    Finished `release` profile [optimized] target(s) in 0.27s   ← 零警告

cargo fmt --check                                            ← 通过

cargo build --release
    Finished `release` profile [optimized] target(s) in 0.39s
TEST PASS                                                      ← 含 M2 T1 用户态线程、全部自检
```

QEMU 实测(修复后启动日志):

```
[000000] [INFO ] M0: boot ok - arch: riscv64, machine: qemu-virt, hartid=0, fdt=0x87e00000
[000040] [INFO ] M1: scheduler selftest ok (cooperative + preemptive)
[000041] [INFO ] M2 T1: user-mode thread ecall ok (user tick=41)
[000041] [INFO ] bench: context switch ≈ 197 ns/op (yield path)
[000100] [INFO ] uptime: 100 ticks (1000 ms)
```

- S1 回归:boot 用户测试的 `map_user_page` 传的是 buddy 分配页(在分配器区内),`page_in_range` 放行,M2 T1 正常。
- S2 回归:S 模式 ecall 在本内核中不存在(scause=9 由 medeleg 保留给 M,不回本向量),新增分支不触发,正常路径零开销(一次读 + 一次位测试)。
- 性能:context switch ≈ 197 ns/op,与修复前基线(196 ns/op)一致,无回归。(slab bench 数值 35→20 ns 属 QEMU 计时噪声,不可作为结论。)

## 5. 遗留风险

- **堆/页表页与用户页同属分配器区**:`page_in_range` 只封住非分配器区域;分配器区内的页"是内核的堆还是用户的页"依赖调用方正确使用 `alloc_pages_zeroed`。完整的页所有权追踪属 T2 每进程地址空间工作。
- **slab 去重无双重释放检测**(buddy 路径有,slab 路径无):受 `GlobalAlloc` 契约约束,需内核自身 bug 才会破坏契约,当前不做 O(n) 检查。
- **内核线程栈无守护页**(D20):线程栈(16KB,堆分配)溢出会静默写坏相邻堆,建议后续专项实现。
- **D1 定时器快速路径**:每次 tick 仍全量保存/恢复 36 字帧,是最大的性能收益点,但改动大、风险高,建议单独里程碑。
- **D22 唤醒即抢占**:唤醒高优线程后需等当前时间片结束(~10ms)才被抢占,对 M2 IPC 延迟影响明显,建议 M2 调度器阶段处理。
