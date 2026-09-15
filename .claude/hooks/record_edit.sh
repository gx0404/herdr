#!/usr/bin/env bash
# PostToolUse 适配器：记录 Edit/Write 到 .claude/live-edits.log（本机文件）。
set -euo pipefail
exec python3 "$(cd "$(dirname "$0")" && pwd)/record_edit.py" "$@"
