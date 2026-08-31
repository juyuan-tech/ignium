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

## 系统调用表(现有 1-7 + 新增 8/9)

| 号 | 名称 | 入参 | 返回 | 状态 |
|---|---|---|---|---|
| 1 | `sys_exit` | — | 不返回(线程退出) | ✅ M2 T1 |
| 2 | `sys_get_ticks` | — | a0 = 当前 tick | ✅ M2 T1 |
| 3 | `ipc_send` | a0 = 目标进程 cap 槽;a1-a5 = 消息 5 字 | 成功 a0=0;配对前阻塞 | ✅ M2 T2a |
| 4 | `ipc_recv` | a0 = 源进程 cap 槽 | 成功 a0=0、a1-a5 = 消息;配对前阻塞 | ✅ M2 T2a |
| 5 | `shm_map` | a0 = 本槽, a1 = 对端槽, a2 = len(页数×4096) | 成功 a0 = shm_id;失败负 errno | ✅ M2 T3c |
| 6 | `cap_revoke` | a0 = 槽 | 成功 a0=0;失败负 errno | ✅ M2 T3c |
| 7 | `cap_dup` | a0 = 源槽, a1 = 目标槽 | 成功 a0=0;失败负 errno | ✅ M2 T3c |
| 8 | `sys_write` | a0 = fd, a1 = buf, a2 = len | 成功 a0 = 写入字节数;失败负 errno | **M3-1 新增** |
| 9 | `sys_read` | a0 = fd, a1 = buf, a2 = len | 本轮返回 `-ENOSYS`(登记保留) | **M3-1 占位** |

## 错误码(负 errno 的 usize 编码)

| 常量 | 值 | 语义 |
|---|---|---|
| `SYS_ERR_EINVAL` | `usize::MAX` | 参数非法(槽越界、非法 len) |
| `SYS_ERR_EACCES` | `usize::MAX - 1` | 未授权(空槽/非目标 cap) |
| `SYS_ERR_ENOENT` | `usize::MAX - 2` | 不存在 |
| `SYS_ERR_ENOMEM` | `usize::MAX - 3` | 内存不足 |
| `SYS_ERR_EBADF` | `usize::MAX - 4` | 非法 fd(M3-1 新增,随 sys_write) |
| `SYS_ERR_EFAULT` | `usize::MAX - 5` | 缓冲越界/不可访问(M3-1 新增,随 sys_write) |
| `-ENOSYS`(未知号) | `usize::MAX` | 未定义 syscall 号;9 号 READ 本轮亦返回此值 |

> 注:`-ENOSYS` 与 `-EINVAL` 同编码(usize::MAX),语义靠上下文区分:
> 未知号与 READ 占位返回它;已定义调用返回 EINVAL。若未来需要区分,
> 移出统一常量区,单独定义。

## sys_write 语义(号 8,M3-1)

- `fd == 1`(stdout)→ `uart::write_bytes(buf)`;`fd != 1` → `-EBADF`。
- buf 校验:当前进程根表**逐页** `mmu::is_mapped`(防跨页越界),
  任一页未映射 → `-EFAULT`;`len > 4096` → `-EINVAL`。
- 返回 a0 = 实际写入字节数(= len)。sepc 前移 4。
- 过渡语义:`sys_write(fd=1)` → UART 为 M3-1 占位,M3-2 uart_server 落地后删除
  (见 M3-DESIGN §4)。

## sys_read 语义(号 9,M3-1 占位)

- 本轮始终返回 `-ENOSYS`(未实现);登记保留号,禁止挪用给其它调用。
- 实现放 M3-2(uart_server/ramfs 就绪后随服务定义)。

## 实现与登记纪律

- 常量唯一来源:`kernel/src/syscall.rs`(号 + errno);本表同步。
- 用户侧(user crate/测试程序)引用号常量时注释"须与 kernel 一致",
  禁止在用户侧复制定义(避免漂移)。
- 新增/改号:先更新本表 + `kernel/src/syscall.rs`,再动实现。
