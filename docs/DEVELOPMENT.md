# herdr 开发闭环

## 环境准备

- Rust：`rust-toolchain.toml` 锁定（当前 1.96.1，含 clippy/rustfmt）。
- `just`：命令入口（全表见 `MAKE_COMMANDS.md`）。
- Python 3：维护脚本与其 unittest（建议 ≥3.11；3.10 需 `tomli`，仓库脚本已带
  回退）。
- Bun：docs 契约与集成资产测试（`just docs-contract-test`、`integration-assets-test`）。
- 一键环境：`scripts/setup_env.sh`（`just setup-env`）——检查 cargo/just/python3/bun，
  并把 sha256 钉版 Zig 0.16.0 安装到**仓库内** `.local/toolchains/zig/`
  （gitignored，不写用户全局状态）。安装后裸 `cargo build` 与 `just` 直接可用：
  build.rs 自动探测项目内钉版（优先级 `$ZIG` > 项目内钉版 > PATH）；CI 由
  workflow 的 setup-zig 步骤提供。`--check` 为只读诊断。
- Windows 交叉验证：`cargo install xwin --locked` + `just setup-windows-cross`
  （一次性，详见 `AGENT_RULES/platform.md`）。
- `just install-hooks` 安装 conventional-commit 守门钩子。

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

本 fork（origin `gx0404/herdr`）跟踪 upstream `herdrdev/herdr`：日常开发在
feature 分支；合并上游后按 `AGENT_RULES/README.md` 的章节映射表移植 AGENTS.md
变化到领域文档。对上游的 issue/PR 行为遵守 `AGENT_RULES/governance.md` 守门。
较大特性建议独立 worktree（见 governance 维护者工作流小节的布局约定）。
