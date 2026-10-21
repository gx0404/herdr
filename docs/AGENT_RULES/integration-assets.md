# integration-assets（agent 集成资产）

范围：`src/integration/**`、上游同步丢弃清单与检查器
（`scripts/upstream_sync_drop_*`，与 `detection.md` 并集）。

## 职责

`src/integration` 为每个受支持的编码 agent（claude、codex、kimi、opencode、pi）
生成或编辑其宿主配置（settings、MCP、hook、keybinding 等），使 agent 能被 herdr 驱动：
`registry.rs` 注册集成、`targets.rs` 定位写入点、`config_edit.rs`/`config_file.rs`
执行最小侵入编辑、`version.rs` 管理集成资产版本。

客户端设置的集成分区只消费 `integration.list` / `integration.install` 共享 API，
不在 chrome 写宿主配置。`client::shell::settings::ClientShellState::install_selected_integration`
的 Enter/主按钮只安装选中且需安装的行；批量安装必须走显式入口
`install_recommended_integrations`，不能把选中行不可用退化成安装全部。

## 会话洞察与状态权威

- 官方 harness 名单、来源标签、钩子权威与发现车道以
  `src/harness/registry.rs::HARNESSES` 为真源；五家窗格来源与只读的 ZCode 外部来源
  不能混为可聚焦/可发送输入的同一能力。
- 注意力上报补充权限问题或失败原因，不改变既有生命周期权威：Claude 仍由屏幕主导，
  Codex 保持既有部分钩子仲裁，Kimi/OpenCode/Pi 保持完整生命周期钩子。读取会话转录
  不得倒过来操纵屏幕 detector 或 PTY 状态。
- hook 必须 fail open，正文与选项有界；只绑定本次根会话/窗格，不让子会话或其他
  会话的错误冒充当前失败。用户主动中止不得伪装为权限阻塞。
- 同一回合的各通知通道使用 `src/server/attention.rs` 的统一决定。有错误洞察能力且
  已绑定会话的完成只等匹配读取或 `server::attention::COMPLETION_INSIGHT_WAIT`
  （1.5 秒）；新回合/身份替换使旧读取无效。结局已通知后到达的错误可保留详情但不重复
  发声或弹窗，不在 PTY/渲染路径同步等待。
- 资产测试只证明事件处理与契约执行；CLI 版本、安装/升级、真实会话信号和清理必须有
  独立 smoke 证据，缺工具或未实际运行时明确记 PENDING。

## 不变量

- **集成资产版本是相对最新 release tag 的迁移版本，不是 master 上的逐 commit
  计数器**：`HERDR_INTEGRATION_VERSION` 标记与对应 `*_INTEGRATION_VERSION` 常量，
  在两个 release 之间多次变更同一资产时只 bump 一次（从最新 release 中的版本起算）。
- 对宿主配置的编辑必须幂等且可回读：重复运行不叠加注入；备份/恢复路径遵循
  `src/platform` 的配置目录约定（见 `platform.md`）。Kimi 的 TOML 管理块标记按语法
  上下文识别，不匹配字符串内容；损坏或多义标记在配置与钩子文件产生任何副作用前拒绝，
  保留无关文本、注释与换行，错误不回显可能含凭据的配置正文。
- **只安装最低支持版本认识的钩子事件**：宿主按固定枚举校验钩子事件，不认识的事件名
  会让整份配置失效——Claude Code 2.1.101 之前忽略整个 `settings.json`（连同用户自己
  的设置，2.1.76 实测），Kimi Code CLI 判整份 `config.toml` 非法。新增事件前先查它最早
  出现的版本，不晚于集成的最低版本才能装，否则先提高最低版本（并同步文档）。最低版本在
  安装时用 `<agent> --version` 校验：版本过旧拒绝安装，探测不到只警告
  （`version.rs::enforce_agent_version`）。Claude Code 最低 2.1.33
  （`mod.rs::CLAUDE_MIN_VERSION`，事件版本表在 `claude_settings` 的测试里：`SubagentStop`
  1.0.41、`SessionStart` 1.0.62、`Notification` matcher 2.0.37、`SubagentStart` 2.0.43、
  `PermissionRequest` 2.0.45、`TaskCompleted` 2.1.33；未装的 `StopFailure` 2.1.78、
  `TaskCreated` 2.1.84）；Kimi Code CLI 最低 0.14.0（`mod.rs::KIMI_MIN_VERSION`，约束写在
  `KIMI_HOOK_EVENTS` 上，bun 测试守门）。
- Codex 最低 `0.150.0`（`mod.rs::CODEX_MIN_VERSION`），安装以 `codex --version`
  校验：过旧拒绝，无法判断只警告。规范钩子以 `targets.rs::codex_managed_hooks` 为准：
  `SessionStart→session`、`UserPromptSubmit→working`、`Stop→idle` 同步 10 秒，
  `Interrupt→idle` 同步 3 秒；`PermissionRequest→attention`、
  `SubagentStart/SubagentStop→activity` 均 10 秒且 `async: true`，各平台一致。
  重装原位升级已有条目、追加缺项；信任哈希包含 timeout/async，升级后须在 Codex
  下次启动的「Hooks need review」重新信任，信任检查不得漏掉 Interrupt。
- 资产内容（`src/integration/assets/**`，含 TS 与 JSON）是受控生成/冻结输入：
  修改后必须跑 `just integration-assets-test`（bun），语义变化同步
  `docs/next` 用户文档（见 `docs-pipeline.md`）。
- 新增 agent 集成时：注册表、检测 manifest（`detection.md`）、集成资产与文档
  四者一起评审，避免「能装但不能识别」或「能识别但不能装」。
- **官方 harness 仅六家**，名单只以 `src/harness/registry.rs::HARNESSES` 为准。
  五家 `pane_harness` 有宿主集成资产；ZCode 已是外部只读来源，无 pane，不支持
  prompt/focus，不在本目录落资产或新增 `IntegrationTarget` 变体。其余集成商的资产与
  注册分支已物理删除，不新增，同步上游时也不合入；丢弃路径、共享文件口径与
  `IntegrationTarget` 墓碑策略见 `README.md` 的「fork 已删除的集成」。
