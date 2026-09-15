#!/usr/bin/env bash
# herdr 一键环境安装（csm setup_env 模式）：检查必备工具链，安装钉版 Zig 到
# 仓库内 .local/toolchains/（gitignored，不写任何用户全局状态）。安装后裸
# `cargo build` 与 `just` 直接可用（build.rs 自动探测项目内钉版，$ZIG 仍可覆盖）。
#
# 用法：scripts/setup_env.sh            安装/补齐（幂等）
#       scripts/setup_env.sh --check    只读诊断，不安装任何东西
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE="${1:-install}"

fail=0
note()  { printf '[setup-env] %s\n' "$*"; }
ok()    { printf '[setup-env] OK %s\n' "$*"; }
miss()  { printf '[setup-env] 缺少 %s — %s\n' "$1" "$2"; fail=1; }

if [ "$MODE" = "--check" ]; then
    command -v cargo   >/dev/null 2>&1 && ok "cargo $(cargo --version | awk '{print $2}')" \
        || miss cargo "安装 rustup（https://rustup.rs）；本脚本不代装系统级工具"
    command -v rustc   >/dev/null 2>&1 && ok "rustc $(rustc --version | awk '{print $2}')" || miss rustc "随 rustup 安装"
    command -v just    >/dev/null 2>&1 && ok "just $(just --version | awk '{print $2}')" \
        || miss just "cargo install just 或下载预编译版到 PATH"
    command -v python3 >/dev/null 2>&1 && ok "python3 $(python3 --version | awk '{print $2}')" || miss python3 "系统包管理器安装（>=3.10，3.10 需 tomli）"
    command -v bun     >/dev/null 2>&1 && ok "bun $(bun --version)" \
        || note "bun 可选（docs 契约与集成资产测试需要；https://bun.sh）"
    python3 "$ROOT/scripts/setup_zig.py" --check
    [ "$fail" -eq 0 ] && echo "[setup-env] 诊断通过" || echo "[setup-env] 存在缺项（见上）"
    exit "$fail"
elif [ "$MODE" != "install" ]; then
    echo "用法：scripts/setup_env.sh [--check]" >&2
    exit 2
fi

command -v cargo   >/dev/null 2>&1 || miss cargo "安装 rustup（https://rustup.rs）；本脚本不代装系统级工具"
command -v python3 >/dev/null 2>&1 || miss python3 "系统包管理器安装（>=3.10，3.10 需 tomli）"
command -v just    >/dev/null 2>&1 || note "just 未安装（可选；cargo 仍可直接编译）"
command -v bun     >/dev/null 2>&1 || note "bun 未安装（可选；docs 契约测试需要）"
if [ "$fail" -ne 0 ]; then
    echo "[setup-env] 必备项缺失，先按上面的提示安装后重试" >&2
    exit 1
fi

python3 "$ROOT/scripts/setup_zig.py" --install
echo "[setup-env] 完成：cd $ROOT && cargo build --release   # 直接编译"
