#!/usr/bin/env bash
# Stop 适配器：写 .claude/review/READY.json 供跨工具复审 watcher 消费。
set -euo pipefail
exec python3 "$(cd "$(dirname "$0")" && pwd)/notify_review.py" "$@"
