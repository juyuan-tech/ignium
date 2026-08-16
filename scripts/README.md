# scripts/ — 开发与质量工具

| 文件 | 用途 | 使用方式 |
|---|---|---|
| `ai_audit.py` | **独立 AI 审计**(防自审盲区):打包源码发给外部模型(默认 DeepSeek `deepseek-v4-pro`)审查,报告存 `docs/audit-reports/` | 见下方 |

## ai_audit.py

```bash
# 密钥二选一(绝不入库、不发给模型):
IGNIUM_AUDIT_KEY=sk-xxx python3 scripts/ai_audit.py     # 环境变量
python3 scripts/ai_audit.py --key-file /tmp/key.txt     # 密钥文件

# 密钥文件可长期保留(建议放仓库之外的路径,如 %TEMP%\opencode\audit-key.txt):
python3 scripts/ai_audit.py --key-file /mnt/c/Users/<user>/AppData/Local/Temp/opencode/audit-key.txt

# 可选配置(环境变量):
IGNIUM_AUDIT_MODEL=deepseek-v4-pro    # 默认;可换 deepseek-v4-flash
IGNIUM_AUDIT_URL=https://api.deepseek.com/chat/completions  # 换供应商
```

注意:

- 依赖:仅 Python 3 标准库(无第三方依赖);在 WSL2 中运行
- 审计报告按时间戳 + 模型名存档,建议对每条发现给出处置记录
  (修复/驳回+理由),再提交到仓库
- pro 模型默认开启 thinking 模式,输出 token 较大(约 50-60K),
  成本约为 flash 模型的 3 倍,单次审计成本约 $0.05 量级
