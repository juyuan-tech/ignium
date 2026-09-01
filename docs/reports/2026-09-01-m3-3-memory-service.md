# M3-3:内存服务(Cap::Page 纯服务授权)收官(2026-09-01)

日期:2026-09-01
阶段:M3 阶段 3(ROADMAP 阶段 3 用户态服务的第二步 = 内存服务)

## 1. 摘要

ROADMAP 阶段 3 的第二项 = **内存服务:cap 发页 + IPC 申请/释放**(验收:用户进程
可申请页)。本轮引入新能力 `Cap::Page`(物理页所有权),并让**用户态 mem_server**
成为发页的唯一入口。

**用户已拍板(AskUserQuestion)**:发放机制 = **纯服务授权** —— 内核**不暴露**
通用分配 syscall(避免 ambient 授权);客户端只能经 mem_server 的 IPC 申请/释放页;
mem_server 创建后由引导编排(当前即测试)注入页池;客户端归还页经反向 `mem_grant`
送回服务,池可复用。最贴合「一切皆能力」,与 M3-2 纯服务化一脉相承。

交付分 **5 个提交**(每提交过全量门禁;文档/代码分离遵循 M3-DESIGN 纪律):

| 提交 | 内容 |
|---|---|
| `5561d4c docs: M3-3 设计` | M3-DESIGN §11 + SYSCALLS 登记(13/14)+ DEFERRED D33-35 |
| `5029028 feat: Cap::Page 注册表 + mem_grant/mem_map(号 13/14)` | pages.rs(alloc/revoke/grant/map)+ Cap::Page 变体 + destroy 钩子 + cap_revoke/dup 分支 + ipc/shm WrongType 分支 + syscall 13/14(**#2+#3 编译器强制合并**,见 F1) |
| `4fd4839 feat: memory_server 用户态服务` | memory_server/mem_client 两 bin + lib.rs helper + build.rs 多 ELF + elf.rs 两 const + SERVICE_MEMORY(临时 `#[expect(dead_code)]`) |
| `ae75242 feat: M3-3 测试 + 两 banner` | `pages::alloc`(引导编排注入)+ T1/T2 + 两 banner + 6 处 grep;移除 expect |
| `docs: M3-3 收官`(本提交) | 本报告 + ROADMAP 勾选 + **M3-DESIGN §11 漂移修正**(§11.2 失效标记 `id=usize::MAX`、函数表去 `unmap`/`map` 签名对齐;§11.4 改"移交隐含解除发送方映射",与 F2/实现一致) |

新增 syscall:`mem_grant`(号 13)、`mem_map`(号 14);新能力 `Cap::Page(id)`(1 cap
= 1 物理页,**单引用**,禁 dup)。

## 2. 交付内容(对照设计)

### D1 Cap::Page + 页注册表(`kernel/src/pages.rs`)

- `Cap::Page(usize)`:1 cap = 1 个物理页(4KB),**单引用**(cap_duplicate 拒绝,
  `Err(WrongType) → -EINVAL`);页生命周期 = 能力生命周期。
- `PageTable { pages: Vec<PageRecord>, free: VecDeque<usize> }`,`MAX_PAGES=64`,
  `static PAGES: SpinLock`,`init()` boot 期 `reserve(MAX_PAGES)`(仿 shm.rs)。
- `PageRecord { id, paddr, owner: pid, map_va: Option<usize> }`;revoke 后 `id =
  usize::MAX` 失效 + 入 free 池(防槽位复用后陈旧记录误命中,仿 SharedPage)。
- `alloc(owner)`(**引导编排专用,无 syscall**)、`revoke(id)`(unmap→free→出表)、
  `grant(id, to_pid)`(owner 移交,隐含解除发送方映射)、`map(id, va)`(单映射
  不变量,已映射 → `Err`)、`paddr`/`is_mapped`(mem_map 预检)。
- 锁序:`TABLE → PAGES`(入口先 cap_target/pid_root 取 TABLE 即放,再取 PAGES;
  revoke/grant 锁外再经 pid_root + mmu unmap),不逆序、不重叠。

### D2 无公开分配 syscall;页池由引导编排注入

- **无 `mem_alloc` syscall**。内核侧 `pages::alloc` 仅由测试 T1/T2 在 mem_server
  spawn 后经 `grant_typed_cap` 注入其 cap 槽 1..=4(4 页)。正式 spawn/init 服务
  落地后改为引导期自动授予(登记 **D33**)。
- 用户态 mem_server 维护 `pool_full: [bool; 4]`(本地,单线程);ALLOC 满槽 →
  mem_grant 移交,池位清空;FREE 空槽乐观置位回填(D35)。

### D3 mem_grant(号 13):受控跨进程页移交

- 签名:`mem_grant(a0=src_slot, a1=peer_slot, a2=dst_slot)`;**move** `Cap::Page`
  (src_slot → peer 的 dst_slot),清 src_slot(单引用,防双持)。
- 门禁:src 持 `Cap::Page`(空槽 -EACCES / 非 Page -EINVAL);peer_slot 持
  `Cap::Proc(peer)`(空槽 -EACCES / 非 Proc -EINVAL)—— **只能移交给已连接进程**;
  peer 存活(-EACCES);dst 越界(-EINVAL)/ 非空(-EEXIST,防静默丢 cap)。
- 移交完整回滚:grant_typed_cap 失败 → owner 交还发送方,不落半状态。

### D4 mem_map(号 14)+ cap_revoke 兼任释放 + cap_dup 禁页

- `mem_map(a0=slot, a1=va)`:va 页对齐 + `< USER_VA_LIMIT`(否则 -EINVAL);槽持
  `Cap::Page`(空槽 -EACCES / 非 Page -EINVAL);页未映射(二次 -EEXIST);
  `mmu::map_user_page(root, va, paddr, 0xC7)`(U RW + 单地址 sfence)→ `pages::map`
  记 map_va;失败完整回滚(先 unmap 再返回)。
- **释放 = cap_revoke(号 6)**:`Cap::Page(id)` → `pages::revoke`(unmap → free →
  出表 + 清槽)。unmap-without-free / remap 延后(D34)。
- **cap_duplicate 禁页**(单引用不变量);Proc/Shm 不受影响。

### D5 process::destroy 钩子 + ipc/shm 类型分支

- destroy 步骤 1(TABLE 锁内)收集全部 `Cap::Shm` + `Cap::Page`,锁外逐个 revoke
  (**在 destroy_root 之前** —— 防 U 叶子 double-free,同 Cap::Shm 纪律);
- `cap_revoke` 分派 match 加 `Cap::Page` 分支;`ipc::send/recv` 的 cap_target match
  加 `Cap::Page → WrongType`(编译器强制);`shm::mmap_share` 同(Page 非 IPC 许可)。

### D6 memory_server 用户态服务 + 归还协议

- **memory_server**:`service_register(SERVICE_MEMORY=2)` → accept-any 循环 →
  OP_ALLOC(选满槽 mem_grant 移交,清池位)/ OP_FREE(选空槽乐观置位,回复
  recv_slot)/ 其它 → PROTO_ERR。
- **mem_client**:`service_connect` → send ALLOC(收页槽 3)→ mem_map(MEM_VA)→
  写 MAGIC + 读回校验 → send FREE → 收 recv_slot → mem_grant(3, peer=2, r) 归还
  → 写 marker `0xC0DE_0000|argc` → sys_exit。
- **归还协议(D34)**:客户端归还的页已映射,而系统无 unmap-without-free syscall →
  `mem_grant` 移交时由 `pages::grant` **自动解除发送方映射**(移交隐含 unmap)。
  这是文档化的设计调整,mem_map/grant 为页映射状态的唯一变更点,状态机封闭。

### D7 boot 编排与测试(T1/T2)

- **T1(单核,引导期协作式)**:spawn mem_server → 注入页池(pages::alloc×4 +
  grant_typed_cap 槽 1..=4)→ yield 至 accept-any 阻塞 → `lookup(SERVICE_MEMORY)`
  → spawn mem_client(marker 页)→ 轮询 marker(申请→映射→读写→归还往返)→
  负面用例 → kill_process 收服务 + destroy 收 client + `free_page_count` 复原。
  banner `M3-3 T1: memory service ok`。
- **T2(跨核,irq_enable 后)**:mem_server 亲和副核 A、client 亲和副核 B,跨核
  Cap::Page 移交(D6 IPI);单核退化仍打 banner。banner
  `M3-3 T2: cross-core mem IPC ok (N harts)`。
- 两 banner 各同步 **6 处 grep**:Makefile `test`/`test-smp`/`test-rva23` +
  ci.yml `build`/`smp`/`rva23` job(AGENTS.md 纪律)。

## 3. 实现中发现的设计缺口与修复

### F1(约束,编译器强制)Cap::Page 变体 dead-code → #2+#3 合并

- **位置**:`kernel/src/pages.rs` + `process.rs` Cap enum + `syscall.rs`。
- **触发**:计划切分 #2(Cap::Page 变体 + 注册表)#3(mem_grant/mem_map)分开提交。
  但 `-D warnings`(dead_code)下,`Cap::Page` 变体只有在 mem_grant(syscall 13)或
  测试池注入中才会被构造 —— 两个提交各自都"缺消费方"无法独立编译通过。
- **处置**:合并为单提交 `5029028`(6 文件 +456 行,commit message 已记录)。
  同样约束在 #4 再现:`MEMORY_SERVER_ELF`/`MEM_CLIENT_ELF`/`SERVICE_MEMORY` 仅被
  #5 测试引用 → 临时 `#[expect(dead_code)]`(#5 引用后移除)。

### F2(设计缺口)归还协议:客户端归还已映射页无 unmap 原语 —— mem_grant 自动 unmap

- **位置**:`kernel/src/pages.rs`(`grant`)。
- **触发**:客户端归还给 mem_server 时页**已映射**(mem_map 过),而系统无
  unmap-without-free syscall(不引入第 3 个页 syscall 的成本)。
- **修复(文档化调整)**:`pages::grant` 在锁内捕获 (old_owner, map_va),owner 移交
  时 `map_va.take()`;锁外若旧持有者已映射则从其根表 unmap。mem_grant 语义 =
  **移交隐含解除发送方映射**,归还协议闭环。门禁不放松:接收方仍须持 Cap::Proc,
  无 ambient 移交。

### F3(门禁纪律)6 处 grep 的 banner 一致性

- T1/T2 各 6 处 grep 已逐处核对(Makefile 3 + ci.yml 3);新 banner 插入位置在
  各 log 的 M3-2 行之后,既有 banner 无回归。

## 4. 验证结果

### 门禁(全部通过,docker 容器 `ignium-dev:1.97.1`)

| 门禁 | 结果 |
|---|---|
| `make build`(release) | PASS |
| `cargo build`(dev) | PASS(双 profile) |
| `cargo clippy --release -- -D warnings` | PASS(零警告) |
| `cargo fmt --check` | PASS |
| `make test`(单核,T1) | **TEST PASS** |
| `make test-smp`(`-smp 4`,T2) | **SMP TEST PASS** |
| `make test-rva23`(`-cpu max`,Zba+Zbb+Zbs+Zicond) | **RVA23 TEST PASS** |

三 log 零字面 `KERNEL PANIC|TRAP:`。

### T1 断言(单核协作式,boot_tests)

1. mem_server accept-any recv 阻塞(内核侧 yield 轮询 `is_blocked`);
2. `services::lookup(SERVICE_MEMORY).is_some()`(注册表已登记);
3. 页池注入:槽 1..=4 各持 `Cap::Page`(pages::alloc ×4 + grant_typed_cap);
4. client 完整往返:connect → ALLOC(mem_grant 移交)→ mem_map(MEM_VA)→ 写
   MAGIC + 读回比对 → FREE(recv_slot)→ mem_grant 归还 → 写 marker
   `0xC0DE_0000|1`;内核轮询 marker 到;
5. 负面用例全覆盖:
   - 重复 `register_service(SERVICE_MEMORY)` → `-EEXIST`;
   - `mem_map` 未对齐 va / va≥USER_VA_LIMIT → `-EINVAL`;空槽 → `-EACCES`;
     非 Page 槽 → `-EINVAL`;二次映射 → `-EEXIST`;
   - `cap_duplicate(Cap::Page)` → `-EINVAL`(单引用);
   - `mem_grant` 空 src / 空 peer → `-EACCES`;dst 越界 → `-EINVAL`;dst 被占 →
     `-EEXIST`;
   - `cap_revoke(Cap::Page)` 释放页(含解除映射)→ 页计数复原;
   - destroy 钩子:授页给临时进程 → destroy → 页计数复原(revoke-before-
     destroy_root,无 double-free);
6. 清理后 `free_page_count` 复原(池 4 页全回 buddy)。

### T2 断言(跨核,irq_enable 后;n=4 用副核 1/2)

- mem_server 亲和副核 A、client 亲和副核 B(A≠B);client 的 ALLOC send 经 **D6
  IPI** 唤醒副核 A 阻塞中的 mem_server → mem_grant 跨核移交 Cap::Page → 回复经
  D6 IPI 唤醒副核 B 阻塞中的 client → mem_map 读写 → FREE 归还 mem_grant 回副核
  A → marker 完成。**双向跨核即时配对**(非定时器轮询)。
- 单核退化(n=1)仍打 banner `M3-3 T2: cross-core mem IPC ok (1 harts)`。

### 提交门禁纪律

6 处 grep 一致性核对(每 banner 恰 6 处);dev+release 双 profile 可编;无字面
`KERNEL PANIC|TRAP:`。

## 5. 遗留风险(登记 DEFERRED.md D33-35)

- **D33 页池注入依赖引导编排**:当前 = 测试注入槽 1..=4;spawn/init 服务落地后
  改为引导期自动授予。
- **D34 页无 unmap-without-free / remap**:revoke = 释放;重定位需重分配。
  `mem_grant` 移交隐含解除发送方映射(归还协议),但无"单方面解除映射再映射
  同页"的路径。
- **D35 OP_FREE 归还乐观置位**:客户端归还 mem_grant 前崩溃 → 池位空但服务误判
  可用(mem_grant 失败可优雅降级 ERR_ENOMEM);服务崩溃恢复里程碑统一处理。
- 页单引用(禁 dup):多引用/共享私有页延后(共享用 shm)。
- `MAX_PAGES=64` / 服务池 4 页的有界配额。
- destroy 持 TABLE→PAGES 锁序,不破坏 TABLE→SERVICES→DEVICES→IPC→SCHED。

---

**下阶段指向**:ramfs 文件系统服务(内存页复用 Cap::Page/共享内存)→ spawn 服务化
+ init + shell(ROADMAP 阶段 3 剩余行)。
