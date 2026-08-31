# M2 待办清零:slab 空页懒回收 + 过期注释修正 + 全量检查 + 自审

- 日期:2026-08-31(代码改动/基准实测 2026-08-30,报告与提交 2026-08-31)
- 阶段:M2(微内核骨架)收官后、M3 之前的一轮清扫
- 提交:单次提交(见 §5),非里程碑收官,不打 tag
- 前置:M2 全部主线 tag `v0.1.0-M2`;本报告对应上阶段 D12 报告
  `2026-08-29-m2-d12-recovery-perf.md` 遗留项处置
- 触发:用户要求"在进入下一阶段之前完成当前阶段可以进行的所有的遗漏/忽略/待办,
  并全量检查、审计、更新所有文档"

## 1. 摘要

进入 M3 前,按 M2 阶段可完成的遗留项做本轮清扫。共三块:

1. **slab 页归还 buddy**(核心改动,`kernel/src/heap.rs`):兑现 heap.rs 模块头
   "归还与页回收在 M2 引入"的既有承诺。此前 slab 页**永不归还** buddy(见
   DESIGN.md 已知限制),反复 alloc/dealloc 同尺寸对象会令每档页数无限增长。
   本轮实现**懒回收**:任一档 `new_slab`(grow)前扫描全部档,把"全空且非 head"
   的 slab 页摘链、复位 `SLAB_PAGE_CLASS`、`mem::free_pages` 归还 buddy。
2. **过期注释修正**(文档卫生):`process.rs` 仍写"销毁/页回收仍留待后续里程碑"
   (D12 已实现);`mmu.rs` 仍写"应收敛为拒绝覆盖 API"(`map_user_page` 已是
   拒绝覆盖 API)。两处更新为现状。
3. **全量检查 + 自审 + 文档更新**:五门禁全绿,热路径与基线**逐指令一致**
   (objdump 比对),bench 数值噪声内一致;报告 + DESIGN.md/D12 报告同步。

**核心约束(满足)**:热路径 alloc/dealloc 与引入前逐指令一致、bench 数值不变
(用户"安全的提升性能/不加分不减分"要求下,归还不能引入热路径成本)。为此否决了
"即时回收"(dealloc 加计数器 + 归还分支,bench ~186→209,+12%),改为 grow 前
懒扫描,冷路径承载全部归还逻辑。

**验证**:五门禁(clippy 零警告 / fmt 干净 / test / test-smp / test-rva23)全绿;
三种 QEMU 配置均见 `kernel heap selftest ok`(含新增步骤 7)且无
`KERNEL PANIC|TRAP:`;bench A/B(同会话交错)基线 ~186 vs 懒回收 ~188(噪声内);
objdump 比对 `GlobalAlloc::alloc/dealloc` 与基线**逐指令一致**。详见 §4。

## 2. 发现明细

### 2.1 slab 页永不归还 buddy(既有承诺未兑现,中优先级,功能缺口)

- **位置**:`kernel/src/heap.rs` 模块头、`SlabHeader`、`new_slab`、`dealloc`
  slab 分支。
- **触发条件**:任何档反复 alloc→dealloc 同尺寸对象(或长期异尺寸增长)。
- **影响**:每档页数只增不减,呈**无界碎片泄漏**。M1/M2 规模下每档 1~2 页尚可,
  但 heap.rs 模块头与 DESIGN.md 均明确"归还与页回收在 M2 引入" —— 承诺未兑现。
- **级别**:不阻塞当前功能,但为文档化承诺 + 无界增长隐患,符合"M2 可完成"标准。

### 2.2 过期注释(文档卫生,低优先级)

- **`kernel/src/process.rs:16`**:"销毁/页回收仍留待后续里程碑" —— D12 已实现
  `process::destroy` / `mmu::destroy_root`,注释与代码现状不符,易误导后续维护。
- **`kernel/src/mmu.rs:109-110`**:"M2 用户态映射前应将此函数收敛为拒绝覆盖
  已有 PTE 的 API" —— `map_user_page` 已是该语义(见模块头),`map_4k` 保留为
  init 期/内核内部映射,注释"待办"已过期。

## 3. 修复明细

### 3.1 方案权衡:即时回收 → 懒回收

**即时回收(否决)**:`dealloc` 热路径加 `used` 计数 + 页空时归还分支。实测 bench
从 ~186 回归 ~209(+12%),违反"数值不变"约束。根因:每笔 alloc→dealloc 都多付
计数器读写与分支;fat-LTO 下归还路径还抬高压栈(见下)。

**懒回收(采用)**:空页检测与归还移出 dealloc,放进 `new_slab` 开头的冷路径扫描。
热路径 alloc/dealloc **零改动**(仅注释),bench 数值不变。

### 3.2 实现细节(`kernel/src/heap.rs`)

1. **`new_slab` 钩子**(`kernel/src/heap.rs:181`):开头 `self.sweep_empty_pages()`,
   随后照旧取页建链 —— 任何档 grow 都会先跑一次全局懒回收。
2. **`sweep_empty_pages()`**(`#[inline(never)]`):扫描 8 档;head 后逐非 head 页
   判空,空页**先摘链**(`prev.next = next`)**再复位判别表**(`SLAB_PAGE_CLASS` →
   NOT_SLAB)**后归还**(`mem::free_pages(page)`,order-0 精确归还)。链上永不残留
   已释放页;判别表复位先于归还,防 buddy 复用该页后被误判为 slab 页。
3. **`slab_page_empty(page, idx)`**:沿空闲槽链数到页容量(槽数)即判空,链尾哨兵
   `usize::MAX`;不依赖 used 计数器。`n > capacity` 为防御性早退(防链表损坏死循环)。
4. **`#[inline(never)]` 关键**:fat-LTO 若把 sweep 内联进 new_slab → slab_alloc,
   hot alloc 寄存器压力 9→16 个被调用方保存寄存器(bench +10%);标注后热路径
   与基线逐指令一致(objdump 实证,§4.3)。
5. **`SlabHeader` 保持 16B 不变**(`next` + `free_list`),槽起始 = `page+size`,
   无布局/容量变化,dealloc 界检查与基线一致。
6. **模块头注释**更新:承诺兑现,注明懒回收语义与 head 保留。

### 3.3 注释修正

- `process.rs:16`:"销毁/页回收已实现(D12 `process::destroy`/`mmu::destroy_root`,
  共享页 cap 先 revoke 防 double-free);多核 Running 线程栈/槽不回收为已知局限
  (见 DESIGN.md 已知限制)"。
- `mmu.rs:109-110`:"用户映射已收敛于 `map_user_page`(拒绝覆盖已有 PTE,见模块头);
  `map_4k` 保留为 init 期/内核内部映射使用"。

### 3.4 自检用例(`self_test()` 新增步骤 7,banner 不变)

自含、不跨用例耦合;banner 仍为 `kernel heap selftest ok`,免 6 处 grep 断言同步:

- 256B 档 churn 出 3 页(1 head + 2 非 head)全部释放 → `mid = free_page_count()`
  (3 页仍被链持有,dealloc 热路径无回收);
- 1024B 档逐步持有分配直到 free 计数变化(证明 new_slab 已执行 → 懒回收已跑),
  指针用栈数组持有,避免循环内堆分配扰动被测档;
- 断言 `free_after > mid`:归还(≥2 页)抵消 grow 新页(1)后仍有净增;
- head 保留:单槽 alloc→dealloc(等价 bench 快路径)后 free 计数**不变**
  (head 若被过早归还,下次 alloc 会重新取页 → 计数 -1)。

## 4. 验证结果

### 4.1 五门禁(2026-08-31 重跑,全部通过)

| 门禁 | 结果 |
|---|---|
| `make clippy`(release,-D warnings) | ✅ 零警告 |
| `make fmt`(cargo fmt --check) | ✅ 无 diff |
| `make test`(单核) | ✅ TEST PASS |
| `make test-smp`(4 核) | ✅ SMP TEST PASS |
| `make test-rva23`(RVA23) | ✅ RVA23 TEST PASS |

### 4.2 QEMU 日志证据(三配置均见 selftest,无 TRAP/PANIC)

单核 `make test`:

```
[000000] [INFO ] M1: kernel heap selftest ok (slab 16B..2KB + page path)
[000129] [INFO ] bench: slab 64B alloc+dealloc ≈ 194 ns/op
...
qemu-system-riscv64: terminating on signal 15 from pid 44 (timeout)
```

SMP `make test-smp`(4 核):

```
[000000] [INFO ] M1: kernel heap selftest ok (slab 16B..2KB + page path)
[000136] [INFO ] bench: slab 64B alloc+dealloc ≈ 212 ns/op
```

RVA23 `make test-rva23`:

```
[000000] [INFO ] M1: kernel heap selftest ok (slab 16B..2KB + page path)
[000130] [INFO ] bench: slab 64B alloc+dealloc ≈ 189 ns/op
```

三份 log 均 `grep -c "KERNEL PANIC|TRAP:"` = 0。新增自检步骤 7 通过即证明:空
页确实归还 buddy(free 计数回升),且 head 页未被过早归还(单槽 churn 计数不变)。
(注:上述 bench 值为本轮单次运行快照,含主机负载抖动 189~212;受控对比见 §4.3。)

### 4.3 热路径逐指令一致(objdump 实证)

- **受控 A/B(同会话交错,各 5 次)**:基线 177/180/182/193/200(均值 ~186);
  懒回收 + `#[inline(never)]` 183/186/188/191/192(均值 ~188)—— 噪声内一致。
- **机器码比对**:`llvm-objdump` 反汇编 `GlobalAlloc::alloc`/`dealloc`,
  baseline 与 懒回收 版本 `diff` **空输出**(逐指令一致)。
- **内联陷阱复现**:未标注 `#[inline(never)]` 时,fat-LTO 把 sweep 内联进
  slab_alloc,被调用方保存寄存器 9→16(压栈 +10%);标注后复刻基线 9 寄存器。
- 该 objdump 结果即"热路径零改动"的最终证明,与自检 head 保留断言互证。

### 4.4 双 profile 可编

`cargo build`(debug)与 `cargo build --release` 均成功(容器内)。

## 5. 提交

单次提交(用户已确认:提交 + 推送,非里程碑收官不打 tag),代码/注释/报告/文档
同提交:

```
refactor: M2 - slab 空页懒回收(归还 buddy)+ 过期注释修正 + 全量检查与文档同步

- heap: 全空非 head slab 页在下次任一档 grow 时懒回收摘链归还 buddy,
  head 页保留作快复用缓存;热路径 alloc/dealloc 零改动,bench 数值噪声内不变
  (即时回收曾 +12% 被否决;objdump 实证热路径逐指令一致)
- heap: self_test 新增步骤 7(空页归还 + head 保留断言),banner 不变
- process/mmu: 更新 D12 后过期注释(销毁/页回收已实现,map_user_page 已收敛)
- docs: DESIGN.md 已知限制更新;D12 报告 §5 交叉引用新报告
```

## 6. 遗留风险 / 后续

- **懒回收时延**:空页在"下次任一档 grow"前暂留于链上(有界,非无界泄漏)。
  若某档峰值后不再有任何 grow,空页不归还 —— 语义上有界、可回收,可接受。
  M3 若需更强实时性,可改为定时/水位触发的后台扫描,但会引入并发遍历锁语义,
  本轮不做。
- **多核 Running 线程栈/槽不回收**(既有):维持 D12 报告 §5 所述,依赖 M3 跨核
  IPI 停核基建。
- **D1 中断快速路径 / ELF 加载器 / RVA23 P2 / D24 多 bank**:维持延后
  (DEFERRED.md),与本轮改动无交互。
