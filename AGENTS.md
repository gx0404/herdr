# herdr

Terminal based agent runtime for coding agents.

本文件是跨工具启动协议（≤16 KiB）。领域规则正文在 `docs/AGENT_RULES/*.md`，
由路由器按路径解析；本文件的索引仅为导航，实际必读集合以路由器输出为准。

## 规则加载协议

任务开始时对本轮全部触及路径运行 resolver（目录自动展开为 Git 可见文件，多路径
取并集；scope 扩大后必须用完整集合重跑）：

```bash
python3 scripts/resolve_agent_rules.py <paths...>
python3 scripts/resolve_agent_rules.py --task review <paths...>  # 审核任务
python3 scripts/resolve_agent_rules.py --check                   # 闭集/体积守门
```

读完输出列出的每份文档再动手。未知路径/任务、路由缺失以退出码 2 失败是有意
设计。禁止在子目录新增 AGENTS.md；禁止把领域规则正文复制进工具私有目录。

## 语言与协作

- 与人类用其使用的语言交流；代码、标识符、commit message 保持英文。
- 规则真源唯一：领域规则只维护在 `docs/AGENT_RULES/`；工具适配层与 reviewer
  只引用 resolver，不复制清单。
- 先读代码再下结论；长期引用写 `模块路径::符号`，不钉行号。
- 完成前对照「完成标准」自检；不确定就报告，不猜测。

## 角色与授权分层

指令分层，完整判定条件与流程见 `docs/AGENT_RULES/governance.md`：

- **通用规则**适用所有在本仓库工作的 agent（包括 fork）。
- **维护者工作流 / Can 本机工作流**：仅在 `governance.md` 列出的账号与环境
  条件全部满足时适用，否则跳过。
- **外部贡献者守门**：对上游 `herdrdev/herdr` 开 issue/PR/推分支前必须按
  `governance.md` 核实身份（`.github/MAINTAINERS`、`APPROVED_CONTRIBUTORS`、
  `CONTRIBUTING.md`）；不满足条件时不得代为提交。
- 本仓库是 `gx0404/herdr` fork（upstream `herdrdev/herdr`）：日常开发按通用
  规则在 fork 分支进行；同步上游时按 `docs/AGENT_RULES/README.md` 的章节映射
  表移植 AGENTS.md 变更。

## 项目模型

- 单 crate Rust（`Cargo.toml` 是版本唯一真源）；TUI 客户端、server、CLI 一体；
  终端内核为 vendored libghostty-vt（Zig 0.16.0）。
- 状态分层：`AppState` 纯数据、`PaneState` 与 `PaneRuntime` 分离、Workspace
  逻辑无 PTY 可测（`app-render.md`、`persistence-session.md`）。
- 架构方向：server 拥有 runtime，TUI 是客户端之一；JSON API + 事件为共享面
  （`protocol-api.md`）。
- 平台行为隔离在 `src/platform/<os>.rs`（`platform.md`）；Windows 交叉编译与
  ConPTY 打包自成工具链。
- 工具链：`just` 是命令入口；测试用 cargo nextest + `python3 -m unittest` 脚本
  清单 + bun 契约测试。

## 常用命令

| 命令 | 用途 |
|---|---|
| `just test` / `just check` | 全量测试 / 格式+测试+Windows lint（提交前） |
| `just lint` / `just windows-lint` | 本地快速 lint / Windows 目标 lint |
| `just maintenance-test` | 维护脚本 unittest 清单 |
| `just docs-contract-test` / `just integration-assets-test` | bun 契约测试 |
| `just agent-rules` / `just framework-check` | resolver 用法 / 框架守门聚合 |
| `just graph` / `just graph-check` / `just kb` / `just kb-check` | 图谱与知识库 |
| `just bench-render-scale` / `just bench-release-smoke` | 渲染扩展 / 发布性能 smoke |
| `just release-docs-check` / `just pre-release-check` / `just release` | 发布链（维护者） |

全表与前置/副作用见 `docs/MAKE_COMMANDS.md`。

## 跨域硬边界

各条详情见括号内领域文档：

- **渲染纯函数**：`compute_view()` 处理几何与变更，`render()` 只取 `&AppState`
  画图；渲染期间禁止状态变更；无上帝对象（`app-render.md`）。
- **乘法性能路径**：视图计算/渲染/后台 pane 重排/PTY 解析/检测/帧扇出按
  频率×基数评估；pane 循环内用窄访问器、最短锁、保留 hidden-source 与
  retained-render 早退；加宽工作量需 1 与 ≥15 pane 的扩展证据（`app-render.md`）。
- **runtime/client 边界**：共享 runtime 事实进 server 并走 JSON API/事件；TUI
  呈现状态留在客户端；不新增私有 socket-only 行为；中性命名（`protocol-api.md`）。
- **稳定端点契约**：generation 1 codec 与方法不可变；冻结 fixture 是契约，
  永不重写期望；wire 变更按 `PROTOCOL_VERSION` 规则 bump（`protocol-api.md`）。
- **平台隔离**：核心模块不得出现 `#[cfg(target_os)]`；平台代码只进
  `src/platform/`（`platform.md`）。
- **Rust 纪律**：生产代码无 `unwrap()`；日志用 `tracing`；`#[allow]` 带理由；
  依赖克制（`project-infra.md`）。
- **检测解耦且基于证据**：detector 只读屏幕快照；manifest 改动走真实 pane
  证据流程（`detection.md`）。
- **文档纪律**：feature/fix 工作不编辑 `docs/next/CHANGELOG.md`、根
  `CHANGELOG.md`/`README.md`、已发布版本树与 `distribution/*.json`
  （`docs-pipeline.md`、`release-channels.md`）。
- **生成物纪律**：受控产物默认只检查，有意变更才重建并审 diff
  （`docs/README.md` 的生成物登记）。

## 提交规范

lowercase conventional commits，无 emoji，无 AI co-author 行；subject 有描述性
（进入 preview 发布说明）。与 GitHub issue 相关的正常 feature/fix commit 在
body 加 `refs #<编号>`，不用 `fixes/closes/resolves`（release CI 在发布后关闭
issue）：

```text
fix: handle pane focus

refs #82
```

`.githooks`（`just install-hooks`）与 CI 用 `scripts/conventional_commits.py`
强制。提交前先提出 commit message 并对齐；未经对齐不 commit/push。

## 领域规则索引

| 域 | 覆盖 |
|---|---|
| `ai-tooling` | AI 工具面、resolver、KB、graphify 自身 |
| `app-render` | `src/app`、`src/ui`、`src/pane` 状态与渲染热路径 |
| `code-review` | 审核输出格式（`--task review` 触发） |
| `detection` | agent 检测、manifest 与发布目录 |
| `docs-pipeline` | `docs/next|preview|versions`、README、翻译与契约 |
| `governance` | 角色分层、维护者工作流、贡献守门、`.github` |
| `integration-assets` | agent 宿主配置集成（`src/integration`） |
| `persistence-session` | 持久化、身份、恢复与 handoff |
| `platform` | 平台隔离、Windows 交叉编译与 ConPTY |
| `product-agent-skill` | `skills/herdr` 与 `distribution/agent-guide` |
| `project-infra` | 构建、config 模型、根级模块、代码约定 |
| `protocol-api` | 协议/API/server/client/CLI 与端点契约 |
| `release-channels` | 渠道、发布资产、CHANGELOG、版本真源 |
| `terminal-core` | 终端、PTY、输入、kitty graphics |
| `testing` | 测试分层、入口与纪律 |
| `vendored-libghostty-vt` | vendored 内核、补丁索引与更新流程 |

## 完成标准

- 规则、文档、生成物与代码同步；`just check`（或明确声明的更窄验证）通过，
  交付时报告实际运行的命令与结果。
- 区分 PASS / FAIL / PENDING / N/A：文件存在只是静态事实；未运行的验证记
  PENDING 并附补验命令，不写假绿；不适用的能力给出理由。
- 用户可见行为变化在发布前有 `docs/next` 文档。
- 交付说明包含：改动内容、真源、通过的检查、未运行项与剩余影响。

## 审核输出

固定四级：严重 / 中 / 轻 / 结论（可合入、需修改后复审、拒绝）。格式与纪律见
`docs/AGENT_RULES/code-review.md`。
