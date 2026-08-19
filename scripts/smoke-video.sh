#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Supervised video-renderer smoke suite (BETA_M1b).
# Mirrors scripts/smoke-supervisor.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. The
# video fixture is generated at runtime with ffmpeg and never committed.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-video-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
fixture="$smoke_root/fixture.mp4"
garbage="$smoke_root/garbage.bin"
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

# The video renderer resolves by default to kwe-video-renderer beside the
# daemon executable: this exercises the default renderer-path resolution.
# The address-space budget is 2048 MiB, not the test renderer's 384 MiB:
# libmpv plus the nvidia VA-API mappings measured ~1-2 GiB of virtual address
# space here, and a 384 MiB limit makes libmpv die silently (SIGSEGV).
# The process ceiling is 4096, not the daemon's 1024 default: the kernel
# RLIMIT_NPROC check counts every thread of the uid (user->processes), and a
# desktop session commonly exceeds 1024 threads, so libmpv's pthread_create
# fails with EAGAIN and mpv_create hangs in its failure path (docs/BETA_M1.md).
start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 500 \
        --renderer-video-startup-timeout-ms 8000 \
        --renderer-frame-timeout-ms 1000 \
        --renderer-stop-grace-ms 80 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --renderer-address-space-mib 2048 \
        --renderer-processes 4096 >"$smoke_root/daemon.log" 2>&1 &
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
    for _attempt in {1..500}; do
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

start_video() {
    local wallpaper_id="$1"
    local content_hash="$2"
    local content="$3"
    local params
    params="$(jq -cn \
        --arg wallpaper_id "$wallpaper_id" \
        --arg content_hash "$content_hash" \
        --arg content "$content" \
        '{wallpaper_id:$wallpaper_id,content_hash:$content_hash,width:160,height:90,fps:30,kind:"video",content:$content}')"
    call_daemon renderer.start "$params"
}

media_state() {
    local generation="$1"
    local playback="$2"
    local params
    params="$(jq -cn --argjson generation "$generation" --arg playback "$playback" \
        '{generation:$generation,playback:$playback}')"
    call_daemon media.state "$params" >/dev/null
}

command -v jq >/dev/null
command -v ffmpeg >/dev/null

# Case 1: runtime-generated synthetic fixture (never committed).
ffmpeg -loglevel error -f lavfi -i "testsrc2=size=64x64:rate=30" -t 2 \
    -pix_fmt yuv420p "$fixture" -y
head -c 65536 /dev/urandom >"$garbage"
echo "video smoke: fixture generated"

cd "$project_root"
cargo build --workspace >/dev/null
start_daemon
call_daemon health >/dev/null

# Case 2: healthy video renderer through the daemon, default video path.
live_params='{"wallpaper_id":"video","content_hash":"hash-video","width":160,"height":90,"fps":30,"kind":"video","content":"'"$fixture"'"}'
call_daemon renderer.start "$live_params" >/dev/null
live_status="$(wait_phase live)"
[[ "$(jq -r '.result.kind' <<<"$live_status")" == "video" ]]
[[ "$(jq -r '.result.content_hash' <<<"$live_status")" == "hash-video" ]]
live_pid="$(jq -r '.result.pid' <<<"$live_status")"
live_generation="$(jq -r '.result.display_generation' <<<"$live_status")"
sequence_first="$(jq -r '.result.sequence' <<<"$live_status")"
sleep 1
sequence_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$sequence_second" -gt "$sequence_first" ]]
[[ "$(jq -r '.result.failures' <<<"$live_status")" == "0" ]]
last_good_file="$(jq -r '.last_good.file' "$state_dir/supervisor-v1.json")"
[[ -s "$state_dir/$last_good_file" ]]
head -c 2 "$state_dir/$last_good_file" | cmp -s - <(printf 'P6')
echo "video smoke passed: live start, kind/content, advancing sequence, last-good P6"

# Case 3: paused media state; keepalive keeps the sequence advancing.
media_state "$live_generation" paused
sleep 1.5
paused_first="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
sleep 1
paused_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$paused_second" -gt "$paused_first" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "video smoke passed: paused media state keeps keepalive publishing, no failure"

# Case 4: playing resumes; renderer stays healthy.
media_state "$live_generation" playing
sleep 0.5
[[ "$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")" == "live" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "video smoke passed: resumed playback, renderer healthy"

# Case 5: kill -9 the active worker; the daemon records one failure (visible
# during the restart window) and auto-restarts. The restarted worker's first
# successful canary then promotes it, and a promotion clears the failure
# record (success resets failure history by daemon design), so the live
# status shows failures 0 with a new pid and no quarantine.
kill -9 "$live_pid"
wait_phase restarting >/dev/null
failed_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.failures' <<<"$failed_status")" == "1" ]]
[[ "$(jq -r '.result.last_failure' <<<"$failed_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$failed_status")" == *"signal_9"* ]]
kill_restart_status="$(wait_phase live)"
[[ "$(jq -r '.result.failures' <<<"$kill_restart_status")" == "0" ]]
[[ "$(jq -r '.result.pid' <<<"$kill_restart_status")" != "null" ]]
[[ "$(jq -r '.result.pid' <<<"$kill_restart_status")" != "$live_pid" ]]
[[ "$(jq -r '.result.phase' <<<"$kill_restart_status")" == "live" ]]
echo "video smoke passed: kill -9 recorded once, auto-restarted, not quarantined"

# Case 6: repeated kill -9s with no intervening success hit the three-failure
# budget -> quarantined, and renderer.start for the same identity is refused
# with the quarantine phase. The first kill takes the promoted active worker;
# the later kills must land on the restarted candidate before its first
# successful promotion, because a promotion clears the failure record.
for _attempt in {1..3}; do
    target=""
    for _poll in {1..100}; do
        status="$(call_daemon renderer.status)"
        target="$(jq -r '.result.pid // .result.candidate_pid // empty' <<<"$status")"
        [[ -n "$target" ]] && break
        sleep 0.02
    done
    [[ -n "$target" ]]
    kill -9 "$target" 2>/dev/null || true
    for _poll in {1..250}; do
        phase="$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")"
        [[ "$phase" == "restarting" || "$phase" == "quarantined" ]] && break
        sleep 0.02
    done
    [[ "$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")" == "quarantined" ]] && break
done
quarantined_status="$(wait_phase quarantined)"
[[ "$(jq -r '.result.failures' <<<"$quarantined_status")" == "3" ]]
[[ "$(jq -r '.result.pid' <<<"$quarantined_status")" == "null" ]]
refused_status="$(call_daemon renderer.start "$live_params")"
[[ "$(jq -r '.result.phase' <<<"$refused_status")" == "quarantined" ]]
[[ "$(jq -r '.result.pid' <<<"$refused_status")" == "null" ]]
echo "video smoke passed: three failures quarantine and refuse the identity"

# Case 7: missing content path is rejected before launch (invalid_params).
missing_params='{"wallpaper_id":"video-missing","content_hash":"hash-video-missing","width":160,"height":90,"fps":30,"kind":"video","content":"/nonexistent/kwe-video.mp4"}'
if call_daemon renderer.start "$missing_params" >/dev/null 2>&1; then
    echo "missing content path was accepted" >&2
    exit 1
fi
echo "video smoke passed: missing content path rejected with invalid_params"

# Case 8: garbage content is launchable (path-level preflight passes) but the
# worker rejects the backend (exit 73) before the canary; the active base
# worker stays live and the failure detail names exit_code_73.
base_params='{"wallpaper_id":"case8-base","content_hash":"hash-case8-base","width":160,"height":90,"fps":30,"kind":"video","content":"'"$fixture"'"}'
call_daemon renderer.start "$base_params" >/dev/null
base_status="$(wait_phase live)"
base_pid="$(jq -r '.result.pid' <<<"$base_status")"
garbage_params='{"wallpaper_id":"case8-garbage","content_hash":"hash-case8-garbage","width":160,"height":90,"fps":30,"kind":"video","content":"'"$garbage"'"}'
call_daemon renderer.start "$garbage_params" >/dev/null
rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$rollback_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$rollback_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$rollback_status")" == *"exit_code_73"* ]]
kill -0 "$base_pid"
echo "video smoke passed: garbage content -> worker exit 73 -> rolled_back with exit_code_73"

call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
call_daemon health >/dev/null
echo "all video smoke cases passed"
