---
description: 文档快照与发布渠道规则入口
paths:
  - "docs/**"
  - "distribution/**"
  - "CHANGELOG.md"
  - "README.md"
---

本机薄适配：运行 `python3 scripts/resolve_agent_rules.py <paths...>` 并阅读输出；规则正文在 `docs/AGENT_RULES/docs-pipeline.md` 与 `release-channels.md`。多数发布文件对 feature 工作是只读的。
