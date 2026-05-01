#!/usr/bin/env bash

set -euo pipefail
IFS=$'\n\t'

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

usage() {
    cat <<'EOF'
Usage:
  scripts/run-with-memory-and-perf.sh [--out-dir DIR] [--repeat N] [--perf-stat] [--timeout-seconds N] [--objects-processed N] -- <program> [args...]

Environment:
  MEMORY_HARNESS_BIN   Override the harness binary path.
  MH_PERF_CALL_GRAPH   perf call graph mode (default: dwarf).
  MH_PERF_ATTACH_SETTLE_MS  Milliseconds to wait after starting perf before releasing the child (default: 300).
EOF
}

perf_tool_supports_control() {
    local subcommand="$1"
    { perf "$subcommand" -h 2>&1 || true; } | grep -q -- '--control'
}

perf_record_supports_control() {
    perf_tool_supports_control record
}

perf_send_control() {
    local ctl_fifo="$1"
    local ack_fifo="$2"
    local control_cmd="$3"
    local timeout_s="${4:-5}"
    local ack_line=""

    timeout "$timeout_s" bash -lc 'printf "%s\n" "$1" > "$2"' _ "$control_cmd" "$ctl_fifo"
    ack_line="$(timeout "$timeout_s" bash -lc 'IFS= read -r line < "$1"; printf "%s" "$line"' _ "$ack_fifo")"
    [[ "$ack_line" == "ack" ]]
}

wait_for_perf_ready() {
    local perf_pid="$1"
    local perf_output="$2"

    for _ in $(seq 1 200); do
        if ! kill -0 "$perf_pid" 2>/dev/null; then
            printf 'perf exited before becoming ready\n' >&2
            return 1
        fi
        if [[ -e "$perf_output" ]]; then
            return 0
        fi
        sleep 0.01
    done
    printf 'timed out waiting for perf to become ready\n' >&2
    return 1
}

wait_for_log_line() {
    local log_path="$1"
    local expected_line="$2"
    local timeout_s="${3:-2}"
    local deadline_ms=$(( $(date +%s%3N) + (timeout_s * 1000) ))

    while [[ "$(date +%s%3N)" -lt "$deadline_ms" ]]; do
        if [[ -f "$log_path" ]] && grep -Fqx "$expected_line" "$log_path"; then
            return 0
        fi
        sleep 0.01
    done
    return 1
}

wait_for_process_exit() {
    local pid="$1"
    local attempts="${2:-100}"
    local sleep_s="${3:-0.05}"

    for _ in $(seq 1 "$attempts"); do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep "$sleep_s"
    done
    return 1
}

force_stop_perf_process() {
    local perf_pid="$1"

    for _ in $(seq 1 20); do
        if ! kill -0 "$perf_pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    if kill -0 "$perf_pid" 2>/dev/null; then
        kill -INT "$perf_pid" 2>/dev/null || true
    fi
    for _ in $(seq 1 20); do
        if ! kill -0 "$perf_pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    if kill -0 "$perf_pid" 2>/dev/null; then
        kill -TERM "$perf_pid" 2>/dev/null || true
    fi
    for _ in $(seq 1 40); do
        if ! kill -0 "$perf_pid" 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    if kill -0 "$perf_pid" 2>/dev/null; then
        kill -KILL "$perf_pid" 2>/dev/null || true
    fi
}

pick_memory_harness_bin() {
    local candidate
    local newest=""
    local newest_mtime=0
    local mtime=0
    for candidate in \
        "$repo_root/target/release/memory-harness" \
        "$repo_root/target/ship/memory-harness" \
        "$repo_root/target/debug/memory-harness"
    do
        if [[ -x "$candidate" ]]; then
            mtime="$(stat -c %Y "$candidate")"
            if [[ "$mtime" -gt "$newest_mtime" ]]; then
                newest="$candidate"
                newest_mtime="$mtime"
            fi
        fi
    done
    if [[ -n "$newest" ]]; then
        printf '%s\n' "$newest"
        return 0
    fi
    return 1
}

run_once() {
    local run_dir="$1"
    shift

    local command_txt="$run_dir/command.txt"
    local combined_log="$run_dir/combined.log"
    local memory_json="$run_dir/memory-harness.json"
    local proc_samples="$run_dir/proc-status.tsv"
    local perf_data="$run_dir/perf.data"
    local perf_report="$run_dir/perf.report.txt"
    local perf_stat="$run_dir/perf.stat.txt"
    local summary_txt="$run_dir/summary.txt"
    local timeout_marker="$run_dir/timeout.marker"
    local sync_dir="$run_dir/sync"
    local child_pid_file="$sync_dir/child.pid"
    local profiler_continue_file="$sync_dir/profiler.continue"
    local perf_control_dir="$sync_dir/perf-control"
    local perf_ctl_fifo="$perf_control_dir/ctl"
    local perf_ack_fifo="$perf_control_dir/ack"
    local memory_status=0
    local perf_record_status=0
    local perf_stat_status=0
    local child_pid=""
    local perf_record_pid=0
    local perf_record_forced_stop=0
    local perf_stat_pid=0
    local perf_stat_forced_stop=0
    local perf_control_mode="signal"
    local timeout_triggered=0
    local timeout_deadline_ms=0
    local memory_status_for_return=0
    local perf_record_status_for_return=0
    local perf_stat_status_for_return=0
    local perf_needs_settle_sleep=0
    local -a harness_timeout_args=()

    mkdir -p "$run_dir"
    mkdir -p "$sync_dir"
    local first=1
    for arg in "$@"; do
        if [[ "$first" -eq 1 ]]; then
            printf '%q' "$arg" >"$command_txt"
            first=0
        else
            printf ' %q' "$arg" >>"$command_txt"
        fi
    done
    printf '\n' >>"$command_txt"

    local -a harness_args=(
        --format
        json
        --sample-proc-status-ms
        10
        --sample-proc-status-out
        "$proc_samples"
        --child-stdout
        "$combined_log"
        --child-stderr
        "$combined_log"
        --timeout-marker-file
        "$timeout_marker"
        --announce-child-pid-file
        "$child_pid_file"
        --await-profiler-file
        "$profiler_continue_file"
    )
    if [[ -n "${objects_processed:-}" ]]; then
        harness_args+=(--objects-processed "$objects_processed")
    fi
    printf 'running memory-harness with %s\n' "$memory_harness_bin" >&2
    set +e
    "$memory_harness_bin" \
        "${harness_args[@]}" \
        "${harness_timeout_args[@]}" \
        -- "$@" >"$memory_json" 2>>"$combined_log" &
    local harness_pid=$!
    set -e

    wait_for_file "$child_pid_file"
    child_pid="$(tr -d '[:space:]' <"$child_pid_file")"
    printf 'attaching perf record to child pid %s\n' "$child_pid" >&2
    if perf_record_supports_control && [[ "${MH_PERF_USE_CONTROL:-1}" != "0" ]]; then
        perf_control_mode="control"
        mkdir -p "$perf_control_dir"
        rm -f "$perf_ctl_fifo" "$perf_ack_fifo"
        mkfifo "$perf_ctl_fifo" "$perf_ack_fifo"
    fi
    set +e
    if [[ "$perf_control_mode" == "control" ]]; then
        perf record \
            --delay=-1 \
            --control "fifo:$perf_ctl_fifo,$perf_ack_fifo" \
            --call-graph "$perf_call_graph" \
            -o "$perf_data" \
            -p "$child_pid" >>"$combined_log" 2>&1 &
    else
        perf record \
            --call-graph "$perf_call_graph" \
            -o "$perf_data" \
            -p "$child_pid" >>"$combined_log" 2>&1 &
    fi
    perf_record_pid=$!
    set -e
    if [[ "$with_perf_stat" -eq 1 ]]; then
        printf 'attaching perf stat to child pid %s\n' "$child_pid" >&2
        set +e
        perf stat \
            -o "$perf_stat" \
            -p "$child_pid" >>"$combined_log" 2>&1 &
        perf_stat_pid=$!
        set -e
    fi
    if [[ "$perf_control_mode" == "control" ]]; then
        if ! wait_for_perf_ready "$perf_record_pid" "$perf_data" || \
            ! {
                perf_send_control "$perf_ctl_fifo" "$perf_ack_fifo" enable ||
                    wait_for_log_line "$combined_log" "Events enabled"
            }
        then
            printf 'failed to enable perf recording before child release\n' >&2
            kill "$harness_pid" 2>/dev/null || true
            wait "$harness_pid" 2>/dev/null || true
            if [[ "$perf_stat_pid" -ne 0 ]]; then
                force_stop_perf_process "$perf_stat_pid"
                wait "$perf_stat_pid" 2>/dev/null || true
            fi
            rm -f "$perf_ctl_fifo" "$perf_ack_fifo"
            return 1
        fi
    else
        perf_needs_settle_sleep=1
    fi
    if [[ "$perf_stat_pid" -ne 0 ]]; then
        perf_needs_settle_sleep=1
    fi
    if [[ "$perf_needs_settle_sleep" -eq 1 ]]; then
        sleep "$(awk "BEGIN { printf \"%.3f\", ${perf_attach_settle_ms}/1000 }")"
    fi
    : >"$profiler_continue_file"
    if [[ -n "$timeout_seconds" ]]; then
        timeout_deadline_ms=$(( $(date +%s%3N) + (timeout_seconds * 1000) ))
        while kill -0 "$harness_pid" 2>/dev/null; do
            if [[ "$timeout_triggered" -eq 0 && "$(date +%s%3N)" -ge "$timeout_deadline_ms" ]]; then
                timeout_triggered=1
                : >"$timeout_marker"
                if kill -0 "$perf_record_pid" 2>/dev/null; then
                    if [[ "$perf_control_mode" == "control" ]]; then
                        if ! perf_send_control "$perf_ctl_fifo" "$perf_ack_fifo" stop; then
                            perf_record_forced_stop=1
                            force_stop_perf_process "$perf_record_pid"
                        fi
                    else
                        perf_record_forced_stop=1
                        force_stop_perf_process "$perf_record_pid"
                    fi
                fi
                if kill -0 "$child_pid" 2>/dev/null; then
                    kill -TERM "$child_pid" 2>/dev/null || true
                    for _ in $(seq 1 10); do
                        if ! kill -0 "$child_pid" 2>/dev/null; then
                            break
                        fi
                        sleep 0.01
                    done
                    if kill -0 "$child_pid" 2>/dev/null; then
                        kill -KILL "$child_pid" 2>/dev/null || true
                    fi
                fi
            fi
            sleep 0.01
        done
    fi

    set +e
    wait "$harness_pid"
    memory_status=$?
    set -e

    if [[ "$timeout_triggered" -eq 0 ]] && kill -0 "$perf_record_pid" 2>/dev/null; then
        if [[ "$perf_control_mode" == "control" ]]; then
            if ! perf_send_control "$perf_ctl_fifo" "$perf_ack_fifo" stop; then
                if ! wait_for_process_exit "$perf_record_pid"; then
                    printf 'perf did not acknowledge stop and remained alive\n' >&2
                    perf_record_forced_stop=1
                    force_stop_perf_process "$perf_record_pid"
                fi
            fi
        else
            perf_record_forced_stop=1
            force_stop_perf_process "$perf_record_pid"
        fi
    fi
    if [[ "$timeout_triggered" -eq 1 ]] && kill -0 "$perf_record_pid" 2>/dev/null; then
        for _ in $(seq 1 100); do
            if ! kill -0 "$perf_record_pid" 2>/dev/null; then
                break
            fi
            sleep 0.05
        done
        if kill -0 "$perf_record_pid" 2>/dev/null; then
            perf_record_forced_stop=1
            force_stop_perf_process "$perf_record_pid"
        fi
    fi
    if [[ "$perf_stat_pid" -ne 0 ]] && [[ "$timeout_triggered" -eq 1 ]] && kill -0 "$perf_stat_pid" 2>/dev/null; then
        for _ in $(seq 1 100); do
            if ! kill -0 "$perf_stat_pid" 2>/dev/null; then
                break
            fi
            sleep 0.05
        done
        if kill -0 "$perf_stat_pid" 2>/dev/null; then
            perf_stat_forced_stop=1
            force_stop_perf_process "$perf_stat_pid"
        fi
    fi
    set +e
    wait "$perf_record_pid"
    perf_record_status=$?
    if [[ "$perf_stat_pid" -ne 0 ]]; then
        wait "$perf_stat_pid"
        perf_stat_status=$?
    fi
    set -e
    rm -f "$perf_ctl_fifo" "$perf_ack_fifo"

    if [[ -f "$perf_data" ]]; then
        perf report --stdio -i "$perf_data" >"$perf_report"
    fi

    memory_status_for_return="$memory_status"
    if [[ -n "$timeout_seconds" && "$memory_status" -eq 124 ]]; then
        memory_status_for_return=0
    fi
    perf_record_status_for_return="$perf_record_status"
    if [[ -n "$timeout_seconds" && "$perf_record_forced_stop" -eq 1 ]]; then
        perf_record_status_for_return=0
    fi
    perf_stat_status_for_return="$perf_stat_status"
    if [[ -n "$timeout_seconds" && "$perf_stat_forced_stop" -eq 1 ]]; then
        perf_stat_status_for_return=0
    fi

    {
        printf 'run_dir=%s\n' "$run_dir"
        printf 'memory_harness_bin=%s\n' "$memory_harness_bin"
        printf 'timeout_seconds=%s\n' "${timeout_seconds:-none}"
        printf 'memory_status=%s\n' "$memory_status"
        printf 'perf_record_status=%s\n' "$perf_record_status"
        printf 'perf_record_forced_stop=%s\n' "$perf_record_forced_stop"
        printf 'perf_control_mode=%s\n' "$perf_control_mode"
        printf 'perf_stat_status=%s\n' "$perf_stat_status"
        printf 'perf_stat_forced_stop=%s\n' "$perf_stat_forced_stop"
        printf 'objects_processed=%s\n' "${objects_processed:-none}"
        printf 'child_pid=%s\n' "$child_pid"
        printf 'combined_log=%s\n' "$combined_log"
        printf 'memory_json=%s\n' "$memory_json"
        printf 'proc_status_samples=%s\n' "$proc_samples"
        printf 'perf_data=%s\n' "$perf_data"
        printf 'perf_report=%s\n' "$perf_report"
        printf 'perf_stat=%s\n' "$perf_stat"
        printf 'command=%s\n' "$(cat "$command_txt")"
    } >"$summary_txt"

    printf 'artifacts written to %s\n' "$run_dir" >&2
    printf '  combined log:      %s\n' "$combined_log" >&2
    printf '  memory summary:    %s\n' "$memory_json" >&2
    printf '  proc-status TSV:   %s\n' "$proc_samples" >&2
    printf '  perf data:         %s\n' "$perf_data" >&2
    printf '  perf report:       %s\n' "$perf_report" >&2
    if [[ "$with_perf_stat" -eq 1 ]]; then
        printf '  perf stat:         %s\n' "$perf_stat" >&2
    fi

    if [[ "$memory_status_for_return" -ne 0 ]]; then
        return "$memory_status_for_return"
    fi
    if [[ "$perf_record_status_for_return" -ne 0 ]]; then
        return "$perf_record_status_for_return"
    fi
    return "$perf_stat_status_for_return"
}

wait_for_file() {
    local path="$1"
    local waited=0
    while [[ ! -f "$path" ]]; do
        sleep 0.01
        waited=$((waited + 1))
        if [[ "$waited" -gt 3000 ]]; then
            printf 'timed out waiting for %s\n' "$path" >&2
            exit 1
        fi
    done
}

if [[ $# -eq 0 ]]; then
    usage >&2
    exit 2
fi

out_dir=""
repeat_count=1
with_perf_stat=0
timeout_seconds=""
objects_processed=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir)
            [[ $# -ge 2 ]] || {
                printf 'missing value for --out-dir\n' >&2
                exit 2
            }
            out_dir="$2"
            shift 2
            ;;
        --repeat)
            [[ $# -ge 2 ]] || {
                printf 'missing value for --repeat\n' >&2
                exit 2
            }
            repeat_count="$2"
            shift 2
            ;;
        --perf-stat)
            with_perf_stat=1
            shift
            ;;
        --timeout-seconds)
            [[ $# -ge 2 ]] || {
                printf 'missing value for --timeout-seconds\n' >&2
                exit 2
            }
            timeout_seconds="$2"
            shift 2
            ;;
        --objects-processed)
            [[ $# -ge 2 ]] || {
                printf 'missing value for --objects-processed\n' >&2
                exit 2
            }
            objects_processed="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *)
            printf 'unknown option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! [[ "$repeat_count" =~ ^[0-9]+$ ]] || [[ "$repeat_count" -lt 1 ]]; then
    printf 'repeat count must be a positive integer, got %s\n' "$repeat_count" >&2
    exit 2
fi

if [[ -n "$timeout_seconds" ]]; then
    if ! [[ "$timeout_seconds" =~ ^[0-9]+$ ]] || [[ "$timeout_seconds" -lt 1 ]]; then
        printf 'timeout seconds must be a positive integer, got %s\n' "$timeout_seconds" >&2
        exit 2
    fi
fi

if [[ -n "$objects_processed" ]] && ! [[ "$objects_processed" =~ ^[0-9]+$ ]]; then
    printf 'objects processed must be a positive integer, got %s\n' "$objects_processed" >&2
    exit 2
fi

if [[ $# -eq 0 ]]; then
    printf 'missing command to profile\n' >&2
    usage >&2
    exit 2
fi

memory_harness_bin="${MEMORY_HARNESS_BIN:-}"
if [[ -z "$memory_harness_bin" ]]; then
    if ! memory_harness_bin="$(pick_memory_harness_bin)"; then
        printf 'could not find memory-harness binary under target/{release,ship,debug}\n' >&2
        printf 'build it first with cargo build --release or cargo build --profile ship\n' >&2
        exit 1
    fi
fi

if [[ ! -x "$memory_harness_bin" ]]; then
    printf 'memory-harness binary is not executable: %s\n' "$memory_harness_bin" >&2
    exit 1
fi

perf_call_graph="${MH_PERF_CALL_GRAPH:-dwarf}"
perf_attach_settle_ms="${MH_PERF_ATTACH_SETTLE_MS:-300}"
if [[ -z "$out_dir" ]]; then
    out_dir="$HOME/.memory-harness/logs/$(date +%Y%m%d-%H%M%S)-$$"
fi

mkdir -p "$out_dir"

for run_idx in $(seq 1 "$repeat_count"); do
    if [[ "$repeat_count" -eq 1 ]]; then
        run_dir="$out_dir"
    else
        run_dir="$out_dir/run-$(printf '%02d' "$run_idx")"
    fi
    run_once "$run_dir" "$@" || exit $?
done
