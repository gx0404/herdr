# .codex/

共享入库的 Codex 项目面；`state/`、`sessions/`、`auth.json` 为本机状态
（根 `.gitignore`）。

- `config.toml`：审批策略 `never`、`danger-full-access` 沙箱模式、项目文档
  读取上限、agents 注册，以及 PreToolUse/PostToolUse/Stop hooks。**hooks
  语法以真实 Codex CLI 为准（v0.154.0 实测）**：事件键是数组表
  `[[hooks.PreToolUse]]`、命令嵌套在 `[[hooks.PreToolUse.hooks]]`；写成
  单表 `[hooks.PreToolUse]` 会让 Codex 启动即报 `invalid type: map,
  expected a sequence` 闪退。形状由 `scripts/test_ai_tool_hooks.py` 的
  `CodexConfigShapeTests` 锁定。
- `hooks/pre_tool_use_policy.py`：Codex 协议适配器，动态加载
  `.claude/hooks/pre_tool_use_gate.py`（策略真源 `dangerous_patterns.conf`
  只有一份）。PreToolUse 只注册 Bash，消费 SHELL 段；不覆盖 FILE 段、
  apply_patch、敏感文件重定向或任意脚本的间接访问。有限模式匹配不构成完整隔离。
- `agents/herdr_reviewer.toml`：只读复审 agent（注册键 `config_file`），
  供跨工具评审闭环使用。只读是职责约定，sandbox 与 approval 继承父会话，
  角色文件不能将它们覆盖为独立只读沙箱。

项目配置与非受管 hooks 需分别获得客户端信任；使用 CLI `/hooks` 检查来源、
当前定义及 hash 信任状态。新增或修改的 hook 未受信任时可能被跳过。
重新打开会话后再验证规则、角色发现及 hook 自动触发；脚本探针不证明这些已生效。
参见[当前子代理契约](https://learn.chatgpt.com/docs/agent-configuration/subagents)与
[hooks 契约](https://learn.chatgpt.com/docs/hooks)（2026-10-22 核对）。

修改 hook 后运行 `python3 -m unittest scripts.test_ai_tool_hooks` 演练
允许/拒绝探针；验证账见 `docs/AI_TOOLS.md`。
