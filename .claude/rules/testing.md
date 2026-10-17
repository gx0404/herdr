---
description: 测试纪律规则入口
paths:
  - "tests/**"
  - "scripts/test_*.py"
  - "scripts/run_test*.py"
  - "scripts/run_nextest.py"
  - "scripts/run_parallel_unittest.py"
  - ".config/test-suites.toml"
  - ".config/nextest.toml"
---

本机薄适配：运行 `python3 scripts/resolve_agent_rules.py <paths...>` 并阅读输出；规则正文只在 `docs/AGENT_RULES/testing.md`。分层与入口见 `docs/TESTING.md`。
