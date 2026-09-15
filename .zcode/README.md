# .zcode/

ZCode 项目面：仅 hooks 配置（注意 ZCode 用毫秒 `timeoutMs`，不是 Claude 的秒）。
直接复用 `.claude/hooks/` 的共享脚本，经 `git rev-parse --show-toplevel` 定位
仓库根；协议兼容性以 `scripts/test_ai_tool_hooks.py` 探针为准，协议分叉时只加
适配不复制策略。`plans/`、`tmp/` 是本机状态（根 `.gitignore`）。
