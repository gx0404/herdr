#!/usr/bin/env bash
# herdr Graphify 固定入口：只索引 src/（Rust AST，无 LLM），全量重建。
# 子命令：rebuild|check|query|path|explain|diagnose|affected|export|save-result|reflect

set -euo pipefail

WRAPPER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${WRAPPER_DIR}/.." && pwd)"
GRAPH_DIR="${ROOT}/graphify-out"
GRAPH_JSON="${GRAPH_DIR}/graph.json"
REPORT="${GRAPH_DIR}/GRAPH_REPORT.md"
MIRROR_REPORT="${ROOT}/docs/graphify/GRAPH_REPORT.md"
PINNED_VERSION="0.9.20"
# 输出目录由项目控制，避免继承外部 GRAPHIFY_OUT。
export GRAPHIFY_OUT=graphify-out

die() {
    echo "[graphify] ERROR: $*" >&2
    exit 1
}

resolve_cli() {
    local configured="${HERDR_GRAPHIFY_CLI:-}"
    local cli
    if [ -n "${configured}" ]; then
        if [[ "${configured}" == */* ]]; then
            [ -x "${configured}" ] || die "HERDR_GRAPHIFY_CLI 不可执行：${configured}"
            cli="${configured}"
        else
            cli="$(command -v "${configured}" 2>/dev/null)" || die "找不到 HERDR_GRAPHIFY_CLI：${configured}"
        fi
    else
        cli="$(command -v graphify 2>/dev/null)" \
            || die "未安装 graphify；请执行 uv tool install graphifyy（不自动安装）"
    fi
    if [ "${HERDR_GRAPHIFY_ALLOW_ANY_VERSION:-0}" != "1" ]; then
        local version
        version="$("${cli}" --version 2>/dev/null | awk '{print $2}')"
        [ "${version}" = "${PINNED_VERSION}" ] \
            || die "graphify 版本 ${version:-unknown} != 固定 ${PINNED_VERSION}；升级前先在临时副本比对节点稳定性（或设 HERDR_GRAPHIFY_ALLOW_ANY_VERSION=1 明确放行）"
    fi
    printf '%s\n' "${cli}"
}

reject_graph_override() {
    local arg
    for arg in "$@"; do
        case "${arg}" in
            --graph|--graph=*) die "禁止覆盖项目固定图谱：${GRAPH_JSON}" ;;
        esac
    done
}

rebuild() {
    local cli="$1"
    # 固定 hash seed，避免社区划分与报告随解释器随机漂移（与 hmi3 实践一致）。
    export PYTHONHASHSEED=0
    cd "${ROOT}"
    # 排除规则在 src/.graphifyignore（生成物不进图）。
    "${cli}" extract src --out . --force --code-only --no-cluster --max-workers 4
    [ -f "${GRAPH_JSON}" ] || die "抽取未生成图谱：${GRAPH_JSON}"
    "${cli}" cluster-only . --graph "${GRAPH_JSON}" --no-viz --no-label
    [ -f "${REPORT}" ] || die "聚类未生成报告：${REPORT}"
    mkdir -p "${ROOT}/docs/graphify"
    cp "${REPORT}" "${MIRROR_REPORT}"
    python3 "${WRAPPER_DIR}/graphify_fingerprint.py" write
    python3 "${WRAPPER_DIR}/graphify_fingerprint.py" check
    echo "[graphify] 图谱已重建：src -> graphify-out/graph.json（报告镜像 docs/graphify/）"
}

command_name="${1:-}"
case "${command_name}" in
    rebuild)
        [ "$#" -eq 1 ] || die "rebuild 不接受额外参数"
        rebuild "$(resolve_cli)"
        ;;
    check)
        [ "$#" -eq 1 ] || die "check 不接受额外参数"
        cd "${ROOT}"
        python3 "${WRAPPER_DIR}/graphify_fingerprint.py" check
        echo "[graphify] 源码指纹与报告镜像均有效"
        ;;
    query|path|explain|diagnose|affected)
        [ -f "${GRAPH_JSON}" ] || die "图谱尚未构建，请先运行 just graph"
        shift
        reject_graph_override "$@"
        cli="$(resolve_cli)"
        cd "${ROOT}"
        exec "${cli}" "${command_name}" "$@" --graph "${GRAPH_JSON}"
        ;;
    export)
        # 本机可选视图：D3 collapsible tree；视图失败不得宣称图谱交付完成。
        [ "${2:-}" = "html" ] || die "本项目只放行 export html"
        [ "$#" -eq 2 ] || die "export html 不接受额外参数"
        [ -f "${GRAPH_JSON}" ] || die "图谱尚未构建，请先运行 just graph"
        cli="$(resolve_cli)"
        cd "${ROOT}"
        exec "${cli}" tree --graph "${GRAPH_JSON}" --label herdr
        ;;
    save-result|reflect)
        # 查询记忆是本机经验层：钉死 --memory-dir 与 reflect --out。
        [ -f "${GRAPH_JSON}" ] || die "图谱尚未构建，请先运行 just graph"
        for arg in "${@:2}"; do
            case "${arg}" in
                --memory-dir|--memory-dir=*)
                    die "${command_name} 禁止覆盖 --memory-dir；固定 ${ROOT}/.graphify-memory"
                    ;;
                --out|--out=*)
                    [ "${command_name}" = "reflect" ] \
                        && die "reflect 禁止覆盖 --out；固定 ${ROOT}/.graphify-memory/reflections/LESSONS.md"
                    ;;
            esac
        done
        extra_args=(--memory-dir "${ROOT}/.graphify-memory")
        [ "${command_name}" = "reflect" ] \
            && extra_args+=(--out "${ROOT}/.graphify-memory/reflections/LESSONS.md" --graph "${GRAPH_JSON}")
        cli="$(resolve_cli)"
        cd "${ROOT}"
        shift
        exec "${cli}" "${command_name}" "$@" "${extra_args[@]}"
        ;;
    extract|update|cluster-only|label|merge-graphs|watch|install|uninstall|add|clone|global*)
        die "禁止直接执行 ${command_name}；本项目固定使用 just graph 全量重建"
        ;;
    ""|help|-h|--help)
        echo "用法：scripts/graphify.sh rebuild|check|query|path|explain|diagnose|affected|export|save-result|reflect ..."
        ;;
    *)
        die "不支持的命令：${command_name}"
        ;;
esac
