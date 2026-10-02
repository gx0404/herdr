---
description: AI 工具面与框架自身规则入口
paths:
  - ".agents/**"
  - ".claude/**"
  - ".codex/**"
  - ".pi/**"
  - ".zcode/**"
  - ".zed/**"
  - "docs/AGENT_RULES/**"
  - "docs/AI_TOOLS.md"
  - "docs/graphify/**"
  - "docs/kb/**"
  - "graphify-out/**"
  - "scripts/agent_kb.py"
  - "scripts/build_agent_kb.py"
  - "scripts/graphify.sh"
  - "scripts/graphify_fingerprint.py"
  - "scripts/resolve_agent_rules.py"
  - "scripts/test_agent_kb.py"
  - "scripts/test_ai_tool_hooks.py"
  - "scripts/test_resolve_agent_rules.py"
  - "src/.graphifyignore"
---

本机薄适配：运行 `python3 scripts/resolve_agent_rules.py <paths...>` 并阅读输出；规则正文只在 `docs/AGENT_RULES/ai-tooling.md`。危险模式改动必须重跑 `python3 -m unittest scripts.test_ai_tool_hooks`。
