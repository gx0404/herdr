# testing（测试分层与验证纪律）

范围：`tests/**`、`scripts/test_*.py`（与各领域并集）、`scripts/
run_test_suite.py`、`.config/nextest.toml`、`scripts/
smoke_live_handoff_sessions.sh`。

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
- 维护脚本测试是 `python3 -m unittest scripts.test_<name>` 模块清单（见
  justfile `maintenance-test`），新增脚本测试必须加入清单，否则不会被收集。
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
