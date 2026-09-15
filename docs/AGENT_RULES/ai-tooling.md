# ai-tooling（AI 工具面与框架自身）

范围：根 AGENTS.md/CLAUDE.md、`.claude/`、`.codex/`、`.zcode/`、`.agents/`、
`.pi/`、`.zed/`、`docs/AGENT_RULES/`、`docs/AI_TOOLS.md`、`docs/kb/`、
`docs/graphify/`、`graphify-out/` 及 resolver/KB/graphify 脚本。

## 规则加载协议

- `docs/AGENT_RULES/routes.toml` 是唯一机器真源；工具 rules、reviewer、CLAUDE.md
  只提醒运行 `python3 scripts/resolve_agent_rules.py <paths...>`，不复制业务清单。
- 根 AGENTS.md ≤ 16384 bytes、领域文档每份 ≤ 16384 bytes，由 `--check` 强制；
  超限时拆域或收敛文字，不放宽上限。
- 禁止在子目录新增 AGENTS.md（`vendor/` 下的上游文件除外）；工具适配层放
  `.claude/rules/*.md`（frontmatter `paths:` + 提醒），不放正文。

## 工具面边界

- 共享入库的是无凭据配置：settings、hooks、薄 rules、reviewer、README。个人
  状态（sessions、auth、`settings.local.json`、review 信号、`.zcode/plans/`）
  由 `.gitignore` 保持本机。
- hooks 是安全门，不是规则加载器：危险模式唯一真源是
  `.claude/hooks/dangerous_patterns.conf`，Claude/Codex/ZCode 各自的协议适配器
  只消费它；改模式后必须跑 `scripts/test_ai_tool_hooks.py` 探针并演练允许/拒绝。
- `.agents/skills/`（pre-release-audit、throwaway-repro、triage）与
  `.pi/prompts/` 是仓库工作流资产；内容与领域文档重复时以领域文档为准收编。
- `.zed/settings.json` 只放编辑器/rust-analyzer 调优，不放 AI 规则。
- 接入细节、逐工具验证账与 PENDING 项见 `docs/AI_TOOLS.md`。

## Graphify 纪律

- 只允许通过 `scripts/graphify.sh`（rebuild/check/query/path/explain/affected）
  使用图谱；索引根固定 `src/`，禁用增量 update，不把整份 graph.json 塞进上下文。
- 入库产物只有 `graphify-out/GRAPH_REPORT.md`、`.graphify_labels.json(+.sig)`、
  `source-fingerprint.json` 与 `docs/graphify/GRAPH_REPORT.md` 镜像；graph.json、
  HTML 视图、缓存在本机重建，`.graphify-memory/` 为个人经验层，不入库不入图。
- 源码变动后 `just graph-check` 必须能解释指纹差异；视图导出失败不得报告
  「视图交付完成」。

## 知识库纪律

- `docs/kb/chunks.json` 由 `scripts/build_agent_kb.py` 生成（确定性输出、默认
  check 模式，`--confirm` 才写入），禁止手改；语料闭集与分层见脚本头注释。
- 命中携带来源路径/anchor/hash；查询走 `scripts/agent_kb.py`（BM25-lite）。
- 检索质量由 `scripts/test_agent_kb.py` 的 golden 查询锁定；规则/文档变化后
  重建 KB 并让 freshness 检查通过，不能只改文档不刷新产物。
