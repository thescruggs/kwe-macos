#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Bounded audio-capture smoke suite (BETA_M1d).
# Mirrors scripts/smoke-video.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. Audio
# capture is directed at a freshly created null sink (pactl module-null-sink
# or pw-cli adapter node) via --audio-capture-node so the worker never touches
# the user's real default sink. If no PipeWire control tool, no pw-record/
# pw-dump, or no reachable PipeWire session is available, the suite prints
# SKIPPED and exits 0.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-audio-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
fixture="$smoke_root/fixture.mp4"
daemon_pid=""
sink_module=""
pw_cli_node=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ -n "$sink_module" ]]; then
        pactl unload-module "$sink_module" 2>/dev/null || true
    elif [[ -n "$pw_cli_node" ]]; then
        pw-cli destroy "$pw_cli_node" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

command -v jq >/dev/null || {
    echo "SKIPPED: jq is not installed"
    exit 0
}
if ! command -v pw-record >/dev/null || ! command -v pw-dump >/dev/null; then
    echo "SKIPPED: pw-record/pw-dump are not installed (bounded PipeWire capture needs them)"
    exit 0
fi
if ! command -v pactl >/dev/null && ! command -v pw-cli >/dev/null; then
    echo "SKIPPED: no pactl/pw-cli (cannot create an isolated null sink)"
    exit 0
fi
# Session probe: tools present but no reachable PipeWire session (headless
# container, missing user session) must skip rather than fail.
if command -v pactl >/dev/null; then
    pactl info >/dev/null 2>&1 || {
        echo "SKIPPED: pactl cannot reach a PipeWire session"
        exit 0
    }
else
    pw-cli info >/dev/null 2>&1 || {
        echo "SKIPPED: pw-cli cannot reach a PipeWire session"
        exit 0
    }
fi

# Isolated capture target: a null sink named kwe_smoke, owned by this script.
if command -v pactl >/dev/null; then
    sink_module="$(pactl load-module module-null-sink sink_name=kwe_smoke)"
    sink_name="kwe_smoke"
else
    pw_cli_node="$(pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=kwe_smoke media.class=Audio/Sink object.linger=true }' | grep -oE '[0-9]+' | tail -1)"
    [[ -n "$pw_cli_node" ]]
    sink_name="kwe_smoke"
fi

call_daemon() {
    local method="$1"
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    "$target_dir/debug/kwe" daemon-call --socket "$socket" --method "$method" --params "$params"
}

# The daemon resolves the audio worker by default to kwe-audio-worker beside
# the daemon executable: this exercises the default worker-path resolution.
# The supervisor timings mirror smoke-video.sh (video renderer lane).
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
        --renderer-processes 4096 \
        --audio-capture \
        --audio-capture-node "$sink_name" >"$smoke_root/daemon.log" 2>&1 &
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
    local params
    params="$(jq -cn \
        --arg content "$fixture" \
        '{wallpaper_id:"audio-case2",content_hash:"hash-audio-case2",width:160,height:90,fps:30,kind:"video",content:$content}')"
    call_daemon renderer.start "$params"
}

cd "$project_root"
cargo build --workspace >/dev/null

# Case 1: the worker starts under --audio-capture; audio.status shows the
# live pid (the worker itself resolved nothing here -- the null sink name is
# passed through as --capture-node, and pw-record captures it directly).
start_daemon
call_daemon health >/dev/null
worker_pid=""
for _attempt in {1..500}; do
    audio_status="$(call_daemon audio.status)"
    worker_pid="$(jq -r '.result.pid' <<<"$audio_status")"
    [[ "$worker_pid" != "null" ]] && break
    sleep 0.02
done
[[ -n "$worker_pid" && "$worker_pid" != "null" ]]
[[ "$(jq -r '.result.enabled' <<<"$audio_status")" == "true" ]]
[[ "$(jq -r '.result.restarts' <<<"$audio_status")" == "0" ]]
kill -0 "$worker_pid"
echo "audio smoke passed: --audio-capture spawns the worker, audio.status enabled with live pid"

# Case 2: with a live video renderer, audio frames flow through the daemon
# and are acked by the renderer (input_ack_sequence follows the promoted
# display generation); the protocol stays error-free. A stop/start generation
# bump demonstrates the ack advancing.
ffmpeg -loglevel error -f lavfi -i "testsrc2=size=64x64:rate=30" -t 2 \
    -pix_fmt yuv420p "$fixture" -y
start_video >/dev/null
live_status="$(wait_phase live)"
generation_a="$(jq -r '.result.display_generation' <<<"$live_status")"
[[ "$generation_a" != "0" ]]
ack_a="0"
for _attempt in {1..500}; do
    ack_a="$(jq -r '.result.input_ack_sequence' <<<"$(call_daemon renderer.status)")"
    [[ "$ack_a" != "0" ]] && break
    sleep 0.02
done
[[ "$ack_a" != "0" ]]
[[ "$(jq -r '.result.input_protocol_errors' <<<"$(call_daemon renderer.status)")" == "0" ]]
# Stop/start the renderer: the promotion bumps the display generation, and
# the worker's refreshed frames are acked at the new generation.
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
start_video >/dev/null
restarted_status="$(wait_phase live)"
generation_b="$(jq -r '.result.display_generation' <<<"$restarted_status")"
[[ "$generation_b" -gt "$generation_a" ]]
ack_b="0"
for _attempt in {1..500}; do
    ack_b="$(jq -r '.result.input_ack_sequence' <<<"$(call_daemon renderer.status)")"
    [[ "$ack_b" -gt "$ack_a" ]] && break
    sleep 0.02
done
[[ "$ack_b" -gt "$ack_a" ]]
[[ "$ack_b" -ge "$generation_b" ]]
[[ "$(jq -r '.result.input_protocol_errors' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "audio smoke passed: audio_bands acked at the promoted generation, input_protocol_errors 0"

# Case 2b: with audio frames still flowing, pointer input must be acked at
# its own (higher) sequence. The audio wire carries the display generation,
# which sits below the pointer sequence, so the ack ceiling must never be
# lowered by audio frames (regression: a ceiling reset rejected in-flight
# pointer acks as protocol errors).
pointer_last="0"
for phase in enter move move move move leave; do
    pointer_input="$(call_daemon renderer.input "{\"generation\":$generation_b,\"phase\":\"$phase\",\"x\":0.5,\"y\":0.5}")"
    pointer_last="$(jq -r '.result.input_sequence' <<<"$pointer_input")"
    [[ -n "$pointer_last" && "$pointer_last" != "null" ]]
done
[[ "$pointer_last" -gt "$generation_b" ]]
ack_pointer="0"
for _attempt in {1..500}; do
    ack_pointer="$(jq -r '.result.input_ack_sequence' <<<"$(call_daemon renderer.status)")"
    [[ "$ack_pointer" -ge "$pointer_last" ]] && break
    sleep 0.02
done
[[ "$ack_pointer" -ge "$pointer_last" ]]
[[ "$(jq -r '.result.input_protocol_errors' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "audio smoke passed: pointer input acked past the display generation with 0 protocol errors"

# Case 3: with no renderer promoted, the daemon's own worker is dropped
# silently (latest-wins) instead of erroring: no client_error storm, and the
# daemon log carries only the rate-limited drop note.
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
sleep 2
[[ "$(grep -c "event=api.client_error" "$smoke_root/daemon.log" || true)" == "0" ]]
drop_lines="$(grep -c "event=audio.forward.dropped" "$smoke_root/daemon.log" || true)"
[[ "$drop_lines" -ge 1 ]]
[[ "$drop_lines" -le 10 ]]
[[ "$(jq -r '.result.input_protocol_errors' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "audio smoke passed: renderer.stop -> silent latest-wins drops, no error storm"

# Case 4: kill -9 the worker; the daemon records the exit, restarts it once,
# and audio.status reports the new pid with restarts 1.
killed_pid="$worker_pid"
kill -9 "$killed_pid"
restarted=""
for _attempt in {1..500}; do
    restarted="$(call_daemon audio.status)"
    new_pid="$(jq -r '.result.pid' <<<"$restarted")"
    [[ "$new_pid" != "null" && "$new_pid" != "$killed_pid" ]] && break
    sleep 0.02
done
[[ -n "$restarted" ]]
[[ "$(jq -r '.result.restarts' <<<"$restarted")" == "1" ]]
new_pid="$(jq -r '.result.pid' <<<"$restarted")"
kill -0 "$new_pid"
echo "audio smoke passed: kill -9 recorded, worker restarted once with a new pid"

# Case 5: SIGTERM the daemon; it stops the worker, whose own graceful path
# (pw-record stopped first) logs event=audio.worker.stopped on its inherited
# stderr and exits 0. A forced kill would leave a different log line, so its
# absence is the exit-0 evidence (a non-child's exit code is not directly
# observable).
kill -TERM "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=""
# The worker observes the parent-death signal and stops pw-record before
# exiting; poll briefly for both the log line and the pid to vanish.
stopped_logs="0"
for _attempt in {1..250}; do
    stopped_logs="$(grep -c "event=audio.worker.stopped" "$smoke_root/daemon.log" || true)"
    kill -0 "$new_pid" 2>/dev/null || break
    sleep 0.02
done
[[ "$stopped_logs" -ge 1 ]]
[[ "$(grep -c "event=audio.worker.forced_kill" "$smoke_root/daemon.log" || true)" == "0" ]]
if kill -0 "$new_pid" 2>/dev/null; then
    echo "audio worker survived daemon shutdown" >&2
    exit 1
fi
echo "all audio smoke cases passed"
