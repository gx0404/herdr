# AGENT_RULES 使用与维护

本目录是 herdr 的 AI 领域规则库。`routes.toml` 是唯一机器真源；本 README 与根
AGENTS.md 的索引仅帮助定位，实际必读集合以路由器输出为准。

## 使用

```bash
# 任务开始时：对本轮全部触及路径解析必读规则（目录会展开，多路径取并集）
python3 scripts/resolve_agent_rules.py <path>...

# 审核任务追加 code-review 规则
python3 scripts/resolve_agent_rules.py --task review <path>...

# 机器可读输出
python3 scripts/resolve_agent_rules.py --json <path>...

# 闭集校验（体积上限、双射、零命中、覆盖、嵌套 AGENTS、重复正文）
python3 scripts/resolve_agent_rules.py --check
```

- scope 扩大后必须用完整集合重跑；resolver 无状态，不缓存上次结果。
- 未知路径 / 未登记任务 / 领域与 root_only 重叠都会以退出码 2 失败，这是有意设计。
- 尚未创建但已路由的文件可以直接传入，父目录 glob 会命中。

## 维护

- 新增仓库文件时同步 `routes.toml`：没有路由的文件会让 `--check` 失败；不想维护
  领域规则的文件才允许进 `root_only`（当前为空，不要用宽 glob 填补覆盖缺口）。
- 每份领域文档 ≤ 16384 bytes、非空、恰好登记一次；`README.md` 不参与闭集。
- 领域规则写不变量、真源符号（`模块路径::符号`）与验证方式，不钉行号，不复制
  其他文档的正文（≥160 字符的重复段落会被拒绝）；引用别的域用相对链接。
- 操作性数字只允许与源码符号同现（如 `PROTOCOL_VERSION` 的当前值），不裸写。
- `scripts/test_resolve_agent_rules.py` 是路由契约的行为测试，改 resolver 或
  routes 语义后必须保持全绿并按需补拒绝项。

## 与上游 herdrdev/herdr 的章节映射

本目录由上游单体 AGENTS.md（约 27 KiB）拆分而来。上游合并后若 AGENTS.md 冲突，
按下表把上游改动移植到对应文档（右侧为真源）：

| 上游章节 | 现在的真源 |
|---|---|
| Scope and Audience | 根 AGENTS.md「角色与授权分层」＋ `governance.md` |
| Universal Project Rules / Principles | 根「跨域硬边界」＋ `app-render.md`、`platform.md`、`detection.md` |
| Multiplicative performance paths | 根「跨域硬边界」摘要 ＋ `app-render.md` 全文 |
| Runtime/client boundary guardrail | 根摘要 ＋ `protocol-api.md` |
| Stable client endpoint contract | `protocol-api.md` |
| Maintainer Workflow | `governance.md` |
| Testing | `testing.md`（根保留一行入口） |
| Local Can Machine Workflow | `governance.md` |
| Agent Detection Updates | `detection.md` |
| Vendored libghostty-vt | `vendored-libghostty-vt.md` |
| Docs | `docs-pipeline.md` |
| Commit Style | 根 AGENTS.md「提交规范」 |
| Code Conventions | `project-infra.md`、`platform.md`、`integration-assets.md`、`protocol-api.md` |
| Release Channels | `release-channels.md` |
| External contributor guardrail | `governance.md` |

框架本身（路由、工具面、图谱、KB、文档集）是 fork 侧资产，上游没有对应物；
上游合并只可能冲突根 AGENTS.md 与既有业务文件。

### fork 已删除的集成

fork **照常同步上游**；本节只约束同步时怎样处理六家以外的集成商——它们的新增
与修改一律不合入（不是「fork 不再同步上游」）。

- **六家名单**：claude、codex、kimi、zcode、pi、opencode。zcode 此刻尚无任何
  文件，属保留名单：日后新增的 zcode 路径不进丢弃清单。
- **丢弃路径**：机器真源是 `scripts/upstream_sync_drop_paths.txt`（每行一个
  glob，语义同 `routes.toml`），覆盖四类整文件属于非六家的路径：
  - `src/integration/assets/<非六家>/**`
  - `src/detect/manifests/<非六家>.toml`
  - `distribution/agent-detection/<非六家>.toml`
  - 非六家专属的 `docs/next` 页面与脚本测试（当前 `docs/next` 没有专属页面，
    集成说明都在共享页面里，按下面的共享文件口径处理）
- **处理口径**：
  - 上游**新增**非六家集成：整块不合入（资产、清单、注册条目、文档、专属
    测试），并在同一次同步里把它的专属路径追加进清单。清单是按名字的拒绝表，
    不会自动发现新厂商，这一步靠人工识别。唯一要收的是冻结面上的改动，见下面
    的墓碑策略。
  - 上游**修改** fork 已删的文件（git 报 `deleted by us`）：保持删除。
  - 上游改**共享文件**里非六家的分支：丢弃该分支、保留其余。共享文件指同时
    承载六家与非六家分支的文件，典型如 `src/integration/registry.rs`、
    `actions.rs`、`targets.rs`、`src/cli/integration.rs`、`src/i18n/*`、
    `src/integration/tests.rs`、`src/detect/manifest/tests.rs`，以及
    `distribution/agent-detection/index.toml` 与 `docs/next` 的共享页面。
- **`IntegrationTarget` 墓碑策略**：
  `src/api/schema/integrations.rs::IntegrationTarget` 是 `integration.install` /
  `integration.uninstall` 的参数与 `integration.list` 的返回值，属 generation-1
  冻结面；非六家变体**不删、不重排**，只标退役（不可安装、不再列出）。
  - **理由**（都可复核）：① `protocol-api.md`「不得增删、重排或重新解释已发布
    codec 可达的字段与枚举变体」，该枚举经 `integration.install` 可达；②
    `tests/fixtures/endpoint-method-shapes-v1.json` 的 `integration.install` 摘要
    含枚举名单（`src/server/client_commands.rs::normalized_wire_schema` 不剥
    `enum` 数组），删变体即红，而 generation-1 期望永不重写；③ 生成物
    `docs/next/api/herdr-api.schema.json` 的 `$defs/IntegrationTarget` 随之变化；
    ④ 混版本链路真实存在：`src/remote/attach.rs::prepare_remote_herdr_ctx` 复用
    远端已有的 herdr，或经 `::remote_release_asset` 按 herdr.dev 的发布清单（仓库
    副本 `distribution/latest.json`，指向 `github.com/herdrdev/herdr` 的 release）
    装上游构建；枚举没有 `Unknown` 回退，fork 删掉的变体一旦出现在上游 server 的
    `integration.list` 返回里，fork 客户端就解析不出这份列表。枚举与 fixture 归
    上游维护，删了还是每次同步的永久冲突。该枚举按名字走 JSON（serde
    `snake_case`），不经 `src/protocol/wire.rs` 的 bincode，变体序号不是理由。
  - **退役的代码形态**（已落地）：变体留在枚举里，位置与 serde 名不动。
    `IntegrationTarget::ALL` 只列官方集成（`src/cli/spec.rs` 由它生成 `--target`
    取值），`IntegrationTarget::is_retired` 即「不在 `ALL` 里」，上游日后追加的
    变体因此自动退役；退役变体没有标签表，错误与日志用
    `IntegrationTarget::wire_name`（serde 名）指名。
    `src/integration/registry.rs::integration_specs`（`integration.list`、
    `herdr integration status` 与更新提示的来源）只登记官方集成，
    `::integration_target_supported` 对退役变体返回 false。
    `registry.rs::integration_target_label`、`::integration_target_command_names`
    与 `src/integration/actions.rs::install_target_inner`、`::uninstall_target`
    这 4 处 match 只为官方集成写显式 arm，其余一律落入 `_` 统一退役分支：
    install / uninstall 在入口返回 `registry.rs::retired_integration_error`
    （文案 `integration_target_retired_fmt`，en / zh_cn），不 panic、不落盘。
    `src/app/api/integrations.rs` 对旧客户端按名字传来的退役 target 回错误码
    `integration_retired`（照常反序列化）；
    `src/cli/integration.rs::parse_integration_target` 只接受 `ALL`，其余能解析
    成冻结变体的名字给退役提示。同步上游时丢弃它对这 4 处 match 新增的非六家
    arm 即可，`_` 分支保证仍然穷尽。
  - **上游日后追加非六家变体**：接收枚举那一行并让它落入退役分支（分支写成
    `_` 兜底则无需再改，写成显式 arm 则把新变体并进去），不进 `ALL`、
    `integration_specs` 与 `parse_integration_target`；其资产、注册、清单、
    文档、专属测试照旧丢弃。**一并接收**上游随附的
    `tests/fixtures/endpoint-method-shapes-v1.json` 中 `integration.install` 摘要
    更新，并按 `docs/README.md` 的生成物登记重建
    `docs/next/api/herdr-api.schema.json`。该摘要是枚举名单的函数（追加与删除
    同样改变它），不是厂商专属测试：fork 的枚举与上游逐字一致，上游的摘要对
    fork 同样成立；丢掉它，
    `advertised_client_shell_method_shapes_stay_at_the_v1_contract` 必红，而
    generation-1 期望归上游维护，fork 只跟随、不自行重写。上游若改用独立冻结
    （参照同一测试里 `pane.link.resolve` 的内联摘要与
    `tests/fixtures/endpoint-observability-shapes-v1.json`），跟随上游的做法。
  - **上游现行做法是不动冻结枚举**：letta 在上游走
    `src/cli/integration.rs::IntegrationCommandTarget` 与
    `src/integration/mod.rs::EXPERIMENTAL_INTEGRATION_TARGET_LABELS` 的 CLI-only
    通道。这类新增不触及冻结面，整块丢弃即可；fork 已把这条旁路整条删除，
    同步时这两个符号连同其调用点都不再合入。
- **检查**：同步后跑 `python3 scripts/upstream_sync_drop_check.py`——清单命中的
  路径仍是 Git 可见文件即退出码 1；`--list` 只列不判。物理删除已落地，该检查
  **已接入** `just maintenance-test`（因此在 `just ci` / `just check` 链上），
  上游同步把清单路径带回来会直接红。脚本逻辑自测
  `scripts/test_upstream_sync_drop_check.py` 同在 maintenance-test 清单内。清单
  里的注释记着删除时处理过的硬阻塞（grok 的 staged 例外，详见
  `release-channels.md` 的 fork 例外），同步上游时先读。
