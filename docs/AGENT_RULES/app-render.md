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
渲染扩展用 `just bench-render-scale`，终端目标查找、BSP 布局、retained graphics
和 API 批处理公平性分别用 `just bench-terminal-targets`、`just bench-bsp-layout`、
`just bench-retained-graphics`、`just bench-api-fairness`；这些画像不替代行为测试。
发布前 `just bench-release-smoke` 必须在隐藏与可见输出两种形态下与当前 stable
二进制对比；结果显著移动或验证性能工作时，用 `HERDR_PERF_SAMPLE_SECONDS=60`
复测并调查受影响场景。`scripts/test_ui_hot_path_architecture.py`（`just
ui-hot-path-architecture-test`）是热路径边界的确定性守门，新增违规用法会红。

## 设计语言

- 默认 Graphite 的结构面使用中性色阶，选择靠底色、粗体和焦点标记，不借用状态色。
  用户显式配置的工作区/机器身份色可以保留；旧主题与 `dots` / `symbols` 指示风格不得丢失。
- 状态桶唯一真源为 `src/harness/bucket.rs::AgentBucket`，字形/状态色/文字层级为
  `src/ui/design.rs`。需要输入 = `◆` / `status_input`，失败 = `✕` / `status_failed`，
  运行中 = 同相位盲文转圈 / `status_running`，待查看 = `●` / `status_review`，
  已查看空闲 = `○` / `status_idle`；未知不等于空闲、不参与汇总。
- 子智能体活动状态统一经 `design::activity_bucket`（`src/ui/design.rs`）映射到状态桶，再取
  共用字形与颜色；子智能体页面、Inspector 与会话树不得另建映射。
- 工作区/标签的汇总顺序为需要输入、失败、运行中、待查看、空闲；按状态列表把待查看
  放在运行中之前。不得在不同页面另建颜色或优先级映射。
- 转圈共用客户端时钟，关闭动画/转圈或离线缓存时保持静态；server 窗格标题使用静态
  字形，不能为转圈推动 pane 重渲染。相对分钟标签不逐秒请求重绘。
- 标题/正文/说明/元信息分别使用粗体 `text`、`text`、`subtext0`、`overlay1`；
  键帽显示实际配置的按键。截断/折行按显示宽度处理，不能拆开 CJK 或组合字符。
- 浮层边框统一 `design::frame` 的中性色；危险语义放标题和操作按钮。选中行用
  `design::selected_row_text` 加首列 `kit::paint_selection_mark`（`▌`），悬浮只换底色。
  `overlay0` 留给结构线、禁用和未知占位，不承担可读说明正文。
- 键帽绘制与量宽共用 `ui::kit::key_cap`，说明用 `design::key_label`；显示转换经
  `ui::key_labels`，不改配置序列化。页脚提示间距为 2 格、状态栏为 4 格。相对时间与
  运行时长分别复用 `design::ago_label`、`design::duration_label`。
- 短暂操作结果走客户端通知；`ClientToastLevel::Warning` 表达需要处理的琥珀告警，
  拒绝/失败用 Error。吐司避开标题栏、状态栏与机器页脚，并保持独立焦点和遮挡登记。
- 监控图表允许分类色：仅曲线/条形使用 `mauve`、`teal`、`peach` 或中性色，
  不使用蓝/绿/黄/红状态色；标题和正常读数保持中性，阈值告警仍用黄/红。
  主题若将分类色映射到状态色或对比不足，`observability::render::tone` 回退中性色。
- 新 fork 外壳视觉集中于 `src/client/shell/chrome/`，已有渲染文件只保留薄调用。
  行模型与滚动/reveal 在计算阶段更新，渲染只读；同一投影几何必须同时用于绘制和
  `client.views.set` 请求，窄屏/显隐不得另算终端尺寸。
- 覆盖终端的浮层必须登记 `occlusion.cover`、清掉覆盖区链接并处理光标归属；指针悬浮
  不改键盘选择。同一控件内无状态变化的移动不重复请求重绘。

## 客户端外壳投影

- Sessions 行模型归 `client::shell::chrome::sessions::SessionRowsCache`；机器/工作区/
  标签/pane 与外部来源共用一份导航投影，不恢复工作区/Agents 双列表。
  `endpoint_agents::AgentAttentionCache` 只维护在线计数与动画标志，不重建 legacy tree。
- `workbench::State::project_geometry` / `dock::DockLayout` 是几何真源，绘制、命中与
  `client.views.set` 复用，正文统一经 `workbench::body`。Arrange 必须取
  `DockLayout::projected_focus`，窄屏隐藏 Inspector 后不能对 raw focus 操作。
  布局 v2 与 `DockLayout::migrate_v1`、旧面板身份的解码兼容不得因重命名删掉。
- `chrome::inspector::State` 的活动树按目标缓存：`TREE_CACHE` 为 8 个目标，
  `TREE_NODES` 为每目标 512 个节点，`TREE_BYTES` 为每目标节点文本合计 128 KiB；
  `TIMELINE_AGENTS` / `TIMELINE_ENTRIES` 为 128 个 agent 各最多 8 条状态转换。
  这些常量均位于 `client::shell::chrome::inspector`，不是整个客户端 RSS 上限。
  只在可见、在线、无浮层且无在途读取时跟随；切目标/隐藏使旧读取代次失效，不能在
  renderer 读文件、查询进程或发起树读取。
- Sessions/Inspector/overview 的行、cache key、滚动与 reveal 在计算/事件阶段维护，
  paint 只读。失效键覆盖配置、快照/boot、端点在线状态、折叠与活动变化；分钟标签不
  为逐秒年龄重新构树。只为注意力计数扩大 pane 级工作量同样须过乘法路径守门。
- 设置导航以 `state::ClientSettingsSection::ALL` 为真源，八个分区在窄屏仍可达；
  `chrome::settings::{settings_layout,nav_items,list_body}` 共用几何。主题及别名以
  `config::theme::{DEFAULT_THEME_NAME,THEME_NAMES,canonical_theme_name}` 为准，默认
  Graphite 随宿主明暗适配，不能删除旧主题或旧状态指示风格。
- `chrome::command` 不另建动作注册表；菜单/搜索复用 `action_table` 的可用性。
  `command_palette::ClientShellState::activate_palette_item` 的当前目标与 MarkUnread
  状态/容量检查必须先于关闭面板与 MRU 写入，缓存的 enabled 不能替代执行时复核。
  通知的键盘焦点独立于 pane：显式聚焦才消费 Enter/Esc，普通输入不清通知。

## 测试入口

新 `AppState` / `Workspace` 行为必须能用 `AppState::test_new()` /
`Workspace::test_new()` 无 PTY 测试（详见 `testing.md`）；客户端 shell 的投影
测试在 `src/client/shell/tests/` 就近维护。
