# protocol-api（runtime/API 边界与稳定客户端契约）

范围：`src/protocol/**`、`src/api/**`、`src/server/**`、`src/client/**`、
`src/remote*`、`src/ipc.rs`、`src/cli*`、`src/main.rs`、`src/events.rs`、
`src/handoff_runtime.rs`、`docs/next/api/**`、endpoint 冻结 fixture。

## Runtime/client 边界（上游全文语义）

herdr 正在向 server 拥有的 runtime 协议 + TUI 作为客户端之一迁移；新工作不得
加深当前 server/TUI 耦合。新增状态、API 字段、事件、命令或 socket 消息前先分类：

- **共享 runtime/会话事实**：属于 server 状态，条件允许时经 JSON API/事件路径
  暴露。
- **TUI 呈现状态**：只属于 TUI/客户端层。

不得新增只经私有 TUI 客户端 socket 才能工作的共享行为；用中性的 server/API 命名，
不使用 sidebar/row/card/widget 等 UI 面名称。例：pane/agent 元数据、进程状态、
终端状态、事件 → server/runtime；侧栏布局、token 摆放、颜色、选中态、modal、
鼠标/viewport 状态 → TUI/客户端。workspace/tab/pane 目前仍是共享会话组织
（`persistence-session.md`），但不作为无关 runtime 特性的强制身份。

## 稳定客户端端点契约（上游全文语义）

客户端拥有的 TUI 端点生成独立于私有同安装协议；generation 1 是 Local/SSH/Cloud
连接的兼容底线，除非因安全原因退役必须保持可用：

- 命名核心 codec 不可变：不得增删、重排或重新解释已发布 codec 可达的字段与
  枚举变体；引入新 codec 名并保留旧 codec 作为回退。
- 基线 JSON 握手与快照字段保持必填；新 JSON 字段必须可选或有字段级默认值；新
  枚举值需要旧客户端可安全忽略的 `Unknown` 回退。
- 通过宣告的 API 方法与可选快照数据添加能力；缺失的可选功能只禁用该动作，
  不得拒绝连接。
- 不得改变已宣告端点方法的含义或承重参数形状——旧 server 若会忽略新字段并
  错误地报成功，必须新增方法名或单独宣告的 capability，并在无它时省略该字段。
- 方法缺失、拒绝、超时与 server 不可用是客户端本地结局：不得断开其他兼容
  server，在 pane 中输入不得 dismiss 其通知。
- 冻结 fixture、bincode 摘要、wire-tag 测试与 `tests/fixtures/
  endpoint-method-shapes-v1.json` 是兼容性契约：**永不为了给 wire 变更盖棺而
  更新 generation-1 期望**；创建并协商新 codec 或方法。
- stable 与 preview update manifest 宣告 `endpoint_generation`；保持发布工具链
  对齐，使旧 updater 知道新 server generation 何时真的需要替换。
- 既有值摘要检测不到追加的枚举变体：逐个审查冻结 codec 可达的枚举为
  append-closed，即使测试仍绿。

## 线协议版本

`src/protocol/wire.rs::PROTOCOL_VERSION` 是 wire 协议版本号。协议变更时先与
stable 和 preview 两个渠道已发布的协议对比：当前源码协议已在任一渠道发布且
wire 格式不兼容变更时 bump 一次；该协议发布前不因多次不兼容变更重复 bump。
同步更新测试中的硬编码协议期望与手工 fixture。

## CLI 面

`src/cli*` 是人与 agent 共同的产品入口：命令语义、输出格式（`--json`）与
退出码属于稳定面，变更按用户可见行为处理（文档见 `docs-pipeline.md`）；
`cli/protocol_guard.rs` 防止未协商的协议使用。
