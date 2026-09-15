---
description: app/ui/pane 状态与渲染热路径规则入口
paths:
  - "src/app/**"
  - "src/ui.rs"
  - "src/ui/**"
  - "src/pane.rs"
  - "src/pane/**"
---

本机薄适配：对本次全部触及路径运行 `python3 scripts/resolve_agent_rules.py <paths...>` 并阅读输出；领域规则正文只在 `docs/AGENT_RULES/app-render.md`，此处不复制。
