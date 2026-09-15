# .codex/

共享入库的 Codex 项目面；`state/`、`sessions/`、`auth.json` 为本机状态
（根 `.gitignore`）。

- `config.toml`：审批策略（on-request）、workspace-write 沙箱、项目文档
  读取上限，以及 PreToolUse/Stop hooks。
- `hooks/pre_tool_use_policy.py`：Codex 协议适配器，动态加载
  `.claude/hooks/pre_tool_use_gate.py`（策略真源 `dangerous_patterns.conf`
  只有一份）。
- `agents/herdr_reviewer.toml`：只读复审 agent，供跨工具评审闭环使用。

修改 hook 后运行 `python3 -m unittest scripts.test_ai_tool_hooks` 演练
允许/拒绝探针；真实 Codex 会话的验证状态见 `docs/AI_TOOLS.md` 验证账。
