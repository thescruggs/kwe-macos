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
garbage="$smoke_root/garbage.mp4"
bad_extension="$smoke_root/garbage.bin"
long_duration="$smoke_root/long-duration.mp4"
oracle="$smoke_root/oracle.mp4"
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
# No process-ceiling override is passed: since M1e the video kind carries
# its own --renderer-video-processes default (32768), because the kernel
# RLIMIT_NPROC check counts every thread of the uid (user->processes) and a
# desktop session commonly exceeds the global 1024 default — this lane
# running without an override IS the proof (docs/BETA_M1.md risk 1).
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
        --renderer-address-space-mib 2048 >"$smoke_root/daemon.log" 2>&1 &
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
command -v ffprobe >/dev/null
command -v python3 >/dev/null

# Bounded pixel oracle for the shared frame file (docs/FRAME_PROTOCOL_V1.md):
# a 64-byte little-endian header, then two tightly packed BGRA8888 slots; the
# active slot and dimensions come from the header. Samples a fixed set of
# pixels from the active slot and compares each channel against the expected
# BGRA of the flat fixture color within a small per-channel tolerance. Prints
# every sampled pixel, so a drift records its exact observed values.
#
# The fixture is 64x64 (1:1) while the render target is 160x90 (16:9); libmpv
# aspect-fits the video inside the target (observed empirically, verified
# 2026-08-19 — see docs/BETA_M1.md), so the caller passes the fitted content
# region [x0, x1) x [y0, y1) and only content-region pixels are sampled.
# contain-fit math: scale = min(160/64, 90/64) = 1.40625 -> 90x90 centered.
check_oracle() {
    local frame_file="$1"
    local delta="$2"
    local region_x0="$3"
    local region_y0="$4"
    local region_x1="$5"
    local region_y1="$6"
    python3 - "$frame_file" "$delta" "$region_x0" "$region_y0" "$region_x1" "$region_y1" <<'PY'
import struct
import sys

path, delta = sys.argv[1], int(sys.argv[2])
x0, y0, x1, y1 = int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
# BGRA8888 premultiplied bytes of the flat fixture color #3366CC: the
# hex triplet is R=0x33, G=0x66, B=0xCC, so the BGRA byte order is
# B=0xCC, G=0x66, R=0x33, A=0xFF.
expected = (0xCC, 0x66, 0x33, 0xFF)
f = open(path, "rb")
try:
    header = f.read(64)
    if len(header) != 64:
        sys.exit("short header")
    magic = struct.unpack_from("<8s", header, 0)[0]
    version = struct.unpack_from("<I", header, 8)[0]
    header_bytes = struct.unpack_from("<I", header, 12)[0]
    total = struct.unpack_from("<Q", header, 16)[0]
    width = struct.unpack_from("<I", header, 24)[0]
    height = struct.unpack_from("<I", header, 28)[0]
    stride = struct.unpack_from("<I", header, 32)[0]
    pixel_format = struct.unpack_from("<I", header, 36)[0]
    slots = struct.unpack_from("<I", header, 40)[0]
    active = struct.unpack_from("<I", header, 56)[0]
    if magic != b"KWEFRM1\0":
        sys.exit("bad magic")
    if version != 1 or header_bytes != 64 or pixel_format != 1 or slots != 2:
        sys.exit("bad header fields")
    # Protocol bounds keep every offset arithmetic in range (frame protocol
    # v1 caps dimensions at 8192 and the file at 512 MiB).
    if not (1 <= width <= 8192 and 1 <= height <= 8192):
        sys.exit("dimensions outside protocol bounds")
    if active not in (0, 1) or stride != width * 4:
        sys.exit("bad slot/stride")
    if total != 64 + 2 * stride * height:
        sys.exit("bad total size")
    if not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height):
        sys.exit("content region outside the frame")
    samples = [
        ((x0 + x1) // 2, (y0 + y1) // 2),
        ((x0 + x1) // 4, (y0 + y1) // 4),
        ((x0 + x1) * 3 // 4, (y0 + y1) * 3 // 4),
        ((x0 + x1) * 3 // 4, (y0 + y1) // 4),
        ((x0 + x1) // 4, (y0 + y1) * 3 // 4),
        (x0 + 1, y0 + 1),
        (x1 - 2, y1 - 2),
        (x0 + 1, y1 - 2),
        (x1 - 2, y0 + 1),
    ]
    # Snapshot algorithm (docs/FRAME_PROTOCOL_V1.md): the producer writes
    # the inactive slot, flips the active-slot atomic, then bumps the
    # generation to even. The consumer samples only at an even generation
    # and accepts the pixels when the generation is unchanged afterwards;
    # bounded retries cover the live producer's publishes. The whole header
    # (including the active-slot atomic) is re-read per attempt, because
    # the producer may flip the slot between attempts.
    import time

    worst = 0
    accepted = False
    for attempt in range(64):
        f.seek(0)
        header = f.read(64)
        generation = struct.unpack_from("<Q", header, 48)[0]
        active = struct.unpack_from("<I", header, 56)[0]
        if generation % 2 != 0 or active not in (0, 1):
            continue
        pixels = []
        for x, y in samples:
            offset = 64 + active * stride * height + y * stride + x * 4
            f.seek(offset)
            bgra = f.read(4)
            if len(bgra) != 4:
                sys.exit("short pixel read")
            pixels.append((x, y, bgra))
        f.seek(48)
        again = struct.unpack_from("<Q", f.read(8), 0)[0]
        if again != generation:
            time.sleep(0.001)
            continue
        accepted = True
        break
    if not accepted:
        sys.exit("frame generation never stabilized")
    for x, y, bgra in pixels:
        deviation = max(
            abs(bgra[0] - expected[0]),
            abs(bgra[1] - expected[1]),
            abs(bgra[2] - expected[2]),
            abs(bgra[3] - expected[3]),
        )
        worst = max(worst, deviation)
        print(
            "oracle pixel (%d,%d) = BGRA %02x %02x %02x %02x (expected %02x %02x %02x %02x)"
            % (x, y, bgra[0], bgra[1], bgra[2], bgra[3], *expected)
        )
finally:
    f.close()
if worst > delta:
    sys.exit("pixel deviation %d exceeds tolerance %d" % (worst, delta))
print("ORACLE-OK worst_channel_deviation=%d tolerance=%d" % (worst, delta))
PY
}

# Case 1: runtime-generated synthetic fixture (never committed).
ffmpeg -loglevel error -f lavfi -i "testsrc2=size=64x64:rate=30" -t 2 \
    -pix_fmt yuv420p "$fixture" -y
head -c 65536 /dev/urandom >"$garbage"
head -c 65536 /dev/urandom >"$bad_extension"
# A >24 h fixture: 30 frames re-timestamped so PTS = real PTS * 90000 (the
# last frame lands at 87000 s). Tiny file, sub-second to generate; the
# worker's duration bound must reject it before the canary. The ffprobe
# guard asserts the fixture really reports >24 h, so the case cannot silently
# regress into a fail-open.
ffmpeg -loglevel error -f lavfi -i "testsrc2=size=64x64:rate=30" -frames:v 30 \
    -vf "setpts=PTS*90000" -fps_mode vfr -c:v libx264 -pix_fmt yuv420p \
    "$long_duration" -y
long_duration_seconds="$(ffprobe -v error -show_entries format=duration \
    -of default=nw=1:nk=1 "$long_duration")"
[[ "$(printf '%.0f' "$long_duration_seconds")" -gt 86400 ]]
# Case 11 oracle fixture: a deterministic FLAT #3366CC frame, so every
# decoded pixel must equal the expected BGRA (a flat frame survives
# scaling and YUV round-trips without chroma-subsampling edge error).
ffmpeg -loglevel error -f lavfi -i "color=c=0x3366CC:s=64x64:r=30" -t 5 \
    -c:v libx264 -pix_fmt yuv420p "$oracle" -y
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

# Case 3: paused media state; keepalive keeps the sequence advancing, and the
# worker's ack round-trips into renderer.status.input_ack_sequence.
media_state "$live_generation" paused
sleep 1.5
paused_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.input_ack_sequence' <<<"$paused_status")" != "0" ]]
paused_first="$(jq -r '.result.sequence' <<<"$paused_status")"
sleep 1
paused_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$paused_second" -gt "$paused_first" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "video smoke passed: paused media state keeps keepalive publishing, acked, no failure"

# Case 4: playing resumes; renderer stays healthy.
media_state "$live_generation" playing
sleep 0.5
[[ "$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")" == "live" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "video smoke passed: resumed playback, renderer healthy"

# Case 4b: stopped media state maps to pause + seek 0 (surfaced on the live
# worker's stderr ring), and keepalive keeps the sequence advancing.
media_state "$live_generation" stopped
sleep 1.5
stop_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$stop_tail" == *"applied=Stop"* ]]
stopped_first="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
sleep 1
stopped_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$stopped_second" -gt "$stopped_first" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
echo "video smoke passed: stopped media state pauses with seek 0, keepalive continues"

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
# with the quarantine phase. A kill landing on the promoted active worker
# would let the next promotion clear the failure record, so each iteration
# prefers the canary-window candidate and the loop runs until the counter
# actually reads 3 (hard cap 8; case 5 already covers the signal path).
for _attempt in {1..8}; do
    target=""
    for _poll in {1..100}; do
        status="$(call_daemon renderer.status)"
        target="$(jq -r '.result.candidate_pid // .result.pid // empty' <<<"$status")"
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
    failures="$(jq -r '.result.failures // 0' <<<"$(call_daemon renderer.status)")"
    [[ "$failures" == "3" ]] && break
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

# Case 8: garbage content inside an allowlisted extension passes the static
# preflight and is launchable, but the worker rejects the backend (exit 73)
# before the canary; the active base worker stays live and the failure detail
# names exit_code_73.
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
echo "video smoke passed: garbage.mp4 content -> worker exit 73 -> rolled_back with exit_code_73"

# Case 9: a disallowed extension is rejected by the static video preflight
# before any worker is spawned (invalid_params naming the preflight reason).
bad_extension_params='{"wallpaper_id":"case9-bad-ext","content_hash":"hash-case9-bad-ext","width":160,"height":90,"fps":30,"kind":"video","content":"'"$bad_extension"'"}'
if rejected="$(call_daemon renderer.start "$bad_extension_params" 2>&1)"; then
    echo "disallowed video extension was accepted" >&2
    exit 1
fi
[[ "$rejected" == *"video preflight rejected"* ]]
[[ "$rejected" == *"unsupported video extension"* ]]
echo "video smoke passed: .bin extension rejected by preflight with reason"

# Case 10: media with a known duration over 24 h passes the static preflight
# but the worker rejects the backend duration bound (exit 73) before the
# canary; the active base worker stays live and the failure detail names
# exit_code_73 and the duration diagnostic.
base10_params='{"wallpaper_id":"case10-base","content_hash":"hash-case10-base","width":160,"height":90,"fps":30,"kind":"video","content":"'"$fixture"'"}'
call_daemon renderer.start "$base10_params" >/dev/null
base10_status="$(wait_phase live)"
base10_pid="$(jq -r '.result.pid' <<<"$base10_status")"
long_params='{"wallpaper_id":"case10-long","content_hash":"hash-case10-long","width":160,"height":90,"fps":30,"kind":"video","content":"'"$long_duration"'"}'
call_daemon renderer.start "$long_params" >/dev/null
long_rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$long_rollback_status")" == "$base10_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$long_rollback_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$long_rollback_status")" == *"exit_code_73"* ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$long_rollback_status")" == *"exceeds the 24 h bound"* ]]
kill -0 "$base10_pid"
echo "video smoke passed: >24 h duration content -> worker exit 73 -> rolled_back with duration diagnostic"

# Case 11: pixel oracle — the solid-color fixture decoded by libmpv must
# land in the shared frame file as the expected BGRA. Reads the active
# worker's frame_file from renderer.status, validates the 64-byte header,
# and samples nine fixed pixels from the active slot. The per-channel
# tolerance covers libmpv's colorspace round-trip of the flat frame; the
# exact observed values are recorded in docs/BETA_M1.md.
oracle_params='{"wallpaper_id":"case11-oracle","content_hash":"hash-case11-oracle","width":160,"height":90,"fps":30,"kind":"video","content":"'"$oracle"'"}'
call_daemon renderer.start "$oracle_params" >/dev/null
oracle_live="$(wait_phase live)"
oracle_first="$(jq -r '.result.sequence' <<<"$oracle_live")"
sleep 1
oracle_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$oracle_second" -gt "$oracle_first" ]]
oracle_frame_file="$(jq -r '.result.frame_file' <<<"$(call_daemon renderer.status)")"
[[ -n "$oracle_frame_file" && -f "$oracle_frame_file" ]]
# Fitted content region for the 64x64 fixture in the 160x90 target:
# contain-fit scale 90/64 keeps the 1:1 aspect, giving 90x90 centered
# horizontally (x from 35 to 125), full height.
check_oracle "$oracle_frame_file" 4 35 0 125 90
echo "video smoke passed: solid-color oracle matches expected BGRA within tolerance"

# Final stop: the daemon stops the active worker and stays healthy. A
# graceful stop records no failure (last_failure surfaces the *requested*
# identity's record — here the quarantined garbage one — so it is not
# asserted); the worker's own exit-0 and Stopping-state evidence is verified
# standalone, docs/BETA_M1.md.
call_daemon renderer.stop >/dev/null
stopped_status="$(wait_phase stopped)"
[[ "$(jq -r '.result.pid' <<<"$stopped_status")" == "null" ]]
call_daemon health >/dev/null
echo "all video smoke cases passed"
