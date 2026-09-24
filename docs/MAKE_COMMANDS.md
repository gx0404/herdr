# just 命令手册

入口统一是 `just`（仓库无 Makefile）。列：用途 / 前置 / 副作用 / 通过条件与证据。
改动 recipe 时同步本表与 CI 调用面。

## 日常开发

| 命令 | 用途 | 前置 | 副作用 | 证据 |
|---|---|---|---|---|
| `just test` | 全量验证：编排器并行跑 nextest + maintenance + 热路径架构 + 资产 + docs 契约（阶段日志在 `target/test-suite-logs/`） | Rust/Python/Bun 工具链 | 编译产物、临时目录 | 退出码 0；阶段汇总各子命令状态 |
| `just nextest-all` | 单独跑全量 nextest（编排器 nextest 阶段的命令真源） | Rust | 编译产物 | 退出码 0 |
| `just test-one <filter>` | 单个 nextest 过滤器 | 同上 | 同上 | 退出码 0 |
| `just maintenance-test` | 维护脚本 unittest 清单（新脚本测试须登记进清单）+ fork 上游同步丢弃路径门禁（`scripts/upstream_sync_drop_check.py`，清单命中的路径重新出现即失败） | Python3（3.10 需 tomli） | 无 | `unittest` OK + 丢弃检查 `OK: … 均无命中` |
| `just ui-hot-path-architecture-test` | UI 热路径架构边界（确定性） | Python3 | 无 | `unittest` OK |
| `just lint` | fmt --check + clippy -D warnings | Rust | 无 | 退出码 0 |
| `just ci [filter]` / `just ci-tests [filter]` | PR CI 等效链（ci 含 lint） | 同上 | 编译产物 | 退出码 0 |
| `just check` | ci + windows-lint + docs 契约 + 提醒（unix）；Windows 走 `windows_check.ps1 -Mode check` | 同上 + Windows SDK（交叉） | 编译产物 | 退出码 0 |
| `just windows-lint` | Windows 目标 clippy（Unix 交叉，`--all-targets` 连测试目标一起查，与 CI `windows_check.ps1 -Mode lint` 同口径；热缓存比只查 bin 多约 20 s） | `just setup-windows-cross` 一次 | 下载 SDK 到 `~/.local/share/herdr/windows-cross/` | 退出码 0 |
| `just setup-env [-- --check/--force]` | 一键环境安装（幂等，已装且有效则跳过）/诊断/覆盖重装钉版 Zig | `--force` 联网重下 | 写仓库内 `.local/toolchains/`（gitignored） | sha256 校验 + `zig version` 0.16.0 |
| `just setup-zig [-- --install/--force]` | 钉版 Zig 0.16.0 工具链安装/诊断（vendored libghostty-vt 构建必需） | 无（--install 联网下载） | 写仓库内 `.local/toolchains/zig/`；build.rs 自动探测 | sha256 校验通过 + `zig version` 输出 0.16.0 |
| `just setup-windows-cross [-- --accept-license]` | 下载 Windows SDK（xwin） | `cargo install xwin --locked` | 下载（仅显式运行） | 脚本成功输出 |
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
| `just docs-contract-test` | docs 快照/版本生命周期工具（bun） | bun test OK |
| `just integration-assets-test` | 捆绑 agent 集成资产（bun） | bun test OK |

## 性能

| 命令 | 用途 | 前置 | 证据 |
|---|---|---|---|
| `just bench-render-scale` | 非门禁全渲染扩展画像（1/15 pane、后台 workspace） | release 构建 | 控制台画像（记录到任务说明） |
| `just bench-release-smoke` | 发布前 CPU 对比（~3–5 分钟；未设 `HERDR_PERF_BASELINE_BIN` 下载 stable） | 网络或本地基线 | 对比结论；显著回归须调查 |

## 发布链（维护者；见 `AGENT_RULES/release-channels.md`）

| 命令 | 用途 | 关键守门 |
|---|---|---|
| `just release-docs-check` | docs/changelog/manifest/config-reference 终稿校验 | diff CHANGELOG、翻译完整、`--require-all-published` |
| `just pre-release-check` | release-docs-check + 两个 bench + skill 提醒 | 全部通过 |
| `just release-prepare <ver>` | 准备 release commit（工作树须干净、tag 未存在、跑 pre-release-check、bump Cargo.toml、同步 changelog） | 产出待审 commit |
| `just release-publish <ver>` | 校验后打 tag 推送（须 master、干净树、origin/master 是祖先） | tag `v<ver>` 触发 release.yml |
| `just release <ver>` | prepare + publish | 同上 |

## 构建辅助

| 命令 | 用途 |
|---|---|
| `just libghostty-bindings [clang_args..]` | bindgen-cli 0.72.1 重新生成 C API 绑定 |
| `just build-libghostty-vt` | 构建 vendored libghostty-vt 源码分发（需 Zig 0.16.0） |
