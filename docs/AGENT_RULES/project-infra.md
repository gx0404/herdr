# project-infra（构建、配置模型与仓库基础）

范围：Cargo/just/clippy/toolchain/flake 等构建文件、`.cargo/**`、`assets/**`、
`justfile`、`src/config*`（配置模型）、根级杂项模块（build_info/logging/sound/
update/release_notes/product_announcements/plugins/render_prof 等）、采集脚本与本机
构建加速脚本 `scripts/local_build_config.py`。

## 代码约定（上游 Code Conventions 全文语义，适用全仓库）

- Rust 生产代码禁用 `unwrap()`；日志用 `tracing`；`#[allow]` 必须带注释说明原因。
- 不无理由加依赖；先确认现有依赖不能覆盖需求。
- 平台门控规则见 `platform.md`；集成资产版本规则见 `integration-assets.md`。

## 构建与命令面

- 任务入口是 `just`（无 Makefile）；语义与新增 recipe 见 `docs/MAKE_COMMANDS.md`。
  修改 recipe 后必须同步该文档与 CI 调用面（`.github/workflows/ci.yml` 等，
  归 `governance.md`/`release-channels.md`）。
- **Zig 工具链是钉版受控的**：vendored libghostty-vt 需要 Zig 0.16.0。仓库自包含
  方案：`scripts/setup_env.sh`（总入口）与 `scripts/setup_zig.py` 把官方归档（Linux /
  macOS 为 tarball、Windows 为 zip，sha256 钉死）装进 `<repo>/.local/toolchains/zig/`；
  `crates/ghostty-vt/build.rs::resolve_zig` 按 `$ZIG` > 项目内钉版 > PATH 解析，装完
  即可裸 `cargo build`。CI 由 workflow 的 setup-zig 步骤提供。多 worktree 共享一份时设
  `HERDR_ZIG_HOME`。升级 Zig = 同步 `setup_zig.py` 钉版表、
  `crates/ghostty-vt/build.rs::resolve_zig` 目录名与
  `vendored-libghostty-vt.md` 版本要求，重跑 `just setup-zig --install --force`。
- `Cargo.toml` 是版本唯一真源（当前语义见 `release-channels.md`）；`Cargo.lock`
  直接被 `nix/package.nix` 以 `cargoLock.lockFile` 引入，release 版本 bump 不需
  要单独更新 Nix cargo hash；若日后加入 Cargo git 依赖，必须随该变更补
  `cargoLock.outputHashes`。
- `build.rs` 与 vendored 绑定生成（bindgen/libghostty）流程见
  `vendored-libghostty-vt.md`；改构建脚本后跑 `just build` 验证两种平台路径。
- `.cargo/config.toml` 只放所有构建都成立的设置；本机专属加速（rust-lld、依赖无调试信息、
  可选的不稳定并行前端）放 gitignored 的 `.cargo/config.local.toml`，经可选 include 引入，
  由 `scripts/local_build_config.py`（`just local-build-config`）生成。不得把这些设置提交进
  仓库配置；`scripts/gx_package.py` 在该文件存在时拒绝打包。

## 配置模型（src/config）

- `src/config/model.rs` 是配置 schema 真源；新增/修改字段必须同步：
  默认值（`default-config` recipe 可打印）、`docs/next/website/src/data/
  config-reference.json`（由 `scripts/config_reference_check.py` 校验）、用户文档
  config-reference 页（`docs-pipeline.md`）与翻译。
- keybind/theme/sidebar/tab_bar 等子模型保持独立文件；UI 消费方式遵守
  `app-render.md`（TUI 交互模式复用既有设置界面）。
- 运行时行为类模块（update、product_announcements、release_notes、sound）改动
  属于用户可见行为：走 `docs/next` 文档与发布纪律，不得手改 `distribution/*.json`。
- `src/config/write.rs::update_file_at` 必须在目标同目录创建临时文件，保留旧权限，写入并
  `sync_all` 后调用平台原子替换，再同步父目录；已有符号链接沿链解析并更新最终普通文件，
  不替换链接。失败时清理临时文件并保留原目标。

## 构建本地性（fork 硬规则）

编译、构建产生的一切必须落在本项目文件夹内，不得写到项目外：

- 默认落点：`target/`（cargo 默认，不改 `CARGO_TARGET_DIR` 指向外部）与 gitignored
  的 `.local/`——Zig 工具链 `.local/toolchains/zig/`、Windows 交叉 SDK
  `.local/windows-cross/`、本机工具 shim `.local/tool-shims/`、性能基线
  `.local/perf-baseline/`、临时 worktree 放 `target/tmp/`。
- 禁止：把构建产物、下载的工具链/SDK、测试沙箱、打包产物写到 `%USERPROFILE%`、
  `%TEMP%` 根、其他磁盘目录等项目外位置（测试运行期的系统临时目录除外，且必须
  自清，见 `testing.md`）。
- 例外（用户级包管理器缓存，只读复用、项目脚本不主动写入）：cargo registry
  （`~/.cargo/registry`）、rustup 工具链、uv/bun 全局缓存。
- Windows 交叉 SDK 默认根为 `<repo>/.local/windows-cross/`
  （`scripts/windows_cross.py::SDK_ROOT`）；跨 worktree/机器共享时设
  `HERDR_WINDOWS_CROSS_ROOT` 显式指向项目外路径；`LIBGHOSTTY_VT_WINDOWS_LIBC`
  覆盖语义不变。
- 用户级运行时（如 Node.js 用户目录安装、uv tool）属例外；本机 shim 只进
  `.local/tool-shims/`（如 graphify 钉版经 uvx 运行 0.9.20 的转发脚本），
  用 `PATH="$PWD/.local/tool-shims:$PATH"` 前缀注入，不污染用户级目录。
