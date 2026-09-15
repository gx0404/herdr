@AGENTS.md

CLAUDE.md 是薄入口：跨工具启动协议在根 `AGENTS.md`，领域规则只来自路由器。

- 任务开始与 scope 扩大后：`python3 scripts/resolve_agent_rules.py <paths...>`；
  审核任务加 `--task review`。`.claude/rules/` 只提醒运行 resolver，不复制规则。
- 工具接入、hooks 与共享/本机边界见 `docs/AI_TOOLS.md`；命令全表见
  `docs/MAKE_COMMANDS.md`；文档索引见 `docs/README.md`。
- `.claude/` 中入库的是团队共享配置；`settings.local.json`、`review/`、
  `sessions/` 是个人状态，保持不入库。
