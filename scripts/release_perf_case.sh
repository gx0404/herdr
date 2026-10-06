#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 9 ]]; then
  echo "usage: $0 <binary> <variant> <scenario> <round> <seconds> <warmup> <output-root> <temporary-root> <platform>" >&2
  exit 2
fi

bin_dir=$(cd "$(dirname "$1")" && pwd)
bin="$bin_dir/$(basename "$1")"
variant=$2
scenario=$3
round=$4
seconds=$5
warmup=$6
out_root=$(cd "$7" && pwd)
temporary_root=$(cd "$8" && pwd)
platform=$9
script_dir=$(cd "$(dirname "$0")" && pwd)
producer="$script_dir/release_perf_producer.pl"
cols=86
rows=47

case "$scenario" in
  visible30) scenario_tag=v3; total_panes=1; writers=visible; rate=30 ;;
  hidden50) scenario_tag=h5; total_panes=50; writers=hidden; rate=60 ;;
  *) echo "unknown scenario: $scenario" >&2; exit 2 ;;
esac
case "$platform" in linux) platform_tag=l ;; macos) platform_tag=m ;; *) exit 2 ;; esac
[[ -d "$temporary_root" && ! -L "$8" && -O "$temporary_root" &&
   -f "$temporary_root/owner.txt" && ! -L "$temporary_root/owner.txt" &&
   -f "$out_root/../runtime-owner.txt" ]] &&
  cmp -s "$temporary_root/owner.txt" "$out_root/../runtime-owner.txt" &&
  grep -Fxq "runtime=$temporary_root" "$temporary_root/owner.txt" &&
  grep -Fxq "uid=$(id -u)" "$temporary_root/owner.txt" &&
  perl -e 'my @s = lstat($ARGV[0]); exit(!@s || ($s[2] & 0777) != 0700 || $s[4] != $<)' "$temporary_root" || {
    echo "runtime ownership receipt missing, mismatched, or not private (0700)" >&2; exit 2;
  }
root_receipt=$(<"$temporary_root/owner.txt")
umask 077
name=p
state=$(mktemp -d "$temporary_root/c-XXXXXX")
case_receipt=$(printf 'runtime=%s\npid=%s\nsession=%s' "$state" "$$" "$name")
printf '%s\n' "$case_receipt" > "$state/owner.txt"
home="$state/home"
xdg="$state/c"
xdg_state="$state/xdg-state"
runtime="$state/run"
xdg_data="$state/xdg-data"
xdg_cache="$state/xdg-cache"
appdata="$state/appdata"
localappdata="$state/localappdata"
herdr_home="$state/herdr-home"
codex_home="$state/codex-home"
kimi_home="$state/kimi-home"
tmp="$state/tmp"
gate="$state/start-output"
out="$out_root/$variant/$scenario/r$round"
tmux_root="$temporary_root/tmux"
mkdir -p "$home" "$xdg" "$xdg_state" "$runtime" "$xdg_data" "$xdg_cache" \
  "$appdata" "$localappdata" "$herdr_home" "$codex_home" "$kimi_home" "$tmp" \
  "$out" "$tmux_root"
chmod 700 "$state" "$home" "$xdg" "$xdg_state" "$runtime" "$xdg_data" "$xdg_cache" \
  "$appdata" "$localappdata" "$herdr_home" "$codex_home" "$kimi_home" "$tmp" "$tmux_root"
tmux_cmd=(env -u TMUX TMUX_TMPDIR="$tmux_root" tmux)

launch_env=(env
  -u HERDR_BIN_PATH -u HERDR_ENV -u HERDR_CONFIG_PATH -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH
  -u HERDR_SESSION -u HERDR_STARTUP_CWD -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_PANE_ID
  HOME="$home" USERPROFILE="$home" XDG_CONFIG_HOME="$xdg" XDG_STATE_HOME="$xdg_state"
  XDG_RUNTIME_DIR="$runtime" XDG_DATA_HOME="$xdg_data" XDG_CACHE_HOME="$xdg_cache"
  APPDATA="$appdata" LOCALAPPDATA="$localappdata" HERDR_HOME="$herdr_home"
  CODEX_HOME="$codex_home" KIMI_CODE_HOME="$kimi_home" TMPDIR="$tmp"
  HERDR_DISABLE_SOUND=1 SHELL=/bin/sh)
control_env=(env
  -u HERDR_BIN_PATH -u HERDR_ENV -u HERDR_CONFIG_PATH -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH
  -u HERDR_STARTUP_CWD -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_PANE_ID
  HOME="$home" USERPROFILE="$home" XDG_CONFIG_HOME="$xdg" XDG_STATE_HOME="$xdg_state"
  XDG_RUNTIME_DIR="$runtime" XDG_DATA_HOME="$xdg_data" XDG_CACHE_HOME="$xdg_cache"
  APPDATA="$appdata" LOCALAPPDATA="$localappdata" HERDR_HOME="$herdr_home"
  CODEX_HOME="$codex_home" KIMI_CODE_HOME="$kimi_home" TMPDIR="$tmp"
  HERDR_DISABLE_SOUND=1 SHELL=/bin/sh HERDR_SESSION="$name")

{
  printf 'variant=%s\nscenario=%s\nround=%s\nseconds=%s\nwarmup=%s\nplatform=%s\n' \
    "$variant" "$scenario" "$round" "$seconds" "$warmup" "$platform"
  printf 'binary=%q\nruntime=%s\nsession=%s\n' "$bin" "$state" "$name"
} > "$out/case-metadata.txt"
printf '%s\n' "$case_receipt" > "$out/runtime-owner.txt"

state_physical=$(cd "$state" && pwd -P)
started=0
tmux_started=0
safe_case_path() {
  local target path remaining component resolved
  [[ -d "$state" && ! -L "$state" && -O "$state" && -r "$state" && -x "$state" ]] || return 1
  resolved=$(cd "$state" && pwd -P) || return 1
  [[ $resolved == "$state_physical" ]] || return 1
  for target in "$@"; do
    [[ $target == "$state" || $target == "$state/"* ]] || return 1
    [[ $target != "$state" ]] || continue
    path="$state"
    remaining=${target#"$state/"}
    while [[ -n $remaining ]]; do
      component=${remaining%%/*}
      [[ -n $component && $component != . && $component != .. ]] || return 1
      path="$path/$component"
      [[ ! -L "$path" ]] || return 1
      [[ -e "$path" ]] || break
      [[ -O "$path" ]] || return 1
      if [[ -d "$path" ]]; then
        [[ -r "$path" && -x "$path" ]] || return 1
      elif [[ $remaining == */* ]]; then
        return 1
      fi
      [[ $remaining == */* ]] || break
      remaining=${remaining#*/}
    done
  done
}
owns_state() {
  [[ -d "$temporary_root" && ! -L "$temporary_root" && -O "$temporary_root" &&
     ! -L "$temporary_root/owner.txt" && -d "$state" && ! -L "$state" &&
     -O "$state" && ! -L "$state/owner.txt" ]] &&
    cmp -s "$state/owner.txt" <(printf '%s\n' "$case_receipt") &&
    cmp -s "$temporary_root/owner.txt" <(printf '%s\n' "$root_receipt") &&
    cmp -s "$temporary_root/owner.txt" "$out_root/../runtime-owner.txt" || return 1
  safe_case_path "$home" "$xdg" "$xdg_state" "$runtime" "$xdg_data" "$xdg_cache" \
    "$appdata" "$localappdata" "$herdr_home" "$codex_home" "$kimi_home" "$tmp" \
    "$xdg/herdr/config.toml" "$xdg/herdr/sessions/$name" \
    "$xdg/herdr/sessions/$name/herdr.sock" "$xdg/herdr/sessions/$name/herdr-client.sock" \
    "$state/t" "$state/tmux.conf"
}
cleanup_control() {
  owns_state || return 1
  "${control_env[@]}" "$bin" "$@"
}
capture_diagnostics() {
  local log source code failed=0
  owns_state || return 1
  : > "$out/diagnostics-status.txt" || return 1
  if [[ $tmux_started -eq 0 && ! -e "$state/t" ]]; then
    printf 'tmux=not-started\n' >> "$out/diagnostics-status.txt" || failed=1
  else
    "${tmux_cmd[@]}" list-panes -t "$name" -F '#{pane_pid} #{pane_dead} #{pane_dead_status}' \
      > "$out/tmux-status.txt" 2>&1
    code=$?
    printf 'tmux-status=%s\n' "$code" >> "$out/diagnostics-status.txt" || failed=1
    [[ $code -eq 0 ]] || failed=1
    "${tmux_cmd[@]}" capture-pane -p -t "$name" -S -200 \
      > "$out/tmux-pane.txt" 2>&1
    code=$?
    printf 'tmux-pane=%s\n' "$code" >> "$out/diagnostics-status.txt" || failed=1
    [[ $code -eq 0 ]] || failed=1
  fi
  for log in herdr.log herdr-client.log herdr-server.log; do
    source="$xdg/herdr/sessions/$name/$log"
    if ! safe_case_path "$source"; then
      printf '%s=unsafe\n' "$log" >> "$out/diagnostics-status.txt"
      failed=1
    elif [[ ! -e "$source" ]]; then
      printf '%s=absent\n' "$log" >> "$out/diagnostics-status.txt" || failed=1
    elif [[ -f "$source" ]]; then
      tail -c 65536 "$source" > "$out/$log.txt"
      code=$?
      printf '%s=%s\n' "$log" "$code" >> "$out/diagnostics-status.txt" || failed=1
      [[ $code -eq 0 ]] || failed=1
    else
      printf '%s=not-regular\n' "$log" >> "$out/diagnostics-status.txt"
      failed=1
    fi
  done
  return "$failed"
}
cleanup() {
  local status=$? cleanup_status=0 code deleted=0 evidence_status=0
  trap - EXIT
  set +e
  if ! owns_state; then
    printf 'ownership=refused\n' > "$out/cleanup.txt"
    cleanup_status=1
  elif [[ $started -eq 0 ]]; then
    printf 'state=not-started\n' > "$out/cleanup.txt" || cleanup_status=1
  else
    capture_diagnostics || evidence_status=1
    cleanup_control session stop "$name" > "$out/stop.txt" 2>&1
    code=$?
    printf 'stop=%s\n' "$code" > "$out/cleanup.txt" || cleanup_status=1
    [[ $code -eq 0 ]] || cleanup_status=1
    if [[ $evidence_status -ne 0 ]]; then
      printf 'diagnostics=failed\ndelete=skipped-preserve-evidence\ntmux=retained-preserve-pane\n' >> "$out/cleanup.txt"
      cleanup_status=1
    else
      for _ in $(seq 1 50); do
        cleanup_control session delete "$name" > "$out/delete.txt" 2>&1
        code=$?
        printf 'delete=%s\n' "$code" >> "$out/cleanup.txt" || cleanup_status=1
        if [[ $code -eq 0 ]]; then deleted=1; break; fi
        sleep 0.1
      done
      [[ $deleted -eq 1 ]] || cleanup_status=1
      cleanup_control session list --json > "$out/session-list.txt" 2> "$out/session-list-stderr.txt"
      code=$?
      printf 'list=%s\n' "$code" >> "$out/cleanup.txt" || cleanup_status=1
      if [[ $code -ne 0 ]] || ! jq -e --arg name "$name" \
        '.sessions | if type == "array" then all(.[]; .name != $name) else false end' \
        "$out/session-list.txt" >/dev/null 2>&1; then cleanup_status=1; fi
      owns_state && "${tmux_cmd[@]}" kill-server > "$out/tmux-cleanup.txt" 2>&1
      code=$?
      printf 'tmux=%s\n' "$code" >> "$out/cleanup.txt" || cleanup_status=1
      [[ $code -eq 0 ]] || cleanup_status=1
      for _ in $(seq 1 50); do
        [[ ! -e "$tmux_socket" && ! -L "$tmux_socket" ]] && break
        sleep 0.1
      done
      if [[ -e "$tmux_socket" || -L "$tmux_socket" ]]; then
        printf 'tmux_socket=still-present\n' >> "$out/cleanup.txt"
        cleanup_status=1
      else
        printf 'tmux_socket=absent\n' >> "$out/cleanup.txt" || cleanup_status=1
      fi
    fi
  fi
  if [[ $cleanup_status -eq 0 ]] && owns_state; then
    rm -rf "$state" || cleanup_status=1
  else
    cleanup_status=1
  fi
  if [[ $cleanup_status -ne 0 ]]; then
    printf 'state=unknown-retained\n' >> "$out/cleanup.txt"
    if [[ -d "$temporary_root" && ! -L "$temporary_root" && -O "$temporary_root" ]]; then
      touch "$temporary_root/retain"
    fi
    [[ $status -ne 0 ]] || status=1
  fi
  printf '%s\n' "$status" > "$out/exit-code.txt"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

socket="$xdg/herdr/sessions/$name/herdr.sock"
client_socket="$xdg/herdr/sessions/$name/herdr-client.sock"
tmux_socket="$state/t"
limit=107
[[ $platform != macos ]] || limit=103
for endpoint in "$socket" "$client_socket" "$tmux_socket"; do
  bytes=$(LC_ALL=C printf '%s' "$endpoint" | wc -c | tr -d ' ')
  printf 'socket=%s bytes=%s limit=%s\n' "$endpoint" "$bytes" "$limit" >> "$out/socket-preflight.txt"
  [[ $bytes -le $limit ]] || { echo "Unix socket path too long: $bytes > $limit: $endpoint" >&2; exit 2; }
done
printf 'set-window-option -g remain-on-exit on\n' > "$state/tmux.conf"
tmux_cmd+=(-S "$tmux_socket" -f "$state/tmux.conf")
printf -v launch 'exec '
printf -v quoted '%q ' "${launch_env[@]}" "$bin" --session "$name"
launch+=$quoted
printf '%s\n' "$launch" > "$out/launch-command.txt"
started=1
"${tmux_cmd[@]}" new-session -d -s "$name" -x "$cols" -y "$rows" "$launch"
tmux_started=1

panes_json=
ready=0
for _ in $(seq 1 150); do
  if "${control_env[@]}" "$bin" pane list > "$out/readiness-stdout.txt" 2> "$out/readiness-stderr.txt"; then
    printf '0\n' > "$out/readiness-exit-code.txt"
    panes_json=$(<"$out/readiness-stdout.txt")
    ready=1
    break
  else
    printf '%s\n' "$?" > "$out/readiness-exit-code.txt"
  fi
  sleep 0.1
done
[[ $ready -eq 1 && -n "$panes_json" ]] || { echo "session API did not become ready" >&2; exit 1; }
root_pane=$(printf '%s\n' "$panes_json" | jq -r '.result.panes[0].pane_id')
workspace_id=$("${control_env[@]}" "$bin" workspace list | jq -r '.result.workspaces[0].workspace_id')
[[ -n "$root_pane" && "$root_pane" != null ]] || { echo "session did not report a root pane" >&2; exit 1; }
[[ -n "$workspace_id" && "$workspace_id" != null ]] || { echo "session did not report a workspace" >&2; exit 1; }
pane_file="$state/pane-ids.txt"
printf '%s\n' "$root_pane" > "$pane_file"

for ((index = 2; index <= total_panes; index++)); do
  created=$("${control_env[@]}" "$bin" tab create --workspace "$workspace_id" --label "bench-$index" --no-focus)
  pane_id=$(printf '%s\n' "$created" | jq -r '.result.root_pane.pane_id')
  [[ -n "$pane_id" && "$pane_id" != null ]] || { echo "tab $index did not return a pane" >&2; exit 1; }
  printf '%s\n' "$pane_id" >> "$pane_file"
done

index=0
while IFS= read -r pane_id; do
  index=$((index + 1))
  if [[ $writers == visible && $index -eq 1 ]] || [[ $writers == hidden && $index -gt 1 ]]; then
    "${control_env[@]}" "$bin" pane run "$pane_id" "$producer" "$rate" "$gate" "p$index" >/dev/null
  fi
done < "$pane_file"
touch "$gate"

socket="$xdg/herdr/sessions/$name/herdr.sock"
server_pid=
for _ in $(seq 1 80); do
  server_pid=$(lsof -t "$socket" 2>/dev/null | head -n1 || true)
  [[ -n "$server_pid" ]] && break
  sleep 0.1
done
[[ -n "$server_pid" ]] || { echo "could not find server pid" >&2; exit 1; }
client_pid=$("${tmux_cmd[@]}" list-panes -s -t "$name" -F '#{pane_pid}')
[[ -n "$client_pid" ]] || { echo "could not find client pid" >&2; exit 1; }
all_pids=("$server_pid" "$client_pid")

sleep "$warmup"
raw="$out/cpu-raw.txt"
if [[ $platform == linux ]]; then
  pid_csv=$(IFS=,; echo "${all_pids[*]}")
  LC_ALL=C pidstat -h -u -p "$pid_csv" 1 "$seconds" > "$raw"
else
  top_args=(top -l $((seconds + 1)) -s 1 -stats pid,cpu,time -n 2)
  for pid in "${all_pids[@]}"; do top_args+=(-pid "$pid"); done
  LC_ALL=C "${top_args[@]}" > "$raw"
fi

mean_linux() {
  awk -v target="$2" '
    /^Linux/ || /^#/ || NF < 5 { next }
    $3 == target && $(NF-2) ~ /^[0-9]+([.][0-9]+)?$/ { sum += $(NF-2); count++ }
    END { if (!count) exit 1; printf "%.6f,%d", sum/count, count }
  ' "$1"
}
mean_macos() {
  awk -v target="$2" '
    $1 == target && $2 ~ /^[0-9]+([.][0-9]+)?%?$/ {
      seen++; if (seen == 1) next; value=$2; gsub(/%/, "", value); sum += value; count++ }
    END { if (!count) exit 1; printf "%.6f,%d", sum/count, count }
  ' "$1"
}

total=0
for pid in "${all_pids[@]}"; do
  if [[ $platform == linux ]]; then parsed=$(mean_linux "$raw" "$pid"); else parsed=$(mean_macos "$raw" "$pid"); fi
  mean=${parsed%,*}
  samples=${parsed#*,}
  [[ $samples -eq $seconds ]] || { echo "expected $seconds samples for pid $pid, got $samples" >&2; exit 1; }
  total=$(awk -v total="$total" -v mean="$mean" 'BEGIN { printf "%.6f", total + mean }')
done

index=0
while IFS= read -r pane_id; do
  index=$((index + 1))
  if [[ $writers == visible && $index -eq 1 ]] || [[ $writers == hidden && $index -gt 1 ]]; then
    read_file="$out/pane-$index.txt"
    "${control_env[@]}" "$bin" pane read "$pane_id" --source visible --format text > "$read_file"
    grep -q 'bench-output-' "$read_file" || { echo "writer pane $index produced no output" >&2; exit 1; }
  fi
done < "$pane_file"

printf '%s\n' "$total" > "$out/total-cpu.txt"
printf '%s,%s,%s,%s\n' "$variant" "$scenario" "$round" "$total"
