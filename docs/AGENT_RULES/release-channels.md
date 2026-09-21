# release-channels（发布渠道与版本真源）

范围：`distribution/**`、`CHANGELOG.md`、`nix/**`、`flake.*`、`packaging/**`、
release/preview 工作流与脚本、`scripts/changelog.py`、`scripts/preview.py`、
perf smoke 脚本。**本域维护者动作（发布、推资产、改渠道文件）仅限核实过的
维护者**（`governance.md`）；日常 feature/fix 工作只读本域。

## 渠道模型（上游全文语义）

单一主分支 + 双更新渠道；stable 与 preview 都从 `master` 构建，无长期 preview
分支。普通用户默认 stable：文档 `/docs/`、更新源 `distribution/latest.json`、
Homebrew/Nix 仅 stable。preview 为直装用户 opt-in：

```bash
herdr channel set preview && herdr update   # 回 stable: herdr channel set stable
```

- preview 发布是 `.github/workflows/preview.yml` 手动触发 + 周三/周五计划产生的
  GitHub prerelease；工作流更新 `distribution/preview.json`（私有网站发布为
  `/preview.json`）。**不手改** `distribution/preview.json`——修 workflow 或
  `scripts/preview.py` 后重跑 preview。
- stable 发布（维护者）：`just check` → `just release 0.x.y`。发布前跑
  pre-release-audit（`.agents/skills/herdr-pre-release-audit`）、finalize
  `docs/next`、`just pre-release-check`（staged docs、distribution 契约、渲染
  扩展）。`just release` 准备 changelog 与 release commit、打 tag、推 tag；
  GitHub Actions 构建、创建 Release、关闭 released issues、快照并晋升 tagged
  docs、更新 `distribution/latest.json`；私有网站仓库负责渲染部署。
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

grok 是已知硬阻塞：`scripts/agent_detection_manifest_check.py` 的
`STAGED_PUBLISHED_MANIFESTS` 钉着它的（捆绑版本、发布版本、sha256）例外，
`scripts/test_agent_detection_manifest_check.py::staged_grok_dirs` 又直接读真实的
`src/detect/manifests/grok.toml` 与 `distribution/agent-detection/grok.toml`。删
grok 时必须在同一次改动里移除该例外条目并处理它的两个 staged 用例（删掉，或
改成自造 manifest 并 patch 例外表），否则 `just maintenance-test` 以
FileNotFoundError 变红；同一提示也写在 `scripts/upstream_sync_drop_paths.txt` 的
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
