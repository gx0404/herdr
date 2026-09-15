# product-agent-skill（面向用户的 agent 操作技能）

范围：`skills/herdr/**`、`distribution/agent-guide.md`。这是产品侧资产——教会
**使用 herdr 的**编码 agent 操作 herdr——与开发者工具面（`ai-tooling.md`）是
两个系统，不共享配置。

## skills/herdr/SKILL.md（上游 Docs 节语义）

- 该技能跟踪**最新 stable 发布**，因为无版本的 `npx skills add herdrdev/herdr
  --skill herdr -g` 从 `master` 安装它。
- feature 与 preview 工作不更新此文件；仅在 stable 发布准备期间评审更新，并随
  release commit 的 `Cargo.toml` 版本 bump 一起提交。preview 构建保留最新 stable
  技能。
- 修改时保持与 `distribution/agent-guide.md` 及 `docs/next` 的 agent 文档
  （agents、agent-automation、agent-skill 页面）口径一致：CLI 命令、socket API
  行为与环境变量以 `src/cli`、`src/api` 实现为准（`protocol-api.md`）。

## agent-guide.md

`distribution/agent-guide.md` 是随安装产物发布的 human/agent 上手指南；发布语义
（渠道、资产、何时同步）见 `release-channels.md`。内容变化属于用户可见行为变化，
需要同步 `docs/next` 文档（`docs-pipeline.md`）。
