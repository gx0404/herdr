# herdr 架构地图

面向贡献者的结构性事实；规范性规则见 `docs/AGENT_RULES/` 对应领域文档。

## 进程与角色

单一二进制 `herdr`，三种运行角色：

- **server**（`src/server/`）：拥有会话与 runtime。接受 TUI/CLI 客户端连接
  （本机 socket / Windows named pipe；远程经 `src/remote/` SSH），管理 pane 的
  PTY、终端状态、检测、持久化与 handoff；headless 渲染在 `server/headless/`。
- **TUI 客户端**（`src/client/`）：attach 到 server，负责呈现（shell 组合、
  sidebar、overlay、copy mode、输入/鼠标）与客户端侧端点协商
  （`client/endpoint/`）。呈现状态只属于客户端（边界见
  `AGENT_RULES/protocol-api.md`）。
- **CLI**（`src/cli/`、`src/main.rs`）：人与 agent 的操作面；命令语义即产品
  API 的一部分。

## 模块地图（src/）

| 模块 | 职责 |
|---|---|
| `app/` | `App` 与纯数据 `AppState`；服务器 API 方法处理、会话动作及旧应用输入/渲染兼容逻辑 |
| `ui/` | 纯渲染组件；`design.rs` 提供状态字形/颜色、文字层级和键帽，`kit/` 提供共用控件 |
| `pane/` | `PaneState` 与 pane 级终端宿主逻辑（光标、OSC、键盘协议） |
| `terminal/` | 终端状态与 runtime 注册；`turn.rs` 记录轮次、注意力和中断，不决定检测状态 |
| `pty/` | PTY actor 与平台 backend（进程生命周期） |
| `ghostty/` | vendored libghostty-vt 的绑定层（`bindings.rs` 由 bindgen 生成） |
| `detect/` | 只读屏幕快照的规则引擎；五家窗格 harness 的捆绑 manifest 与 Codex/Kimi fork 叠加规则，ZCode 为外部来源 |
| `harness/` | 六家注册表、统一状态桶、工具展示/文本/时间/JSON、有界尾读缓存、子进程与洞察组装 |
| `api/` | socket API：schema（schemars → `docs/next/api/herdr-api.schema.json`）、订阅、事件 hub |
| `protocol/` | 客户端/server 线协议（`wire.rs::PROTOCOL_VERSION`）、端点 codec、surface 复用 |
| `server/` | runtime 编排；`agent_activity/` 适配会话数据，`attention.rs` 集中通知决策，`observability/` 采集主机资源 |
| `client/` | attach 传输、事件循环、端点协商；`shell/chrome/` 集中 fork 外壳视觉，`shell/` 拥有客户端呈现与交互状态 |
| `workspace/`、`persist/`、`session.rs` | 会话组织（workspace/tab/pane）与快照/恢复 |
| `platform/` | OS 隔离层：`<os>.rs` + 共享契约（`mod.rs`） |
| `integration/` | 各编码 agent 的宿主配置集成（settings/MCP/hooks 写入） |
| `config/` | 配置模型真源（keybinds/theme/sidebar…） |
| `input/` | 键绑定模型、编码与帮助 |
| `kitty_graphics/` | 图形协议宿主侧 |
| `remote/` | SSH 远程机器与 attach |
| `update.rs`、`release_notes.rs`、`product_announcements.rs` | 更新渠道客户端与发布叙事 |

## 状态所有者

- **server 拥有**：会话/工作区/标签/pane 身份、终端与进程状态、agent 有效生命周期、
  注意力载荷、轮次计时、会话洞察及事件流（经 JSON API/快照暴露）。
- **TUI 客户端拥有**：viewport/滚动、选区、modal/overlay、主题、鼠标/键盘选择、
  停靠布局、Inspector 预览目标、通知键盘焦点，以及本客户端的已读/未读投影。
  这些本地偏好不改写检测权威，也不代表 server 已取消请求或改变通知结局。
- `AppState` 与 `PaneState` 是数据；`TerminalRuntimeRegistry` 持有活终端运行时，
  `TerminalRuntime` 当前委托 pane runtime 实现。状态/布局测试可不启动 PTY。

## 关键数据流

按键 → 客户端输入编码（`input/`）→ server `pane_input` → PTY actor →
libghostty-vt 解析（逐字节热路径）→ 终端状态 →（隐藏 pane 仍解析但不触发呈现）
→ 检测快照评估（`detect/`）→ render stream / retained surface → 客户端帧输出。

harness 的屏幕/进程/既有钩子信号 → `TerminalState` 有效状态仲裁 →
`TurnTracker` 轮次与注意力记账 → `server::attention` 通知决定及 API/客户端投影。
后台 `server::agent_activity` 按文件与子进程两条发现车道读取有界会话数据，
输出活动树和 `AgentInsight`；洞察缓存指纹未变时不重复解析，慢查询不阻塞文件来源。
有错误洞察能力且已绑定会话的完成等待匹配读取或
`server::attention::COMPLETION_INSIGHT_WAIT`（1.5 秒）后决定单次结局，
不在 PTY/渲染热路径同步等待。Pi 快照目前仅提供当前工具，ZCode 是外部只读来源。

绘制消费快照、纯布局与缓存的行模型；点击产生已宣告的 API 请求，缺少可选方法的旧
端点保留兼容连接并禁用相应动作。标题栏/状态栏/面板头位于客户端 chrome，pane
内容和静态状态标题由 server 绘制。主机监控仍由独立资源采集服务提供。

## 客户端桌面外壳

桌面化仍是 attach 到 server 的 TUI，不另建 runtime 或原生 GUI。

- **Sessions**：`client::shell::chrome::sessions::SessionRowsCache` 统一机器、工作区、
  标签、pane agent、普通终端与外部来源的行模型，支持按工作区或状态分组；窄屏投影为
  rail，不再绘制工作区/Agents 双列表。`endpoint_agents::AgentAttentionCache` 只缓存
  在线状态桶计数与动画标志，不为计数或时钟重新构建旧活动树。
- **停靠与几何**：`workbench::State::project_geometry` 与 `dock::DockLayout` 提供
  绘制、命中和 `client.views.set` 共用的投影，正文尺寸由 `workbench::body` 扣除面板头。
  保存布局为 v2；`DockLayout::migrate_v1` 保留合法旧布局并迁移右侧 Inspector。
  持久化标识 `PanelId::Workspaces` / `PanelId::Agents` 分别承载 Sessions / Inspector，
  不因界面改名重写身份。Arrange 使用 `DockLayout::projected_focus`，不会操作窄屏已隐藏
  Inspector 的原始焦点。
- **Inspector**：`chrome::inspector::State` 展示当前或显式预览的单个 pane agent，
  只读洞察、状态转换与按需活动树。`Target` 用端点、boot、pane 与 launch 身份隔离缓存，
  读取代次另行隔离响应；隐藏、离线或有浮层时不启动跟随读取。缓存与时间线预算见
  `AGENT_RULES/app-render.md`，文件/子进程读取始终在 server 后台而非 renderer。
- **总览**：`chrome::overview` 复用 Navigator 浮层，提供状态/位置分组、搜索、预览、
  跳转与本地已读标记；只有在线 Idle/Review pane agent 且宣告 `agent.prompt` 时可显式
  回复。提交走共享 API 且 `wait: None`，不会代答权限阻塞；超时保留草稿并提示可能已提交。
  已读标记以端点存储键及 boot/pane/launch/state-change sequence 保存，首个快照前保留
  尚未导入的远端偏好，停用/退役端点清理，导入后按当前身份重新校验。
- **设置与入口**：`state::ClientSettingsSection::ALL` 是八个设置分区的真源，
  `chrome::settings` 的宽屏导航/窄屏标签与正文共享几何。主题来自
  `config::theme::{DEFAULT_THEME_NAME,THEME_NAMES}`：默认自适应 Graphite，保留旧主题与
  `dots` / `symbols`。`chrome::welcome` 在在线空工作区提供目录、工作区和帮助入口，
  复用既有动作；`chrome::command` 提供描述与视觉，命令语义仍归 `action_table`。
  `command_palette::ClientShellState::activate_palette_item` 在关闭面板、写 MRU 前复核
  目标，MarkUnread 还复核在线状态桶与标记容量。
- **通知与监控**：`chrome::notification` 绘制通知，客户端显式通知焦点才消费 Enter
  跳转或 Esc 忽略；普通终端输入不因此 dismiss。监控通过 `system.*` API 消费主机资源与
  进程数据，不是 agent 账号监控。旧 `PanelId::Accounts` 仅解码后移除，旧
  `monitor_tab = "accounts"` 宽松回落到系统页；不删除这些迁移兼容入口。

## 关键契约面

线协议 `PROTOCOL_VERSION`（`protocol/wire.rs`）、稳定端点 generation
（`client/endpoint/`，冻结 fixture `tests/fixtures/endpoint-method-shapes-v1.json`）、
API JSON schema（`api/schema`）、检测 manifest（`src/detect/manifests` ↔
`distribution/agent-detection`）、配置参考（`src/config` ↔ config-reference.json）。
变更规则见 `AGENT_RULES/protocol-api.md` 与 `detection.md`。

## 性能模型

乘法路径 = 每字节/每事件/每渲染 × pane/tab/workspace × 挂载客户端。渲染分层
（`server/render_stream.rs`、retained surface、`render_prof.rs`）与检测快照化是
主要手段；扩展验证用 `just bench-render-scale`（1 与 ≥15 pane）。规则全文见
`AGENT_RULES/app-render.md`。

## 结构查询

代码关系用图谱查（节点/边来自 `src/` Rust 提取）：
`scripts/graphify.sh query|path|explain "<问题>"`；重建与一致性见
`docs/README.md` 生成物登记。
