# integration-assets（agent 集成资产）

范围：`src/integration/**`、上游同步丢弃清单与检查器
（`scripts/upstream_sync_drop_*`，与 `detection.md` 并集）。

## 职责

`src/integration` 为每个受支持的编码 agent（claude、codex、kimi、opencode、pi）
生成或编辑其宿主配置（settings、MCP、hook、keybinding 等），使 agent 能被 herdr 驱动：
`registry.rs` 注册集成、`targets.rs` 定位写入点、`config_edit.rs`/`config_file.rs`
执行最小侵入编辑、`version.rs` 管理集成资产版本。

## 不变量

- **集成资产版本是相对最新 release tag 的迁移版本，不是 master 上的逐 commit
  计数器**：`HERDR_INTEGRATION_VERSION` 标记与对应 `*_INTEGRATION_VERSION` 常量，
  在两个 release 之间多次变更同一资产时只 bump 一次（从最新 release 中的版本起算）。
- 对宿主配置的编辑必须幂等且可回读：重复运行不叠加注入；备份/恢复路径遵循
  `src/platform` 的配置目录约定（见 `platform.md`）。
- 资产内容（`src/integration/assets/**`，含 TS 与 JSON）是受控生成/冻结输入：
  修改后必须跑 `just integration-assets-test`（bun），语义变化同步
  `docs/next` 用户文档（见 `docs-pipeline.md`）。
- 新增 agent 集成时：注册表、检测 manifest（`detection.md`）、集成资产与文档
  四者一起评审，避免「能装但不能识别」或「能识别但不能装」。
- **官方集成仅六家**（claude、codex、kimi、zcode、pi、opencode；zcode 计划以
  外部 agent 来源形式支持，不在本目录落资产与 `IntegrationTarget` 变体）：其余
  集成商的资产与注册分支已物理删除，不新增，同步上游时也不合入；丢弃路径、共享
  文件口径与 `IntegrationTarget` 墓碑策略见 `README.md` 的「fork 已删除的集成」。
