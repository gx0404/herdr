# release-channels（发布渠道与版本真源）

范围：`distribution/**`、`CHANGELOG.md`、`nix/**`、`flake.*`、`packaging/**`、
release/preview 工作流与脚本、`scripts/changelog.py`、`scripts/preview.py`、
`scripts/release.py`（含 `release-workflows.test.ts` 契约）、perf smoke 脚本。
**本域维护者动作（发布、推资产、改渠道文件）仅限核实过的维护者**
（`governance.md`）；日常 feature/fix 工作只读本域。

## 渠道模型（上游全文语义）

单一主分支 + 双更新渠道，无长期 release/preview 分支：常规 preview 从 `master`
选一个提交；stable 晋升一个**已发布的 preview**，从不取最新 `master`。普通用户
默认 stable：文档 `/docs/`、更新源 `distribution/latest.json`、Homebrew/Nix 仅
stable。preview 为直装用户 opt-in：

```bash
herdr channel set preview && herdr update   # 回 stable: herdr channel set stable
```

### preview

- `.github/workflows/preview.yml` 只由 `preview-*` tag 推送触发，产出 GitHub
  prerelease。`just preview <提交或 ref>`（默认 HEAD）校验源提交、创建带注释的
  `preview-<提交日期>-<短 sha>` tag 并推送。常规源提交必须可从 master 到达且已含
  tag 触发的 preview 工作流；只支持手动触发的旧修订不能靠打 tag 预览。
- 隔离 hotfix：从当前 stable tag 建临时 `release/<name>` 分支，只放评审过的修复，
  推送该分支后在其顶端跑 `just preview`。CI 校验的是被 tag 的提交而非移动分支；
  分支命名与祖先关系只防选错，不替代评审 hotfix diff。修复也必须进 master；
  hotfix 同样必须先 preview。基于旧 stable 的 hotfix 须先带上晋升工具链更新，
  CI 拒绝仍带旧版无门控 stable 工作流的候选。
- preview 说明只含构建号（日期 + 源 SHA）与对比链接，不生成分类提交摘要；
  策展的发布说明属于 stable。
- 工作流更新 `distribution/preview.json`（私有网站发布为 `/preview.json`）。
  **不手改** `distribution/preview.json`——修 workflow 或 `scripts/preview.py`
  后重跑 preview。已发布的 preview release 与 tag 保留：CI 不得删受保护 tag，
  也不得留下没有对应 release 的旧 preview tag。

### tag 保护

全部 tag 受仓库 `release-tags` ruleset 保护，只有仓库 admin 能建/改/删；不给
GitHub Actions 或有写权限的 bot 开 tag 旁路。两个发布工作流都要求 tag-push 事件，
并在发布前核查原始触发者与重跑者当前的仓库 admin 权限；普通 PR 测试工作流不变。
immutable release 保护已发布二进制。

### stable（维护者）

- 在所选已发布 preview tag 的独立检出里开始，而不是当前 master：在那里提交策展
  过的发布文档，然后 `just check` → `just release 0.x.y preview-<build-id>`。
  `release-prepare` / `release-publish` 同样必须带 preview tag 参数；不得把更新的
  master 提交 merge/rebase 进候选。
- 发布前对当前已发布 stable tag 跑 pre-release-audit
  （`.agents/skills/herdr-pre-release-audit`）、finalize `docs/next`、
  `just pre-release-check`（staged docs、distribution 契约、渲染扩展）。
  `just release` 准备 changelog 与 release commit、校验 preview→release diff，只推
  带注释的 stable tag；tag 的 `Preview` 与 `Previous-Stable` trailer 是必需的来源
  证明，不是可选备注。
- 与 preview 相比只允许这些不同：Cargo.toml/Cargo.lock 中的 herdr 包版本、
  changelog、staged README、staged 网站文案、产品公告与 stable skill；代码、依赖、
  API schema、构建配置等必须一致，本地与 CI 在 stable 构建前都会校验。没有这套
  晋升工具链的旧 preview 须先出新 preview。stable 以 stable 版本身份重新构建所选
  源码，不复用 preview 二进制。
- GitHub Actions 构建二进制、创建 Release、按记录的上一个 stable 边界关闭
  issue、快照 tag 上的文档、更新 `distribution/latest.json`；只把发布准备 diff
  三方合并回 master（保留更新的开发）。冲突会中止 distribution 发布、需人工解决，
  不得用整棵发布树覆盖 master。发布与元数据对齐后删除临时 release/hotfix 分支。
  私有网站仓库负责渲染部署。
- 首个 stable Windows 发布前，必须先发布并验证一个含 stable 渠道支持的 preview
  （存量 Windows preview 用户需要它来迁移 `herdr channel set stable`）。

## 资产契约

release 工作流必须发布五个资产：`herdr-linux-x86_64`、`herdr-linux-aarch64`、
`herdr-macos-x86_64`、`herdr-macos-aarch64`、`herdr-windows-x86_64.zip`。
Windows 归档必须含 `herdr.exe` 与其 app-local ConPTY 运行时（`platform.md`），
不得发布裸可执行文件作为 stable Windows 资产。

## fork 例外：agent-detection 发布目录

`distribution/agent-detection/` 的归属**不变**：仍是上游发布目录（语义见
`detection.md`「发布目录」），fork 不自行发布它。唯一例外是 fork 删除六家以外
的捆绑 manifest 时，随之删减该目录里对应的 `<agent>.toml` 与 `index.toml`
条目——只为让 `scripts/agent_detection_manifest_check.py` 的捆绑↔发布一致性
继续成立；不借此改动六家自身的发布副本，也不触碰 `distribution/*.json` 等其他
渠道文件。名单与同步口径见 `README.md` 的「fork 已删除的集成」。

删减已落地：目录里只剩五家的副本与 `index.toml` 的五个条目。上游曾在
`scripts/agent_detection_manifest_check.py::STAGED_PUBLISHED_MANIFESTS` 为 grok 钉
（捆绑版本、发布版本、sha256）例外，fork 删 grok 时已清空该表，
`scripts/test_agent_detection_manifest_check.py` 的 staged 用例改用自造 manifest
并 patch 例外表，不再读取任何真实清单文件。同步上游时不要把 grok 条目与读取真实
`grok.toml` 的用例合回来；同一提示也写在 `scripts/upstream_sync_drop_paths.txt` 的
grok 行上方。

## CHANGELOG 与版本

- 版本唯一真源是 `Cargo.toml` 的 `version`。根 `CHANGELOG.md` 为 Keep-a-Changelog
  格式（`## Unreleased` + `## [x.y.z] - 日期`），`docs/next/CHANGELOG.md` 是其
  未发布副本；由 `scripts/changelog.py` 在 release 流程同步，**日常 feature/fix
  不手改两者**（`docs-pipeline.md`）。
- 保持用户可见 commit subject 有描述性并带 `refs #<issue>` 行，stable 发布准备
  才能用完整区间盘点生成 changelog；网站-only/文档-only/CI/构建/仓库维护变更
  不记 changelog。

## 性能 smoke

`just bench-release-smoke`（约 3–5 分钟，未设 `HERDR_PERF_BASELINE_BIN` 时下载
stable 作基线）在 hidden 与 visible 输出两种形态对比候选与当前 stable；语义见
`app-render.md` 乘法路径。
