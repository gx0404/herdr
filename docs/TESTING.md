# herdr 测试分层

## 入口与分层

| 层 | 入口 | 证明 / 不证明 |
|---|---|---|
| 格式与静态 | `just lint`（fmt + clippy -D warnings）、`just windows-lint` | 格式与静态契约 / 真实行为 |
| Rust 单测（贴代码） | `cargo nextest`（`just test` 并行编排、`just nextest-all`、`just test-one <filter>`） | 边界与纯状态逻辑（`AppState::test_new()` 等）/ 真实装配 |
| 集成（`tests/`） | `just test`（api_ping、client_mode、live_handoff、detach_reattach、cross_area、multi_client、remote_attach、machine_api…） | 入口、生命周期、多客户端交接 / 真实外部机器 |
| 维护脚本契约 | `just maintenance-test`（`python3 -m unittest scripts.test_*` 清单） | changelog/preview/manifest/翻译/打包脚本行为 |
| UI 热路径架构 | `just ui-hot-path-architecture-test` | 渲染热路径边界（确定性）/ 运行时性能 |
| bun 契约 | `just docs-contract-test`、`just integration-assets-test` | docs 生命周期与集成资产 / Rust 行为 |
| 框架守门 | `just agent-rules-check`、`just framework-check`、`just graph-check`、`just kb-check` | 规则闭集、hook 探针、图谱/KB 新鲜度 |
| 性能 | `just bench-render-scale`（非门禁）、`just bench-release-smoke`（发布前，vs stable 基线） | 声明条件下的扩展与耗时 / 普遍结论 |

CI（`.github/workflows/ci.yml`）：Linux `just lint` + `just ci-tests`（分片）；
macOS `just ci`；ghostty 专项；Windows `just check` + ConPTY 冒烟与打包；
conventional-commit 检查独立 job。PR 政策门在 `pr-gate.yml`（无测试）。

## 纪律要点（规则全文见 `AGENT_RULES/testing.md`）

- 新脚本测试必须加入 justfile `maintenance-test` 的模块清单，否则不会被收集。
- 失败路径优先：超时、断线、取消、旧响应、代际变化、重复操作；拒绝门也要测
  合法输入（本仓库实践：resolver/hooks/KB 的每个拒绝项都有独立注入测试）。
- 必要工具链缺失不得整族 skip 后报绿；跑不了就明确 PENDING。
- fixture 是契约：不为过门修改期望；生成物 freshness 失败先查输入是否有意。
- 会拉起守护进程的测试必须能在被强杀后自清理：`Drop` 在 SIGKILL/SIGTERM 下
  不执行。`tests/ssh_e2e.rs` 用独立会话的收割进程接管清理，并在下次启动时清扫
  陈旧沙箱；排查残留用 `ls -d /tmp/herdr-ssh-e2e-*` 与
  `pgrep -af herdr-ssh-e2e`，正常应为空，重跑该测试即自愈。
- `tests/cli` 的临时目录是 `/tmp/hcli-<pid>-<纳秒>`。拉起 server 的用例结尾用
  `cleanup_spawned_herdr` / `cleanup_test_base` 先收掉 server 再删目录（失败时
  走不到收尾，目录留作现场）；不拉起 server 的用例用 `harness::TestDirGuard`，
  离开作用域（含断言失败）即删。要保留后一类的现场，设 `HERDR_TEST_KEEP_DIRS=1`
  （守卫只在 stderr 报路径、不删）；该开关只对用 `TestDirGuard` 的用例生效。
  全量跑完应不新增 `/tmp/hcli-*`，有残留先查是哪条用例没收尾。
- **UI 截图：N/A**——herdr 是 TUI。等效证据 = `herdr-throwaway-repro` 隔离会话
  中真实操作 + `herdr agent read`/`capture_agent_screen.py` 读回；键盘/终端
  行为用 `tests/fixtures` 的实测 TSV 语料。
- 在 herdr 内测 herdr：`env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH
  cargo run -- <command>`，避免污染安装版会话。
