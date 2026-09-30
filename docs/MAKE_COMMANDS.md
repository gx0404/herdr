# just 命令手册

入口统一是 `just`（仓库无 Makefile）。列：用途 / 前置 / 副作用 / 通过条件与证据。
改动 recipe 时同步本表与 CI 调用面。

`nextest-all`、`test-one` 和 `ci-tests` 使用 `--status-level leak`：运行中显示失败、重试、慢测试及 leaky 用例名称，便于定位测试结束后仍占用输出管道的子进程；测试通过条件与退出码判定不变。

## 日常开发

| 命令 | 用途 | 前置 | 副作用 | 证据 |
|---|---|---|---|---|
| `just test` | 全量验证：编排器并行跑 nextest + maintenance + 热路径架构 + 资产 + docs 契约（阶段日志在 `target/test-suite-logs/`） | Rust/Python/Bun/Node 工具链 | 编译产物、临时目录 | 退出码 0；阶段汇总各子命令状态 |
| `just nextest-all` | 单独跑全量 nextest（编排器 nextest 阶段的命令真源） | Rust | 编译产物 | 退出码 0 |
| `just test-one <filter>` | 单个 nextest 过滤器 | 同上 | 同上 | 退出码 0 |
| `just maintenance-test` | 维护脚本 unittest 清单（新脚本测试须登记进清单）+ 发布工作流契约（`bun test scripts/release-workflows.test.ts`）+ fork 上游同步丢弃路径门禁（`scripts/upstream_sync_drop_check.py`，清单命中的路径重新出现即失败）。清单由 `scripts/run_parallel_unittest.py` 按类拆成子进程、以 CPU 数并发执行；上一轮耗时记在 `target/test-suite-logs/unittest-durations.json`，据此把慢类拆块、从长到短派发 | Python3（3.10 需 tomli）、Bun | 写 `target/test-suite-logs/unittest-durations.json` | 执行器末行 `OK` + bun test OK + 丢弃检查 `OK: … 均无命中`；失败单元回放完整输出 |
| `just test-windows-input [args..]` | 仅 Windows：本机交互式 Windows Terminal 输入资格测试（`scripts/test_windows_input.ps1`，注入输入并清空剪贴板；普通 CI 不跑） | Windows、pwsh | 注入键鼠输入、清空剪贴板 | 报告中各输入路径的覆盖结论 |
| `just ui-hot-path-architecture-test` | UI 热路径架构边界（确定性） | Python3 | 无 | `unittest` OK |
| `just lint` | fmt --check + clippy -D warnings | Rust | 无 | 退出码 0 |
| `just ci [filter]` / `just ci-tests [filter]` | PR CI 等效链（ci 含 lint） | 同上 | 编译产物 | 退出码 0 |
| `just check` | ci + windows-lint + docs 契约 + 提醒（unix）；Windows 走 `windows_check.ps1 -Mode check` | 同 `just test`（含 Node）+ Windows SDK（交叉） | 编译产物 | 退出码 0 |
| `just windows-lint` | Windows 目标 clippy（Unix 交叉，`--all-targets` 连测试目标一起查，与 CI `windows_check.ps1 -Mode lint` 同口径；热缓存比只查 bin 多约 20 s） | `just setup-windows-cross` 一次 | 下载 SDK 到 `~/.local/share/herdr/windows-cross/` | 退出码 0 |
| `just setup-env [-- --check/--force]` | 一键环境安装（幂等，已装且有效则跳过）/诊断/覆盖重装钉版 Zig | `--force` 联网重下 | 写仓库内 `.local/toolchains/`（gitignored） | sha256 校验 + `zig version` 0.16.0 |
| `just setup-zig [-- --install/--force]` | 钉版 Zig 0.16.0 工具链安装/诊断（vendored libghostty-vt 构建必需） | 无（--install 联网下载） | 写仓库内 `.local/toolchains/zig/`；build.rs 自动探测 | sha256 校验通过 + `zig version` 输出 0.16.0 |
| `just setup-windows-cross [-- --accept-license]` | 下载 Windows SDK（xwin） | `cargo install xwin --locked` | 下载（仅显式运行） | 脚本成功输出 |
| `just local-build-config [--enable [--parallel-frontend[=N]] \| --disable \| --status]` | 本机构建加速（只作用于本机）：生成 gitignored 的 `.cargo/config.local.toml`，由仓库 `.cargo/config.toml` 可选 include 引入。默认 Windows MSVC 用 rust-lld 链接、依赖不生成调试信息（本机实测小改动后增量 dev 构建约快 12%）；`--parallel-frontend` 另借 `RUSTC_BOOTSTRAP=1` 开启不稳定的 `-Zthreads=N`（默认 8，冷构建约快 44%，增量无收益，可能 ICE） | Python3、rustc | 写/删 `.cargo/config.local.toml`；开关变化后下一次构建全量重编；存在时 `gx_package.py` 拒绝打包 | 打印生成内容 / 当前状态 |
| `just install-hooks` | 安装 `.githooks`（conventional commits） | git | `core.hooksPath` 配置 | 提示安装完成 |
| `just build` | release 构建 | Rust（vendored vt 需 Zig 或用预生成） | `target/` | 构建成功 |
| `just default-config` | 打印默认配置 | 同上 | 无 | stdout |

## 框架面（AI 协作）

| 命令 | 用途 | 前置 | 副作用 | 证据 |
|---|---|---|---|---|
| `just agent-rules [paths..]` | 解析本轮必读领域规则（转发 resolver） | Python3 | 无 | 文档路径列表；未知路径退出 2 |
| `just agent-rules-check` | 规则闭集/体积/双射守门（resolver --check） | Python3 | 无 | `OK: 16 份领域规则…` |
| `just framework-check` | agent-rules-check + hook 探针 + kb-check + graph-check | Python3、图谱/KB 产物 | 无 | 各子检查 OK |
| `just graph` | 重建代码图谱（仅 `src/`，全量重建） | graphify（`graphifyy`，wrapper 校验版本） | 写 `graphify-out/`（大文件本机） | 报告生成 + 指纹更新 |
| `just graph-check` | 指纹 + 报告镜像一致性校验 | 已建图 | 无 | 退出码 0 |
| `just kb` | 重建知识库 `docs/kb/chunks.json` | Python3 | 写 tracked 生成物（审 diff） | 确定性 diff |
| `just kb-check` | KB freshness（只读） | 同上 | 无 | 退出码 0 |

## 契约测试

| 命令 | 用途 | 证据 |
|---|---|---|
| `just docs-contract-test` | docs 快照/版本生命周期工具（bun；集成用例以 `node` 子进程执行 `scripts/docs/*.mjs`，需 Node 在 PATH） | bun test OK |
| `just integration-assets-test` | 捆绑 agent 集成资产（bun） | bun test OK |

## 性能

| 命令 | 用途 | 前置 | 证据 |
|---|---|---|---|
| `just bench-render-scale` | 非门禁全渲染扩展画像（1/15 pane、后台 workspace） | release 构建 | 控制台画像（记录到任务说明） |
| `just bench-release-smoke` | 发布前 CPU 对比（~3–5 分钟；未设 `HERDR_PERF_BASELINE_BIN` 下载 stable） | 网络或本地基线 | 对比结论；显著回归须调查 |

## 发布链（上游保留入口；见 `AGENT_RULES/release-channels.md`）

下表描述上游维护者流程。本 fork 的对应 workflow 已原样移至
`.github/workflows-archive/`，不会因推送这些 tag 自动发布；fork 发包使用后面的 GX 入口。

| 命令 | 用途 | 关键守门 |
|---|---|---|
| `just release-docs-check` | docs/changelog/manifest/config-reference 终稿校验 | diff CHANGELOG、翻译完整、`--require-all-published` |
| `just pre-release-check` | release-docs-check + 两个 bench + skill 提醒 | 全部通过 |
| `just preview [ref]` | 校验源提交（默认 HEAD，须可从 master 或 `release/*` 到达）后创建并推送带注释的 `preview-<提交日期>-<短 sha>` tag | tag 推送触发 preview.yml |
| `just release-prepare <ver> <preview-tag>` | 在基于所选 preview tag 的检出里准备 release commit（工作树须干净、tag 未存在、`scripts/release.py check-source` 前后各校验一次、跑 pre-release-check、bump Cargo.toml、同步 changelog） | 产出待审 commit |
| `just release-publish <ver> <preview-tag>` | 校验 preview→release 差异后打带 `Preview` / `Previous-Stable` trailer 的 tag 并只推 tag（不移动 master） | tag `v<ver>` 触发 release.yml |
| `just release <ver> <preview-tag>` | prepare + publish（晋升已发布的 preview，不取最新 master） | 同上 |

## GX fork 安装包

| 命令 | 用途 | 前置与副作用 |
|---|---|---|
| `just package-windows --check` | Windows 只读预检 | 已安装钉版 Rust/Zig、MSVC target、Inno Setup（可用 `ISCC` 指定）；不自动装工具 |
| `just package-deb --check` | Linux 只读预检 | 钉版 Rust/Zig、musl target、musl-gcc、dpkg-deb、readelf/nm 等 |
| `just package-windows` | 构建 Windows x64 Setup EXE | 每次运行 Cargo；完整 ConPTY payload；输出包、manifest、sha256 |
| `just package-deb` | 构建 Ubuntu amd64 deb | musl 静态程序；root-owned `/usr/bin/herdr` 和许可；不在宿主安装 |

版本只读 `Cargo.toml`。默认要求工作树干净；本地测试可以加 `--allow-dirty`，但这类包不能发布。
本机启用了 `just local-build-config` 时预检会拒绝打包（链接器与不稳定选项不得带进安装包），先 `--disable`。
Cargo 编译目录隔离在 `target/gx/<platform>`，最终默认输出 `target/packages`；
`--output-dir <目录>` 可选择其他产物目录。同名产物禁止覆盖，重跑应使用新的空目录。
Windows 与 WSL 同一检出的 vendored Zig 输出仍共享，不要并发运行双平台构建。
WSL 在 `/mnt/` 共享盘遇到 Zig 缓存 rename `AccessDenied` 时，将 `ZIG_LOCAL_CACHE_DIR`
指向自己新建的 Linux 原生临时目录（如 `mktemp -d /tmp/herdr-gx-zig.XXXXXX`），完成后仅清理
该临时目录；不要删除或复用正在使用的 Windows 缓存。非默认目录的 Linux Zig 用 `ZIG` 指定。

本 fork 的活动 Actions 只有 **CI** 和 **GX release**；CI 检查默认分支
`feature/gx_herdr` 的 push 及 PR，其余九份定义在 `.github/workflows-archive/` 原样归档。
发布使用 **GX release**：源码 `ref` 默认 `feature/gx_herdr`，默认 `publish=false`
完整构建、安装 smoke 和汇总验证；显式 `publish=true` 才以 `gx-v<版本>` 发布到
`gx0404/herdr`。仅发布两包、`manifest.json`、`SHA256SUMS`，不写上游渠道文件。
已发布版本不可覆盖；同源同摘要的中断草稿可恢复。工具链、源码和平台必须一致，发布者及
重跑者须有仓库 admin 权限；tag rules 拒绝时停止，不修改保护规则。
`previous_tag` 留空时自动选择低于当前 Cargo 版本的最大已公开 GX 版本做真实升级测试；
也可指定更旧的 `gx-v<版本>`。只读下载器验证来源、完整四资产及摘要，再恢复 smoke 所需
sidecar。显式标签错误或旧资产异常会失败；只有确实没有旧版本时升级才记 N/A。

维护脚本测试包含 `scripts.test_gx_package`、`scripts.test_gx_release`、`scripts.test_gx_smoke`，
由 `just maintenance-test` 收集。安装 smoke 不属于本地常规测试：Windows 必须同时设置
`HERDR_GX_DISPOSABLE=1` 且处于 GitHub-hosted runner；Linux 必须在显式一次性 Ubuntu
容器中以 root 运行。手动调用 smoke 时，旧版升级需要传入更旧版本包及对应 manifest；
未提供时该次升级验证报告 N/A，不将同版重装计为升级通过。发布 workflow 会按上述规则
自动准备旧包。禁止为了测试在用户宿主安装或卸载 Herdr。

## 构建辅助

| 命令 | 用途 |
|---|---|
| `just libghostty-bindings [clang_args..]` | bindgen-cli 0.72.1 重新生成 C API 绑定 |
| `just build-libghostty-vt` | 构建 vendored libghostty-vt 源码分发（需 Zig 0.16.0） |
