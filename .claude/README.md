# .claude/

共享入库的 Claude Code 项目面。个人状态（`settings.local.json`、`review/`、
`sessions/`、`live-edits.log`）由根 `.gitignore` 保持本机。

- `settings.json`：权限与 hooks。hooks 是安全门，不是规则加载器；命令经
  `git rev-parse --show-toplevel` 定位仓库根，ZCode 侧复用同一批脚本。
- `rules/*.md`：带 `paths:` frontmatter 的薄适配，只提醒运行
  `python3 scripts/resolve_agent_rules.py <paths...>`，不复制领域规则。
- `hooks/`：`dangerous_patterns.conf` 是危险模式唯一真源（SHELL/FILE 两段，
  TSV 四列），`pre_tool_use_gate.py` 是判定逻辑，`.sh` 是各协议适配器；
  `record_edit.py` 记录编辑（PostToolUse），`notify_review.py` 写
  `review/READY.json`（Stop，跨工具复审信号）。
- 修改模式/脚本后运行：`python3 -m unittest scripts.test_ai_tool_hooks`。

接入与逐工具验证账见 `docs/AI_TOOLS.md`。
