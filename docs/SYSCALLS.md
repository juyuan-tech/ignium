# 系统调用 ABI(L1,唯一来源)

> **L1 syscall ABI 唯一事实来源**。与 `kernel/src/syscall.rs` 常量一一对应;
> 新增/修改号必须同步本表 + `kernel/src/syscall.rs` + `docs/M3-DESIGN.md` §5。
> ABI 形态对齐 LiteOS-A 风格(M2-DESIGN §4.1):a7=号、a0-a5=参数、
> a0=结果、错误以负 errno 的 usize 编码返回(不阻塞)。

## 寄存器约定

```
a7     = syscall 号
a0-a5  = 参数(按各调用定义)
返回   : a0 = 结果(成功值或负 errno 编码);a1-a5 按调用可携带附加消息
错误   : 负 errno,编码为 usize::MAX 附近(见 §错误码)
```

## 系统调用表(现有 1-7 + M3 系列 8-14)

| 号 | 名称 | 入参 | 返回 | 状态 |
|---|---|---|---|---|
| 1 | `sys_exit` | — | 不返回(线程退出) | ✅ M2 T1 |
| 2 | `sys_get_ticks` | — | a0 = 当前 tick | ✅ M2 T1 |
| 3 | `ipc_send` | a0 = 目标进程 cap 槽;a1-a5 = 消息 5 字 | 成功 a0=0;配对前阻塞 | ✅ M2 T2a |
| 4 | `ipc_recv` | a0 = 源进程 cap 槽 | 成功 a0=0、a1-a5 = 消息;配对前阻塞 | ✅ M2 T2a |
| 5 | `shm_map` | a0 = 本槽, a1 = 对端槽, a2 = len(页数×4096) | 成功 a0 = shm_id;失败负 errno | ✅ M2 T3c |
| 6 | `cap_revoke` | a0 = 槽 | 成功 a0=0;失败负 errno | ✅ M2 T3c |
| 7 | `cap_dup` | a0 = 源槽, a1 = 目标槽 | 成功 a0=0;失败负 errno | ✅ M2 T3c |
| 8 | ~~`sys_write`~~ | — | — | **M3-2 移除,号保留**(打印经 IPC 到 uart_server) |
| 9 | ~~`sys_read`~~ | — | — | **M3-2 移除,号保留**(读取经用户态库 `uart_read`) |
| 10 | `service_register` | a0 = 服务 id | 成功 a0=0;失败负 errno | **M3-2 新增** |
| 11 | `service_connect` | a0 = id, a1 = client 槽, a2 = server 槽 | 成功 a0=0;失败负 errno | **M3-2 新增** |
| 12 | `map_device` | a0 = dev_id, a1 = va | 成功 a0=0;失败负 errno | **M3-2 新增** |
| 13 | `mem_grant` | a0 = 源槽, a1 = peer 槽, a2 = 对端目标槽 | 成功 a0=0;失败负 errno | **M3-3 新增** |
| 14 | `mem_map` | a0 = 槽, a1 = va | 成功 a0=0;失败负 errno | **M3-3 新增** |

## 错误码(负 errno 的 usize 编码)

| 常量 | 值 | 语义 |
|---|---|---|
| `SYS_ERR_EINVAL` | `usize::MAX` | 参数非法(槽越界、非法 len、非法服务 id/dev_id/va) |
| `SYS_ERR_EACCES` | `usize::MAX - 1` | 未授权(空槽/非目标 cap/服务连接自身) |
| `SYS_ERR_ENOENT` | `usize::MAX - 2` | 不存在(服务 id 未注册) |
| `SYS_ERR_ENOMEM` | `usize::MAX - 3` | 内存不足 |
| `SYS_ERR_EBADF` | `usize::MAX - 4` | ~~非法 fd~~(**M3-2 随 sys_write 移除**,号值保留不复用) |
| `SYS_ERR_EFAULT` | `usize::MAX - 5` | ~~缓冲越界/不可访问~~(**M3-2 随 sys_write 移除**,号值保留不复用) |
| `SYS_ERR_EEXIST` | `usize::MAX - 6` | **M3-2 新增**:服务 id 已注册 / 设备已被他进程 claim |
| `-ENOSYS`(未知号) | `usize::MAX` | 未定义 syscall 号 |

> 注:`-ENOSYS` 与 `-EINVAL` 同编码(usize::MAX),语义靠上下文区分:
> 未知号返回它;已定义调用返回 EINVAL。若未来需要区分,移出统一常量区,
> 单独定义。

## sys_write 语义(号 8,**M3-2 已移除,号保留**)

- M3-1 过渡占位(`fd=1 → uart::write_bytes`)已按设计定案删除;内核不再直碰 UART。
- 用户打印一律走 IPC 到 uart_server(见 M3-DESIGN §10.4)。号 8 禁止挪用给其它调用。

## sys_read 语义(号 9,**M3-2 已移除,号保留**)

- 读取改为**用户态库函数** `uart_read(buf)`:IPC 到 uart_server 请求 READ,从 SHM_VA
  拷回 buf(见 M3-DESIGN §10.5;`kernel/src/user-lib` 侧见 user crate `lib.rs`)。
- 设计理由:fd 型读取属用户态兼容层(铁律 2),内核只认 IPC 原语;未来 ramfs 读取
  同样走服务 IPC,内核无需 fd 命名空间。号 9 禁止挪用给其它调用。

## service_register 语义(号 10,M3-2 新增)

- a0 = 服务 id(1..=7 合法;0 保留;`SERVICE_UART=1`)。
- 服务进程自报注册;id 越界 → `-EINVAL`;id 已占用 → `-EEXIST`;成功 a0=0。
- 服务进程被销毁(含被杀)时内核自动注销。

## service_connect 语义(号 11,M3-2 新增)

- a0 = 服务 id, a1 = client 槽, a2 = server 槽。
- 双向授予:client 槽写 `Cap::Proc(server)`,server 槽写 `Cap::Proc(client)`
  (服务端须持 Cap::Proc(client) 才能 `ipc_recv`)。id 未注册 → `-ENOENT`;
  server == client → `-EACCES`;槽越界 → `-EINVAL`;成功 a0=0。

## map_device 语义(号 12,M3-2 新增)

- a0 = dev_id(0 = UART → `board::uart_base()`), a1 = va(页对齐,< USER_VA_LIMIT)。
- 把白名单设备 MMIO 页以 U 位映射进调用进程根表(排他 claim,生命周期随进程)。
  dev_id 未知 → `-EINVAL`;va 非法/被占 → `-EINVAL`;设备已 claim → `-EEXIST`;
  成功 a0=0。uart_server 用 `map_device(0, 0x6000_0000)` 独占 UART。

## mem_grant 语义(号 13,M3-3 新增)

- a0 = 源槽(调用方持 `Cap::Page`), a1 = peer 槽(调用方持 `Cap::Proc(peer)`),
  a2 = 对端目标槽。
- **move** Cap::Page:调用方 src_slot → peer 的 dst_slot,并清调用方 src_slot
  (单引用,防双持)。src_slot 空/非 Page → `-EACCES`;peer_slot 空/非 Proc → `-EACCES`;
  页已映射 → `-EINVAL`;dst_slot 非空 → `-EEXIST`;peer 已亡 → `-EINVAL`;成功 a0=0。
- 只允许移交给已连接(持 Cap::Proc)的进程,无 ambient 移交。见 M3-DESIGN §11.4。

## mem_map 语义(号 14,M3-3 新增)

- a0 = 槽(调用方持 `Cap::Page`), a1 = va(页对齐,< USER_VA_LIMIT)。
- 把页以 U RW(0xC7)映射进调用进程根表并记 map_va。槽空/非 Page → `-EACCES`;
  va 未对齐或 ≥ USER_VA_LIMIT → `-EINVAL`;页已映射 → `-EEXIST`;成功 a0=0。
- 释放 = `cap_revoke`(号 6)扩展(先 unmap 再 free)。见 M3-DESIGN §11.5。

## mem 服务消息协议(用户态,5 字 IPC)

请求 `[op, arg1, 0, 0, 0]`:op 0x04 ALLOC(arg1=client_dst_slot,收页槽)/ 0x05 FREE
(arg1=client_page_slot,归还页槽)。回复 `[op|0x80, status, recv_slot, 0, 0]`
(ALLOC 成功 recv_slot=0;FREE 成功 recv_slot = 服务端归还接收槽)。详见
M3-DESIGN §11.8。

## uart 服务消息协议(用户态,SHM_VA + 5 字 IPC)

请求 `[op, arg1, 0, 0, 0]`:op 0x01 WRITE(arg1=len,数据在 SHM_VA[0..len])/
0x02 READ(arg1=max_len)/ 0x03 PING。回复 `[op|0x80, status, len, 0, 0]`
(status=0 成功 / 负 errno)。详见 M3-DESIGN §10.8。

## 实现与登记纪律

- 常量唯一来源:`kernel/src/syscall.rs`(号 + errno);本表同步。
- 用户侧(user crate/测试程序)引用号常量时注释"须与 kernel 一致",
  禁止在用户侧复制定义(避免漂移)。
- 新增/改号:先更新本表 + `kernel/src/syscall.rs`,再动实现。
