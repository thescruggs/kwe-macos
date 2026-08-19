#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Supervised SceneScript smoke suite (BETA_M3a).
# Mirrors scripts/smoke-video.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. The
# scene.json + script.js fixtures are generated at runtime and never
# committed, and the SceneScript engine is exercised end-to-end: the frame
# oracle proves update() drives rendering (the clear color is scripted, not
# the scene.json default), a throwing script stays contained, and a final
# standalone llvmpipe lane runs the worker directly under the software
# rasterizer with --device llvmpipe (docs/BETA_M3.md).
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-scene-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
scene="$smoke_root/scene.json"
script="$smoke_root/script.js"
throw_scene="$smoke_root/throw.json"
throw_script="$smoke_root/throw.js"
garbage_scene="$smoke_root/garbage.json"
missing_scene="$smoke_root/missing.json"
daemon_pid=""
plasma_before="$(pgrep -x plasmashell | head -1 || true)"

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

# The scene renderer resolves by default to kwe-scene-renderer beside the
# daemon executable: this exercises the default renderer-path resolution.
start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 500 \
        --renderer-frame-timeout-ms 1000 \
        --renderer-stop-grace-ms 80 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 >"$smoke_root/daemon.log" 2>&1 &
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

start_scene() {
    local wallpaper_id="$1"
    local content_hash="$2"
    local content="$3"
    local params
    params="$(jq -cn \
        --arg wallpaper_id "$wallpaper_id" \
        --arg content_hash "$content_hash" \
        --arg content "$content" \
        '{wallpaper_id:$wallpaper_id,content_hash:$content_hash,width:160,height:90,fps:30,kind:"scene",content:$content}')"
    call_daemon renderer.start "$params"
}

command -v jq >/dev/null
command -v python3 >/dev/null

# Scene pixel oracle for the shared frame file (docs/FRAME_PROTOCOL_V1.md):
# a 64-byte little-endian header, then two tightly packed BGRA8888 slots; the
# active slot and dimensions come from the header. The scene fixture scripts
# the clear color: init() pins g=0.3, b=0.4, a=1.0, and update() sweeps
# r=(t % 2) / 2 over a 2 s sawtooth, so any two samples ~1.5 s apart always
# differ in r by >= 0.25 (>= 63 in u8). The oracle snapshots the center
# pixel twice (snapshot algorithm: even generation, stable re-read) and
# asserts: header fields sane, premultiplied alpha opaque, g/b matching the
# init()-set channels (proving init() ran and the scene.json default clear
# color was replaced), and |r1 - r2| >= 32 (proving update() drives
# rendering). The printout records the exact observed values.
#
# IMPORTANT: each attempt re-opens the file and reads it whole (the
# frame-read.py pattern). A persistent handle with seek + partial reads
# served stale pages of the mmap'd file here, making the oracle read a
# frozen frame while the producer was publishing normally — verified
# 2026-08-19 during M3a debugging; docs/BETA_M3.md risk 5.
scene_oracle() {
    local frame_file="$1"
    local interval_s="$2"
    python3 - "$frame_file" "$interval_s" <<'PY'
import struct
import sys
import time

path, interval = sys.argv[1], float(sys.argv[2])


def read_header(data):
    if len(data) < 64:
        sys.exit("short header")
    if data[0:8] != b"KWEFRM1\0":
        sys.exit("bad magic")
    header = {}
    for name, offset, fmt in (
        ("version", 8, "<I"),
        ("header_bytes", 12, "<I"),
        ("total", 16, "<Q"),
        ("width", 24, "<I"),
        ("height", 28, "<I"),
        ("stride", 32, "<I"),
        ("pixel_format", 36, "<I"),
        ("slots", 40, "<I"),
        ("generation", 48, "<Q"),
        ("active", 56, "<I"),
    ):
        (value,) = struct.unpack_from(fmt, data, offset)
        header[name] = value
    if header["version"] != 1 or header["header_bytes"] != 64:
        sys.exit("bad header version/size")
    if header["pixel_format"] != 1 or header["slots"] != 2:
        sys.exit("bad header format/slots")
    if not (1 <= header["width"] <= 8192 and 1 <= header["height"] <= 8192):
        sys.exit("dimensions outside protocol bounds")
    if header["stride"] != header["width"] * 4:
        sys.exit("bad stride")
    if header["total"] != 64 + 2 * header["stride"] * header["height"]:
        sys.exit("bad total size")
    if header["active"] not in (0, 1):
        sys.exit("bad active slot")
    return header


def snapshot():
    # (r, g, b, a) of the center pixel at a stable even generation.
    for _ in range(64):
        with open(path, "rb") as f:
            data = f.read()
        header = read_header(data)
        if header["generation"] % 2 != 0:
            continue  # publish in progress; retry
        slot = header["active"]
        offset = 64 + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if (
            header2["generation"] != header["generation"]
            or header2["active"] != slot
        ):
            continue  # writer advanced mid-read; retry
        x, y = header["width"] // 2, header["height"] // 2
        i = y * header["stride"] + x * 4
        # BGRA bytes: B, G, R, A in memory order (B8G8R8A8 identity).
        return (pixels[i + 2], pixels[i + 1], pixels[i], pixels[i + 3])
    sys.exit("frame generation never stabilized")


r1, g1, b1, a1 = snapshot()
time.sleep(interval)
r2, g2, b2, a2 = snapshot()
print("oracle center t0 = R %d G %d B %d A %d" % (r1, g1, b1, a1))
print("oracle center t1 = R %d G %d B %d A %d" % (r2, g2, b2, a2))
# Premultiplied alpha: script pins a=1.0, so opaque.
if a1 != 255 or a2 != 255:
    sys.exit("alpha is not opaque")
# init() pinned g=0.3, b=0.4 before update() ever ran: 0.3*255=76.5, 0.4*255=102.
# The scene.json default clear color (0.1, 0.1, 0.1) must NOT be visible.
if abs(g1 - 76) > 3 or abs(b1 - 102) > 3 or abs(g2 - 76) > 3 or abs(b2 - 102) > 3:
    sys.exit("g/b do not match the init()-set clear color (init() did not run)")
# The 2 s sawtooth moves r by >= 0.25 over any 1.5 s window (>= 63 in u8);
# the >= 32 bound is deliberately loose.
if abs(r1 - r2) < 32:
    sys.exit("r barely changed between samples: update() is not driving rendering")
print("ORACLE-OK delta_r=%d (>= 32 required), init-color g/b confirmed" % abs(r1 - r2))
PY
}

# Case 1: runtime-generated fixtures (never committed). scene.json carries
# the script-relative path and a deliberately different default clear color
# than the script applies in init(), so the oracle cannot pass without the
# script engine running.
cat >"$script" <<'JS'
function init() {
  Engine.clearcolor.r = 0.2;
  Engine.clearcolor.g = 0.3;
  Engine.clearcolor.b = 0.4;
  Engine.clearcolor.a = 1.0;
}
var t = 0;
function update(dt) {
  t += dt;
  // Sawtooth: r sweeps 0..1 over 2 s, so two samples ~1.5 s apart always
  // differ in r by >= 0.25 -> >= 63 in u8 (oracle bound: 32).
  Engine.clearcolor.r = (t % 2) / 2;
}
JS
cat >"$scene" <<JSON
{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "resolution": [160, 90], "fps": 30, "script": "script.js"}}
JSON
# Throwing script: every update() past t=0.7 throws; the exception class is
# stable, so the engine's 30 s re-report window bounds the diagnostics while
# the renderer keeps rendering the last state.
cat >"$throw_script" <<'JS'
function init() {
  Engine.clearcolor.r = 0.8;
  Engine.clearcolor.g = 0.2;
  Engine.clearcolor.b = 0.2;
  Engine.clearcolor.a = 1.0;
}
var t = 0;
function update(dt) {
  t += dt;
  if (t > 0.7) {
    throw new Error("boom");
  }
  Engine.clearcolor.r = 0.1;
}
JS
cat >"$throw_scene" <<JSON
{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "resolution": [160, 90], "fps": 30, "script": "throw.js"}}
JSON
# Garbage scene: valid JSON that passes the static scene preflight (json
# extension, regular file) but fails the worker's scene entry parse — the
# "script" field must be a string.
cat >"$garbage_scene" <<'JSON'
{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "script": 42}}
JSON
# Missing script: passes static preflight, the worker rejects the missing
# script file before the canary.
cat >"$missing_scene" <<'JSON'
{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "script": "nonexistent.js"}}
JSON
echo "scene smoke: fixtures generated"

cd "$project_root"
cargo build --workspace >/dev/null
start_daemon
call_daemon health >/dev/null

# Case 2: healthy scene renderer through the daemon, default scene path.
call_daemon renderer.start "$(jq -cn --arg content "$scene" \
    '{wallpaper_id:"scene-live",content_hash:"hash-scene-live",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
live_status="$(wait_phase live)"
[[ "$(jq -r '.result.kind' <<<"$live_status")" == "scene" ]]
[[ "$(jq -r '.result.content_hash' <<<"$live_status")" == "hash-scene-live" ]]
live_pid="$(jq -r '.result.pid' <<<"$live_status")"
sequence_first="$(jq -r '.result.sequence' <<<"$live_status")"
sleep 1
sequence_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$sequence_second" -gt "$sequence_first" ]]
[[ "$(jq -r '.result.failures' <<<"$live_status")" == "0" ]]
frame_file="$(jq -r '.result.frame_file' <<<"$(call_daemon renderer.status)")"
[[ -n "$frame_file" && -f "$frame_file" ]]
echo "scene smoke passed: live start, kind/content, advancing sequence, frame file present"

# Case 3: frame oracle — two center-pixel samples ~1.5 s apart must differ in
# r by >= 32 (update() drives rendering) while g/b match the init()-set
# channels (the scene.json default clear color was replaced). Also validates
# the frame header and the snapshot algorithm end to end.
scene_oracle "$frame_file" 1.5
echo "scene smoke passed: scripted clear color oracle (init() + update() drive rendering)"

# Case 4: a throwing script stays contained. The renderer keeps rendering the
# last state, the sequence keeps advancing, no failure is recorded, and the
# bounded re-report window keeps script_error diagnostics rare in the stderr
# ring (>= 1 seen, at most 2 within the case window: ERROR_REREPORT_WINDOW=30s).
call_daemon renderer.start "$(jq -cn --arg content "$throw_scene" \
    '{wallpaper_id:"scene-throw",content_hash:"hash-scene-throw",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
throw_status="$(wait_phase live)"
throw_pid="$(jq -r '.result.pid' <<<"$throw_status")"
throw_errors_seen=""
for _attempt in {1..300}; do
    tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
    if [[ "$tail" == *"event=renderer.scene.script_error"* ]]; then
        throw_errors_seen="$(grep -c "event=renderer.scene.script_error" <<<"$tail" || true)"
        break
    fi
    sleep 0.05
done
[[ -n "$throw_errors_seen" ]]
[[ "$throw_errors_seen" -ge 1 && "$throw_errors_seen" -le 2 ]]
[[ "$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")" == "live" ]]
throw_first="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
sleep 1
throw_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$throw_second" -gt "$throw_first" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
kill -0 "$throw_pid"
echo "scene smoke passed: throwing script contained, sequence advances, diagnostics bounded"

# Case 5: kill -9 the active worker; the daemon records one failure (visible
# during the restart window) and auto-restarts. The restarted worker's first
# successful canary then promotes it, and a promotion clears the failure
# record (success resets failure history by daemon design), so the live
# status shows failures 0 with a new pid and no quarantine.
kill -9 "$throw_pid"
wait_phase restarting >/dev/null
failed_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.failures' <<<"$failed_status")" == "1" ]]
[[ "$(jq -r '.result.last_failure' <<<"$failed_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$failed_status")" == *"signal_9"* ]]
kill_restart_status="$(wait_phase live)"
[[ "$(jq -r '.result.failures' <<<"$kill_restart_status")" == "0" ]]
[[ "$(jq -r '.result.pid' <<<"$kill_restart_status")" != "null" ]]
[[ "$(jq -r '.result.pid' <<<"$kill_restart_status")" != "$throw_pid" ]]
[[ "$(jq -r '.result.phase' <<<"$kill_restart_status")" == "live" ]]
echo "scene smoke passed: kill -9 recorded once, auto-restarted, not quarantined"

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
refused_status="$(call_daemon renderer.start "$(jq -cn --arg content "$throw_scene" \
    '{wallpaper_id:"scene-throw",content_hash:"hash-scene-throw",width:160,height:90,fps:30,kind:"scene",content:$content}')")"
[[ "$(jq -r '.result.phase' <<<"$refused_status")" == "quarantined" ]]
[[ "$(jq -r '.result.pid' <<<"$refused_status")" == "null" ]]
echo "scene smoke passed: three failures quarantine and refuse the identity"

# Case 7: garbage scene.json (valid JSON, passes the static scene preflight)
# is rejected by the worker's scene entry parse (exit 73) before the canary;
# the active base worker stays live and the failure detail names exit_code_73.
base_params="$(jq -cn --arg content "$scene" \
    '{wallpaper_id:"scene-base",content_hash:"hash-scene-base",width:160,height:90,fps:30,kind:"scene",content:$content}')"
call_daemon renderer.start "$base_params" >/dev/null
base_status="$(wait_phase live)"
base_pid="$(jq -r '.result.pid' <<<"$base_status")"
garbage_params="$(jq -cn --arg content "$garbage_scene" \
    '{wallpaper_id:"scene-garbage",content_hash:"hash-scene-garbage",width:160,height:90,fps:30,kind:"scene",content:$content}')"
call_daemon renderer.start "$garbage_params" >/dev/null
rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$rollback_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$rollback_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$rollback_status")" == *"exit_code_73"* ]]
kill -0 "$base_pid"
echo "scene smoke passed: garbage scene.json -> worker exit 73 -> rolled_back with exit_code_73"

# Case 8: a scene whose script file is missing passes the static preflight
# but the worker rejects the backend (exit 73) before the canary; the active
# base worker stays live and the failure detail names exit_code_73.
missing_params="$(jq -cn --arg content "$missing_scene" \
    '{wallpaper_id:"scene-missing",content_hash:"hash-scene-missing",width:160,height:90,fps:30,kind:"scene",content:$content}')"
call_daemon renderer.start "$missing_params" >/dev/null
missing_rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$missing_rollback_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$missing_rollback_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$missing_rollback_status")" == *"exit_code_73"* ]]
kill -0 "$base_pid"
echo "scene smoke passed: missing script file -> worker exit 73 -> rolled_back with exit_code_73"

# Final stop: the daemon stops the active worker and stays healthy.
call_daemon renderer.stop >/dev/null
stopped_status="$(wait_phase stopped)"
[[ "$(jq -r '.result.pid' <<<"$stopped_status")" == "null" ]]
call_daemon health >/dev/null

# The plasmashell pid guard: a supervised scene renderer must never touch a
# running plasmashell (no KDE integration in M3a). Recorded before the daemon
# started, asserted after it stopped (mirrors scripts/smoke-web.sh).
[[ "$(pgrep -x plasmashell | head -1 || true)" == "$plasma_before" ]]
echo "scene smoke passed: plasmashell pid unchanged across the whole suite"

# Standalone llvmpipe lane: run the worker directly under the software
# rasterizer (VK_ICD_FILENAMES pins the ICD; --device llvmpipe restricts the
# physical-device pick the same way the daemon lane would if asked). This is
# the CI-friendly lane: it needs no discrete GPU. The same scripted-color
# oracle runs against the worker's own frame file, then SIGTERM must exit 0
# with the Stopping state and the renderer.complete diagnostic.
lvp_icd="/usr/share/vulkan/icd.d/lvp_icd.json"
[[ -f "$lvp_icd" ]]
standalone="$smoke_root/standalone.bin"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$standalone" --width 160 --height 90 --fps 30 \
    --content "$scene" --device llvmpipe >"$smoke_root/standalone.log" 2>&1 &
standalone_pid=$!
for _attempt in {1..400}; do
    [[ -f "$standalone" ]] && head -c 8 "$standalone" | grep -q KWEFRM1 && break
    kill -0 "$standalone_pid" 2>/dev/null || {
        echo "standalone renderer exited early" >&2
        sed -n '1,120p' "$smoke_root/standalone.log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$standalone" | grep -q KWEFRM1
grep -q "event=renderer.scene.device name=.*llvmpipe" "$smoke_root/standalone.log"
scene_oracle "$standalone" 1.5
kill -TERM "$standalone_pid"
wait "$standalone_pid"
standalone_exit=$?
[[ "$standalone_exit" == "0" ]]
# The Stopping producer state is written into the frame header before exit
# (producer states: Starting=1, Running=2, Stopping=3, Failed=4).
state_field="$(python3 -c "
import struct
with open('$standalone', 'rb') as f:
    f.seek(60)
    print(struct.unpack('<I', f.read(4))[0])
")"
[[ "$state_field" == "3" ]]
grep -q "event=renderer.complete frames=" "$smoke_root/standalone.log"
grep -q "script_errors=0 soft_timeouts=0 hard_timeouts=0" "$smoke_root/standalone.log"
echo "scene smoke passed: standalone llvmpipe lane (scripted color, SIGTERM exit 0, Stopping state)"

echo "all scene smoke cases passed"
