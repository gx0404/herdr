# .codex/

共享入库的 Codex 项目面；`state/`、`sessions/`、`auth.json` 为本机状态
（根 `.gitignore`）。

- `config.toml`：审批策略（on-request）、workspace-write 沙箱、项目文档
  读取上限、agents 注册，以及 PreToolUse/PostToolUse/Stop hooks。**hooks
  语法以真实 Codex CLI 为准（v0.154.0 实测）**：事件键是数组表
  `[[hooks.PreToolUse]]`、命令嵌套在 `[[hooks.PreToolUse.hooks]]`；写成
  单表 `[hooks.PreToolUse]` 会让 Codex 启动即报 `invalid type: map,
  expected a sequence` 闪退。形状由 `scripts/test_ai_tool_hooks.py` 的
  `CodexConfigShapeTests` 锁定。
- `hooks/pre_tool_use_policy.py`：Codex 协议适配器，动态加载
  `.claude/hooks/pre_tool_use_gate.py`（策略真源 `dangerous_patterns.conf`
  只有一份）。FILE 段在 Codex 侧只覆盖 Bash 面（apply_patch 无
  file_path 字段）；敏感文件重定向写入已由 SHELL 段拦截。
- `agents/herdr_reviewer.toml`：只读复审 agent（注册键 `config_file`），
  供跨工具评审闭环使用。

修改 hook 后运行 `python3 -m unittest scripts.test_ai_tool_hooks` 演练
允许/拒绝探针；验证账见 `docs/AI_TOOLS.md`。
