#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <candidate-binary>" >&2
  exit 2
fi

script_dir=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
result_root="$repo_root/.local/perf-baseline"
mkdir -p "$result_root"
run_dir=$(mktemp -d "$result_root/run-$(date -u +%Y%m%dT%H%M%S)-XXXXXX")
run_id=$(basename "$run_dir")
temporary_root="$run_dir/tmp"
results="$run_dir/results"
summary="$run_dir/summary.txt"
metadata="$run_dir/metadata.txt"
isolated_home="$temporary_root/home"
isolated_config="$temporary_root/xdg-config"
isolated_state="$temporary_root/xdg-state"
isolated_runtime="$temporary_root/xdg-runtime"
isolated_data="$temporary_root/xdg-data"
isolated_cache="$temporary_root/xdg-cache"
isolated_appdata="$temporary_root/appdata"
isolated_localappdata="$temporary_root/localappdata"
isolated_herdr_home="$temporary_root/herdr-home"
isolated_codex_home="$temporary_root/codex-home"
isolated_kimi_home="$temporary_root/kimi-home"
isolated_tmp="$temporary_root/tmp"
mkdir -p "$temporary_root" "$results" "$isolated_home" "$isolated_config" "$isolated_state" \
  "$isolated_runtime" "$isolated_data" "$isolated_cache" "$isolated_appdata" \
  "$isolated_localappdata" "$isolated_herdr_home" "$isolated_codex_home" \
  "$isolated_kimi_home" "$isolated_tmp"
chmod 700 "$temporary_root" "$isolated_home" "$isolated_config" "$isolated_state" \
  "$isolated_runtime" "$isolated_data" "$isolated_cache" "$isolated_appdata" \
  "$isolated_localappdata" "$isolated_herdr_home" "$isolated_codex_home" \
  "$isolated_kimi_home" "$isolated_tmp"
export HOME="$isolated_home"
export USERPROFILE="$isolated_home"
export XDG_CONFIG_HOME="$isolated_config"
export XDG_STATE_HOME="$isolated_state"
export XDG_RUNTIME_DIR="$isolated_runtime"
export XDG_DATA_HOME="$isolated_data"
export XDG_CACHE_HOME="$isolated_cache"
export APPDATA="$isolated_appdata"
export LOCALAPPDATA="$isolated_localappdata"
export HERDR_HOME="$isolated_herdr_home"
export CODEX_HOME="$isolated_codex_home"
export KIMI_CODE_HOME="$isolated_kimi_home"
export TMPDIR="$isolated_tmp"
unset HERDR_CONFIG_PATH
printf '%s\n' "$run_id" > "$run_dir/run-id.txt"
printf 'run_id=%s\nstarted_at=%s\nrepository=%s\nisolation_root=%s\n' \
  "$run_id" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$repo_root" "$temporary_root" > "$metadata"
printf 'status=running\nrun_id=%s\n' "$run_id" > "$summary"
printf 'input=%s\n' "$1" > "$run_dir/candidate-command.txt"
printf 'source=%s\n' "${HERDR_PERF_BASELINE_BIN:-downloaded stable binary}" > "$run_dir/baseline-command.txt"

cleanup() {
  local status=$?
  set +e
  printf '%s\n' "$status" > "$run_dir/exit-code.txt"
  printf 'result=%s\nexit_code=%s\nfinished_at=%s\n' \
    "$([[ $status -eq 0 ]] && echo passed || echo failed)" "$status" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$summary"
  rm -rf "$temporary_root"
  trap - EXIT
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

exec > >(tee -a "$run_dir/run.log") 2>&1

candidate=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
printf 'exec %q\n' "$candidate" > "$run_dir/candidate-command.txt"
printf 'candidate=%s\n' "$candidate" >> "$metadata"
[[ -x "$candidate" ]] || { echo "candidate binary is not executable: $candidate" >&2; exit 1; }

baseline=${HERDR_PERF_BASELINE_BIN:-}
for command in jq lsof perl tee tmux; do
  command -v "$command" >/dev/null || { echo "required command not found: $command" >&2; exit 1; }
done
if [[ -z "$baseline" ]]; then
  command -v curl >/dev/null || { echo "required command not found: curl" >&2; exit 1; }
fi

case "$(uname -s)" in
  Linux) platform=linux; command -v pidstat >/dev/null || { echo "required command not found: pidstat" >&2; exit 1; } ;;
  Darwin) platform=macos ;;
  *) echo "release performance smoke supports Linux and macOS" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

printf 'platform=%s\narchitecture=%s\nsample_seconds=%s\nwarmup_seconds=%s\nrounds=2\nscenarios=hidden50,visible30\nexecution=serial\n' \
  "$platform" "$arch" "${HERDR_PERF_SAMPLE_SECONDS:-10}" "${HERDR_PERF_WARMUP_SECONDS:-3}" >> "$metadata"

if [[ -z "$baseline" ]]; then
  baseline_version=$(jq -er '.version' "$repo_root/distribution/latest.json")
  baseline="$run_dir/baseline/herdr-baseline"
  mkdir -p "$(dirname "$baseline")"
  baseline_url="https://github.com/herdrdev/herdr/releases/download/v${baseline_version}/herdr-${platform}-${arch}"
  printf 'curl -fL --retry 3 %q -o %q\n' "$baseline_url" "$baseline" >> "$run_dir/baseline-command.txt"
  curl -fL --retry 3 "$baseline_url" -o "$baseline"
  chmod +x "$baseline"
else
  baseline=$(cd "$(dirname "$baseline")" && pwd)/$(basename "$baseline")
fi
printf 'exec %q\n' "$baseline" >> "$run_dir/baseline-command.txt"
printf 'baseline=%s\n' "$baseline" >> "$metadata"
[[ -x "$baseline" ]] || { echo "baseline binary is not executable: $baseline" >&2; exit 1; }

case_script="$script_dir/release_perf_case.sh"
seconds=${HERDR_PERF_SAMPLE_SECONDS:-10}
warmup=${HERDR_PERF_WARMUP_SECONDS:-3}
printf 'case_script=%s\n' "$case_script" >> "$metadata"

for round in 1 2; do
  if [[ $round -eq 1 ]]; then variants="baseline candidate"; else variants="candidate baseline"; fi
  for scenario in hidden50 visible30; do
    for variant in $variants; do
      if [[ $variant == baseline ]]; then binary=$baseline; else binary=$candidate; fi
      "$case_script" "$binary" "$variant" "$scenario" "$round" "$seconds" "$warmup" "$results" "$temporary_root" "$platform"
    done
  done
done

mean_total() {
  awk '{ sum += $1; count++ } END { if (!count) exit 1; printf "%.3f", sum/count }' \
    "$results/$1/$2/r1/total-cpu.txt" \
    "$results/$1/$2/r2/total-cpu.txt"
}

failed=0
{
  printf '\nrelease performance smoke (%s/%s, two rounds, %ss samples)\n' "$platform" "$arch" "$seconds"
  printf '%-12s %12s %12s %12s\n' scenario baseline candidate change
} | tee -a "$summary"
for scenario in hidden50 visible30; do
  baseline_total=$(mean_total baseline "$scenario")
  candidate_total=$(mean_total candidate "$scenario")
  if awk -v before="$baseline_total" -v after="$candidate_total" 'BEGIN { exit !(before <= 0 || after <= 0) }'; then
    echo "error: $scenario measured no CPU usage; the benchmark did not exercise the binaries" >&2
    failed=1
  fi
  change=$(awk -v before="$baseline_total" -v after="$candidate_total" 'BEGIN { if (before == 0) print "n/a"; else printf "%+.1f%%", (after-before)/before*100 }')
  printf '%-12s %12s %12s %12s\n' "$scenario" "$baseline_total" "$candidate_total" "$change" | tee -a "$summary"
  if awk -v before="$baseline_total" -v after="$candidate_total" 'BEGIN { exit !(after > before * 1.25 && after - before > 0.5) }'; then
    echo "error: $scenario candidate CPU exceeds baseline by more than 25% and 0.5 CPU points" >&2
    failed=1
  fi
done

if [[ $failed -ne 0 ]]; then
  echo "release performance smoke failed" | tee -a "$summary"
  exit 1
fi
echo "release performance smoke passed" | tee -a "$summary"
