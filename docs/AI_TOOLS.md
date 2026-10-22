# AI 工具接入与验证账

herdr 的 AI 协作面是「共享真源 + 薄适配器」：规则真源在 `docs/AGENT_RULES/`
（机器真源 `routes.toml` + resolver），危险模式真源在
`.claude/hooks/dangerous_patterns.conf`，各工具只做协议适配，不复制清单。

## 接入矩阵

| 工具 | 规则加载 | hooks | 验证状态 |
|---|---|---|---|
| ZCode | 根 AGENTS.md；resolver 显式读取 | 当前版本忽略项目 hooks；可选手动安装见 `.zcode/README.md` | 项目 hooks N/A；用户安装与真实调用 PENDING |
| Claude Code | `CLAUDE.md`（`@AGENTS.md` 薄入口）+ `.claude/rules/*.md`（`paths:` 提醒） | `.claude/settings.json` → `block_dangerous.sh` / `record_edit.sh` / `notify_review.sh` | 分层证据见下账；真实会话 PENDING |
| Codex | 根 AGENTS.md（`project_doc_max_bytes=32768`）；reviewer 用 resolver | `.codex/hooks/pre_tool_use_policy.py`（复用 gate，codex 协议）；Stop 复用 notify_review | 分层证据见下账；真实会话 PENDING |
| Pi | `.pi/prompts/`（pre-release-audit） | 无 | 既有资产，未动 |
| 其他（kimi 等） | 读根 AGENTS.md + resolver | 无 | N/A（当前无接入需求） |

## 验证账（按会话逐项记账，未运行不写 PASS）

2026-10-18 曾运行配置解析、决策矩阵与原样注册命令探针；这些是受控子进程
证据，不代表已在 ZCode、Claude Code 或 Codex 客户端内开启新会话并验证自动触发。

2026-10-21 按用户授权收敛为「最小核心防线」：`dangerous_patterns.conf` 仅保留
force push、根/家目录 rm、凭据/私钥读取与写入四类 deny（根/家目录 rm 为本次新增，
原仓库未登记）；`.claude/settings.json` 的白名单、ask 级规则与 `Read(./.git/**)`
deny 已移除。当时同步重跑探针（`python3 -m unittest scripts.test_ai_tool_hooks`），
仍属受控子进程证据，真实客户端会话内的触发状态不变。

| 检查 | ZCode | Claude Code | Codex |
|---|---|---|---|
| 配置文件可解析 | N/A（移除不执行的项目 hooks 配置） | PASS（2026-10-23 JSON 解析） | PASS（2026-10-23 TOML 解析与形状检查）；原生项目配置加载 PENDING |
| 规则加载（客户端新会话自动读取） | PENDING（历史会话记录不代证本轮） | PENDING | PENDING |
| PreToolUse 拒绝（`git push --force` 等） | N/A（无项目注册） | PASS（2026-10-23 按 matcher 的脚本探针） | PASS（2026-10-23 Bash 注册入口脚本探针） |
| PreToolUse 放行（常规命令/编辑） | N/A（无项目注册） | PASS（2026-10-23 正反矩阵） | PASS（2026-10-23 Bash 正反矩阵） |
| PostToolUse 记录 / Stop 信号 | N/A（无项目注册） | PASS（2026-10-23 隔离脚本检查） | PASS（同一共享脚本与登记路径检查） |
| 真实客户端会话内 hook 触发 | PENDING（另行安装后验证，当前项目不提供） | PENDING | PENDING（先核对项目与 hook 信任） |

2026-10-23 修订接入边界：移除 ZCode 无效项目注册；注册入口测试按实际 matcher 选择
输入，不再向 Codex 的 Bash-only 注册注入 Edit 并据此声称 FILE 保护。保留五条最小
策略，仅纠正覆盖说明，不新增发布、PR、vendor 等已撤销的限制。

2026-10-23 主会话运行 `python -B scripts/setup_env.py --run python -B -m unittest scripts.test_ai_tool_hooks`：
26 项通过（6.402 秒）；以上 PASS 仅对应 JSON/TOML、匹配与协议探针、登记路径和隔离信号脚本。
Codex CLI 0.163.0 的隔离 `debug prompt-input` 已实际读到根规则，但未发现 reviewer，
破损项目 TOML 的负对照也未触发错误，因此该诊断不能证明项目配置已加载，角色发现保持
PENDING。该版本明确拒绝 `--strict-config` 与 `debug` 组合，此组合记 N/A，不作为通过证据。
真实客户端补验：另开会话检查规则与角色发现，在隔离测试仓库使用无副作用目标验证允许与
拒绝两侧（例如对隔离仓库运行只读 `git status`，以及仅指向本地裸仓库的强推探针）；
确认工具执行前拒绝及理由。不得用真实远程或敏感文件作为探针目标，直接 stdin 调用 hook
只能证明脚本行为。完成对应层验证后才改账为 PASS 并注明日期。

ZCode 的项目配置不执行，不能靠新开会话使其生效；仅保留共享脚本与手动接入说明，
本轮不改用户配置、不安装插件。[官方配置来源说明](https://zcode.z.ai/en/docs/hooks)
已于 2026-10-22 核对。

## 最小策略的覆盖边界

SHELL 段只匹配登记的 Bash 命令形状；FILE 段只检查支持的编辑工具路径字段。
Codex PreToolUse 当前只匹配 Bash，因此不消费 FILE 段，也不拦截 apply_patch。
例如 `echo placeholder > .env` 和写入私钥路径的重定向不会命中现有 SHELL 模式，
但 `Edit`/`Write` 传入相同路径会命中 FILE 模式。测试只把这些内容作为 JSON 字符串，
不执行命令、不读取或写入秘密文件。任意脚本、动态路径与其他 shell 形式均不能由这组
有限正则提供完整隔离。没有匹配表示 hook 未作决定，不等于任务授权或其他客户端权限允许。

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
启动参数仍须一致，已经启动的会话可能保留旧快照。`never` 控制是否询问；共享 hook
只匹配已登记的有限形状，配置值本身不替代 hook 探针或真实会话验证。
参见[官方配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)。

Codex 项目层须受信任；非受管 hook 还需通过 CLI `/hooks` 审阅当前定义并信任其 hash。
新建或修改的 hook 未受信任时会被跳过，脚本探针不涵盖这一步。reviewer 的只读职责是
提示词约定，sandbox 与 approval 继承父会话，不能靠角色文件创建独立只读沙箱。
当前 CLI 0.163.0 的 `--version`/`--help` 已核对；它们不证明配置已加载。
执行严格解析或 `debug prompt-input` 时应使用隔离无凭据配置，只保留项目规则与角色
发现的必要证据，不把完整真实 prompt 写入共享记录。
参见[官方子代理契约](https://learn.chatgpt.com/docs/agent-configuration/subagents)。

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
