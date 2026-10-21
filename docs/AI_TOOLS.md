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

2026-10-18 本轮重新运行配置解析、决策矩阵与原样注册命令探针；这些是受控子进程
证据，不代表已在 ZCode、Claude Code 或 Codex 客户端内开启新会话并验证自动触发。

2026-10-21 按用户授权收敛为「最小核心防线」：`dangerous_patterns.conf` 仅保留
force push、根/家目录 rm、凭据/私钥读取与写入四类 deny（根/家目录 rm 为本次新增，
原仓库未登记）；`.claude/settings.json` 的白名单、ask 级规则与 `Read(./.git/**)`
deny 已移除。本轮同步重跑探针（`python3 -m unittest scripts.test_ai_tool_hooks`），
仍属受控子进程证据，真实客户端会话内的触发状态不变。

| 检查 | ZCode | Claude Code | Codex |
|---|---|---|---|
| 配置文件可解析 | PASS（本轮 JSON 探针加载） | PASS（本轮 settings.json JSON 解析） | PASS（本轮 TOML 与数组表形状探针；2026-09 的 v0.154.0 启动为历史证据） |
| 规则加载（客户端新会话自动读取） | PENDING（历史会话记录不代证本轮） | PENDING | PENDING |
| PreToolUse 拒绝（`git push --force` 等） | PASS（探针 + bash wrapper 实测） | PASS（同一 wrapper） | PASS（codex 适配器实测） |
| PreToolUse 放行（常规命令/编辑） | PASS（探针矩阵） | PASS（同上） | PASS |
| PostToolUse 记录 / Stop 信号 | PASS（临时仓库实测） | PASS（同脚本） | PASS |
| 真实客户端会话内 hook 触发 | PENDING：使用下述隔离探针补验 | PENDING | PENDING：真实会话自动触发待补验 |

PENDING 补验方式：在对应工具的真实会话中尝试探针命令
（通过隔离临时仓库与无副作用 hook 输入，不对真实 PR 或渠道文件执行危险动作），确认被拒并显示理由；
完成后把上行改为 PASS 并注明日期。

## 测试与性能命令（AI agent）

AI agent 默认通过 `just test` 进入固定默认五阶段回归；扩展分类见 `.config/test-suites.toml`。
完整回归使用 `just test-full`，测试框架修改验收使用 `just check-full`。默认目标热缓存 60 秒，
超过目标仍跑完，不把默认集或单个 phase 通过称为完整通过。
`HERDR_TEST_BUDGET`/`--test-budget` 控制 combined 默认预算（默认 `min(8, cpu_count)`）；
`HERDR_TEST_PHASE_JOBS`/`--phase-jobs`/`--jobs` 控制 phase 并发（默认
`min(3, 5, budget)`）；`HERDR_MAINTENANCE_JOBS`/`--maintenance-jobs` 控制 maintenance
（并发默认 `min(4, max(1, budget-1))`，串行默认 `min(4, budget)`）；
`HERDR_NEXTEST_JOBS`/`--nextest-jobs` 控制 Rust worker（并发为剩余预算，串行用 budget）。命令行优先于环境变量，显式值必须为正整数，编排器会把
解析后的 maintenance/nextest 值传给 phase 子进程。每个 run 的 `manifest.json` 原子记录
`run_id`、`status`、`manifest`、`test_budget`、`phase_jobs`、`maintenance_jobs`、
`nextest_jobs`、`failures`，以及每个 phase 的 `recipe`、`status`、`exit_code`、`seconds`、
`log`；证据路径是 `target/test-suite-logs/<run-id>/`。

`just nextest-all`、`just test-one` 是直接 nextest 入口，justfile 使用
`HERDR_NEXTEST_JOBS`，未设置时默认 4；直接 `run_parallel_unittest.py` 的 `--jobs` 与
`HERDR_MAINTENANCE_JOBS` 只影响 maintenance 执行器。`bench-release-smoke` 仅支持
Linux/macOS，且候选与 baseline 必须在 run 认领的短运行根中隔离 `HOME`、`USERPROFILE`、
`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`XDG_RUNTIME_DIR`、`XDG_DATA_HOME`、
`XDG_CACHE_HOME`、`APPDATA`、`LOCALAPPDATA`、`HERDR_HOME`、`CODEX_HOME`、
`KIMI_CODE_HOME`、`TMPDIR` 中运行；case 还设置 `TMUX_TMPDIR` 并清除继承的 herdr
socket/session。运行根为 receipt 关联的项目 `.local/p-*/` 0700 私有目录，case 独占
子目录并预检 socket 字节长度；仅清理归属匹配且已确认停止的临时态，清理不确定须失败。
长证据目录保留 metadata、命令、采样、summary、run log、exit-code 与清理前诊断 txt，
避免 AI 工具用户状态污染。

## 维护

Codex 项目配置以 `.codex/config.toml` 为准：当前设置 `approval_policy = "never"`、
`sandbox_mode = "danger-full-access"`、`project_doc_max_bytes = 32768`；用户级配置和
启动参数仍须一致，已经启动的会话可能保留旧快照。`never` 控制是否询问；危险操作仍由
共享 hook 策略拦截，配置值本身不替代 hook 探针或真实会话验证。
参见[官方配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)。

Codex 的 PreToolUse 响应采用 `hookSpecificOutput.permissionDecision`。
当前官方协议不支持 `ask`，因此共享策略里的 ask 模式（2026-10 起未登记，分叉
逻辑保留）在 Codex 适配中转为带原因的拒绝，防止无效响应被忽略后继续执行；
Claude/ZCode 仍保持原协议。
常规测试、建临时目录和读取画面不触发此门。协议探针为 PASS，真实客户端
重新加载后的权限状态仍需单独核验，不能把静态配置当成已生效的运行时。
参见[官方 hooks 协议](https://learn.chatgpt.com/docs/hooks)。

- 改危险模式：编辑 `.claude/hooks/dangerous_patterns.conf`（TSV 四列），跑
  `python3 -m unittest scripts.test_ai_tool_hooks`，并在真实会话演练允许与拒绝
  两侧；PostToolUse 不得静默改源码。
- 改规则/路由：见 `docs/AGENT_RULES/README.md` 维护节；工具面零改动。
- 个人状态（`settings.local.json`、`review/`、`sessions/`、`.codex/state/`、
  `.zcode/plans/`）不入库；共享文件保持无凭据。
- hooks 是第二道安全门，不是规则加载器；`.claude/settings.json` 自 2026-10
  起仅保留凭据读取 deny（白名单与 ask 级规则已按用户授权放开），两者语义
  不得互相替代。
