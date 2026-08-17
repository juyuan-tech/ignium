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

## 已知安全模型(截至 M1)

- 单核、无用户态;已启用 Sv39 分页(身份映射,RAM 整体 RWX ——
  权限拆分列入 M1.5)
- 当前威胁面 = 引导链与自身代码缺陷;陷阱/异常路径为"诊断后停机",
  无恢复路径
- M2 引入用户态后,威胁模型将显著扩大,本页随架构演进持续更新
