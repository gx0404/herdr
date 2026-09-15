# project-infra（构建、配置模型与仓库基础）

范围：Cargo/just/clippy/toolchain/flake 等构建文件、`.cargo/**`、`assets/**`、
`justfile`、`src/config*`（配置模型）、根级杂项模块（build_info/logging/sound/
update/release_notes/product_announcements/plugins/render_prof 等）与采集脚本。

## 代码约定（上游 Code Conventions 全文语义，适用全仓库）

- Rust 生产代码禁用 `unwrap()`；日志用 `tracing`；`#[allow]` 必须带注释说明原因。
- 不无理由加依赖；先确认现有依赖不能覆盖需求。
- 平台门控规则见 `platform.md`；集成资产版本规则见 `integration-assets.md`。

## 构建与命令面

- 任务入口是 `just`（无 Makefile）；语义与新增 recipe 见 `docs/MAKE_COMMANDS.md`。
  修改 recipe 后必须同步该文档与 CI 调用面（`.github/workflows/ci.yml` 等，
  归 `governance.md`/`release-channels.md`）。
- `Cargo.toml` 是版本唯一真源（当前语义见 `release-channels.md`）；`Cargo.lock`
  直接被 `nix/package.nix` 以 `cargoLock.lockFile` 引入，release 版本 bump 不需
  要单独更新 Nix cargo hash；若日后加入 Cargo git 依赖，必须随该变更补
  `cargoLock.outputHashes`。
- `build.rs` 与 vendored 绑定生成（bindgen/libghostty）流程见
  `vendored-libghostty-vt.md`；改构建脚本后跑 `just build` 验证两种平台路径。

## 配置模型（src/config）

- `src/config/model.rs` 是配置 schema 真源；新增/修改字段必须同步：
  默认值（`default-config` recipe 可打印）、`docs/next/website/src/data/
  config-reference.json`（由 `scripts/config_reference_check.py` 校验）、用户文档
  config-reference 页（`docs-pipeline.md`）与翻译。
- keybind/theme/sidebar/tab_bar 等子模型保持独立文件；UI 消费方式遵守
  `app-render.md`（TUI 交互模式复用既有设置界面）。
- 运行时行为类模块（update、product_announcements、release_notes、sound）改动
  属于用户可见行为：走 `docs/next` 文档与发布纪律，不得手改 `distribution/*.json`。
