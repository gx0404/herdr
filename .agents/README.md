# .agents/

跨工具共享的仓库工作流资产。项目规范闭集 = 根 `AGENTS.md` +
`docs/AGENT_RULES/routes.toml` + 其清单列出的领域规则；任何工具面都不得复制
其中正文。

- `skills/herdr-pre-release-audit`：发布就绪审计（对比上个 release 以来的
  commit 与 changelog/docs）。
- `skills/herdr-throwaway-repro`：一次性命名会话，隔离复现 runtime/pane/PTY/
  API 行为（也用于检测 manifest 的证据采集）。
- `skills/triage`：GitHub issue 分诊为决策优先表。

用户级 skills 安装在 `~/.agents/skills/`，不属于本仓库。规则加载统一走
`python3 scripts/resolve_agent_rules.py <paths...>`。
