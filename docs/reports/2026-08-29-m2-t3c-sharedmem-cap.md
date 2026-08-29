# M2 T3c 报告:共享内存(mmap_share)+ 能力 revoke/dup

- 日期:2026-08-29
- 阶段:M2(微内核骨架)子任务 T3c(共享内存大消息 + 能力 revoke/duplicate)
- 提交:`feat: M2 T3c - shared memory (mmap_share) + cap revoke/dup`
- 前置:T3a(多核 bring-up,D7/D8/D9)→ T3b(per-CPU 调度,D19)→ 本阶段按 T3
  计划拆 T3a → T3b → T3c
- M2 验收(本阶段对应部分):**一页物理内存映射进两个进程地址空间(U 权限,
  能力 = 共享页所有权);Cap 枚举化 + duplicate/revoke;revoke 撤双方映射 +
  tlb_flush**

## 1. 摘要

M2 T3 收官阶段完成**共享内存 + 能力扩展**,落地 M2-DESIGN 的"能力即所有权":

- **Cap 枚举化**(`process.rs`):能力槽从 `Option<usize>`(目标进程 pid)改为
  `Option<Cap>`,`enum Cap { Proc(usize), Shm(usize) }`。`Cap::Shm(id)` =
  共享页所有权;IPC 只接受 `Cap::Proc`,槽内是 `Cap::Shm` → `-EINVAL`(经
  `cap_errno(WrongType)` 编码)。
- **共享内存**(新模块 `shm.rs`):`mmap_share`(syscall 5 `SHM_MAP`)把一页
  清零物理页映射进调用方与对端的固定 `SHM_VA = 0x5000_0000`(与既有用户
  测试段 0x4000_0000 不重叠);**双槽改授 `Cap::Shm(id)`**(调用方槽覆盖原
  `Cap::Proc` = 能力即所有权,旧 IPC 许可消失);失败路径完整回滚
  (unmap + free + 出表,不泄漏页、不留半映射)。
- **能力 revoke/dup**(syscall 6/7):`cap_revoke` 按类型分派 —— `Cap::Shm`
  → **整页撤销**(撤双方映射 + 清双方槽 + 释放物理页 + 注册表出列 +
  `tlb_flush`);`Cap::Proc` → 仅清本进程该槽。`cap_duplicate(from, to)`
  复制槽值(共享所有权不变,槽位增引用)。
- **syscall 路径非 panic 校验**:新增 `process::pid_root -> Option<usize>`,
  `mmap_share` 的对端 pid / 双槽 / len 全部先校验,防用户非法 pid/slot
  触发 `root()` panic(fail-loudly 只留给内核自身编程错误)。
- **errno 单一来源**:`SYS_ERR_EINVAL/EACCES/ENOENT/ENOMEM` 统一放
  `syscall.rs`,ipc.rs 别名与 `process::cap_errno` 全部引用,不再分散定义。
- **测试**:`boot_shm_test`(A 建共享 → A 写 0xA5 → B 读回 0xA5 + 写 0xB5 →
  B 用户态 `syscall 6` 撤销 → 主上下文断言双 root 映射消失 / 双槽失效 /
  注册表出列)、`boot_cap_test`(Proc cap 的 dup + revoke 语义,revoke 只清
  原槽、dup 副本不受影响)。双 banner 入 Makefile test/test-smp/test-rva23
  与 ci.yml build/smp/rva23 共 6 处 grep 断言。

**验证**:五门禁(clippy/fmt/test/test-smp/test-rva23)全绿;test-smp 3 连跑
全确定;单核 / 4 核 / RVA23(-cpu max)日志全见 T3c 双 banner,无 panic。详见 §4。

**本次实测修复的关键 bug**(见 §2.1):`boot_shm_test` 的 B 端读回失败 ——
`prog_b` 的 `sw t3, 0(t0)` 机器码 `0x00c2a023` 实际编码的是 `sw a2, 0(t0)`
(rs2=a2=x12),不是 `sw t3, 0(t0)`(应为 `0x01c2a023`,rs2=t3=x28)。症状极具
迷惑性:B 的写 `sw t2, 4(t1)` 正确落页(revoke 前 pa[4]==0xB5)、B 的 SHM_VA
leaf PTE 与 A 相同均指向 pa(pa[0]==0xA5)、B 的 satp==root_b,但 B 读回
shared[0]==0 —— 一度指向 TLB 陈旧条目 / satp 切换路径。在 revoke syscall
内加"撤销前 pa 内容快照"锁定矛盾后才定位为 S 型编码 rs2 错写。

## 2. 发现明细

### 2.1 B 端共享页读回失败:S 型编码 `sw t3, 0(t0)` 错写 rs2(阻塞级缺陷)

- **级别**:阻塞级缺陷(boot_shm_test 失败,测试无法通过)。
- **位置**:`tests.rs` `boot_shm_test` 的 `prog_b`(手写 RISC-V 机器码)。
- **现象**:`shm: B must read A marker (got 0x0)`。B 读 `SHM_VA[0]` 结果存
  shared[0]==0,但:
  - 主上下文已断言 `pa[0]==0xA5`(A 经 SHM_VA 写入,物理页内容正确);
  - B 的 SHM_VA leaf PTE 与 A 相同(0x2182c8d7 → pa=0x860b2000);
  - B 的 `sw t2, 4(t1)` **正确落页**(撤销前 pa[4]==0xB5 —— 同一 4KB 页的
    写翻译正确,读却得 0);
  - B 的 satp==root_b(0x860a1,revoke syscall 内实测),`is_mapped(root_b,
    SHM_VA)==true`。
- **根因**:`prog_b[3]` 的 `sw t3, 0(t0)` 写成了 `0x00c2a023`。S 型 STORE
  布局为 `imm[11:5] | rs2 | rs1 | funct3 | imm[4:0] | opcode`;
  `0x00c2a023` 解码为 rs2=a2(x12)、imm[4:0]=0 → `sw a2, 0(t0)`。B 从不设置
  a2(进程帧初值 0),故 shared[0] 被写 0;`lw t3, 0(t1)` 本身编码正确,B
  实际读到了 0xA5,只是**存**错了寄存器。正确编码 `sw t3, 0(t0)` =
  `0x01c2a023`(rs2=t3=x28=0b11100)。逐条核对后:prog_a 3 条 sw、prog_b 其余
  6 条 sw、cap test 全部 sw 均正确,仅此一条错误。
- **影响**:读回断言失败 → T3c 无法验收;同时暴露"手写机器码"流程的编码
  脆弱性。
- **附带教训**:调试曾长期误入 TLB 陈旧条目 / satp 切换 / 页面复用假设;
  最终靠"同一页内写落页正确而读回 0"这一不可能在正常翻译下出现的矛盾,
  加上 revoke 前物理页快照,才把根因收敛到用户程序本身。

### 2.2 clippy / 编译期修正(开发期,非缺陷)

- `ipc.rs`:`IPC_ERR_EINVAL` 在 Cap 枚举化后无调用方(全部走 `cap_errno`)
  → 删除;`IPC_ERR_EACCES` 改别名 `crate::syscall::SYS_ERR_EACCES`。
- `shm.rs` 开发期编译修正:E0425(`crate::shm_revoke` 自引用 → 裸
  `shm_revoke`)、E0308(`read_volatile` 返回 u32 传 `shm_paddr(usize)` →
  `as usize`)、E0502(mut/immut 同借用 → 先读 capacity)、clippy
  `let-and-return`。
- `process.rs` `cap_revoke` doc `doc_lazy_continuation` → 列表项后补空 `///`。
- `mmu.rs` `tlb_flush` 去 `#[allow(dead_code)]`(现被 `shm_revoke` 使用)。

## 3. 修复明细

### 3.1 process.rs:Cap 枚举化 + duplicate/revoke/pid_root

- `enum Cap { Proc(usize), Shm(usize) }`;能力槽 `[Option<Cap>; MAX_CAPS]`。
- `grant_typed_cap` 作公共底层;`grant_cap`(Proc)签名不变(调用方零改动)、
  新增 `grant_shm_cap(pid, slot, shm_id)`。
- `cap_target` → `Result<Cap, CapError>`;`CapError` 增 `WrongType`(槽内是
  另一类能力 → `-EINVAL`)、`ShmNotFound`(指向的共享页已 revoke → `-ENOENT`);
  `cap_errno` 统一编码(InvalidSlot/WrongType→EINVAL、NotFound→EACCES、
  ShmNotFound→ENOENT)。
- `cap_duplicate(pid, from, to)`:复制槽值(共享所有权不变);`cap_revoke`:
  先解析槽内类型(TABLE 锁释放后)再分派 —— `Cap::Shm` → `shm::shm_revoke`
  (失败映射 `ShmNotFound`),`Cap::Proc` → `clear_cap`(仅清本槽);`clear_cap`
  幂等(空槽 revoke 无害)。
- `pid_root(pid) -> Option<usize>`:非 panic 根表读取(syscall 路径对端 pid
  校验用)。

### 3.2 shm.rs(新模块)

- `SHM_VA=0x5000_0000`、`SHM_LEN=4096`、`MAX_SHMS=16`;注册表槽式
  (索引=id 稳定)+ free 池复用;revoke 把 id 置 `usize::MAX` 失效后入池。
- `mmap_share(caller, a_slot, b_slot, len)`:
  - `len != 4096` → `-EINVAL`(单页共享,多页留待后续);
  - `cap_target(caller, a_slot)` 须 `Cap::Proc(peer)`(定对端 + 授权;槽内是
    `Cap::Shm` → `-EINVAL`);
  - `b_slot < MAX_CAPS`;`pid_root(peer)`/`pid_root(caller)`(None → `-EACCES`,
    防 `root()` panic);
  - `alloc_pages_zeroed(0)`(None → `-ENOMEM`);
  - `map_user_page` 双 root(`SHM_VA`,U|R|W);任一失败回滚 unmap + free;
  - 入表(表满 → 回滚映射 + free → `-ENOMEM`);
  - 双槽改授 `Cap::Shm(id)`;失败 → `shm_revoke(id).ok()` 完整回滚 +
    `-ENOMEM`(防御性,前置校验已保证只可能成功)。
- `shm_revoke(id)`:SHM 锁内快照槽(id 置 MAX 失效 + 入池)→ 撤双方映射 +
  清双方槽(幂等)→ `free_pages`(失败映射 `ShmNotFound` 暴露页泄漏)→
  `tlb_flush`(当前核全量兜底;跨核 shootdown 以 satp 切换全刷简化,M2)。
- `init()`:boot 期容量预留(在 boot_tests 之前,引导期非 ISR 分配)。

### 3.3 syscall.rs

- `SYSCALL_SHM_MAP=5`(a0=本槽,a1=对端槽,a2=len,返回 a0=shm_id)、
  `SYSCALL_CAP_REVOKE=6`(a0=槽)、`SYSCALL_CAP_DUP=7`(a0=源槽,a1=目标槽)。
- `SYS_ERR_EINVAL/EACCES/ENOENT/ENOMEM` 单一来源(usize 编码,L1 ABI 一致)。
- 全部分发经 `current_proc()` + `cap_target`/`pid_root`/界检查,非 panic。

### 3.4 ipc.rs / mmu.rs / main.rs

- `ipc.rs`:send/recv 目标解析匹配 `Cap::Proc`,`Cap::Shm` → `-EINVAL`(经
  `cap_errno(WrongType)`);errno 改引用 syscall.rs 常量。
- `mmu.rs`:`tlb_flush` 启用,doc 注明 T3c 共享页 revoke 用途。
- `main.rs`:`mod shm` + `shm::init()`(boot_tests 之前)。

### 3.5 tests.rs

- `boot_shm_test`(完整共享页生命周期:建 → A 写 → B 读回/写 → B 撤销 →
  主上下文断言双映射消失 / 双槽 NotFound / 注册表出列)、`boot_cap_test`
  (Proc cap dup + revoke 语义,dup 副本不受 revoke 影响)。
- **修复** `prog_b` 的 `sw t3, 0(t0)` 编码 `0x00c2a023` → `0x01c2a023`。

### 3.6 Makefile / ci.yml

- 2 条新 banner(`M2 T3c: shared mem ok` / `M2 T3c: cap dup/revoke ok`)入
  Makefile test / test-smp / test-rva23 与 ci.yml build / smp / rva23,共
  **6 处同步**(部分匹配,兼容 N=1/N=4)。

## 4. 验证结果

五门禁全绿(dev profile 由 `cargo build` 单独覆盖):

```
=== GATE: clippy(--release -D warnings) === PASS     === GATE: fmt === PASS
=== GATE: test === PASS      === GATE: test-smp(-smp 4)=== PASS(×3 连跑)
=== GATE: test-rva23(-cpu max) === PASS     (+ dev `cargo build` PASS)
```

单核(-smp 1)日志(boot hart=0):

```
[000126] [INFO ] M2 T3c: shared mem ok (map/revoke)
[000126] [INFO ] M2 T3c: cap dup/revoke ok
[000126] [INFO ] M2 T3a: multi-core boot ok (1 harts online)
[000127] [INFO ] M2 T3b: per-CPU sched ok (1 harts)
```

4 核(-smp 4)日志(3 条 `hart N online` + T3a/T3b banner + T3c 双 banner,
无 panic、无 `TRAP:`):

```
[000127] [INFO ] M2 T3c: shared mem ok (map/revoke)
[000127] [INFO ] M2 T3c: cap dup/revoke ok
[000127] [INFO ] hart 1 online
[000127] [INFO ] hart 2 online
[000127] [INFO ] hart 3 online
[000127] [INFO ] M2 T3a: multi-core boot ok (4 harts online)
[000135] [INFO ] M2 T3b: per-CPU sched ok (4 harts)
```

- 既有 12 条 banner 全保留(T2a 同步 IPC / T2a 抢占 / T2b PIP / T2b IPC
  stress / T1 用户态 ecall / T1.5 地址空间 / M0-M1 自检),回归无差异。
- `uptime:` ≥ 2(定时器 + sret 链路持续存活);`! KERNEL PANIC|TRAP:`。
- RVA23(-cpu max, Zba/Zbb/Zbs/Zicond):同断言全过。

## 5. 遗留风险

- **跨核 TLB shootdown 简化**:revoke 以"satp 切换全刷 + 本核 sfence"简化
  (M2 单页共享、revoke 即销毁页,无残留映射复用);真实多核 shootdown 归 T4。
- **单页共享上限**:`mmap_share` 仅接受 4096(单页);大消息 / 多页 / 文件
  映射留待后续里程碑。
- **无动态权限集**:共享页固定 U|R|W,无只读 / 只写权限协商。
- **revoke 级联 / 子能力未做**:`Cap::Shm` revoke 即整页销毁,无子能力
  撤销级联。
- **进程销毁未做**:`shm_revoke` 对已不存在进程的 root 仅跳过 unmap;
  进程表条目生命周期 / 页回收整体留待后续里程碑。
- **机器码手写流程**:本次 bug 源于 S 型立即数字段与 rs2 混排;测试用机器
  码靠逐条人工核对,后续用户程序应引入汇编器生成 / 自动解码校验。

## 附:T3c 与 T3 计划的偏差说明

计划中的能力槽双槽改授、revoke 整页销毁、注册表槽式复用均按设计落地;偏差:
- 新增 `SYS_ERR_*` 单一来源常量(计划未显式列出,为消除 errno 分散定义);
- `mmap_share` 的表满 / grant 失败回滚从 `expect` 改为防御性回滚(维持
  syscall 路径"用户输入不 panic"纪律);
- 测试断言含 revoke 前物理页内容校验(`shm_paddr` 读回 0xA5),为 T3c
  首个实现的回归锚点。
其余按计划落地。
