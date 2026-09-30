# testing（测试分层与验证纪律）

范围：`tests/**`、`scripts/test_*.py`（与各领域并集）、`scripts/
run_test_suite.py`、`scripts/run_parallel_unittest.py`、`.config/nextest.toml`、
`scripts/smoke_live_handoff_sessions.sh`。

## 命令入口（上游 Testing 全文语义）

默认用 `just` recipe 而非直接 cargo/脚本：

```bash
just test     # cargo nextest + maintenance 脚本测试 + 热路径架构测试 + 资产/docs 契约测试
just check    # 格式检查 + nextest + maintenance + Windows 目标 lint
```

提交前跑 `just check`，除非明确接受更窄的验证；不得绕过失败检查——修好，或
准确说明为何更窄的检查足够。

- 单测贴代码放 `#[cfg(test)] mod tests`；新 `AppState`/`Workspace` 行为必须可用
  `AppState::test_new()` / `Workspace::test_new()` 无 PTY 测试（不变量武器见
  `persistence-session.md`）。
- 维护脚本测试是 `scripts.test_<name>` 模块清单（见 justfile `maintenance-test`），
  新增脚本测试必须加入清单，否则不会被收集。清单由 `scripts/run_parallel_unittest.py`
  按类（慢类按历史耗时再拆块）放进子进程并发跑，所以脚本测试必须跨进程并发安全：
  不写仓库工作树（git 写操作只在唯一临时仓库）、不共用固定临时路径/端口/管道/锁文件。
- 在既有 herdr 会话内测试新构建时，用 `cargo run -- ...` 并清除继承的 socket
  覆盖，让 debug 二进制连 debug `herdr-dev` server：
  `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- <command>`。
- 一次性隔离会话复现用 `herdr-throwaway-repro` skill（`.agents/skills/`），
  不碰默认会话。

## 分层与风险

区分静态检查、单测、集成（`tests/`：api_ping、client_mode、live_handoff、
detach_reattach、cross_area、server_headless 等）、bun 契约测试
（`just docs-contract-test`、`just integration-assets-test`）、性能
（`bench-render-scale` 非门禁、`bench-release-smoke` 发布前）。UI 截图对 TUI
不适用（N/A）：等效证据是 throwaway-repro 真会话 + `scripts/capture_agent_screen.py`
读回。

宽泛重构或发布风险回归先分类风险：触及两个以上核心面、持久化状态、协议/API
ID、workspace/tab/pane 身份、restore/handoff、agent 检测权威或 UI/输入状态投影
即为 refactor-risk——移动代码前指明受保护行为并补 characterization 测试。
宽泛重构与发布风险回归要跑 roundtable；日常局部修复不需要。

## 新测试要求

- 覆盖关键失败路径：超时、断线、取消、旧响应、代际变化、重复操作；拒绝门
  也要测合法输入，防「全部拒绝」假安全。
- 测试真正被入口收集执行：marker 与清单并存时验证两边无漏项；必要工具链缺失
  不得整族 skip 后报绿。
- 拉起脱离进程树的守护进程或监听端口（detached `herdr server`、`sshd`、端口
  转发）的测试不得只靠 `Drop` 清理：测试进程被信号杀死（nextest 超时、Ctrl-C、
  agent 会话中断）时 `Drop` 不执行，孤儿会存活数小时。必须同时具备进程外回收
  （独立会话的收割进程，以 `getppid()` 变化感知属主死亡）与启动时清扫陈旧沙箱
  的兜底；回收按「属于沙箱」判定（环境变量值或可执行文件位于沙箱内），不按
  命令行包含路径判定，避免误杀 `tail -f` 沙箱日志的旁观进程。参考实现
  `tests/ssh_e2e.rs::spawn_orphan_reaper`、`::sweep_stale_roots`、
  `::belongs_to_root`；新增此类测试须演练「运行中途 SIGKILL 进程组」后零残留。
- fixture 是契约（`tests/fixtures/`：endpoint 形状、键盘 TSV、插件 smoke）：
  不得为过门改 fixture；生成物登记与 freshness 纪律见 `docs/README.md`。

## 测试隔离与并发

`just test`（nextest，每测一进程）是标准入口；线程模式的 `cargo test`（同进程多线程）
也须保持可靠。新测试按以下约定写：

- 目录：单测不得读写开发机真实的 herdr 配置/状态目录。需要时用
  `config::test_dirs::isolate_dirs(name)`：本线程的 `config_dir()`、`state_dir()`、
  `config_path()` 与主目录覆盖指向唯一临时根，析构时还原并删除；不改 `XDG_*`。测试构建
  里这三者永不解析到临时目录之外（兜底是共享沙箱 `herdr-unit-sandbox-<user>`，只作安全网，
  不应有东西落进去）。agent 检测的本地覆盖、远端缓存与更新状态未隔离时读为空、写即
  panic，manifest 缓存按线程隔离。覆盖是线程本地的，生产代码自起的线程看不到。
- 环境变量：全 crate 只有一把锁 `config::test_config_env_lock()`（同线程可重入、不中毒，
  最外层 guard 释放时整体还原进程环境）。改环境变量的测试全程持锁，辅助函数把 guard
  交回调用方；持锁时不得等待同样要这把锁的线程。能用线程本地覆盖就不改进程环境：
  `test_dirs::override_home_dir` / `override_search_path` 只影响进程内的查找，要让子进程
  看到仍须持锁改环境，且 PATH 往前追加而不整体替换。
- 命名与清理：临时路径、socket、命名管道名带 `test_dirs::unique_id()`（时间戳在并发线程间
  会撞）。测试建的临时目录在结束时删除（含 panic）：`test_dirs::TempDir` /
  `remove_dir_eventually`（Windows 上刚退出的子进程、杀毒扫描会短暂占着句柄，有界重试）；
  清理前等插件命令、pane 进程退出。测试里删 worktree 走
  `worktree::run_worktree_remove_command_with_recovery`。
- 等待：等异步结果用与负载无关的宽上限（30 s `LOADED_WAIT`，条件满足即返回）；验证
  「不应发生」的短窗口保持短。
- 集成测试（`tests/**`）：每个被拉起的 herdr 都把 `XDG_CONFIG_HOME` 与 `XDG_STATE_HOME`
  设在测试根下，配置写进 `<XDG_CONFIG_HOME>/<app 目录>`（debug 为 `herdr-dev`，用
  `support::app_dir_name()`）；拉起辅助函数清掉 `support::INHERITED_DIR_OVERRIDES`；可能写
  agent/ssh 目录的命令用隔离的 HOME（cli harness 的 `herdr_command`）。
- 泄漏审计：先照常构建，再把 `TEMP`/`TMP` 指向真实临时目录下的空私有目录（Windows 须在
  C: 上，ACL 敏感的 `platform::windows::config_backup` 用例放到 D: 会失败）、
  `APPDATA`/`LOCALAPPDATA` 指向临时目录外的空目录跑全量；跑完私有临时目录里只应剩测试构建
  的 codex shim（`herdr-unit-codex-shim`），资料目录保持为空。
