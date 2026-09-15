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
| `app/` | TUI 侧 `AppState` 纯状态 + actions/input 分层；`app/api/` 是对 server API 的客户端调度 |
| `ui/` | 纯渲染组件（sidebar、status、widgets、onboarding） |
| `pane/` | `PaneState` 与 pane 级终端宿主逻辑（光标、OSC、键盘协议） |
| `terminal/` | 终端核心状态与 runtime 注册（与 PTY/渲染解耦） |
| `pty/` | PTY actor 与平台 backend（进程生命周期） |
| `ghostty/` | vendored libghostty-vt 的绑定层（`bindings.rs` 由 bindgen 生成） |
| `detect/` | agent 状态检测：读屏幕快照的规则引擎 + `manifests/`（22 个 agent TOML） |
| `api/` | socket API：schema（schemars → `docs/next/api/herdr-api.schema.json`）、订阅、事件 hub |
| `protocol/` | 客户端/server 线协议（`wire.rs::PROTOCOL_VERSION`）、端点 codec、surface 复用 |
| `server/` | runtime 编排：client 接入/命令、render stream、handoff、autodetect |
| `client/` | attach 传输、事件循环、shell UI、endpoint 激活与 supervisor |
| `workspace/`、`persist/`、`session.rs` | 会话组织（workspace/tab/pane）与快照/恢复 |
| `platform/` | OS 隔离层：`<os>.rs` + 共享契约（`mod.rs`） |
| `integration/` | 各编码 agent 的宿主配置集成（settings/MCP/hooks 写入） |
| `config/` | 配置模型真源（keybinds/theme/sidebar…） |
| `input/` | 键绑定模型、编码与帮助 |
| `kitty_graphics/` | 图形协议宿主侧 |
| `remote/` | SSH 远程机器与 attach |
| `update.rs`、`release_notes.rs`、`product_announcements.rs` | 更新渠道客户端与发布叙事 |

## 状态所有者

- **server 拥有**：会话/工作区/标签/pane 身份、终端状态、进程状态、agent 检测
  状态、事件流（经 JSON API/事件暴露）。
- **TUI 客户端拥有**：viewport/滚动、选区、modal/overlay、主题呈现、鼠标状态、
  sidebar 布局。
- `AppState`（纯数据）↔ `PaneState` ↔ `PaneRuntime` 的拆分让 TUI 状态可无 PTY
  测试（`AppState::test_new()`）。

## 关键数据流

按键 → 客户端输入编码（`input/`）→ server `pane_input` → PTY actor →
libghostty-vt 解析（逐字节热路径）→ 终端状态 →（隐藏 pane 仍解析但不触发呈现）
→ 检测快照评估（`detect/`）→ render stream / retained surface → 客户端帧输出。
反向：agent 输出 → 检测状态机 → sidebar 状态 / 通知 → API 事件订阅者。

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
