# docs-pipeline（文档快照与发布管线）

范围：`docs/**`、`README.md`、`README.zh-CN.md`、`scripts/docs/**`、翻译对齐与
config-reference 校验脚本。框架自身文档（AGENT_RULES、AI_TOOLS、kb、graphify）
的归属见 `ai-tooling.md`，物理上仍在 `docs/` 下。

## 目录职责（上游全文语义）

- `skills/herdr/SKILL.md` 跟踪最新 stable 发布（无版本 `npx skills add
  herdrdev/herdr --skill herdr -g` 从 master 安装）；feature/preview 工作不改它，
  只在 stable 发布准备时随版本 bump 一起改（见 `product-agent-skill.md`）。
- 未发布文档放 `docs/next/website/src/content/docs/`；用户可见行为变化在发布前
  需要文档时更新这里。`docs/next/README.md` 暂存根 README 变化；
  `docs/next/CHANGELOG.md` 在 stable 发布准备期间人工策展，日常 feature/fix 不维护。
- 活动 preview 文档在 `docs/preview/website/`：preview CI 拥有这份可变快照并与
  `distribution/preview.json` 原子提交，**严禁手改**；用
  `node scripts/docs/preview.mjs check` 校验。
- 已发布 stable 文档在 `docs/versions/`：release CI 从打 tag 的 `docs/next` 树
  播种每个版本；维护者事后可修正已发布版本中的事实性文档错误，同时把修正套用
  到 `docs/next`（若适用于未来版本）；不得用当前草稿替换已发布树。私有网站从
  `docs/versions/manifest.json` 选版本渲染；herdr 仓库是公开快照的真源。

## 硬性禁改项

日常 feature/fix 工作不编辑：根 `README.md`、根 `CHANGELOG.md`、
`docs/next/CHANGELOG.md`、已发布版本树、`distribution/latest.json`——除非是对
已发布文档的聚焦修正或明确要求。发布编号与 changelog 策略见
`release-channels.md`。

## 校验入口

- `just docs-contract-test`（bun test ./scripts/docs）：快照/版本生命周期工具。
- `scripts/docs_translation_parity.py`：翻译完整性；新增英文页必须带 zh-cn/ja
  翻译或显式登记豁免。
- `scripts/config_reference_check.py`：`docs/next/website/src/data/
  config-reference.json` 与 `src/config` 模型一致（生成物登记见 `docs/README.md`）。
- 网站内容只读 `docs/next/website`、`docs/preview/website`、`docs/versions`；
  在 `docs/` 顶层新增框架文档不影响这些脚本（已核实无 `docs/*` 全局 glob）。

## 变更范围纪律（上游补充条款）

- 本地 PRD、规划笔记与探索性 spec 放 `.local/prd/`；`.local/` 整体被 gitignore，
  由本地自行管理，不入仓。
- 刷新较旧的 pull request 时，先移除其 changelog-only diff（`docs/next/
  CHANGELOG.md` 的历史冲突源头）；该文件不由分支长期维护。commit subject 与
  `refs #` 盘点要求见 `release-channels.md`。
