# herdr 开发闭环

## 环境准备

- Rust：`rust-toolchain.toml` 锁定（当前 1.96.1，含 clippy/rustfmt）。
- `just`：命令入口（全表见 `MAKE_COMMANDS.md`）。
- Python 3：维护脚本与其 unittest（建议 ≥3.11；3.10 需 `tomli`，仓库脚本已带
  回退）。
- Bun：docs 契约与集成资产测试（`just docs-contract-test`、`integration-assets-test`）。
- Node.js（LTS，`node` 须在 PATH）：`scripts/docs/*.mjs` 的运行时。docs 契约的集成
  用例以 `node` 子进程执行这些脚本（与发布链同一运行时，不改用 bun），因此
  `just docs-contract-test`（及包含它的 `just test`、`just check`）和
  `just release-docs-check` 都需要。CI 用 GitHub-hosted runner 自带的 Node；Windows
  无管理员权限时可将官方 win-x64 zip 解压到用户目录并追加进用户 PATH。
- 一键环境：`scripts/setup_env.sh`（`just setup-env`）——检查 cargo/just/python3/bun，
  并把 sha256 钉版 Zig 0.16.0 安装到**仓库内** `.local/toolchains/zig/`
  （gitignored，不写用户全局状态）。安装后裸 `cargo build` 与 `just` 直接可用：
  build.rs 自动探测项目内钉版（优先级 `$ZIG` > 项目内钉版 > PATH）；CI 由
  workflow 的 setup-zig 步骤提供。`--check` 为只读诊断。
- Windows 交叉验证：`cargo install xwin --locked` + `just setup-windows-cross`
  （一次性，详见 `AGENT_RULES/platform.md`）。
- `just install-hooks` 安装 conventional-commit 守门钩子。

## 本机构建与测试提速

- `just local-build-config --enable` 生成只属于本机的 `.cargo/config.local.toml`
  （gitignored，经仓库 `.cargo/config.toml` 的可选 include 引入；CI 与发布构建读不到）：
  Windows 改用 rust-lld 链接、依赖不带调试信息。追加 `--parallel-frontend` 可开启
  rustc 并行前端（不稳定选项，冷构建明显变快、增量构建不变，出问题即 `--disable`）。
  开关变化后第一次构建会全量重编；打包前必须 `--disable`。
- herdr 是单个约 45 万行的 crate：小改动的增量构建主要耗在整 crate 的宏展开、名称
  解析与增量缓存读写，这部分单线程、与 CPU 核数无关；只有拆分 crate 才能继续压缩。
- Windows 上杀毒软件的实时扫描是本机测试最大的瓶颈：nextest 每个用例一个进程，测试
  还会大量拉起 git/PowerShell/控制台进程，进程创建被逐个扫描后会限流（实测每秒只能
  创建十几个进程，全量 nextest 比 4 核 CI 慢数倍）。在杀软里把仓库 `target\`、
  `%USERPROFILE%\.cargo`、`%LOCALAPPDATA%\zig` 与 `vendor\libghostty-vt\.zig-cache` 加入
  排除，并把 rustc/cargo/cargo-nextest/link/zig 设为受信任程序；Microsoft Defender 可改用
  Dev Drive（ReFS + 性能模式）。这些是本机安全设置，按个人风险偏好决定。
- `just maintenance-test` 按类并行执行维护脚本测试，并据上一轮耗时把慢类拆块；日常改动
  用 `just test-one <filter>` 缩小范围，比全量 `just test` 快得多。

## 每个任务的闭环

1. **定 scope**：列出本轮会读/改/审的路径；运行
   `python3 scripts/resolve_agent_rules.py <paths...>` 并读完输出文档；scope
   扩大后用完整集合重跑。
2. **实现**：遵守 `AGENT_RULES/` 领域不变量（渲染纯函数、平台隔离、端点冻结、
   性能乘法路径等）。
3. **针对性检查**：`just test-one <filter>` / `just maintenance-test` /
   `just docs-contract-test` 按影响面选；提交前 `just check`。
4. **用户可见行为**：同步 `docs/next` 文档与翻译（发布纪律见
   `AGENT_RULES/docs-pipeline.md`；CHANGELOG 不在日常范围）。
5. **生成物**：命中 `docs/README.md` 生成物登记的输入时，先跑 check 确认差异、
   有意才重建并审 diff。
6. **验证证据**：运行期行为用 `herdr-throwaway-repro` skill 建隔离会话复现
   （在既有 herdr 会话内测试新构建用
   `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- ...`）。
7. **复审**：修复杂度高的改动走独立只读复审（`--task review` 规则）；输出
   严重/中/轻/结论。
8. **提交**：conventional commit + `refs #<issue>`；提交前提出 message 对齐；
   只精确暂存相关文件。

## 分支与上游

本 fork（origin `gx0404/herdr`）跟踪 upstream `herdrdev/herdr`：维护分支为 `gx`，
特性分支从 `gx` 派生；合并上游后按 `AGENT_RULES/README.md` 的章节映射表移植
AGENTS.md 变化到领域文档。对上游的 issue/PR 行为遵守 `AGENT_RULES/governance.md`
守门。较大特性建议独立 worktree（见 governance 维护者工作流小节的布局约定）。

## GX Shell 外部源码消费

GX Shell 是编排仓，不保存 herdr 源码。Oh My Zsh 外部 builder 消费本仓的独立
checkout 或源码归档；无须旧 `gx_shell` Git 对象、父目录文件或单仓路径前缀。
源码根必须直接包含 `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`build.rs`、
`src/`、`crates/ghostty-vt/`、`vendor/`、`LICENSE` 和 Windows ConPTY 打包输入。

下游 manifest/依赖锁必须记录：

| 字段 | 约定 |
|---|---|
| `repository` | `https://github.com/gx0404/herdr` |
| `branch_provenance` | `gx`，仅作来源说明，不用于选择构建时的移动分支 |
| `revision` | 本仓完整 40 位 commit SHA，不是 GX Shell 或 Oh My Zsh 的 SHA |
| `version` | 该 revision 的 `Cargo.toml` package version |
| `source.url` / `filename` / `sha256` / `size` | 完整 SHA 对应的归档及实际下载字节的摘要/长度；删除旧 `monorepo_path` |
| `rust` / `zig` | `rust-toolchain.toml` 精确版本与 `scripts/gx_package.py::ZIG_VERSION` |
| `targets` | `windows-x64` → `x86_64-pc-windows-msvc`；`ubuntu-amd64` → `x86_64-unknown-linux-musl` |
| `conpty` | 保留钉版 NuGet、许可摘要及七文件布局；不得因外置而放宽校验 |

- checkout 必须核对独立仓根、完整 HEAD、干净状态和未跟踪源码；支持 `.git` 为文件的
  worktree。`scripts/gx_package.py::source_info` 拒绝把外层仓库的 HEAD 当作源码身份。
- 归档先验证锁中的 SHA256/size，再安全解包并剥掉唯一的归档顶层目录；拒绝路径穿越、
  意外符号链接和本机 `.cargo/config.local.toml`。无 `.git` 的归档由外部 builder
  处理，不能靠向安装包脚本传一个自报 SHA 来代替来源验证。
- GitHub 按完整 SHA 的归档与本地 `git archive` 字节不一定相同：分别计算摘要，
  不把本地摘要写到未经下载验证的远端 URL。候选尚未推送时，远端归档摘要记 PENDING。
- 构建传入 `HERDR_BUILD_COMMIT=<revision>` 及 `HERDR_PACKAGE_MANAGER=windows-installer`
  或 `deb`；二进制 `--version` 必须为 `herdr <version>-gx.<manager>.<revision>`。
  包身份仍禁止上游自更新/切换渠道。使用锁定 Rust/Zig、`--locked` 和经过验证的
  Cargo vendor/离线构建，不能依赖开发机加速配置。
- 外部 builder 继续核对根许可、Cargo/path/workspace 许可、vendored libghostty-vt、
  Zig 依赖及 ConPTY 许可；保留 `redistribution/BUILD.json`、许可证清单和源码归档。
  receipt 的 `repository`、`branch_provenance`、`revision`、`source_sha256`、
  `version`、`target`、`rust`、`zig`、`package_manager`、文件与许可摘要必须与锁一致。
  不生成占位 receipt，也不以静态检查替代真实构建。
- 本机构建只用外部 builder 的 `GX_LOCAL_BUILD_ROOT` 隔离入口，receipt 必须是
  `builder=local` 且不可发布；只有真实一次性 CI 构建可记 `builder=github-actions`。
  迁移不改写已发布 tag/release；Windows 安装和 deb 冒烟只在一次性 runner/容器运行。
