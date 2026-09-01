# Security Policy

## 报告漏洞

请**不要**在公开 Issue 中报告未修复的漏洞。

- 首选:GitHub Private Vulnerability Reporting(仓库 Settings → Security → Advisories)
- 或发送邮件至项目维护者(见仓库主页)

## 范围内

- 内核代码(kernel/):内存安全、特权级、陷阱处理、日志/panic 路径
- 构建与发布链:CI 配置、链接脚本、工具链锁定
- 审计工具(scripts/):密钥处理方式

## 响应承诺

- 确认:48 小时内回复
- 修复:按严重程度(CRITICAL/HIGH/MEDIUM/LOW)排序,CRITICAL 优先
- 公开:修复发布后通过 GitHub Security Advisory 披露

## 已知安全模型(截至 M3 收官)

- **多核**(4 核 QEMU)+ **用户态 + 每进程独立地址空间**:Sv39 分页,
  U/S 权限隔离(用户区 U=1、内核区 U=0),段级权限拆分(代码 RX /
  只读数据 R / 可写数据 RW / 堆栈 RW),用户/内核栈守护页,用户页交接前
  清零(D10)。
- **能力模型**:进程间通信必须经能力授权(未授权 → `-errno`);共享页
  所有权 = 能力,revoke 撤双方映射 + **跨核 TLB shootdown**(M3 T2)。
- **用户态故障恢复(D12)**:用户进程触发页故障/非法指令等 → 杀进程
  (清 IPC 挂起 + 标记线程退出 + 撤捐赠 + 销毁地址空间页),**系统存活、
  其余进程不受影响**;内核态故障仍停机(属内核 bug)。
- **跨核停核 + 线程回收(M3 T2)**:被杀进程 Running 于其它核的线程经 IPI
  立即回收(栈→reaper、槽→free_slots、状态→Exited),不再依赖下次访存
  自愈;超时回退 D12 自愈路径。
- **内核线程栈守护页(M3 T3)**:内核线程栈 16KB 下 4KB 守卫页,溢出由"静默
  写坏堆"变为 S 模式页故障 fail-loudly(带 proc 内核线程的守卫为已知局限,
  见 docs/DEFERRED.md D27)。
- **已知局限**(详见 `docs/DESIGN.md`「已知限制」与 `docs/DEFERRED.md`):
  SCHED 全局锁(正确性优先,缩放评估后延 M4);ELF 加载器仅 M3-1 范围
  (无任意物理地址、PT_LOAD 全经内核校验)。
- **威胁面**:引导链、自身代码缺陷、以及经用户态可达的 syscall/能力
  错误路径(ELF 加载器为新增攻击面,逐段校验 + 溢出守卫);本页随架构
  演进持续更新。
