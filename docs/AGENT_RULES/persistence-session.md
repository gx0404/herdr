# persistence-session（会话持久化、身份与恢复）

范围：`src/persist*`、`src/session.rs`、`src/workspace*`、`src/worktree.rs`、
`src/agent_resume.rs`、`tests/live_handoff.rs`。

## 身份不变量

- workspace/tab/pane 是共享的会话组织身份（当前仍由 server 拥有，见
  `protocol-api.md` 的边界约束），但不得把三者做成无关 runtime 特性的强制身份。
- 身份是持久化与恢复的键：ID 生成（`src/app/ids.rs`）、快照（`persist/snapshot.rs`）
  与恢复（`persist/restore.rs`）必须对「旧数据、旧 ID、跨版本快照」保持兼容；
  破坏兼容需要显式迁移与回滚说明。

## 重构风险与测试武器

本域触及两个以上核心面、持久化状态、workspace/tab/pane 身份或恢复/交接时按
refactor-risk 处理（流程见 `testing.md`）：移动代码前先指明受保护行为并补
characterization 测试；身份/状态重构用测试专用不变量断言：

- `AppState::assert_invariants_for_test()` /
  `Workspace::assert_invariants_for_test()`
- 对抗状态：`AppState::test_with_adversarial_identity_state()` /
  `Workspace::test_adversarial_identity_state()`

覆盖超时、断线、旧响应、代际变化与重复恢复等失败路径；`tests/live_handoff.rs`
与 `src/handoff_runtime.rs`（后者归 `protocol-api.md` 域）共同守护 handoff 语义，
改恢复逻辑必须让 live handoff 集成测试跑过。

## 恢复相关守门

- agent resume（`src/agent_resume.rs`、`src/app/agent_resume.rs`）恢复的是
  「用户可见的 agent 会话上下文」，与检测状态（`detection.md`）解耦。
- 持久化写入是 IO 面：禁止在渲染/解析热路径内做持久化分配或写盘（见
  `app-render.md` 乘法路径）；快照序列化保持确定性字段顺序以便 diff 与测试。
