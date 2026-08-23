#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-supervisor-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
daemon_pid=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

call_daemon() {
    local method="$1"
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    "$target_dir/debug/kwe" daemon-call --socket "$socket" --method "$method" --params "$params"
}

start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --renderer "$target_dir/debug/kwe-test-renderer" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 500 \
        --renderer-frame-timeout-ms 250 \
        --renderer-stop-grace-ms 80 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --renderer-address-space-mib 384 \
        --allow-test-faults >"$smoke_root/daemon.log" 2>&1 &
    daemon_pid=$!
    for _attempt in {1..100}; do
        [[ -S "$socket" ]] && return
        kill -0 "$daemon_pid" 2>/dev/null || {
            echo "daemon exited during startup" >&2
            sed -n '1,120p' "$smoke_root/daemon.log" >&2
            return 1
        }
        sleep 0.02
    done
    echo "daemon socket did not appear" >&2
    return 1
}

wait_phase() {
    local expected="$1"
    local output=""
    for _attempt in {1..250}; do
        output="$(call_daemon renderer.status)"
        if [[ "$(jq -r '.result.phase' <<<"$output")" == "$expected" ]]; then
            printf '%s\n' "$output"
            return
        fi
        sleep 0.02
    done
    echo "timed out waiting for renderer phase $expected" >&2
    printf '%s\n' "$output" >&2
    return 1
}

start_fault() {
    local wallpaper_id="$1"
    local content_hash="$2"
    local kind="$3"
    local after="${4:-3}"
    local mib="${5:-null}"
    local params
    params="$(jq -cn \
        --arg wallpaper_id "$wallpaper_id" \
        --arg content_hash "$content_hash" \
        --arg kind "$kind" \
        --argjson after "$after" \
        --argjson mib "$mib" \
        '{wallpaper_id:$wallpaper_id,content_hash:$content_hash,width:160,height:90,fps:60,test_fault:{kind:$kind,after:$after,mib:$mib}}')"
    call_daemon renderer.start "$params" >/dev/null
}

assert_quarantine() {
    local name="$1"
    local fault="$2"
    local expected_failure="$3"
    start_fault "$name" "hash-$name" "$fault"
    local status
    status="$(wait_phase quarantined)"
    [[ "$(jq -r '.result.failures' <<<"$status")" == "3" ]]
    [[ "$(jq -r '.result.pid' <<<"$status")" == "null" ]]
    [[ "$(jq -r '.result.last_failure' <<<"$status")" == "$expected_failure" ]]
    echo "supervisor fault passed: $fault -> $expected_failure -> quarantined"
}

command -v jq >/dev/null
cd "$project_root"
cargo build --workspace >/dev/null
start_daemon
call_daemon health >/dev/null
limits_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.resource_limits.address_space_mib' <<<"$limits_status")" == "384" ]]
[[ "$(jq -r '.result.resource_limits.core_dump_bytes' <<<"$limits_status")" == "0" ]]
echo "supervisor effective resource-limit diagnostics passed"
if call_daemon renderer.input '{"generation":1,"phase":"move","x":0.5,"y":0.5}' >/dev/null 2>&1; then
    echo "pointer input was accepted without an active renderer" >&2
    exit 1
fi

healthy_params='{"wallpaper_id":"healthy","content_hash":"hash-healthy","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$healthy_params" >/dev/null
healthy_status="$(wait_phase live)"
healthy_generation="$(jq -r '.result.display_generation' <<<"$healthy_status")"
input_status="$(call_daemon renderer.input "{\"generation\":$healthy_generation,\"phase\":\"enter\",\"x\":0.25,\"y\":0.75}")"
input_sequence="$(jq -r '.result.input_sequence' <<<"$input_status")"
[[ "$input_sequence" == "1" ]]
for _attempt in {1..100}; do
    input_status="$(call_daemon renderer.status)"
    [[ "$(jq -r '.result.input_ack_sequence' <<<"$input_status")" == "$input_sequence" ]] && break
    sleep 0.02
done
[[ "$(jq -r '.result.input_ack_sequence' <<<"$input_status")" == "$input_sequence" ]]
[[ "$(jq -r '.result.pointer_inside' <<<"$input_status")" == "true" ]]
[[ "$(jq -r '.result.pointer_x' <<<"$input_status")" == "16384" ]]
[[ "$(jq -r '.result.pointer_y' <<<"$input_status")" == "49151" ]]
if call_daemon renderer.input '{"generation":0,"phase":"move","x":0.5,"y":0.5}' >/dev/null 2>&1; then
    echo "stale input generation was accepted" >&2
    exit 1
fi
if call_daemon renderer.input "{\"generation\":$healthy_generation,\"phase\":\"move\",\"x\":1.1,\"y\":0.5}" >/dev/null 2>&1; then
    echo "out-of-range pointer coordinate was accepted" >&2
    exit 1
fi
call_daemon renderer.input "{\"generation\":$healthy_generation,\"phase\":\"leave\",\"x\":0.25,\"y\":0.75}" >/dev/null
echo "supervisor generation-bound pointer input and renderer acknowledgement passed"
last_good_file="$(jq -r '.last_good.file' "$state_dir/supervisor-v1.json")"
[[ -s "$state_dir/$last_good_file" ]]
head -c 2 "$state_dir/$last_good_file" | cmp -s - <(printf 'P6')
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
echo "supervisor healthy start/frame/fallback/stop passed"

stderr_params='{"wallpaper_id":"stderr-tail","content_hash":"hash-stderr-tail","width":160,"height":90,"fps":60,"kind":"test","stderr_lines":100}'
call_daemon renderer.start "$stderr_params" >/dev/null
wait_phase live >/dev/null
for _attempt in {1..100}; do
    stderr_status="$(call_daemon renderer.status)"
    [[ "$(jq -r '.result.stderr_tail | length' <<<"$stderr_status")" == "64" ]] && break
    sleep 0.02
done
[[ "$(jq -r '.result.stderr_tail | length' <<<"$stderr_status")" == "64" ]]
[[ "$(jq -r '.result.stderr_tail[-1] | contains("index=99")' <<<"$stderr_status")" == "true" ]]
[[ "$(jq -r '.result.stderr_dropped_bytes' <<<"$stderr_status")" != "0" ]]
[[ "$(jq -r '.result.kind' <<<"$stderr_status")" == "test" ]]
call_daemon renderer.stop >/dev/null
echo "supervisor bounded stderr ring surfaced passed"

base_params='{"wallpaper_id":"transaction-base","content_hash":"hash-transaction-base","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$base_params" >/dev/null
base_status="$(wait_phase live)"
base_pid="$(jq -r '.result.pid' <<<"$base_status")"
base_frame="$(jq -r '.result.frame_file' <<<"$base_status")"
base_generation="$(jq -r '.result.display_generation' <<<"$base_status")"

start_fault transaction-bad hash-transaction-bad hang 3
candidate_input_status="$(call_daemon renderer.input "{\"generation\":$base_generation,\"phase\":\"move\",\"x\":0.6,\"y\":0.4}")"
[[ "$(jq -r '.result.pid' <<<"$candidate_input_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.candidate_pid' <<<"$candidate_input_status")" != "null" ]]
rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$rollback_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.frame_file' <<<"$rollback_status")" == "$base_frame" ]]
[[ "$(jq -r '.result.display_generation' <<<"$rollback_status")" == "$base_generation" ]]
kill -0 "$base_pid"
echo "supervisor active-preserving rollback passed"

replacement_params='{"wallpaper_id":"transaction-bad","content_hash":"hash-transaction-bad","width":160,"height":90,"fps":60}'
call_daemon renderer.retry "$replacement_params" >/dev/null
handoff_status="$(wait_phase awaiting_ack)"
replacement_generation="$(jq -r '.result.display_generation' <<<"$handoff_status")"
[[ "$replacement_generation" -gt "$base_generation" ]]
[[ "$(jq -r '.result.previous_pid' <<<"$handoff_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.previous_frame_file' <<<"$handoff_status")" == "$base_frame" ]]
kill -0 "$base_pid"
if call_daemon renderer.ack '{"generation":0}' >/dev/null 2>&1; then
    echo "stale display generation was accepted" >&2
    exit 1
fi
ack_status="$(call_daemon renderer.ack "{\"generation\":$replacement_generation}")"
[[ "$(jq -r '.result.phase' <<<"$ack_status")" == "live" ]]
[[ "$(jq -r '.result.previous_pid' <<<"$ack_status")" == "null" ]]
for _attempt in {1..100}; do
    ! kill -0 "$base_pid" 2>/dev/null && break
    sleep 0.02
done
! kill -0 "$base_pid" 2>/dev/null
call_daemon renderer.stop >/dev/null
echo "supervisor acknowledged display handoff passed"

post_promotion_base='{"wallpaper_id":"post-promotion-base","content_hash":"hash-post-promotion-base","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$post_promotion_base" >/dev/null
post_base_status="$(wait_phase live)"
post_base_pid="$(jq -r '.result.pid' <<<"$post_base_status")"
post_base_frame="$(jq -r '.result.frame_file' <<<"$post_base_status")"
start_fault post-promotion-fail hash-post-promotion-fail exit 20
post_rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$post_rollback_status")" == "$post_base_pid" ]]
[[ "$(jq -r '.result.frame_file' <<<"$post_rollback_status")" == "$post_base_frame" ]]
[[ "$(jq -r '.last_good.wallpaper_id' "$state_dir/supervisor-v1.json")" == "post-promotion-base" ]]
kill -0 "$post_base_pid"
call_daemon renderer.stop >/dev/null
echo "supervisor pre-ack failure rollback passed"

timeout_base='{"wallpaper_id":"timeout-base","content_hash":"hash-timeout-base","width":160,"height":90,"fps":60}'
timeout_new='{"wallpaper_id":"timeout-new","content_hash":"hash-timeout-new","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$timeout_base" >/dev/null
timeout_base_status="$(wait_phase live)"
timeout_base_pid="$(jq -r '.result.pid' <<<"$timeout_base_status")"
call_daemon renderer.start "$timeout_new" >/dev/null
wait_phase awaiting_ack >/dev/null
wait_phase live >/dev/null
! kill -0 "$timeout_base_pid" 2>/dev/null
[[ "$(jq -r '.last_good.wallpaper_id' "$state_dir/supervisor-v1.json")" == "timeout-new" ]]
call_daemon renderer.stop >/dev/null
echo "supervisor bounded handoff timeout commit passed"

start_fault forced-stop hash-forced ignore_term_hang 100
wait_phase live >/dev/null
forced_status="$(call_daemon renderer.stop)"
[[ "$(jq -r '.result.forced_kill_count' <<<"$forced_status")" == "1" ]]
echo "supervisor forced kill and reap passed"

assert_quarantine stale hang frame_timeout
retry_params='{"wallpaper_id":"stale","content_hash":"hash-stale","width":160,"height":90,"fps":60}'
call_daemon renderer.retry "$retry_params" >/dev/null
retry_status="$(wait_phase live)"
[[ "$(jq -r '.result.failures' <<<"$retry_status")" == "0" ]]
call_daemon renderer.stop >/dev/null
echo "supervisor explicit quarantine retry passed"

assert_quarantine corrupt corrupt invalid_frame
assert_quarantine crash exit process_exit
assert_quarantine startup startup_hang startup_timeout

pressure_base='{"wallpaper_id":"pressure-base","content_hash":"hash-pressure-base","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$pressure_base" >/dev/null
pressure_base_status="$(wait_phase live)"
pressure_base_pid="$(jq -r '.result.pid' <<<"$pressure_base_status")"
pressure_base_frame="$(jq -r '.result.frame_file' <<<"$pressure_base_status")"
start_fault pressure-candidate hash-pressure-candidate memory_pressure 3 1024
pressure_rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.last_failure' <<<"$pressure_rollback_status")" == "resource_limit" ]]
[[ "$(jq -r '.result.failures' <<<"$pressure_rollback_status")" == "3" ]]
[[ "$(jq -r '.result.pid' <<<"$pressure_rollback_status")" == "$pressure_base_pid" ]]
[[ "$(jq -r '.result.frame_file' <<<"$pressure_rollback_status")" == "$pressure_base_frame" ]]
kill -0 "$pressure_base_pid"
call_daemon renderer.stop >/dev/null
echo "supervisor memory-pressure containment and active rollback passed"

parent_death_params='{"wallpaper_id":"parent-death","content_hash":"hash-parent-death","width":160,"height":90,"fps":60}'
call_daemon renderer.start "$parent_death_params" >/dev/null
parent_death_status="$(wait_phase live)"
renderer_pid="$(jq -r '.result.pid' <<<"$parent_death_status")"
kill "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
for _attempt in {1..100}; do
    ! kill -0 "$renderer_pid" 2>/dev/null && break
    sleep 0.02
done
if kill -0 "$renderer_pid" 2>/dev/null; then
    echo "renderer survived daemon exit" >&2
    exit 1
fi
echo "supervisor parent-death cleanup passed"

rm -f -- "$socket"
start_daemon
persistent_params='{"wallpaper_id":"startup","content_hash":"hash-startup","width":160,"height":90,"fps":60}'
persistent_status="$(call_daemon renderer.start "$persistent_params")"
[[ "$(jq -r '.result.phase' <<<"$persistent_status")" == "quarantined" ]]
[[ "$(jq -r '.result.pid' <<<"$persistent_status")" == "null" ]]
echo "supervisor persisted quarantine passed"

echo "all supervisor smoke cases passed"
