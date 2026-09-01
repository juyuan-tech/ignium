# M3-2:uart_server 服务化 + 跨核 IPC 实测(2026-09-01)

日期:2026-09-01
阶段:M3 阶段 2(ROADMAP 阶段 3 用户态服务的第一步服务化)

## 1. 摘要

ROADMAP 阶段 3 的第一步服务化 = **uart_server 进程独占 UART**:打印走 IPC,内核
不再直碰 UART(M3-1 的 `sys_write(fd=1)→UART` 过渡占位按定案删除)。本轮同时
落地**内核服务注册表**(客户端如何找到 uart_server)与 **sys_read**(读取经服务),
并实测 B1 跨核 IPC(上轮收尾审查审计点名"跨核分支落地后实测")。

**用户已拍板三项决策**(AskUserQuestion):

1. **范围** = uart_server 服务化全套:设备页授予 + 服务注册 + sys_read +
   移除 sys_write 占位 + 跨核 IPC 实测;
2. **服务注册** = **内核服务注册表**(新 syscall 10/11);
3. **sys_write** = **彻底删除**(内核不再提供;用户打印经 IPC 到 uart_server)。

交付分 5 个提交(每提交过全量门禁):

| 提交 | 内容 |
|---|---|
| `docs: M3-2 设计` | M3-DESIGN §10 + SYSCALLS 登记(8/9 移除保留、10/11/12、EEXIST)+ DEFERRED D29-32 |
| `feat: 设备页授予 + destroy 修复` | device.rs + mmu `map_device_page` + `destroy_root` 非分配器 U 页不 free 修复 + syscall 12 + EEXIST |
| `feat: 内核服务注册表` | services.rs + syscall 10/11 + process destroy 钩子(锁序 TABLE→SERVICES 不破) |
| `feat: uart_server 服务化` | user crate 重构(lib + hello/uart_server 两 bin)+ build.rs 多 ELF + sys_write/read 移除 + hello 重写 |
| `feat: 跨核 IPC IPI + M3-2 测试` | `ipi_wake_cross_hart` 补 SBI IPI + T1/T2 测试 + 两 banner + 6 处 grep |

新增 syscall:`service_register`(号 10)、`service_connect`(号 11)、`map_device`
(号 12),新 errno `SYS_ERR_EEXIST = usize::MAX-6`;号 8/9 移除、号保留(登记
SYSCALLS.md)。

## 2. 交付内容(对照设计)

### D1 设备页授予(号 12 `map_device` + `kernel/src/device.rs`)

- 白名单:`dev_id=0 → board::uart_base()`,其它 `-EINVAL`;禁任意 paddr。
- 校验:va 页对齐、`va < USER_VA_LIMIT`、未映射(拒覆盖)、**未被其它进程 claim**(排他)。
- 新 mmu 接口 `map_device_page(root, vaddr, paddr, flags)`:与 `map_user_page` 同构,
  **但跳过 `page_in_range`**(Safety 注释:device.rs 白名单把关)。
- **`destroy_root` 耦合修复(必需)**:U 叶子无条件 `free_pages` 会 free 非分配器
  UART 页 → buddy 损坏;改为 `page_in_range` 检查后才 free(设备页只 unmap 不 free)。
- 锁序:`TABLE → DEVICES`,destroy 持 TABLE 取 DEVICES,不逆序。

### D2 内核服务注册表(号 10/11 + `kernel/src/services.rs`)

- `MAX_SERVICES=8`,`SERVICE_UART=1`;`service_register(id)`(服务自报),重复 → `-EEXIST`;
  `service_connect(id, client_slot, server_slot)` 查表(无 → `-ENOENT`)、自连 → `-EACCES`、
  槽越界 → `-EINVAL`,**双向授予** Cap::Proc(服务端无 Cap::Proc(客户端)则 recv 被拒)。
- destroy 钩子:步骤 2(TABLE 锁内)`services::unregister_all_locked(pid)` +
  `device::release_all(pid)`;服务进程被杀 → 注册表自动清。

### D3 uart_server 用户程序 + SHM 打印路径(user crate 重构)

- user crate 包名 `ignium-user`:`[lib]`(syscall helper/常量/panic_handler/uart client 库)+
  两 bin(hello / uart_server)。kernel/build.rs 编译两 bin 拷 `hello.elf`/`uart_server.elf`
  供 `include_bytes!`。
- 固定常量(lib.rs 共享):`SERVER_ACCEPT_SLOT=0`、`SERVER_SHM_SLOT=1`、
  `CLIENT_IPC_SLOT=2`、`CLIENT_SHM_SLOT=3`、`UART_MMIO_VA=0x6000_0000`、
  `UART_SERVICE_ID=1`。
- **uart_server 主流程**:`map_device(0, UART_MMIO_VA)` → `service_register(1)` →
  循环 `ipc_recv(槽 0)` → 按 op 处理 → `ipc_send(槽 0, 回复)`。
- **打印路径**:client 写字节到 `SHM_VA` → `ipc_send({op=WRITE, len})` →
  uart_server 读 `SHM_VA[0..len]` 逐字节 TX(`\n→\r\n` 复刻旧语义)→ 回复 →
  client `ipc_recv`。
- 并发限制:accept 单槽 + SHM_VA 单页 → 本轮 1 并发 client(登记 D30)。

### D4 sys_read 落地 = 用户态库函数(非内核 syscall)

- lib.rs 提供 `uart_read(buf)`:send READ → recv reply → 从 SHM_VA 拷回。
- 无数据返回 nread=0(EOF 语义,确定性,CI 可测);真实键盘交互在 `make qemu`。
- 内核号 9 移除、登记"保留未用"。理由:读取是 fd 型兼容接口,属用户态库
  (POSIX 兼容零进内核),与"打印走 IPC"对称。

### D5 sys_write 移除

- 内核删 `sys_write` 处理器 + `MAX_WRITE_LEN`;号 8 登记"已移除,号保留"。
- user 侧 `SYS_WRITE` 删;hello 重写为:连接服务 → SHM 写 → IPC 打印。

### D6 跨核 IPC IPI 补强 + 实测

- `ipc_wake_with_msg/err`(Blocked 分支,enqueue 后)补 SBI IPI:目标线程已入其亲和核
  队列后,若目标核 idle 且非本核 → `send_ipi(1<<tgt, 0)`。判定与发 IPI 同在
  SCHED 锁临界区,与目标核"查空→wfi"临界区互斥 → 无丢失唤醒窗口(D19 同款)。
  失败仅 warn 一次,降级为 ≤1 tick 的定时器唤醒。

### D7 boot 编排与测试(T1/T2)

- **T1(单核,引导期协作式)**:spawn uart_server → yield 至 accept-any recv 阻塞 →
  断言 `services::lookup(SERVICE_UART)` → spawn hello(client)→ 轮询 marker
  (完整往返证明)→ 负面用例 → kill_process 收阻塞线程 + 页回收断言。banner
  `M3-2 T1: uart_server service ok`。
- **T2(跨核,irq_enable 后)**:uart_server 亲和副核 A、client 亲和副核 B,双向即时
  配对(D6 IPI 实测);单核退化为 (0,0) 仍打 banner。banner
  `M3-2 T2: cross-core IPC ok (N harts)`。
- 两 banner 各同步 **6 处 grep**:Makefile `test`/`test-smp`/`test-rva23` +
  ci.yml `build`/`smp`/`rva23` job(AGENTS.md 纪律)。

## 3. 实现中发现的设计缺口与修复

### F1(HIGH,设计缺口)空槽 recv 无法承载"服务端先阻塞" —— 引入 accept-any 语义

- **位置**:`kernel/src/ipc.rs`(`recv`/`send` 配对匹配)。
- **触发**:T1 要求 uart_server **先阻塞 recv、客户端后 connect**。原实现空槽
  recv 返回 `-EACCES`,服务端必须先拿到 `Cap::Proc(client)` 才能挂起 —— 但
  connect 双向授予正是要测的路径,鸡生蛋。
- **候选方案否决**:(a) 轮询+yield —— **无用户态 yield syscall**(syscall 表
  1-7、10-12),单核协作式 boot 测试下服务端会饿死空闲;(b) 预授权 —— 违背
  "测 service_connect"目的。
- **修复(accept-any)**:空槽 `ipc_recv` 改为**监听语义**:`src_pid = IPC_ACCEPT_ANY`
  (`usize::MAX`,真实 pid 从 1 起,永不碰撞)挂起阻塞;`send` 匹配 specific 优先
  (`src_pid == pid`)、通配兜底(`src_pid == IPC_ACCEPT_ANY`)。**安全性**:能向
  本进程 send 者必持 `Cap::Proc(本进程)`(仅 connect 或引导期授予可得)→ 接收方
  只收自己已授权的发送方,能力模型不破。recv 侧 accept-any 永不匹配既有 pending
  send(`sender_pid == ACCEPT_ANY` 恒假)→ 不吞特定消息,只登记监听。PIP 捐赠
  `src != ACCEPT_ANY` 时跳过(无特定期望发送方,捐赠目标未知)。

### F2(HIGH,构建缺口)build.rs 未跟踪 `CARGO_MANIFEST_DIR` 环境依赖 —— 跨环境 target/ 复用陈旧输出

- **位置**:`kernel/build.rs` + `user/build.rs`。
- **触发**:构建脚本输出含 `-T{CARGO_MANIFEST_DIR}/linker.ld` 的**绝对路径**,但未声明
  `cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR` → cargo 指纹不跟踪该 env。
  主机构建(路径 `/home/gxyarch/Code/ignium`)与 docker 容器构建(路径 `/work`)
  共享同一 `target/`(docker-make.sh bind mount)且 OUT_DIR 哈希路径无关 →
  **容器内复用主机的陈旧 build 脚本输出**,按主机路径找链接脚本 → 实测报
  `rust-lld: error: cannot find linker script /home/.../user/linker.ld`。
- **修复**:两处 build.rs 顶部加 `println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR")`
  —— 环境切换必重跑 build 脚本,输出恒指向真实路径。

### F3(MED,真 bug)purge_process 不杀自身挂起 recv —— pid 复用后陈旧监听错误命中

- **位置**:`kernel/src/ipc.rs`(`purge_process`)。
- **触发**:进程在 accept-any recv 阻塞中被杀,挂起的 pending recv 残留。此后若
  某新进程复用该 pid,其 `send` 会命中**已死进程的陈旧 recv** —— 消息投给死
  线程、发送方误报 Done,且 accept-any 监听永远无法清除。
- **修复**:`purge_process` 增步骤 4:`ipc.recvs.retain(|r| r.recver_pid != pid)`
  (含空槽 accept-any 监听;recver 即被杀进程,无需唤醒)。

### F4(约束)阻塞线程的进程销毁须走 `sched::kill_process`

- **位置**:`kernel/src/tests.rs`(T1/T2 清理)。
- **触发**:`process::destroy` 不处理阻塞线程(会遗留孤儿线程 + 陈旧 pending
  recv,内核栈泄漏)。
- **处置**:复用既有模式(smp_crosscore_test 同款)——`sched::kill_process(pid)`
  内部含 purge_process + 线程回收 + destroy。FAULT_KILL_COUNT 增量仅相对读取,无害。

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

1. uart_server 空槽 accept-any recv 阻塞(内核侧 yield 轮询 `is_blocked`);
2. `services::lookup(SERVICE_UART).is_some()`(注册表已登记);
3. client(hello)连接服务 → SHM 写 → IPC WRITE → uart_server 跨进程 TX(`\n→\r\n`)
   → 回复 → client 收回复写 marker;内核轮询 marker 到 `0xC0DE_0000 | 1`;
4. 负面用例全覆盖:
   - 重复 `register_service` → `-EEXIST`;
   - `connect(未注册 id)` → `-ENOENT`;
   - `connect(服务连自身)` → `-EACCES`;
   - 他进程 `map_device(UART)` → `-EEXIST`(排他 claim);
   - 非法 va / 未知 dev_id → `-EINVAL`;
5. 清理后 `free_page_count` 复原(设备页只 unmap 不 free,无 double-free,buddy 不坏)。

### T2 断言(跨核,irq_enable 后;n=4 用副核 1/2)

- uart_server 亲和副核 A、client 亲和副核 B(A≠B);client 的 send 经 **D6 IPI**
  唤醒副核 A 阻塞中的 uart_server,回复再经 D6 IPI 唤醒副核 B 阻塞中的 client
  —— **双向跨核即时配对**(非定时器轮询,验证 B1 跨核分支)。marker 轮询证明
  完整往返。

### 实测日志摘录

```
hello, ignium!
argc=1
...                                    (T1:uart_server 跨进程 TX ×1)
M3-2 T1: uart_server service ok
...                                    (T2:副核 A 服务端 TX ×1)
[000127] [INFO ] M3-2 T1: uart_server service ok
[000165] [INFO ] M3-2 T2: cross-core IPC ok (4 harts)
```

(smp 日志含 T1 单核协作式 + T2 跨核共 2 条 "hello, ignium!";uptime ≥ 2 条心跳;
无 m32t1/m32t2 断言失败;无 `KERNEL PANIC|TRAP:`。)

### 提交门禁纪律

6 处 grep 一致性核对:Makefile `test`/`test-smp`/`test-rva23` + ci.yml
`build`/`smp`/`rva23` 各自含两条新 banner 的 `grep -q`,缺一不可(已逐处核对)。
dev+release 双 profile 可编。

## 5. 遗留风险(登记 DEFERRED.md D29-32)

- **D29 服务端 client 消亡后的 recv 恢复**:陈旧 `Cap::Proc(client)` 令 uart_server
  的 recv 永久阻塞(无对端再配对)。`purge_process` 已可对阻塞 recv 投 -ENOENT;
  预留「cap_revoke + 重连协议」→ M3-3(服务崩溃恢复)。
- **D30 多并发 client**:accept 单槽 + SHM_VA 单页限制为 1 并发 client;需多
  SHM VA + 服务端槽池 → M3-3(ramfs/init-shell 多进程交互)。
- **D31 设备页能力化**:`Cap::Dev` + 手动 revoke + 特权授予;`map_device` 当前
  无特权校验(任何进程可 claim UART),M3-2 靠「uart_server 先 spawn 先 claim」
  引导序保证 → M3-3(设备驱动服务化)。
- **D32 uart_server 崩溃即 UART 卡死**:无看门狗/服务重启 → M3-3+。
- 服务端唤醒只发一次 IPI,失败降级为 ≤1 tick 定时器唤醒(正确性不破,已去重
  告警)。

---

**下阶段指向**:内存服务(cap 发页)→ ramfs → virtio-blk → spawn 服务化 + shell
(ROADMAP 阶段 3 剩余行)。`Cap::Page` 发页能力、多并发服务槽池随 M3-3 设计。
