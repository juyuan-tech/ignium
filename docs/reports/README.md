# 报告索引(Report Index)

本目录收录**每次修复/更新的详尽报告**(AGENTS.md 纪律:每次落地必写)。
按日期 + 主题命名;最下方为最近报告。外部 AI 审计原始转储见
`../audit-reports/`(本目录报告是处置结论)。

## 里程碑 / 阶段报告

| 报告 | 摘要 |
|---|---|
| [2026-09-01-m3-4-ramfs.md](2026-09-01-m3-4-ramfs.md) | **M3-4:ramfs 文件系统服务(一切皆能力)**:纯用户态 ramfs_server,内核**零新 syscall/零新 Cap**;数据面 = 客户端自建 SHM 窗(Cap::Shm)、存储面 = mem_server 服务链(Cap::Page 经 IPC 申请);fd 绑定连接无全局命名空间;open/read/write/close/unlink 全链 + 跨核往返 + 客户端谓词槽位 bug 修复 |
| [2026-09-01-m3-3-memory-service.md](2026-09-01-m3-3-memory-service.md) | **M3-3:内存服务(Cap::Page 纯服务授权)**:`Cap::Page` 能力 + 页注册表 + mem_grant/mem_map(号 13/14)+ mem_server 用户态服务(内核不暴露分配 syscall,避免 ambient 授权)+ 客户端经 IPC 申请/映射/归还页 + T1/T2(含跨核 Cap::Page 移交) |
| [2026-09-01-m3-2-uart-server.md](2026-09-01-m3-2-uart-server.md) | **M3-2:uart_server 服务化**:设备页授予(号 12)+ 内核服务注册表(号 10/11)+ sys_write/read 移除(打印/读取走 IPC)+ 跨核 IPC IPI 实测(T1/T2);accept-any 空槽监听语义 + build.rs 跨环境缓存修复 |
| [2026-09-01-m3-winddown-audit.md](2026-09-01-m3-winddown-audit.md) | **M3 收尾全面审查 + 修复**:B1 跨核 IPC 唤醒竞态 / B2 sys_write 共享页 TOCTOU / B3 unmap 静默分配 / B4 ELF 段上界溢出 / sys_write 回绕 / P2 单地址 sfence + 文档体系规整 |
| [2026-09-01-m3-entry.md](2026-09-01-m3-entry.md) | **M3 入口**:ELF 加载器(M3 T1)+ 跨核 IPI 停核/Running 线程回收/跨核 TLB shootdown(M3 T2)+ 内核线程栈守护页(M3 T3) |
| [2026-08-20-m2-t1.md](2026-08-20-m2-t1.md) | **M2 T1**:用户态线程 + ecall 系统调用 |
| [2026-08-17-m1-complete.md](2026-08-17-m1-complete.md) | **M1 里程碑完成** |

## M2 阶段报告

| 报告 | 摘要 |
|---|---|
| [2026-08-28-m2-t15-addrspace.md](2026-08-28-m2-t15-addrspace.md) | 每进程独立地址空间 + 用户栈守护页(D20) |
| [2026-08-28-m2-t2a-ipc.md](2026-08-28-m2-t2a-ipc.md) | 同步 IPC + 寄存器消息 + 简化能力表 + D22 woken 抢占 |
| [2026-08-28-m2-t2b-pip.md](2026-08-28-m2-t2b-pip.md) | 优先级继承(PIP)+ IPC 压力测试 |
| [2026-08-28-m2-t3a-multicore.md](2026-08-28-m2-t3a-multicore.md) | 多核 bring-up(D7/D8/D9) |
| [2026-08-28-m2-t3b-percpu-sched.md](2026-08-28-m2-t3b-percpu-sched.md) | per-CPU 调度器(D19) |
| [2026-08-29-m2-t3c-sharedmem-cap.md](2026-08-29-m2-t3c-sharedmem-cap.md) | 共享内存(mmap_share)+ 能力 revoke/dup |
| [2026-08-29-m2-d12-recovery-perf.md](2026-08-29-m2-d12-recovery-perf.md) | 用户态异常恢复 + 进程销毁/页回收 + 安全性能 + 自审 |

## 外部审计处置(轮次)

| 报告 | 摘要 |
|---|---|
| [2026-08-17-m1-sv39-paging.md](2026-08-17-m1-sv39-paging.md) | 审计第 9 轮处置 + Sv39 分页实现 |
| [2026-08-17-audit-10.md](2026-08-17-audit-10.md) | 外部审计第 10 轮处置 |
| [2026-08-17-audit-11.md](2026-08-17-audit-11.md) | 外部审计第 11 轮 + 历史忽略项清理 |
| [2026-08-17-audit-12.md](2026-08-17-audit-12.md) | 外部审计第 12 轮处置 |
| [2026-08-17-audit-14.md](2026-08-17-audit-14.md) | 外部审计第 14 轮处置 |
| [2026-08-17-audit-15.md](2026-08-17-audit-15.md) | 外部审计第 15 轮处置 |
| [2026-08-17-audit-16.md](2026-08-17-audit-16.md) | 外部审计第 16 轮处置 |
| [2026-08-17-audit-16-proactive.md](2026-08-17-audit-16-proactive.md) | 审计 16 轮后的前瞻收敛(状态机自审 + 自检增强) |
| [2026-08-17-audit-17.md](2026-08-17-audit-17.md) | 审计 17 轮处置(deepseek-v4-pro 独立审计) |
| [2026-08-18-audit-18.md](2026-08-18-audit-18.md) | 审计 18 轮:全量代码审计 + 文档一致性(自审续) |
| [2026-08-17-m1-audit-disposition.md](2026-08-17-m1-audit-disposition.md) | M1 完成后的全量外部审计处置 |

## 深度自审 / 结构 / 加固

| 报告 | 摘要 |
|---|---|
| [2026-08-17-deep-self-audit-12.md](2026-08-17-deep-self-audit-12.md) | 深度自审(第 12 轮):多核引导仲裁 bug + 文档漂移清理 |
| [2026-08-28-project-structure-audit.md](2026-08-28-project-structure-audit.md) | 项目结构优化与全量自我审计 |
| [2026-08-28-docs-structure-roadmap.md](2026-08-28-docs-structure-roadmap.md) | 文档结构规整与路线图补全 |
| [2026-08-28-security-perf-hardening.md](2026-08-28-security-perf-hardening.md) | 安全/性能加固 |
| [2026-08-31-m2-slab-return-cleanup.md](2026-08-31-m2-slab-return-cleanup.md) | M2 待办清零:slab 空页懒回收 + 过期注释修正 + 全量检查 |

## M1 阶段报告(历史)

| 报告 | 摘要 |
|---|---|
| [2026-08-17-m1-heap.md](2026-08-17-m1-heap.md) | M1 内核堆(slab)实现 + bring-up 双 bug 修复 |
| [2026-08-17-m1-optimization.md](2026-08-17-m1-optimization.md) | M1 代码优化 + 初步自审 |
| [2026-08-17-m1-perf.md](2026-08-17-m1-perf.md) | M1 性能优化轮 |

## 纪律

- 新报告文件名格式:`YYYY-MM-DD-<主题>.md`(与既有系列编号一致)。
- 落笔范围:摘要 / 发现 / 修复 / 验证 / 遗留(见 AGENTS.md 报告规范)。
- 历史报告是审计记录,保留不删;本索引随新增报告同步更新。
