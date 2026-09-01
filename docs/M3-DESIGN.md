# M3 设计:用户态服务 + L2 兼容(草案)

> 目标阶段:**M3**(用户态服务 + L2 兼容),承接 M2 收官(v0.1.0-M2)。
> 本文为 M3 设计基线,先文档后代码(遵循 DESIGN.md「先读透 seL4/rCore,
> 前 3 个月重文档轻代码」)。**M3-1(本轮)**:ELF 加载器 + 内核 `sys_write`
> 过渡占位 + 跨核 IPI 停核/Running 线程回收 + 跨核 TLB shootdown + 内核线程
> 栈守护页。M3-2 及以后:uart_server 服务化、内存服务、ramfs、virtio-blk、
> spawn/init/shell、musl/busybox(L2)、服务崩溃恢复。

## 1. 目标与验收(对齐 ROADMAP 阶段 3)

| 任务 | 验收 | 本轮(M3-1)处置 |
|---|---|---|
| uart_server 进程独占 UART,打印走 IPC | 内核不再直碰 UART | 定案:M3-1 内核 `sys_write(fd=1)` → UART 为**过渡占位**,标注 M3-2 uart_server 落地后删除(微内核"内核直碰 UART"临时例外,非兼容代码) |
| 内存服务:cap 发页 + IPC 申请/释放 | 用户进程可申请页 | 设计延后(M3-2);M3-1 不引入 `Cap::Page`,ELF 映射由内核加载器直接完成 |
| ramfs 文件系统服务(open/read/write/close) | IPC 客户端可读写删文件 | 延后 M3-2 |
| virtio-blk 驱动服务 + 持久文件系统 | 重启数据仍在 | 延后 M3-2 |
| spawn 服务化 + init 进程 + shell | shell 跑通 echo/cat 重定向 | 设计延后(M3-2);M3-1 只交付内核 `spawn_elf` 原语 |
| **musl 移植 + busybox 跑通(L2)** | busybox 常用命令可用 | 延后 M3-2 |
| 服务崩溃恢复:杀 FS 服务,系统存活可重启 | 故障注入测试通过 | 基础已具备(D12 杀进程 + 系统存活);跨核停核回收本轮补强(M3 T2) |

**M3-1 验收标准**:
1. Rust 编译的 `riscv64gc-unknown-none-elf` 用户 ELF 被内核加载、映射到用户
   地址空间、U 模式运行、回写结果(banner `M3 T1: ELF loader ok (user ELF ran)`)。
2. 被杀进程 Running 于其它核的线程**立即回收**(栈→reaper、槽→free_slots、
   状态→Exited),共享内存 revoke 跨核 TLB 失效(banner
   `M3 T2: cross-core kill/shootdown ok (N harts)`)。
3. 内核线程栈守护页落地(自检断言,不新增 banner)。
4. 五门禁全绿 + dev/release 双 profile 可编 + 三配置 log 无字面
   `KERNEL PANIC|TRAP:`。

## 2. 文档空白处置决策(每项给"补/延后"结论)

| 文档空白 | 处置 | 说明 |
|---|---|---|
| `docs/SYSCALLS.md` | **本轮新建** | L1 ABI 唯一来源,登记 1-7 现有号 + 8/9 新增号,与 `kernel/src/syscall.rs` 常量一一对应;9 号(READ)本轮返回 -ENOSYS |
| compat-baseline(对齐 LiteOS-A 的 POSIX 子集清单) | 延后 M3-2 | 等 uart_server/ramfs 服务定案后一起写,避免边写边改 |
| M3-DESIGN(本文) | **本轮新建** | 设计基线 |
| 服务注册机制(spawn 服务化) | 设计延后 M3-2 | M3-1 服务 = 独立进程,IPC 目标经 cap 槽解析 |
| `Cap::Page` 发页能力 | 设计延后 M3-2 | M3-1 ELF 映射由内核加载器直接完成 |
| spawn/init/shell 设计 | 设计延后 M3-2 | M3-1 只交付内核 `spawn_elf` 原语 |
| uart_server 过渡方案 | **本轮定案** | M3-1 内核 `sys_write(fd=1)` → UART 为过渡占位,M3-2 落地 uart_server 后删除 |
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
| **`sys_write`** | **8(新增)** | a0=fd, a1=buf, a2=len | 成功 a0=len / 负 errno |
| **`sys_read`** | **9(新增,占位)** | a0=fd, a1=buf, a2=len | 本轮返回 -ENOSYS(登记保留) |

**sys_write 语义(本轮)**:fd=1 → `uart::write_bytes(buf)`(NS16550,多核安全);
fd≠1 → `-EBADF`(新 `SYS_ERR_EBADF = MAX-4`)。**逐页**校验 buf 在当前进程根表
映射为**用户页**(`mmu::is_user_mapped`,限 U 位 —— 防跨页越界,亦防放行内核区
页泄漏内核内存),len 上限 4096,越界 → `-EFAULT`(新 `SYS_ERR_EFAULT = MAX-5`)。
拷贝须置 `sstatus.SUM=1`(S 模式直读 U 页;trap 恢复路径写回原值,临时置位
不泄漏;SIE=0 无抢占)—— M3-T1 实测首版缺 SUM 时 S 模式对 U 页立即
`scause=0xd`,此处为修复记录。

## 6. 跨核 IPI 停核 + Running 线程回收 + 跨核 TLB shootdown

### 6.1 问题(现状)

- `kill_current_process`(sched.rs:1180)对其它核 `state==Running` 线程
  `continue`(sched.rs:1208-1211):栈不回收 / TCB 槽不复用 / 状态不改 —— 泄漏,
  依赖其下次用户访存故障自愈。
- `mmu::tlb_flush()`(mmu.rs:542)只刷当前核;shm revoke 的 TLB 失效只对当前核
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

**SSIP handler 改造(riscv64.rs:534)**:
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

## 关联登记

- DEFERRED:M1(ELF 延至 M3)→ **本轮落地**;跨核 Running 线程回收/D20 内核栈
  守护页 → **本轮落地**;SCHED 缩放 / D1 / slab → 评估后延后(§7 移入)。
- docs/DESIGN.md 已知限制(§):跨核 shootdown/内核栈守护页 → 本轮消项。
- docs/ROADMAP.md 阶段 3:ELF 加载器 M3-1 勾选。
