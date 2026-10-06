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
  Cargo 与 `just build-libghostty-vt` 的 Zig 子进程默认使用仓库内
  `.local/zig-cache/global`、`.local/zig-cache/local`；可分别显式设置
  `ZIG_GLOBAL_CACHE_DIR`、`ZIG_LOCAL_CACHE_DIR`，不会修改用户或父进程环境。
  显式值原样保留；相对路径由 vendored 源码工作目录解释，空值不被静默替换。
- Windows 交叉验证：`cargo install xwin --locked` + `just setup-windows-cross`
  （一次性，详见 `AGENT_RULES/platform.md`）。
- `just install-hooks` 安装 conventional-commit 守门钩子。

## 本机构建与测试提速

- `just local-build-config --enable` 生成只属于本机的 `.cargo/config.local.toml`
  （gitignored，经仓库 `.cargo/config.toml` 的可选 include 引入；CI 与发布构建读不到）：
  Windows 改用 rust-lld 链接、依赖不带调试信息。追加 `--parallel-frontend` 可开启
  rustc 并行前端（不稳定选项，冷构建明显变快、增量构建不变，出问题即 `--disable`）。
  开关变化后第一次构建会全量重编；打包前必须 `--disable`。
- herdr 的主要 Rust 代码集中在单个 crate；小改动也可能触发较多前端、增量缓存和链接
  工作。先区分冷构建、增量构建和纯测试耗时，再调整 Cargo jobs 与 rustc 前端并发，
  不预设增加线程必然更快，也不为缩短一次构建贸然拆分 crate。
- Windows 上 nextest 每个用例启动独立进程，测试还会拉起 git/PowerShell/控制台进程。
  遇到 CPU 利用率低而测试缓慢时，分别量测进程创建、启动后执行和 runner 调度耗时；
  仅凭这些现象不能断定是杀毒软件限流。性能对照使用相同二进制、测试集合和总 worker
  预算，避开其它构建负载。仓库脚本不修改杀软排除、系统安全设置或全局工具链配置。
- `just maintenance-test` 按类并行执行维护脚本测试，并据上一轮耗时把慢类拆块；默认
  worker 数为 `min(4, cpu_count)`，可用 `HERDR_MAINTENANCE_JOBS` 或执行器 `--jobs` 覆盖。
  `just test` 由 `scripts/run_test_suite.py` 编排五个 phase，combined `test_budget` 默认
  `min(8, cpu_count)`，可用 `HERDR_TEST_BUDGET`/`--test-budget` 覆盖；phase 默认
  `min(2, 5, test_budget)`（`HERDR_TEST_PHASE_JOBS`/`--phase-jobs`/`--jobs`），
  并发 phase 时 maintenance 默认 `min(4, max(1, test_budget-1))`，nextest 使用剩余的
  `max(1, test_budget-maintenance_jobs)`；串行 phase 时分别默认 `min(4, test_budget)`
  与 `test_budget`。两者可用 `HERDR_MAINTENANCE_JOBS`/`--maintenance-jobs` 和
  `HERDR_NEXTEST_JOBS`/`--nextest-jobs` 覆盖。budget=1 默认串行，phase 默认值按本次选定的
  budget 计算。命令行优先于环境变量，显式值不自动截断；该预算不是整机 CPU 硬限额。
  编排器把解析后的 maintenance/nextest 值注入 phase 子进程。每次运行用独立 run-id 目录
  保存逐行刷新的阶段日志和原子 `manifest.json`，启动时立即打印路径。顶层含 `run_id`、
  `status`、`manifest`、budget、三个 jobs 字段与 `failures`，`phases` 含 `recipe`、
  `status`、`exit_code`、`seconds`、`log`；阶段状态区分待执行 `pending`、执行中 `running`
  和完成后的通过/失败。外部强制终止后留下的 `running` 不能作为完成或通过证据，`seconds`
  也只代表最近一次 manifest 更新时的已运行时间。日常改动用 `just test-one <filter>`
  缩小范围，比全量 `just test` 快得多。
- 若实测瓶颈是单 runner 的进程启动吞吐，可为 `just nextest-all`（以及调用它的
  `just test` / Windows `just check`）显式设置 `HERDR_NEXTEST_SHARDS`。默认 1 保持单
  runner；例如 nextest jobs=8、shards=4 会分配 4×2 个测试线程，而不是 4×8。先编译一次，
  然后各分片复用同一份 metadata，校验测试集合不重不漏，独立保存日志与 manifest。
  分片数必须为正整数且不超过 jobs；自定义 profile 或测试组等调度约束不兼容时明确失败，
  可设 shards=1 恢复原路径。`just test-one` / `just ci-tests` 不受分片变量影响。
  先用固定集合、相同总预算做重复对照，再用完整标准检查衡量总耗时，不外推小样本倍数。
- 发布性能 smoke 只在 Linux/macOS 执行；`just bench-release-smoke` 在
  `.local/perf-baseline/run-*/` 保留 run-id、metadata、candidate/baseline 命令、原始
  采样、摘要、run log 和退出码。smoke 与每个 case 都把 `HOME`、`USERPROFILE`、
  `XDG_CONFIG_HOME`、`XDG_STATE_HOME`、`XDG_RUNTIME_DIR`、`XDG_DATA_HOME`、
  `XDG_CACHE_HOME`、`APPDATA`、`LOCALAPPDATA`、`HERDR_HOME`、`CODEX_HOME`、
  `KIMI_CODE_HOME`、`TMPDIR`（case 另设 `TMUX_TMPDIR`）指向 run 内私有临时状态。
  smoke 顶层清除继承的 `HERDR_CONFIG_PATH`；独立 case 的启动、控制和 stop/delete 清理
  命令也清除此覆盖，且不继承调用者的 herdr socket/session。只删除临时运行态。
  其余 `bench-*` recipe 是非门禁画像，命令与证据口径见 `MAKE_COMMANDS.md`。

## 每个任务的闭环

1. **定 scope**：列出本轮会读/改/审的路径；运行
   `python3 scripts/resolve_agent_rules.py <paths...>` 并读完输出文档；scope
   扩大后用完整集合重跑。
2. **实现**：遵守 `AGENT_RULES/` 领域不变量（渲染纯函数、平台隔离、端点冻结、
   性能乘法路径等）。
3. **针对性检查**：`just test-one <filter>` / `just maintenance-test` /
   `just docs-contract-test` 按影响面选；提交前 `just check`。
4. **用户可见行为**：同步 `docs/next` 文档与翻译（发布纪律见
   `AGENT_RULES/docs-pipeline.md`；CHANGELOG 不在日常范围）。测试编排/性能结果等开发者
   行为同步 `AGENT_RULES/testing.md`、`release-channels.md` 和 `MAKE_COMMANDS.md`。
5. **生成物**：命中 `docs/README.md` 生成物登记的输入时，先跑 check 确认差异、
   有意才重建并审 diff。
6. **验证证据**：运行期行为用 `herdr-throwaway-repro` skill 建隔离会话复现
   （在既有 herdr 会话内测试新构建用
   `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- ...`）。Windows
   handshake、PTY admission、Codex native-tools 边界必须记为目标平台证据；未在 Windows
   实机或对应 CI 运行的结果保持 PENDING。
7. **复审**：修复杂度高的改动走独立只读复审（`--task review` 规则）；输出
   严重/中/轻/结论。
8. **提交**：conventional commit + `refs #<issue>`；提交前提出 message 对齐；
   只精确暂存相关文件。

## Windows 隔离 smoke

```powershell
powershell -NoProfile -File scripts/windows_tui_compat.ps1 -ExePath target/release/herdr.exe -Shell powershell
pwsh -NoProfile -File scripts/windows_tui_compat.ps1 -ExePath target/release/herdr.exe -Shell pwsh
powershell -NoProfile -File scripts/windows_smoke_conpty_path.ps1 -ExePath target/release/herdr.exe
```

这些入口只支持原生 Windows x64；默认要求二进制附带匹配的 app-local ConPTY。
小型私有 launcher 和假 DLL 从 `rust-toolchain.toml` 派生版本，使用已安装的 x64 MSVC
rustc／Rust LLD，缺失即失败，不自动安装工具。共享的
`scripts/windows_input/windows_smoke_helpers.ps1` 在恢复子进程执行前将其加入私有 Job，
只凭自有句柄收尾，不以裸 PID 终止进程。每次生成独立会话与
`target/tmp/windows-smoke/<session>/`，隔离配置、用户目录、临时路径及 PowerShell 模块缓存，
保留命令、stdout、stderr、退出码、终端读回和 `result.json`。实际 pane shell 经私有
launcher 添加 `-NoLogo -NoProfile`，不依赖修改 HOME 来阻止真实 profile 执行；shell 选择、
原参数和 ConPTY stdio 不变。ConPTY PATH 脚本的 `-Session` 用作前缀，后附随机标识，不连接
同名已有会话；所有 launcher 和假 DLL 构建都留在该项目目录。

默认 `-ConptyMode auto` 清除继承的 `HERDR_WINDOWS_CONPTY`；需要验证系统 ConPTY 时必须
显式传 `-ConptyMode system`。在同一 PowerShell 宿主中连续调用时，每次立即检查
`$LASTEXITCODE`，不能只依赖 `catch`，也不能让后一条成功覆盖前一条失败。`-PassThru` 返回
结构化报告；报告中的 `runtime_stage`、`server_stderr` 和 `server_exit_code` 可区分预期的
坏 bundle 拒绝与编译等前置失败。CI 只损坏新私有副本，核实确切的 bundle-invalid 诊断及
正常清理后，才用显式 system 模式验证恢复，不修改原构建资产。

`runtime=PASS` 不等于整体通过：stop/delete 失败、无法确认 Job 已空、需要 forced cleanup
或报告无法写入时均非零退出；原始运行错误与清理错误分别保留。只有退出码 0、
`cleanup=PASS`、`cleanup_mode=graceful` 和 `active_processes=0` 才算完整成功；已退出的
server 由原始句柄确认，记录 `shutdown=already_exited`，但不跳过 Job 和 session 清理。
默认非交互模式只验证 shell/ConPTY 的输入输出；`-Interactive` 保留人工 TUI 验证，必须另行
授权，不得用 headless 结果替代 GUI、IME、剪贴板或真实桌面输入资格。

## 分支与上游

本 fork（origin `gx0404/herdr`）跟踪 upstream `herdrdev/herdr`：维护分支为 `feature/gx_herdr`，
特性分支从 `feature/gx_herdr` 派生；合并上游后按 `AGENT_RULES/README.md` 的章节映射表移植
AGENTS.md 变化到领域文档。对上游的 issue/PR 行为遵守 `AGENT_RULES/governance.md`
守门。较大特性建议独立 worktree（见 governance 维护者工作流小节的布局约定）。

`master` 保留上游基线，GX 取舍与兼容修复留在 `feature/gx_herdr`。同步前确认工作区干净、
保存两条分支的旧 SHA 与本地回退引用，再抓取并审查上游增量。固定本轮上游 SHA 后按以下顺序操作：

```bash
git fetch upstream master
git switch master
git merge --ff-only <已审查的上游完整SHA>
git switch feature/gx_herdr
git merge --no-ff --no-commit master
```

master 不能快进时先核查分叉原因，不强制重置。feature 上逐块解决冲突，保留六家集成边界、
GX 功能、端点冻结契约及工作流归档；不以整文件 ours/theirs、squash 或 cherry-pick 替代合并。
运行 `python3 scripts/upstream_sync_drop_check.py`、相关回归与 `just check` 后，按提交规范完成
真正的双亲 merge；用 `git merge-base --is-ancestor master feature/gx_herdr` 核对祖先关系。
本地同步不自动推送或发布，远端更新另行授权。下次仍从上次 master 基线增量合并。

## GX Shell 外部源码消费

GX Shell 是编排仓，不保存 herdr 源码。Oh My Zsh 外部 builder 消费本仓的独立
checkout 或源码归档；无须旧 `gx_shell` Git 对象、父目录文件或单仓路径前缀。
源码根必须直接包含 `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`build.rs`、
`src/`、`crates/ghostty-vt/`、`vendor/`、`LICENSE` 和 Windows ConPTY 打包输入。

下游 manifest/依赖锁必须记录：

| 字段 | 约定 |
|---|---|
| `repository` | `https://github.com/gx0404/herdr` |
| `branch_provenance` | `feature/gx_herdr`，仅作来源说明，不用于选择构建时的移动分支 |
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
