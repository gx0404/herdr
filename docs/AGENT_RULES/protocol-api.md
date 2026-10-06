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

客户端连接诊断随内部连接代际隔离：后台 completion 只携带失败分类，不能直接改写诊断表；
`EndpointSupervisors::record_status` 仅在 endpoint 存在且代际匹配后提交。旧成功不能清除新失败，
旧失败不能覆盖新分类；已接受的无分类状态清理旧诊断及对应 shell mirror。内部连接代际不是
下述 generation 1 wire 版本；不因此改变重试、认证批准或画面就绪策略。

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

## 传输边界与公平性预算

- 初始客户端握手必须在 4 秒绝对 deadline 内读完；Windows named-pipe 的分片读取不能
  续期该 deadline。握手首帧使用普通 `MAX_FRAME_SIZE`（2 MiB），同时最多保留 32 个
  握手 worker；Welcome 与连接事件入队后释放握手许可，再进入正常读循环。
- 每客户端 control 车道上限为 4 MiB/4096 条，完全停滞的写入 30 秒后失败，实际写进展
  续期；单条客户端输入上限为 1 MiB。channel 事件和合并的传输补漏通知共享每轮最多 64 条
  预算，不得以补扫绕过限额或让断连越过同客户端已接受的输入。scheduled work 和渲染必须
  有执行机会；这些预算不是 generation 1 wire 兼容字段。
- 端点大响应使用连接持有的有界 cursor：每连接 3 个、server 全局 6 个 bulk 许可，方法执行
  之前准入，许可贯穿 preparing/worker/待发事件/cursor。关联 request ID 后的完整 JSON body
  最多 64 MiB，超限在首块前变为请求级错误，不承诺回滚已执行操作；保留 body 最多 384 MiB
  不等于总 RSS 上限。相同 active ID 不重复执行或产生第二个 final。
  两个异步 unsubscribe 另有每连接 1 个小控制身份，小 body 有独立上限；订阅推送仍是原有
  droppable 语义，不借克隆长期持有一次响应许可。
- 大响应帧最多 512 KiB，队列 Full 只暂停生产，不能当作控制洪泛而 Abort；为普通 control
  预留 2 MiB 加长度头及 64 个槽，不扩总队列。每次 pump 至多一块，每外层循环合计最多四块，
  render/endpoint credit 分开确认，沿用合并通知和 64 事件预算。只有 final 成功入队才清对应
  command/lease；旧 boot、旧 identity、旧 surface revision 不能清新请求或触发旧导航。
  generation 1 无单请求取消消息，不得声称客户端本地超时立即回收 server 端响应。
- 正常关闭先封生产端，保留已接受的 control 及在途帧；Abort 清队列并取消两方向。
  server 最终退出为所有剩余连接提供共享 1 秒排空窗口，到期统一 Abort，再最多等待 1 秒
  取消完成；不得在主事件循环逐客户端 sleep/join，也不得把 Abort 报成成功排空。
  未注册的已握手连接同样受收尾管理，不能仅靠丢弃外部 writer 等读循环自己醒来。
  writer 启动前须在独立于 Connected 事件准入的 registry 登记；登记与最终封门原子互斥，
  活跃项随 worker 完成注销，不维持额外 producer 或历史连接列表。关闭快照在首次 await 前
  归 server 持有，取消 future 或取消超时不得丢失 Drop 所需的连接 owner。

## 线协议版本

`src/protocol/wire.rs::PROTOCOL_VERSION` 是 wire 协议版本号。协议变更时先与
stable 和 preview 两个渠道已发布的协议对比：当前源码协议已在任一渠道发布且
wire 格式不兼容变更时 bump 一次；该协议发布前不因多次不兼容变更重复 bump。
同步更新测试中的硬编码协议期望与手工 fixture。

## CLI 面

`src/cli*` 是人与 agent 共同的产品入口：命令语义、输出格式（`--json`）与
退出码属于稳定面，变更按用户可见行为处理（文档见 `docs-pipeline.md`）；
`cli/protocol_guard.rs` 防止未协商的协议使用。
