#!/usr/bin/env bash
# Claude/ZCode 协议适配器。策略真源：dangerous_patterns.conf；逻辑：pre_tool_use_gate.py。
set -euo pipefail
exec python3 "$(cd "$(dirname "$0")" && pwd)/pre_tool_use_gate.py" --protocol claude "$@"
