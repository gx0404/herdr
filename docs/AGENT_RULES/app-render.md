# app-render（应用状态与渲染热路径）

范围：`src/app/**`、`src/ui*`、`src/pane*`、布局/选区/弹窗/主题等纯展示逻辑，
以及 `scripts/test_ui_hot_path_architecture.py` 架构守门。

## 架构不变量（来自上游 Principles，违反即退回）

- **状态与运行时分离**：`AppState` 是纯数据（`src/app/state.rs`），可脱离 PTY
  与异步测试；`PaneState` 与 `PaneRuntime` 分离；workspace 逻辑不依赖真实终端。
- **渲染纯函数**：`compute_view()` 负责几何与变更；`render()` 只取 `&AppState`
  画图。渲染期间禁止任何状态变更。
- **禁止上帝对象**：模块做过宽就拆；`app/` 已按 state/actions/input 分层，保持。
- **UI 模式复用**：herdr 是 mouse-first TUI；新对话框、onboarding、设置与更新
  后流程复用既有 modal/screen 结构、交互与关闭方式，不发明一次性屏幕。
- 平台相关渲染差异走 `src/platform/`（见 `platform.md`），不在本域写 cfg。

## 乘法性能路径（上游全文语义）

视图计算、渲染、后台 pane 重排、PTY 解析、检测、客户端帧扇出是乘法路径。
改动前先定性频率与基数：每字节/每事件/每渲染 × pane/tab/workspace × 挂载客户端。

pane 规模的渲染与布局循环内：

- 使用窄终端状态访问器；不收集聚合输入状态、不格式化终端快照、不查进程树、
  不做文件系统 I/O；一个标量事实足够时禁止分配。
- 终端核心锁的持有时长保持最短。
- 保留 hidden-source 与 retained-render 早退：隐藏 pane 仍解析输出，但其输出
  不得仅为「保持终端/检测状态最新」而触发呈现工作。
- 在这些循环里加宽工作量的改动，必须用固定几何 1 个与 ≥15 个已填充 pane 做
  扩展测试并报告增量；用 `just bench-render-scale` 同时覆盖后台 workspace 与
  活动 pane 基数。

优先确定性操作/架构测试而非墙钟 CI 限制；性能基准是行为覆盖的补充证据。
发布前 `just bench-release-smoke` 必须在隐藏与可见输出两种形态下与当前 stable
二进制对比；结果显著移动或验证性能工作时，用 `HERDR_PERF_SAMPLE_SECONDS=60`
复测并调查受影响场景。`scripts/test_ui_hot_path_architecture.py`（`just
ui-hot-path-architecture-test`）是热路径边界的确定性守门，新增违规用法会红。

## 测试入口

新 `AppState` / `Workspace` 行为必须能用 `AppState::test_new()` /
`Workspace::test_new()` 无 PTY 测试（详见 `testing.md`）；客户端 shell 的投影
测试在 `src/client/shell/tests/` 就近维护。
