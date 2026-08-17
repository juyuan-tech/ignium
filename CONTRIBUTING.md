# Contributing — 贡献指南

欢迎参与 Ignium(炬元微内核)开发。本指南是团队协作的入口文档。

## 起步

1. 在 WSL2 (Ubuntu 24.04) 配置环境(见 README「快速开始」)
2. 读 `docs/DESIGN.md`(架构铁律)与 `AGENTS.md`(执行规范)
3. 从 `ROADMAP.md` 的当前里程碑领任务(未列出的任务先开 Issue 讨论)

## 开发流程

```
1. 建分支:git checkout -b feat/<name>
2. 改代码(遵守 AGENTS.md 红线)
3. 本地验证:make clippy && make fmt && make test
4. 提交(信息格式:feat:/fix:/docs:/refactor:/ci: + 中文说明)
5. 开 PR:描述改动 + 验证结果截图/日志
6. CI 全绿后合并;里程碑完成打 tag + Release
```

## 质量门禁(合并前提)

- `make clippy` 零警告、`make fmt` 通过、`make test` PASS
- dev + release 双 profile 编译通过
- 外部 AI 审计发现的问题在 PR 中说明处置(修复或驳回理由)

## 规则

- **串行推进**:每步验收通过才进下一步(ROADMAP 阶段顺序)
- **接口对齐、代码自研**:可参考 LiteOS-A 的 POSIX 子集行为,不复制实现
- **注释是必需品**:模块文档、unsafe 的 Safety 说明、非显然决策的 why
- **小步提交**:一个逻辑改动一个提交,便于 review 与回滚
- **详尽报告**:每次修复/更新必须写 `docs/reports/<日期>-<主题>.md`
  并随提交入库(章节规范见 AGENTS.md「详尽报告规范」)

## 联系方式

- Issue:bug 报告/功能请求(使用模板)
- 讨论:GitHub Discussions
