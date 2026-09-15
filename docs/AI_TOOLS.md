# AI 工具接入与验证账

herdr 的 AI 协作面是「共享真源 + 薄适配器」：规则真源在 `docs/AGENT_RULES/`
（机器真源 `routes.toml` + resolver），危险模式真源在
`.claude/hooks/dangerous_patterns.conf`，各工具只做协议适配，不复制清单。

## 接入矩阵

| 工具 | 规则加载 | hooks | 验证状态 |
|---|---|---|---|
| ZCode | 根 AGENTS.md + `.zcode/config.json`；resolver 手动/入口提示 | PreToolUse/PostToolUse/Stop 复用 `.claude/hooks/*`（`timeoutMs` 毫秒） | 探针 PASS（`scripts/test_ai_tool_hooks`）；真实会话新开验证见下账 |
| Claude Code | `CLAUDE.md`（`@AGENTS.md` 薄入口）+ `.claude/rules/*.md`（`paths:` 提醒） | `.claude/settings.json` → `block_dangerous.sh` / `record_edit.sh` / `notify_review.sh` | 探针 PASS；真实会话 PENDING（见下） |
| Codex | 根 AGENTS.md（`project_doc_max_bytes=32768`）；reviewer 用 resolver | `.codex/hooks/pre_tool_use_policy.py`（复用 gate，codex 协议）；Stop 复用 notify_review | 探针 PASS；真实会话 PENDING |
| Pi | `.pi/prompts/`（pre-release-audit） | 无 | 既有资产，未动 |
| 其他（kimi 等） | 读根 AGENTS.md + resolver | 无 | N/A（当前无接入需求） |

## 验证账（按会话逐项记账，未运行不写 PASS）

| 检查 | ZCode | Claude Code | Codex |
|---|---|---|---|
| 配置文件可解析 | PASS（JSON 探针加载） | PASS（settings.json JSON 语法） | PASS（2026-09 实测 codex v0.154.0 启动解析；hooks 为数组表语法，`scripts.test_ai_tool_hooks.CodexConfigShapeTests` 锁定形状） |
| 规则加载（新会话读 AGENTS.md） | PASS（本会话即按其执行） | PENDING | PENDING |
| PreToolUse 拒绝（`gh pr merge` 等） | PASS（探针 + bash wrapper 实测） | PASS（同一 wrapper） | PASS（codex 适配器实测） |
| PreToolUse 放行（常规命令/编辑） | PASS（探针矩阵） | PASS（同上） | PASS |
| PostToolUse 记录 / Stop 信号 | PASS（临时仓库实测） | PASS（同脚本） | PASS |
| 真实客户端会话内 hook 触发 | PENDING：下次 ZCode 会话执行 `gh pr merge` 类命令观察拒绝 | PENDING | PENDING：配置解析已实测通过（v0.154.0），TUI 会话内探针命令待跑 |

PENDING 补验方式：在对应工具的真实会话中尝试探针命令
（`gh pr merge 1`、编辑 `distribution/latest.json`），确认被拒并显示理由；
完成后把上行改为 PASS 并注明日期。

## 维护

- 改危险模式：编辑 `.claude/hooks/dangerous_patterns.conf`（TSV 四列），跑
  `python3 -m unittest scripts.test_ai_tool_hooks`，并在真实会话演练允许与拒绝
  两侧；PostToolUse 不得静默改源码。
- 改规则/路由：见 `docs/AGENT_RULES/README.md` 维护节；工具面零改动。
- 个人状态（`settings.local.json`、`review/`、`sessions/`、`.codex/state/`、
  `.zcode/plans/`）不入库；共享文件保持无凭据。
- hooks 是第二道安全门，不是规则加载器；权限白名单在 `.claude/settings.json`，
  两者语义不得互相替代。
