# persistence-session（会话持久化、身份与恢复）

范围：`src/persist*`、`src/session.rs`、`src/workspace*`、`src/worktree.rs`、
`src/agent_resume.rs`、`tests/live_handoff.rs`。

## 身份不变量

- workspace/tab/pane 是共享的会话组织身份（当前仍由 server 拥有，见
  `protocol-api.md` 的边界约束），但不得把三者做成无关 runtime 特性的强制身份。
- 会话保存护栏按 workspace/tab/pane 的公共编号集合追踪布局身份，而不只比较数量；
  pane 级级联退出期间，未显式授权的收缩或等量替换都不写小快照。用户/API 已完成的
  布局变更只按该次同步修改前后的身份差量授权，累积到已有保护基线；排序不授权，
  不用当前全局布局追认先前丢失，清空快照也先过护栏。默认 workspace 替换只在有效
  保护基线与新布局均为单 workspace、单 tab、单 pane 时放行；护栏到期后才接受未授权
  的身份丢失。此前的保存可能延迟，重启仍可恢复旧布局；显式删除会话走独立入口。
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
- 普通 shell 冷恢复的首次 spawn 使用已知 headless 区域与每个 remapped PaneId 的内容尺寸；
  与新建布局共用 BSP/chrome/gutter/clamp 计算，不先统一 24×80 再靠 resize 补救。zoom 焦点
  使用全区、隐藏 leaf 仍按普通布局。原生 agent pending 恢复与 Unix handoff 导入几何不变，
  不增加第二次 ID 分配；验证必须观察首次 spawn 参数而非仅最终画面。
- 持久化写入是 IO 面：禁止在渲染/解析热路径内做持久化分配或写盘（见
  `app-render.md` 乘法路径）；快照序列化保持确定性字段顺序以便 diff 与测试。
- 主布局/history 走同目录独占私有临时文件，准备权限、写入、文件同步后原子替换，再真实
  同步目录。错误须区分替换前和替换后；后者保留新完整文件、清除 unloaded 保护并报告
  耐久性未确认，不能删目标或记成功。history 失败不回滚已提交布局，不承诺跨文件事务。
- 必需 unloaded 备份未确认耐久时不得改主文件/history 或放行 clear；成功备份后的主写失败
  不反复轮换原件。恢复副本用单步不覆盖发布，文件及两层目录同步成功后才裁剪旧副本。
  已发布后的同步失败保留副本；重试前后核对自有句柄身份、摘要及对应源内容，不把 foreign
  或同内容的新对象当作已同步副本。未知状态不解除保护、不 prune；重启不只凭 mtime 跳过同步。
- 保存沿相对/断链解析最终目标，循环和超限拒绝；clear 仍删除请求路径本身，不改为删除
  符号链接目标。权限或附加数据无法安全保留时在替换前拒绝；平台支持边界见 `platform.md`。
