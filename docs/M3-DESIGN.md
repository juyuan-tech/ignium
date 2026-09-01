# M3 设计:用户态服务 + L2 兼容(草案)

> 目标阶段:**M3**(用户态服务 + L2 兼容),承接 M2 收官(v0.1.0-M2)。
> 本文为 M3 设计基线,先文档后代码(遵循 DESIGN.md「先读透 seL4/rCore,
> 前 3 个月重文档轻代码」)。**M3-1(已落地,2026-09-01)**:ELF 加载器 +
> 内核 `sys_write` 过渡占位 + 跨核 IPI 停核/Running 线程回收 + 跨核 TLB
> shootdown + 内核线程栈守护页。**M3-2(已落地,2026-09-01)**:uart_server
> 服务化(见 §10)—— 设备页授予 + 内核服务注册表 + 移除 sys_write 占位 +
> sys_read 落地 + 跨核 IPC 实测。**M3-3(本轮)**:内存服务 Cap::Page(见 §11)
> —— 页能力 + 页注册表 + mem_server 服务 + 申请/释放 IPC(纯服务授权)。
> M3-3 及以后:ramfs、virtio-blk、spawn/init/shell、musl/busybox(L2)、
> 服务崩溃恢复。

## 1. 目标与验收(对齐 ROADMAP 阶段 3)

| 任务 | 验收 | 处置 |
|---|---|---|
| uart_server 进程独占 UART,打印走 IPC | 内核不再直碰 UART | **已落地(M3-2,§10)**:uart_server 独占 UART(设备页授予 U 映射),内核 `sys_write` 占位已删除;打印/读取走 IPC |
| 内存服务:cap 发页 + IPC 申请/释放 | 用户进程可申请页 | **本轮(M3-3,§11)**:纯服务授权,mem_server 唯一发页入口 |
| ramfs 文件系统服务(open/read/write/close) | IPC 客户端可读写删文件 | 延后 M3-3 |
| virtio-blk 驱动服务 + 持久文件系统 | 重启数据仍在 | 延后 M3-3 |
| spawn 服务化 + init 进程 + shell | shell 跑通 echo/cat 重定向 | 延后 M3-3(M3-2 只落地**内核服务注册表** §10 D2) |
| **musl 移植 + busybox 跑通(L2)** | busybox 常用命令可用 | 延后 M3-3 |
| 服务崩溃恢复:杀 FS 服务,系统存活可重启 | 故障注入测试通过 | 延后 M3-3+;基础已具备(D12 杀进程 + 系统存活) |

**M3-1 验收标准(已满足,2026-09-01)**:①ELF 加载器(hello 跑通,`M3 T1` banner);
②跨核 kill/shootdown(`M3 T2` banner);③内核线程栈守护页(自检断言);④五门禁全绿
+ dev/release 双 profile + 三配置 log 零字面 `KERNEL PANIC|TRAP:`。

**M3-2 验收标准**:
1. uart_server 用户进程独占 UART:客户端经 IPC 打印/读取,内核不再直碰 UART
   (banner `M3-2 T1: uart_server service ok`)。
2. 内核服务注册表:服务进程 `service_register` 自报,客户端 `service_connect`
   获得双向 IPC 能力(cap 槽)。
3. 跨核 IPC 实测:uart_server 亲和一核、client 亲和另一核阻塞配对即时成功
   (验证 B1 跨核分支,banner `M3-2 T2: cross-core IPC ok (N harts)`)。
4. 五门禁全绿 + dev/release 双 profile 可编 + 三配置 log 无字面
   `KERNEL PANIC|TRAP:`;既有全部 banner 不回归。

## 2. 文档空白处置决策(每项给"补/延后"结论)

| 文档空白 | 处置 | 说明 |
|---|---|---|
| `docs/SYSCALLS.md` | **本轮新建** | L1 ABI 唯一来源,登记 1-7 现有号 + 8/9 新增号,与 `kernel/src/syscall.rs` 常量一一对应;9 号(READ)本轮返回 -ENOSYS |
| compat-baseline(对齐 LiteOS-A 的 POSIX 子集清单) | 延后 M3-2 | 等 uart_server/ramfs 服务定案后一起写,避免边写边改 |
| M3-DESIGN(本文) | **本轮新建** | 设计基线 |
| 服务注册机制(spawn 服务化) | **M3-2 落地(§10 D2)** | 内核服务注册表:`service_register`(号 10)+ `service_connect`(号 11)双向授予;spawn 服务化本身延后 M3-3 |
| `Cap::Page` 发页能力 | **M3-3 落地(§11)** | 页能力 + 页注册表;纯服务授权(仅 mem_server 发页,无公开分配 syscall);新增号 13/14 |
| spawn/init/shell 设计 | 延后 M3-3 | M3-2 落地内核服务注册表后,spawn 服务化顺理成章 |
| uart_server 过渡方案 | **已落地(M3-2)** | M3-1 内核 `sys_write(fd=1)` → UART 过渡占位已删除,uart_server 独占 UART(§10) |
| SCHED 锁缩放 / D1 / slab 水位 | **本轮评估后延后** | 评估结论见 §7,移入 DEFERRED.md |

## 3. ELF 加载器(固化 M2-DESIGN §7)

M2-DESIGN §7 仅提纲,本轮固化:**解析 + 校验 + 映射 + 用户栈 + argc/argv + 构建集成**。

### 3.1 范围与产物

- 新 `kernel/src/elf.rs`:`parse(&[u8]) -> Result<ElfInfo, ElfError>`(只读校验)
  + `load(pid, bytes, args) -> Result<(entry, sp), ElfError>`(映射 + 初始栈)。
- 新 `user/` 独立 crate `ignium-user-hello`(`#![no_std] #![no_main]`,
  `_start` 裸函数,inline-asm syscall helper,自供 `user/linker.ld`)。
- `kernel/build.rs` 方案 A(cargo-in-cargo)编译 user crate,产物拷入 OUT_DIR,
  kernel `include_bytes!` 内嵌;方案 B(预编译 ELF 提交入库)为回退。
- 测试 `boot_elf_test()`:正常路径 + 负面用例 + 用户栈守护页断言。

### 3.2 校验(parse,只读,零分配)

- magic `\x7fELF` / class=2(64 位)/ data=1(LE)/ EM_RISCV=243 / ET_EXEC。
- `e_phoff + e_phnum*56 ≤ len`;每个 `PT_LOAD`(type=1):
  - `p_offset + p_filesz ≤ len`(文件内越界 → `ElfError::Truncated`);
  - `p_memsz ≥ p_filesz`(bss 段 memsz>filesz);
  - `p_vaddr < USER_VA_LIMIT`(= 2^38,即 `0x4000_0000_00`;Sv39 用户区上限,
    单一来源 `mmu::USER_VA_LIMIT` —— 审计修正:M3-T1 发现曾误写 2^46);
  - 页内偏移匹配:`p_vaddr & 0xfff == p_offset & 0xfff`(段映射页对齐约束);
  - 段间无覆盖:按 `p_vaddr` 排序后逐对检查
    `p_vaddr + p_memsz > next.p_vaddr` → `ElfError::Overlap`。
- **无任意物理地址**:所有物理页由 `mem::alloc_pages_zeroed` 新分配,
  `p_paddr` 完全忽略(微内核不信任可执行文件声明的物理地址)。

### 3.3 映射(load)

逐段、逐页映射(拒绝覆盖语义,沿用 `map_user_page`):

- 对每 PT_LOAD,页范围 `align_down(p_vaddr) .. align_up(p_vaddr + p_memsz)` 逐页:
  `mem::alloc_pages_zeroed(order=0)` → 拷贝文件交集(bss 尾部 `memsz>filesz`
  天然为整页零)→ `mmu::map_user_page(root, va, pa, flags)`(自动置 U 位,A/D)。
- flags:X→`PTE_LEAF_RX`(0xCB)/ W→`PTE_LEAF_RW`(0xC7)/ R→`PTE_LEAF_R`(0xC3),
  与 M2 内核镜像段权限拆分同语义(代码可执行不可写 / 数据可写不可执行)。
- 失败回退:已映射页逐页 `unmap_4k` + `mem::free_pages` 归还,返回错误。

### 3.4 用户栈 + argc/argv(RISC-V ABI)

- `USER_STACK_TOP = USER_VA_LIMIT - 64K`(= 2^38 − 64K = `0x3FFFFF_0000`,
  栈顶留余量且保证 bit38=0 规范化;审计修正:M3-T1 曾误用 2^46−2^40,
  非规范 VA 致 QEMU TRANSLATE_FAIL),8 页(32KB);栈底下方 1 页守护
  (VA 空洞不分配,沿用 D20 语义)。栈 8 页物理页独立分配、非连续,写按
  所在页解引用物理地址。
- `build_initial_stack`:自栈顶向下写 `argv[0..n]` 字符串 + 指针数组 + `argc`,
  对齐 16B;初始帧 a0=argc、a1=argv(与 spawn_user 现有帧构造一致)。

### 3.5 构建集成

- **方案 A(首选)**:`kernel/build.rs` 内
  `cargo build --manifest-path ../user/Cargo.toml --target
  riscv64gc-unknown-none-elf --release`,env `CARGO_TARGET_DIR=$OUT_DIR/user-target`
  (规避 cargo-in-cargo 锁冲突);产物拷 `$OUT_DIR/hello.elf`;
  kernel `include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"))`;
  `rerun-if-changed=../user/**`。容器/CI 实证不稳 → 回退方案 B。
- **方案 B(回退)**:`user/hello.elf` 预编译提交入库,kernel 直接 `include_bytes!`。
- workspace 约束:根 `.cargo/config.toml` `build.target` 已默认
  riscv64gc-unknown-none-elf,user crate 可独立于 kernel 编译(避免 kernel 依赖
  的 crate 混入)。

## 4. 用户态服务运行机制(M3-1 定案部分)

- **服务 = 独立进程**(与普通用户进程同构):ELF 经内核加载器映射进自身地址空间,
  U 模式运行;IPC 目标是 cap 槽(`Cap::Proc`),授权语义沿用 M2。
- M3-1 无服务注册表:测试直接创建进程 + 加载 ELF + IPC 交互。
- **`sys_write(fd=1)` → UART 为过渡占位**:u0 服务进程可打印;内核直碰 UART
  是微内核的**临时例外**(铁律 2"兼容代码零进入内核"不受影响 —— 这是内核
  自身输出,非兼容层),`kernel/src/syscall.rs` 注释 + M3-DESIGN 明确 M3-2
  uart_server 落地后删除。

## 5. L1 syscall ABI 扩展

唯一来源见 `docs/SYSCALLS.md`;本节省略号定义:

| syscall | 号 | 入参 | 返回 |
|---|---|---|---|
| `sys_exit` | 1 | — | 不返回 |
| `sys_get_ticks` | 2 | — | a0 = tick |
| `ipc_send` | 3 | a0=槽, a1-a5=消息 | 成功 0 / 负 errno |
| `ipc_recv` | 4 | a0=槽 | 成功 0 + a1-a5 消息 / 负 errno |
| `shm_map` | 5 | a0=本槽, a1=对端槽, a2=len | 成功 a0=shm_id / 负 errno |
| `cap_revoke` | 6 | a0=槽 | 成功 0 / 负 errno |
| `cap_dup` | 7 | a0=源槽, a1=目标槽 | 成功 0 / 负 errno |
| ~~`sys_write`~~ | 8 | — | **M3-2 移除,号保留**(见 §10 D5) |
| ~~`sys_read`~~ | 9 | — | **M3-2 移除,号保留**(读取经用户态库 `uart_read`,§10 D4) |
| **`service_register`** | **10(新增)** | a0=服务 id | 成功 0 / -EINVAL / **-EEXIST** |
| **`service_connect`** | **11(新增)** | a0=id, a1=client 槽, a2=server 槽 | 成功 0 / -EINVAL / -ENOENT / -EACCES |
| **`map_device`** | **12(新增)** | a0=dev_id, a1=va | 成功 0 / -EINVAL / **-EEXIST** |
| **`mem_grant`** | **13(新增)** | a0=源槽, a1=peer 槽, a2=对端目标槽 | 成功 0 / -EINVAL / -EACCES / -EEXIST |
| **`mem_map`** | **14(新增)** | a0=槽, a1=va | 成功 0 / -EINVAL / -EACCES / -EEXIST |

**M3-1 的 `sys_write` 语义(历史记录,已删除)**:fd=1 → `uart::write_bytes(buf)`;
fd≠1 → `-EBADF`;逐页校验 buf(限 U 位);`len > 4096` → `-EINVAL`;拷贝置
`sstatus.SUM=1`。**M3-2 删除**:打印/读取一律走 IPC 到 uart_server,内核不再直碰
UART(见 §10)。新 syscall 语义见 §10 与 `docs/SYSCALLS.md`(唯一来源)。

## 6. 跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown

### 6.1 问题(现状)→ M3 T2 已解决

> **实现状态(2026-09-01)**:本节问题已由本轮 M3 T2 修复。`kill_current_process`
> 现为薄封装,公共核心 `kill_process(pid)`(sched.rs:1511);SSIP handler
> (riscv64.rs)经 `REMOTE_REQ` 位图分发 TLB_FLUSH/FORCE_KILL,`force_kill_current`
> 回收其它核 Running 线程(栈→reaper、槽→free_slots、状态→Exited);shm revoke
> 经 `tlb_shootdown_remote` 跨核失效。行号随重构漂移,以下为历史动机记录。

- `kill_current_process`(原 sched.rs:1180)对其它核 `state==Running` 线程
  `continue`(原 sched.rs:1208-1211):栈不回收 / TCB 槽不复用 / 状态不改 —— 泄漏,
  依赖其下次用户访存故障自愈。
- `mmu::tlb_flush()`(原 mmu.rs:542)只刷当前核;shm revoke 的 TLB 失效只对当前核
  生效 —— 其它核读共享页仍命中陈旧 TLB。

### 6.2 设计

**全局请求队列**(原子位图,ISR 安全,零分配):
- `static REMOTE_REQ: [AtomicUsize; MAX_HARTS]`(bit0=TLB_FLUSH, bit1=FORCE_KILL);
  置位后 `sbi::send_ipi(1 << hart, 0)`。
- `static THREAD_KILL_REQUEST: [AtomicBool; MAX_THREADS]`(按线程 ID)。

**`kill_process(pid)`(公共核心,重构自 kill_current_process)**:
1. `purge_process(pid)`(IPC 挂起清理,锁序 TABLE→IPC);
2. 收集 victims(当前进程的线程),摘就绪队列;
3. 非 Running → 照旧回收(栈→reaper、槽→free_slots、状态→Exited);
4. **Running 于其它核** → 置 `THREAD_KILL_REQUEST[tid]` + `REMOTE_REQ[hart].fetch_or(FORCE_KILL)`
   + drop SCHED + 锁外 `send_ipi(1<<hart, 0)` → **有界等待**(约 200ms 轮询
   `THREAD_KILL_REQUEST` 全清;超时 warn + 回退旧自愈路径);
5. `switch_root(kernel_root)` → `process::destroy(pid)` → `exit_from_trap`。
`kill_current_process` 变薄封装(仅处理本核 Running)。

**SSIP handler 改造(riscv64.rs:555,`trap_handler` 内 CAUSE_SUPERVISOR_SOFTWARE 分支)**:
- `csrc sip, SSIP` → `REMOTE_REQ[h].swap(0)`:
  - bit0(TLB_FLUSH)→ 本核 `sfence.vma zero, zero`;
  - bit1(FORCE_KILL)→ `return sched::force_kill_current(frame, h)`。

**`force_kill_current(frame, hart) -> *mut usize`**(与 on_tick 同构,
ISR 内零分配零日志,只取 SCHED 锁,不碰 TABLE/IPC):
- `cur = current[hart]`;`!THREAD_KILL_REQUEST[cur]` → 原帧(非目标);
- 否则:捕获帧进 TCB → 状态 Exited → 栈→reaper → 槽→free_slots → 清请求 →
  撤销捐赠(双向)→ `pick_next` → `switch_root` → 返回 next 帧。
- SSIP 只打断 U 执行或 SIE=1 内核线程执行,帧均完整;若目标线程正于 syscall
  处理中(sret 后 U 再 trap),IPI 在 sret 回 U 后立即触发,天然闭环。

**shm revoke 跨核 shootdown**:
- unmap 双方后对其它在线核 `REMOTE_REQ[hart].fetch_or(TLB_FLUSH)` + `send_ipi`
  + 有界等待位清空;远端只取 TLB(不经 SHM/表锁),无锁序反转。
- 本核 `unmap_4k` 自带 sfence,只对远端补 shootdown。

**D12 自愈保留**:主动回收为主路径;`process::destroy` 幂等,
超时回退仍走旧自愈(下次用户访存故障再回收);普通段错误仍走 D12。

### 6.3 测试

- `smp_kill_test`:核 1 跑 busy-loop 用户线程 → boot hart `kill_process` →
  断言请求全清 / free_slots 恢复 / pid_root 失效 / 核 1 可再 spawn。
- `smp_shootdown_test`:核 1 线程反复读 `SHM_VA` → revoke → 断言其下次访存
  故障自愈而非读陈旧数据。
- 单核退化(MAX_HARTS=1)仍过。

## 7. 遗留项处置决策(评估后延后,已同步 DEFERRED.md)

| # | 项 | 评估结论 | 处置 |
|---|---|---|---|
| SCHED 全局锁拆分 | 现状正确(D19);M3 已叠加两个高敏改动,拆分风险不可控;收益有限(QEMU 4 核 IPC 瓶颈是 current_id 取锁,非整锁带宽);成本高(跨核 enqueue 需按 hart 序取锁防死锁;on_tick/block/exit/yield 全部重审) | **延后 M4**;可选低风险子项(per-CPU `CURRENT_TID` 原子缓存)单独评估,不并入本轮 |
| D1 中断快速路径 | asm ABI 重构(仅存 caller-saved)需在切换点把 s0-s11 从陷阱栈搬进 TCB(on_tick/block/force_kill 全改),TRAP_FRAME 索引是跨 4 文件单一事实来源;收益 <5%(SSTC 已移除每 tick SBI ecall) | **不做** |
| slab 水位扫描 | 现状有界(非 head 空页下次 grow 懒回收,head 保留),内核堆用量几 KB 级 / RAM 128MB,收益 ≈ 0 | **不做**,M4 或真实内存压力时再评估 |

## 8. 实现顺序与提交切分

1. **T1 = ELF 加载器 + sys_write/read + user hello**(主交付)。
2. **T2 = 跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown**。
3. **T3 = 内核线程栈守护页**(自检,不新增 banner)。

提交切分(每提交过五门禁 + 6 处 grep 一致):
1. `docs: M3 入口设计 - M3-DESIGN + SYSCALLS + 遗留项处置`(纯文档)。
2. `feat: M3 T1 - ELF 加载器 + sys_write/read + user hello`。
3. `feat: M3 T2 - 跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown`。
4. `feat: M3 T3 - 内核线程栈守护页`。
5. `docs: M3 收官 - DEFERRED 同步 + 自审报告 + ROADMAP 勾选`。

## 9. 风险与对策

| 风险 | 对策 |
|---|---|
| ELF 校验遗漏 → 恶意/损坏 ELF 破坏内核 | parse 全字段校验(§3.2);映射逐页回退;无任意物理地址 |
| cargo-in-cargo 锁冲突 / CI 不稳定 | CARGO_TARGET_DIR 隔离;方案 B(预编译 ELF)回退 |
| 跨核 kill 死锁(SCHED 锁持有中发 IPI,远端 force_kill 取 SCHED) | 置位 + drop SCHED 后再 send_ipi;远端只取 SCHED 不取 TABLE/IPC |
| force_kill 与 on_tick 竞态(同核 IPI vs 定时器) | SSIP 在 trap_handler 中先于 timer 分发;force_kill 与 on_tick 都只取 SCHED(同锁互斥) |
| 内核线程栈守卫在进程根表失效(带 proc 内核线程) | M3-DESIGN 文档化残余局限:守卫只在内核根表生效,带 proc 内核线程运行于进程根表时守卫跨根表拆分传播延后(见 DEFERRED 关联) |
| 守护页解映射破坏身份映射不变量(bring-up 实测) | 内核恒经 VA==PA 访问物理页,堆/页表页经 buddy 原样取用不自行映射;栈归还前必须 `remap_kernel_4k` 恢复守护页映射再 dealloc —— 否则该 PA 被复用为页表页后 memset 即 store page fault(实测 scause=0xf @ 守护 PA) |

## 10. M3-2:uart_server 服务化(本轮)

M3-1 已交付 ELF 加载器;服务化的第一步 = uart_server 进程独占 UART。打印走 IPC、
内核不再直碰 UART(删除 M3-1 的 `sys_write(fd=1)→UART` 过渡占位),同时落地
**内核服务注册表**(客户端如何找到 uart_server)与 **sys_read**(读取经服务),
并实测 B1 跨核 IPC(审计点名"M3-2 落地后实测")。

### 10.1 现状约束(设计前提)

- **用户态无法访问 UART MMIO**:设备页(0x1000_0000)在所有根表 U=0,`map_user_page`
  拒绝非分配器 paddr(mmu.rs:187)→ 需专用**设备页授予原语**。
- **`destroy_root` 无条件释放 U=1 叶子**(mmu.rs:459-463):UART 页(非分配器)若 U-映射
  进进程根表,销毁时会对 buddy 外页 free → **必须配套修复**。
- **`ipc::recv` 要求本进程槽持 `Cap::Proc(src)`**(ipc.rs:202)→ 服务端必须持有
  Cap::Proc(客户端)才能 recv → `service_connect` 须**双向授予(互认介绍)**。
- **无头 CI 无法注入 UART 键盘**(Makefile `-nographic`)→ sys_read 自动测试只测
  "无数据"路径;真实键盘在 `make qemu`。
- **单 ELF 限制**:`kernel/build.rs` 只编译 `ignium-user-hello` → 需扩展多 ELF。

### 10.2 设备页授予:`map_device(dev_id, va)`(号 12)+ `kernel/src/device.rs`

- 白名单:`dev_id=0 → board::uart_base()`(0x1000_0000,页对齐);其它 → `-EINVAL`。
- 校验:va 页对齐、`va < USER_VA_LIMIT`、va 未映射(拒覆盖)、设备未被其它进程 claim
  (排他)。`MAX_DEVICES=8`,`slots: [Option<(pid, va)>; 8]`,独立 SpinLock。
- 新 mmu 接口 `map_device_page(root, vaddr, paddr, flags)`:与 `map_user_page` 同构
  (ensure_table + 拒覆盖 + 置 U 位 + 单地址 sfence),**跳过 `page_in_range`**;Safety
  注释:调用方(device.rs)须白名单校验 paddr。
- **destroy_root 耦合修复**:释放 U=1 叶子前先 `mem::page_in_range(pa)` 判断,设备页
  (非分配器)只 unmap 不 free。
- **不引入 `Cap::Dev`**:设备页生命周期随进程,`process::destroy` 调 `device::release_all(pid)`
  清 claim;revoke/特权授予延后(M3-3)。
- 锁序:`TABLE → DEVICES`,不逆序。安全局限(记录):任何进程可 claim UART(无特权校验),
  M3-2 靠引导序(uart_server 先 spawn 先 claim)保证。

### 10.3 内核服务注册表:`service_register`(号 10)/ `service_connect`(号 11)+ `kernel/src/services.rs`

- `MAX_SERVICES=8`,`slots: [Option<ServiceEntry{id,pid}>; 8]`,独立 SpinLock;
  `SERVICE_UART=1`(id 0 保留,1..=7 合法)。
- `service_register(id)`(进程自报):id 范围 → `-EINVAL`;已占用 → **`-EEXIST`**(新 errno
  MAX-6)。原子性:`process::register_service` 持 TABLE 锁校验进程存活再取 SERVICES 插入
  (`services::register_locked`),与 destroy 的 TABLE 锁串行化 → 杜绝"注册于已亡进程
  + pid 复用"竞态。
- `service_connect(id, client_slot, server_slot)`:SERVICES 查 id → server_pid(无 →
  `-ENOENT`);`server == caller` → `-EACCES`;槽越界 → `-EINVAL`;**双向授予**:
  `grant_cap(caller, client_slot, server_pid)` 且 `grant_cap(server, server_slot, caller)`
  (服务端无 Cap::Proc(客户端)则 recv 被拒)。
- `process::destroy` 步骤 2(TABLE 锁内)调 `services::unregister_all_locked(pid)` +
  `device::release_all(pid)`。锁序全程 `TABLE → SERVICES → DEVICES → IPC → SCHED`。

### 10.4 uart_server 用户程序 + SHM 打印路径(user crate 重构)

- user crate:包名 `ignium-user`,`[lib] lib.rs`(共享 syscall helper/常量/`#[panic_handler]`/
  uart client 库 `uart_write`/`uart_read`)+ `[[bin]] hello` + `[[bin]] uart_server`。
  `kernel/build.rs` 编译两 bin 拷 `hello.elf`/`uart_server.elf`;`elf.rs` 加 `UART_SERVER_ELF`。
- 固定常量(lib.rs):`SERVER_ACCEPT_SLOT=0`、`SERVER_SHM_SLOT=1`、`CLIENT_IPC_SLOT=2`、
  `CLIENT_SHM_SLOT=3`、`UART_MMIO_VA=0x6000_0000`(设备窗口,避开 ELF 0x4000_0000 /
  SHM 0x5000_0000)、`UART_SERVICE_ID=1`。
- uart_server 主流程:`sys_map_device(0, UART_MMIO_VA)` → `service_register(1)` → 循环
  `ipc_recv(slot 0)` → 按 op 处理 → `ipc_send(slot 0, reply)`。
- 打印数据路径:client 写字节到 `SHM_VA` → `ipc_send({op=WRITE, len})` → uart_server
  读 `SHM_VA[0..len]` 逐字节 TX(`\n→\r\n`)→ reply → client `ipc_recv`。
- 槽编排(client):connect 得 `Cap::Proc(server)`(槽 2)→ `cap_dup(2→3)` 保 IPC 槽 →
  `shm_map(a_slot=3, b_slot=SERVER_SHM_SLOT, len=4096)` → 双槽变 `Cap::Shm(id)`。
- RX:READ 请求时轮询 LSR bit0(DR)→ 读 RBR → 写 SHM_VA → reply nread;无数据 → nread=0。
- 并发限制:accept 单槽 + SHM_VA 单页 → **本轮 1 并发 client**(多 client 延后 M3-3)。

### 10.5 sys_read(号 9 移除 → 用户态库 `uart_read`)

读取是 fd 型兼容接口,属用户态库(POSIX 兼容零进内核),与"打印走 IPC"对称;未来
ramfs 读取同样走服务 IPC,内核无需 fd 命名空间。`lib.rs` 的 `uart_read(buf)`:
send READ → recv reply → 从 SHM_VA 拷回 buf;越界由服务端 `clamp(len ≤ SHM_LEN)`
防护;无数据返回 `nread=0`(EOF 语义,确定性,CI 可测)。

### 10.6 sys_write 移除(号 8)

内核删 `sys_write` 处理器 + `MAX_WRITE_LEN`;user 侧 `SYS_WRITE` 常量删;hello 重写为
"连接服务 → SHM 写 → IPC 打印"。boot_elf_test 只断言 marker(服务未注册时 connect
→ -ENOENT 非致命,marker 仍写)。

### 10.7 跨核 IPC IPI 补强 + 实测

`ipc_wake_with_msg/err`(sched.rs)Blocked 分支 enqueue 后补:
`let tgt = threads[tid].hart; if tgt != my_hart && current[tgt] == idle[tgt] →
sbi::send_ipi(1 << tgt, 0)`(仿 `wake()`,失败仅 warn 一次,降级 ≤1 tick)。B1 的
"未阻塞存 `IpcWake`"分支不需发 IPI(目标在阻塞点消费,不切走)。

### 10.8 消息协议(5 字 IPC + SHM 单页)

```
请求 = [op, arg1, 0, 0, 0]   op 0x01 WRITE:arg1=len,数据在 SHM_VA[0..len]
                             op 0x02 READ :arg1=max_len,服务端读 RBR → SHM_VA
                             op 0x03 PING :连通性(测试)
回复 = [op|0x80, status, len, 0, 0]   status=0 成功 / 负 errno;len=读写字节数
```

### 10.9 测试与 banner

- **T1(单核,boot_tests,协作式)** 仿 boot_ipc_test:spawn uart_server(ELF)→ yield_ 至
  recv 阻塞 → spawn hello(client)→ 轮询 marker → 断言;destroy 后 `free_page_count`
  断言(设备页不 free、无 double-free)。banner `M3-2 T1: uart_server service ok`。
- **T2(smp 阶段)** `smp_uart_ipc_test()`:uart_server 亲和 A、client 亲和 B 阻塞配对;
  单核退化仍打 banner `M3-2 T2: cross-core IPC ok (N harts)`。
- 负面用例:register 重 → -EEXIST;connect 未注册 → -ENOENT;map_device 二次 claim →
  -EEXIST;map_device 非法 va → -EINVAL。
- **新 banner 同步 6 处 grep**(AGENTS.md 纪律):Makefile test/smp/rva23 +
  ci.yml build/smp/rva23。

### 10.10 提交切分(每提交过五门禁 + 6 grep 一致)

1. `docs: M3-2 设计`(本节 + SYSCALLS + DEFERRED);
2. `feat: 设备页授予 + destroy 修复`(device.rs + mmu + 号 12 + EEXIST);
3. `feat: 内核服务注册表`(services.rs + 号 10/11 + destroy 钩子);
4. `feat: uart_server 服务化`(user 重构 + build.rs 多 ELF + 删 sys_write/read + hello 重写);
5. `feat: 跨核 IPC IPI + M3-2 测试`(ipc_wake 补 IPI + T1/T2 + 两 banner + 6 grep);
6. `docs: M3-2 收官`(报告 + ROADMAP 勾选)。

### 10.11 风险与遗留(登记 DEFERRED)

- 服务端 client 消亡后 recv 永久阻塞(陈旧 Cap::Proc):不做恢复(purge_process 已可投
  -ENOENT,预留 cap_revoke + 重连协议)。
- 并发 client 受限(accept 单槽 + SHM_VA 单页):本轮 1 并发,多 client 需多 SHM VA + 槽池。
- map_device 无特权校验:靠引导序保证;Cap::Dev/特权授予延后。
- 设备页不可手动 revoke(无 Cap::Dev):生命周期随进程。
- uart_server 崩溃即 UART 卡死(无看门狗)。
- 锁内发 IPI:仿 wake() 同款(目标核 SSIP handler 自旋等 SCHED 锁),已证安全。
- destroy_root 跳过非分配器 U 页:当前唯一来源是 map_device_page(白名单),安全。

## 11. M3-3:内存服务(Cap::Page,纯服务授权)—— 本轮

M3-2 已交付 uart_server 服务化(打印/读取走 IPC)。阶段 3 的第二个服务 = **内存服务**:
把物理页所有权能力化为 `Cap::Page`,并让用户态 **mem_server** 成为发页的唯一入口
(纯服务授权,用户拍板):内核**不暴露**通用分配 syscall(避免 ambient 授权),客户端
只能经 mem_server 的 IPC 申请/释放页;mem_server 创建后由引导编排(当前即测试)
注入页池;客户端归还页经反向 `mem_grant` 送回服务,池可复用。最贴合
「一切皆能力」,与 M3-2 纯服务化一脉相承。

### 11.1 现状约束(设计前提)

- **无跨进程 cap 转移原语**:现有跨进程授槽仅 shm::mmap_share(Cap::Shm 双槽)与
  services::connect(Cap::Proc 双向),均属内核按白名单/服务表写对端槽。`Cap::Page`
  需要新的**受控移交**接口(mem_grant,号 13)。
- **发页内存安全约束**:`mmu::map_user_page` 强制 `page_in_range`(只映射 buddy 区)
  + 拒覆盖;授予页必须来自 `mem::alloc_pages`。`mmu::destroy_root` 对分配器 U 叶子
  会 `free_pages` → `Cap::Page` 必须在 `process::destroy` **revoke-before-
  destroy_root**(与 Cap::Shm 同纪律),否则同页 double-free。
- **类型分支非穷尽**:`ipc::send/recv` 对 `cap_target` 的 match 加 `Cap::Page` 后由
  编译器强制补 `WrongType`;`cap_duplicate` 当前泛型复制任意 Cap → 页须禁复制
  (单引用不变量)。
- **无公开分配 syscall(设计选择)**:发页只经 mem_server 的 IPC;内核 `pages::alloc`
  仅由引导编排(测试 T1/T2)在 spawn 后注入池。

### 11.2 `Cap::Page` + 页注册表(`kernel/src/pages.rs`)

- `Cap::Page(usize)`:1 cap = 1 个物理页(4KB),**单引用**(禁 dup)。`Cap` 枚举加变体。
- `PageRegistry { pages: Vec<PageRecord>, free: VecDeque<usize> }`,`MAX_PAGES=64`,
  `static PAGES: SpinLock`,`init()` 在 boot 期 `reserve(MAX_PAGES)`(仿 shm.rs)。
- `PageRecord { id, paddr, owner: pid, map_va: Option<usize> }`(id = 槽索引;revoke 后
  `paddr=0` 防陈旧,仿 SharedPage)。`owner` = 当前持有者(防御性不变量);`map_va` =
  单映射 VA(revoke 时定位 unmap)。
- 函数:`alloc(owner) -> Result<usize, usize>`(`alloc_pages(0)`,表满 → -ENOMEM +
  回滚 free_pages)、`revoke(id) -> Result<(), ()>`(free_pages + 失效 + 入 free 池)、
  `grant(id, to_pid)`(owner 移交)、`map(id, pid, va)`(map_va 置位,已映射 → -EEXIST)、
  `unmap(id, pid, va)`(map_va 清除)。
- 锁序:`TABLE → PAGES`(destroy 持 TABLE 取 PAGES,同 `TABLE → SHM`,不逆序)。

### 11.3 纯服务授权:页池由引导编排注入

- **无 `mem_alloc` syscall**。内核侧 `pages::alloc` 仅由引导编排(当前 = 测试 T1/T2)
  在 mem_server spawn 后经 `grant_typed_cap` 注入其 cap 表(池 = 槽 1..=4,4 页)。
- 正式 spawn/init 服务落地后改为引导期自动授予(登记 D33)。

### 11.4 `mem_grant`(号 13):受控跨进程页移交

- 签名:`mem_grant(a0=src_slot, a1=peer_slot, a2=dst_slot)`。
- 语义:**move** Cap::Page(调用方 src_slot → peer 的 dst_slot);清调用方 src_slot
  (单引用,防双持)。
- 门禁:调用方 src_slot 持 `Cap::Page(id)`(否则 -EACCES);peer_slot 持 `Cap::Proc(peer)`
  (否则 -EACCES)——**只能移交给已连接(持 Cap::Proc)的进程**,无 ambient 移交;页
  未映射(map_va None,否则 -EINVAL);dst_slot 空(否则 -EEXIST,防静默丢 cap);
  peer 存活。成功:`grant_typed_cap(peer, dst_slot, Cap::Page(id))` +
  `clear_cap(caller, src_slot)` + `PageRecord.owner = peer_pid`。
- 安全:接收方同意 = 其经 IPC 请求里声明的 dst_slot;发送方经 Cap::Proc 绑定对端。

### 11.5 `mem_map`(号 14)+ cap_revoke 兼任释放 + cap_dup 禁页

- `mem_map(a0=slot, a1=va)`:槽持 `Cap::Page(id)`;va 页对齐、`va < USER_VA_LIMIT`、
  未映射(map_va None,否则 -EEXIST);`mmu::map_user_page(root, va, page_paddr, 0xC7)`
  (U RW + 单地址 sfence;page_in_range 天然满足 → 分配器页);`pages::map` 记 map_va。
- **释放 = cap_revoke(号 6)扩展**:`Cap::Page(id)` → 若 map_va Some 先 `unmap_4k` →
  `pages::revoke`(free_pages + clear_cap)。unmap-without-free / remap 延后(D34)。
- **cap_duplicate(号 7)禁页**:复制 `Cap::Page` → -EINVAL(单引用不变量;Proc/Shm
  不受影响)。

### 11.6 process::destroy 钩子 + ipc 类型分支

- destroy 步骤 1(TABLE 锁内)收集本进程全部 `Cap::Page(id)`,锁外逐个 `pages::revoke`
  (unmap+free+清槽)**在 destroy_root 之前**(同 Cap::Shm 纪律,防 double-free)。
- `cap_revoke` 分派 match 加 `Cap::Page` 分支;`ipc::send/recv` 的 `cap_target` match
  加 `Ok(Cap::Page(_)) => WrongType`(编译器强制)。

### 11.7 memory_server 用户态服务 + 归还协议

- user 常量(lib.rs):`SERVICE_MEMORY=2`、`MEM_VA=0x7000_0000`(避开 ELF 0x4000_0000 /
  SHM 0x5000_0000 / UART 0x6000_0000)、`OP_ALLOC=0x04`、`OP_FREE=0x05`、
  `OP_REPLY_FLAG=0x80`、`CLIENT_PAGE_SLOT=3`(client 收页槽)、`SERVER_POOL_SLOTS=1..=4`。
- **memory_server**(bin):`sys_service_register(SERVICE_MEMORY)` → 循环 `ipc_recv(槽 0)`
  → 按 op:
  - `OP_ALLOC`[client_dst_slot]:选池内可用槽 i(持 Cap::Page)→ `mem_grant(i, peer=0,
    client_dst_slot)` → 池位空 → reply `[OK]`;无可用页 → `[-ENOMEM]`。
  - `OP_FREE`[client_page_slot]:选一个空池槽 r → reply `[OK, recv_slot=r]`;客户端随后
    `mem_grant(client_page_slot, peer=2, r)` 归还,池位回填(乐观置位,崩溃边缘 D35)。
  - 其它 op → `[PROTO_ERR]`。
- **mem_client**(bin):`service_connect(MEMORY_SERVICE_ID, CLIENT_IPC_SLOT=2,
  SERVER_ACCEPT_SLOT=0)` → send `[OP_ALLOC, 3]` → recv `[OK]` → `mem_map(3, MEM_VA)`
  → 写/读回校验 → send `[OP_FREE, 3]` → recv `[OK, recv_slot]` →
  `mem_grant(3, peer=2, recv_slot)` 归还 → 写 marker `0xC0DE_0000|argc`(置于 END,
  复用 ELF_MARKER_VA 0x4000_2000)→ `sys_exit`。

### 11.8 消息协议(5 字 IPC)

```
请求 = [op, arg1, 0, 0, 0]   op 0x04 ALLOC:arg1=client_dst_slot(收页槽)
                             op 0x05 FREE :arg1=client_page_slot(归还页槽)
回复 = [op|0x80, status, len, 0, 0]   status=0 成功 / 负 errno
      ALLOC 成功 → [0x84, 0, 0];FREE 成功 → [0x85, 0, recv_slot]
```

### 11.9 测试与 banner

- **T1(单核,boot_tests,协作式)** `boot_memory_service_test()`:spawn mem_server(ELF)→
  **注入页池**(`pages::alloc`×4 + `grant_typed_cap` 槽 1..=4)→ yield_ 至 accept-any
  recv 阻塞 → 断言 `services::lookup(SERVICE_MEMORY)` → spawn mem_client(marker 页)→
  轮询 marker(申请→映射→读写→归还完整往返)→ 负面用例 → 清理(kill_process server +
  destroy client + drain_reaper)+ `free_page_count` 复原(池 4 页 + 归还页全部回 buddy,
  无泄漏/无 double-free)。banner `M3-3 T1: memory service ok`。
- **T2(smp 阶段)** `smp_memory_ipc_test()`:mem_server 亲和副核 A、client 亲和副核 B,
  完整往返(D6 IPI 跨核唤醒);单核退化仍打 banner `M3-3 T2: cross-core mem IPC ok (N harts)`。
- 负面用例(内核直调):mem_grant 空 src_slot → -EACCES / 空 peer_slot → -EACCES /
  dst 槽占用 → -EEXIST / 页已映射 → -EINVAL;mem_map 空槽 → -EACCES / 未对齐 va →
  -EINVAL / va≥USER_VA_LIMIT → -EINVAL / 二次映射 → -EEXIST;cap_duplicate(页) →
  -EINVAL;cap_revoke 释放后页计数复原;destroy 钩子回收:页授给临时进程→destroy→页计数
  复原;register 重复 → -EEXIST。
- **新 banner 同步 6 处 grep**(AGENTS.md 纪律):Makefile test/smp/rva23 + ci.yml
  build/smp/rva23。

### 11.10 提交切分(每提交过五门禁 + 6 grep 一致)

1. `docs: M3-3 设计`(本节 + SYSCALLS 登记 13/14 + DEFERRED D33-35);
2. `feat: Cap::Page + 页注册表`(pages.rs + 变体 + destroy 钩子 + revoke/dup/ipc 分支);
3. `feat: mem_grant/mem_map(号 13/14)`(受控移交 + 映射);
4. `feat: memory_server 用户态服务`(memory_server + mem_client + build.rs 多 ELF +
   elf.rs 两 const + user lib helper + SERVICE_MEMORY);
5. `feat: M3-3 测试 + banner`(T1/T2 + 两 banner + 6 grep);
6. `docs: M3-3 收官`(报告 + ROADMAP 勾选)。

### 11.11 风险与遗留(登记 DEFERRED D33-35)

- **D33** 页池注入依赖引导编排(spawn 服务落地后改为引导期自动授予)。
- **D34** 页无 unmap-without-free / remap(revoke=释放;重定位需重分配)。
- **D35** OP_FREE 归还乐观置位:客户端归还前崩溃 → 池位空但服务误判可用
  (mem_grant 失败可优雅降级;服务崩溃恢复里程碑统一处理)。
- 页单引用(禁 dup):多引用/共享私有页延后(共享用 shm)。
- MAX_PAGES=64 / 服务池 4 页的有界配额。
- destroy 持 TABLE→PAGES 锁序,不破坏 TABLE→SERVICES→DEVICES→IPC→SCHED。

## 关联登记

- DEFERRED:M1(ELF 延至 M3)→ M3-1 落地;跨核 Running 线程回收/D20 内核栈
  守护页 → M3-1 落地;SCHED 缩放 / D1 / slab → 评估后延后(§7 移入)。
  M3-2 遗留(服务端 client 消亡恢复/并发 client/设备页特权与 revoke/uart_server
  崩溃看门狗)→ 登记 DEFERRED 待办(M3-3+ 触发)。
  M3-3 遗留(页池注入依赖引导编排/页无 unmap-without-free/归还乐观置位)→
  登记 DEFERRED 待办(D33-35,§11.11)。
- docs/DESIGN.md 已知限制(§):跨核 shootdown/内核栈守护页 → M3-1 消项;
  「物理页发配」能力化 → M3-3 落地(`Cap::Page`)。
- ROADMAP.md 阶段 3:ELF 加载器 M3-1 勾选;uart_server 服务化 M3-2 勾选;
  内存服务 M3-3 勾选(本轮)。
