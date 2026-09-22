# codex 活动树夹具

手写脱敏样本：目录与键名照 codex-cli 0.155.1 的真实 rollout 结构，内容全是假的。
`tree/` 当作 `home` 注入 `src/server/agent_activity/codex.rs` 的测试；根线程
`01a0c6d7-6500-7000-8000-0000000000a1`。

| 文件（`tree/.codex/sessions/2026/09/22/` 下，日期目录与文件名是本地时间） | 用途 |
|---|---|
| `…-0000000000a1.jsonl` | 根线程，`source: "cli"`，本身不是节点 |
| `…-0000000000a2.jsonl` | 子 agent（昵称 Ada / worker），`task_complete` → Done；第 1 行是父 meta 副本 |
| `…-0000000000a3.jsonl` | 子 agent（无昵称 / explorer），最后是 `task_started` → Running |
| `…-0000000000a4.jsonl` | 子 agent（无昵称无角色，`agent_path` 末段作显示名），`turn_aborted` → Failed |
| `…-0000000000a5.jsonl` | 孙线程（父 = a3，depth 2） |
| `…-0000000000a6.jsonl` | 曾孙线程（父 = a5，depth 3；昵称/角色/路径全空 → 显示 id） |
| `…-0000000000a7.jsonl` | guardian 审核线程 `{"subagent":{"other":"guardian"}}` → Background |
| `…-0000000000a8.jsonl` | 另一个顶层线程，不进树 |
| `…-0000000000a9.jsonl` | 首行 JSON 截断，整文件跳过 |
| `…-0000000000b1.jsonl` | 未知 `source` 形态 → kind Unknown，节点保留；夹一行未知事件与一行坏 JSON，尾窗口解析都跳过 |
| `…-0000000000b2.jsonl` | `task_complete.error` 非空 → Failed，summary 取 error.message |
| `…-0000000000b3.jsonl` | 只有 meta，没有任何回合标记 → Pending |
| `…-0000000000b4.jsonl.zst` | 压缩变体：discover 跳过，read 回 Unsupported |
| `…-0000000000b6.jsonl` | 首行是 `event_msg` 而非 `session_meta`，跳过 |
| `…-0000000000b7.jsonl` | 父线程不在树里（孤儿），不进树 |
| `…-legacy-child-0001.jsonl` | 非 UUID id、meta 只有 `id` 与 `parent_thread_id` → 缺字段全 None |
| `2026/09/19/…-0000000000b5.jsonl` | 声称父 = 根但日期早于根：日期下界把它排除 |
