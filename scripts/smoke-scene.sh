#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Supervised SceneScript smoke suite (BETA_M3a..M3c).
# Mirrors scripts/smoke-video.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. The
# scene.json + script.js fixtures (and the M3c solid-PNG images) are
# generated at runtime and never committed, and the SceneScript engine is
# exercised end-to-end: the frame oracle proves update() drives rendering
# (the clear color is scripted, not the scene.json default), a throwing
# script stays contained, and a final standalone llvmpipe lane runs the
# worker directly under the software rasterizer with --device llvmpipe
# (docs/BETA_M3.md). The M3c cases (a)-(f) exercise the compositor with
# pixel oracles: two-layer composites, the src-over blend math, draw order,
# missing-image skips, the 256-layer cap, and script-driven layer
# transforms via Scene.getLayer.
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
oom_scene="$smoke_root/oom.json"
oom_script="$smoke_root/oom.js"
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
# Real QuickJS heap-cap OOM: init() makes ONE oversized allocation (128 MiB
# vs the 64 MiB cap). The allocation check rejects it immediately — far
# under the 33 ms hard load budget — so the memory-limit exit fires
# deterministically, not the interrupt.
cat >"$oom_script" <<'JS'
function init() {
  var huge = new Uint8Array(128 * 1024 * 1024);
}
JS
cat >"$oom_scene" <<JSON
{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "resolution": [160, 90], "fps": 30, "script": "oom.js"}}
JSON
# M3b: packaged-scene fixtures. The pkg builder mirrors the verified corpus
# layout (PKGV0001, length-prefixed paths, raw payloads — docs/SCENE_FORMAT_V1.md
# "scene.pkg"); corrupt/truncated/traversal/nested variants are derived from
# the good package, exactly like a tampered Workshop download would arrive.
pkg_scene="$smoke_root/scene.pkg"
pkg_corrupt="$smoke_root/corrupt.pkg"
pkg_truncated="$smoke_root/truncated.pkg"
pkg_nested="$smoke_root/nested.pkg"
pkg_traversal="$smoke_root/traversal.pkg"
pkg_oversized="$smoke_root/oversized.pkg"
pkg_oversized_script="$smoke_root/oversized-script.pkg"
# Packaged scene.json uses the corpus serialization: 59 of 60 real wallpapers
# carry clearcolor as a space-separated "r g b" STRING, not the array form the
# file-based fixtures use. Exercising both shapes e2e is an M3b acceptance item.
scene_pkg_json="$smoke_root/scene-string.json"
cat >"$scene_pkg_json" <<JSON
{"general": {"clearcolor": "0.7 0.7 0.7", "resolution": [160, 90], "fps": 30, "script": "script.js"}}
JSON
python3 - "$pkg_scene" "$pkg_corrupt" "$pkg_truncated" "$pkg_nested" "$pkg_traversal" "$pkg_oversized" "$pkg_oversized_script" "$script" "$scene" "$scene_pkg_json" <<'PY'
import struct
import sys

pkg_scene, pkg_corrupt, pkg_truncated = sys.argv[1], sys.argv[2], sys.argv[3]
pkg_nested, pkg_traversal = sys.argv[4], sys.argv[5]
pkg_oversized, pkg_oversized_script = sys.argv[6], sys.argv[7]
script = open(sys.argv[8]).read().encode()
scene_json = open(sys.argv[9]).read().encode()
pkg_scene_json = open(sys.argv[10]).read().encode()


def build_pkg(entries, version="0001"):
    out = bytearray(struct.pack("<I", 8) + b"PKGV" + version.encode())
    out += struct.pack("<I", len(entries))
    offset = 0
    table = bytearray()
    for path, payload in entries:
        table += struct.pack("<I", len(path.encode()))
        table += path.encode()
        table += struct.pack("<I", offset)
        table += struct.pack("<I", len(payload))
        offset += len(payload)
    out += table
    for _, payload in entries:
        out += payload
    return bytes(out)


open(pkg_scene, "wb").write(
    build_pkg([("scene.json", pkg_scene_json), ("script.js", script)])
)
good = build_pkg([("scene.json", pkg_scene_json)])
# Corrupt magic: valid length prefix, wrong magic bytes.
open(pkg_corrupt, "wb").write(good[:4] + b"XXXX0001" + good[12:])
# Truncated table: header + count + a half-written entry.
good = build_pkg([("scene.json", pkg_scene_json), ("script.js", script)])
open(pkg_truncated, "wb").write(good[: 16 + 4 + len("scene.json") + 2])
# Nested archive: a scene.pkg entry and no scene.json.
open(pkg_nested, "wb").write(
    build_pkg([("scene.pkg", build_pkg([("scene.json", pkg_scene_json)]))])
)
# Traversal entry inside an otherwise valid package.
open(pkg_traversal, "wb").write(
    build_pkg([("../evil", b"x"), ("scene.json", pkg_scene_json)])
)
# Cap parity: a scene.json entry over the 16 MiB descriptor cap, and a
# script entry over the 2 MiB cap referenced from a valid descriptor. Both
# must be refused at preflight (invalid_params), never bounced as workers.
open(pkg_oversized, "wb").write(
    build_pkg([("scene.json", b"\x00" * (16 * 1024 * 1024 + 1))])
)
open(pkg_oversized_script, "wb").write(
    build_pkg([("scene.json", pkg_scene_json), ("script.js", b"\x00" * (2 * 1024 * 1024 + 1))])
)
PY
echo "scene smoke: pkg fixtures generated"

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

# Case 3b (M3b): a packaged scene runs end-to-end through the daemon. The
# pkg's scene.json entry is parsed in memory, its script entry is extracted
# into the worker's private 0700 HOME, and the frame oracle proves the
# packaged script drives rendering (same init()/update() semantics as the
# file-based lane). The pkg diagnostic names the archive path taken.
call_daemon renderer.start "$(jq -cn --arg content "$pkg_scene" \
    '{wallpaper_id:"scene-pkg",content_hash:"hash-scene-pkg",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
pkg_status="$(wait_phase live)"
[[ "$(jq -r '.result.kind' <<<"$pkg_status")" == "scene" ]]
pkg_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$pkg_status")"
[[ "$pkg_tail" == *"event=renderer.scene.pkg entries=2 script_entry=true"* ]]
pkg_frame="$(jq -r '.result.frame_file' <<<"$pkg_status")"
[[ -n "$pkg_frame" && -f "$pkg_frame" ]]
scene_oracle "$pkg_frame" 1.5
echo "scene smoke passed: packaged scene.pkg e2e (extracted script entry drives the oracle)"

# Case 3c (M3b, optional): a pkg whose script entry is an LZ4 frame. The
# reader's defensive decompression path, end to end — only when the lz4 CLI
# (which emits frame format) is present. The oracle must pass unchanged:
# the extracted script is byte-identical after decompression.
if command -v lz4 >/dev/null; then
    pkg_lz4="$smoke_root/lz4.pkg"
    python3 - "$pkg_lz4" "$script" "$scene" <<'PY'
import struct
import subprocess
import sys

pkg_lz4, script_path, scene_path = sys.argv[1], sys.argv[2], sys.argv[3]
script = open(script_path).read().encode()
scene_json = open(scene_path).read().encode()
compressed = subprocess.run(
    ["lz4", "-z", "-q", "-c"], input=script, capture_output=True, check=True
).stdout
assert compressed[:4] == b"\x04\x22\x4d\x18", "lz4 CLI must emit an LZ4 frame"
out = bytearray(struct.pack("<I", 8) + b"PKGV0001")
out += struct.pack("<I", 2)
for path, payload in [("scene.json", scene_json), ("script.js", compressed)]:
    out += struct.pack("<I", len(path.encode()))
    out += path.encode()
    out += struct.pack("<I", 0 if path == "scene.json" else len(scene_json))
    out += struct.pack("<I", len(payload))
out += scene_json
out += compressed
open(pkg_lz4, "wb").write(bytes(out))
PY
    call_daemon renderer.start "$(jq -cn --arg content "$pkg_lz4" \
        '{wallpaper_id:"scene-pkg-lz4",content_hash:"hash-scene-pkg-lz4",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
    lz4_status="$(wait_phase live)"
    lz4_frame="$(jq -r '.result.frame_file' <<<"$lz4_status")"
    scene_oracle "$lz4_frame" 1.5
    echo "scene smoke passed: pkg with LZ4-frame script entry decompressed and ran (optional lz4 CLI)"
else
    echo "scene smoke: SKIPPED pkg LZ4 case (lz4 CLI not found)"
fi

# Case 3d (M3b): corrupt, truncated, and traversal packages fail the
# structural preflight before any worker spawns — renderer.start answers
# invalid_params. This closes M1 finding G12, which previously passed any
# .pkg through preflight unvalidated.
for fixture in "$pkg_corrupt" "$pkg_truncated" "$pkg_traversal"; do
    reject="$(call_daemon renderer.start "$(jq -cn --arg content "$fixture" \
        '{wallpaper_id:"scene-badpkg",content_hash:"hash-badpkg",width:160,height:90,fps:30,kind:"scene",content:$content}')" || true)"
    [[ "$(jq -r '.ok' <<<"$reject")" == "false" ]]
    [[ "$(jq -r '.result.error' <<<"$reject")" == "invalid_params" ]]
    [[ "$(jq -r '.result.detail' <<<"$reject")" == *"scene preflight rejected"* ]]
    [[ "$(jq -r '.result.detail' <<<"$reject")" == *"scene package is invalid"* ]]
done
echo "scene smoke passed: corrupt/truncated/traversal pkg -> preflight invalid_params"

# Case 3f (M3b review follow-up): preflight/worker cap parity — an
# oversized scene.json (16 MiB cap) or script (2 MiB cap) entry is caught
# statically at preflight (invalid_params), never bounced as a worker.
for fixture in "$pkg_oversized" "$pkg_oversized_script"; do
    reject="$(call_daemon renderer.start "$(jq -cn --arg content "$fixture" \
        '{wallpaper_id:"scene-bigpkg",content_hash:"hash-bigpkg",width:160,height:90,fps:30,kind:"scene",content:$content}')" || true)"
    [[ "$(jq -r '.ok' <<<"$reject")" == "false" ]]
    [[ "$(jq -r '.result.error' <<<"$reject")" == "invalid_params" ]]
    [[ "$(jq -r '.result.detail' <<<"$reject")" == *"scene preflight rejected"* ]]
    [[ "$(jq -r '.result.detail' <<<"$reject")" == *"byte cap"* ]]
done
echo "scene smoke passed: oversized scene.json/script entries -> preflight invalid_params"

# Case 3e (M3b): a nested scene.pkg passes the structural preflight (it is a
# valid archive) but the worker refuses it before the canary: exit 73,
# rolled back to the base worker, detail names exit_code_73 and the nested
# reason (nested archives are refused, never recursed).
call_daemon renderer.start "$(jq -cn --arg content "$scene" \
    '{wallpaper_id:"scene-base",content_hash:"hash-scene-base",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
base_status="$(wait_phase live)"
base_pid="$(jq -r '.result.pid' <<<"$base_status")"
call_daemon renderer.start "$(jq -cn --arg content "$pkg_nested" \
    '{wallpaper_id:"scene-nested",content_hash:"hash-nested",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
nested_rollback="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$nested_rollback")" == "$base_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$nested_rollback")" == "process_exit" ]]
# The nested worker's own stderr is captured in the failure detail (the
# ring tail belongs to the restarted base worker at this point).
[[ "$(jq -r '.result.last_failure_detail' <<<"$nested_rollback")" == *"exit_code_73"* ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$nested_rollback")" == *"nested scene.pkg inside the package is not supported"* ]]
kill -0 "$base_pid"
echo "scene smoke passed: nested scene.pkg -> worker exit 73 -> rolled_back with exit_code_73"

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

# Case 8b: the REAL 64 MiB QuickJS heap cap — a script that allocates past
# it in init() (one oversized allocation, rejected at the allocation check
# far under the 33 ms hard load budget). The worker exits 71; the daemon
# maps ANY exit 71 to a resource_limit failure (the unconditional mapping,
# not the test-fault path) and rolls back to the base worker.
oom_params="$(jq -cn --arg content "$oom_scene" \
    '{wallpaper_id:"scene-oom",content_hash:"hash-scene-oom",width:160,height:90,fps:30,kind:"scene",content:$content}')"
call_daemon renderer.start "$oom_params" >/dev/null
oom_rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.last_failure' <<<"$oom_rollback_status")" == "resource_limit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$oom_rollback_status")" == "memory_allocation_denied" ]]
[[ "$(jq -r '.result.pid' <<<"$oom_rollback_status")" == "$base_pid" ]]
kill -0 "$base_pid"
echo "scene smoke passed: real QuickJS heap-cap OOM -> exit 71 -> rolled_back with resource_limit"

# ---------------------------------------------------------------------------
# M3c: 2D image layers (BETA_M3c) — cases (a)-(f). The M3c fixtures are
# solid PNGs generated at runtime; the compositor stretches each texture
# over its layer quad. Scene coordinates map to frame pixels as scene
# (0,0) = frame CENTER (the WE origin semantics; the daemon lane renders
# 160x90, so scene (x,y) is frame pixel (80 + x, 45 + y)). A layer's
# `origin` is its CENTER (WE alignment "center", the default), so a layer
# at origin (60,34) with size (40,22) spans scene x in [40,100],
# y in [23,45]. The pixel oracle reads an arbitrary frame pixel through
# the same stable even-generation snapshot as the clear-color oracle.
m3c_red="$smoke_root/m3c-red.png"
m3c_blue="$smoke_root/m3c-blue.png"
m3c_blend="$smoke_root/m3c-blend.png"
m3c_ab_scene="$smoke_root/m3c-ab.json"
m3c_b_scene="$smoke_root/m3c-b.json"
m3c_c_scene="$smoke_root/m3c-c.json"
m3c_d_scene="$smoke_root/m3c-d.json"
m3c_e_scene="$smoke_root/m3c-e.json"
m3c_f_scene="$smoke_root/m3c-f.json"
m3c_f_script="$smoke_root/m3c-f.js"
python3 - "$m3c_red" "$m3c_blue" "$m3c_blend" "$m3c_ab_scene" "$m3c_b_scene" "$m3c_c_scene" "$m3c_d_scene" "$m3c_e_scene" "$m3c_f_scene" "$m3c_f_script" <<'PY'
import json
import struct
import sys
import zlib


def png_solid(r, g, b, a=255):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", 8, 8, 8, 6, 0, 0, 0)  # 8x8, color type 6 (RGBA)
    raw = b"".join(b"\x00" + bytes((r, g, b, a)) * 8 for _ in range(8))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


red, blue, blend = sys.argv[1], sys.argv[2], sys.argv[3]
open(red, "wb").write(png_solid(255, 0, 0))
open(blue, "wb").write(png_solid(0, 0, 255))
open(blend, "wb").write(png_solid(64, 103, 142))


def scene(objects, clear=(0.0, 0.0, 0.0, 1.0), script=None):
    general = {"clearcolor": list(clear), "resolution": [160, 90], "fps": 30}
    if script is not None:
        general["script"] = script
    return {"general": general, "objects": objects}


bg = {"name": "bg", "image": "m3c-red.png", "origin": [0.0, 0.0], "size": [160.0, 90.0], "alpha": 1.0, "visible": True}
mark = {"name": "mark", "image": "m3c-blue.png", "origin": [60.0, 34.0], "size": [40.0, 22.0], "alpha": 1.0, "visible": True}
# (a): bg under mark. (b): a single fullscreen blend texel over a fully
# transparent black clear — dst contributes nothing, so the delivered pixel
# is exactly the layer's src-over at alpha 191/255, premultiplied at
# readback. (c): same geometry as (a) with mark FIRST — the fullscreen bg
# draws last and covers it. (d): a layer whose image is missing. (e): 257
# image layers, over the 256 cap. (f): the script moves the mark (origin
# AND size) via Scene.getLayer in init().
json.dump(scene([bg, mark]), open(sys.argv[4], "w"))
json.dump(
    scene(
        [{"name": "blend", "image": "m3c-blend.png", "origin": [0.0, 0.0], "size": [160.0, 90.0], "alpha": 191.0 / 255.0}],
        clear=(0.0, 0.0, 0.0, 0.0),
    ),
    open(sys.argv[5], "w"),
)
json.dump(scene([mark, bg]), open(sys.argv[6], "w"))
json.dump(
    scene([bg, {"name": "broken", "image": "no-such-file.png", "origin": [0.0, 0.0], "size": [10.0, 10.0]}]),
    open(sys.argv[7], "w"),
)
json.dump(
    scene([{"name": "l%d" % i, "image": "m3c-red.png", "origin": [0.0, 0.0], "size": [10.0, 10.0]} for i in range(257)]),
    open(sys.argv[8], "w"),
)
json.dump(
    scene(
        [bg, {"name": "mark", "image": "m3c-blue.png", "origin": [10.0, 10.0], "size": [60.0, 33.0]}],
        script="m3c-f.js",
    ),
    open(sys.argv[9], "w"),
)
open(sys.argv[10], "w").write(
    "function init() {\n"
    "  var mark = Scene.getLayer(\"mark\");\n"
    "  if (mark === null) throw new Error(\"layer not registered\");\n"
    "  mark.origin.x = 60; mark.origin.y = 34;\n"
    "  mark.size.x = 40; mark.size.y = 22;\n"
    "}\n"
)
PY
echo "scene smoke: M3c fixtures generated"

# Frame pixel oracle for the shared frame file: like scene_oracle, but for
# one arbitrary pixel against an expected BGRA value with a tolerance
# (driver float rounding). Reads whole, stable even generation.
scene_pixel_oracle() {
    local frame_file="$1"
    local x="$2"
    local y="$3"
    local expected="$4"
    local tolerance="$5"
    python3 - "$frame_file" "$x" "$y" "$expected" "$tolerance" <<'PY'
import struct
import sys

path = sys.argv[1]
x, y = int(sys.argv[2]), int(sys.argv[3])
expected = tuple(int(v) for v in sys.argv[4].split(","))
tol = int(sys.argv[5])


def read_header(data):
    if len(data) < 64 or data[0:8] != b"KWEFRM1\0":
        sys.exit("bad header")
    header = {}
    for name, offset, fmt in (
        ("version", 8, "<I"),
        ("header_bytes", 12, "<I"),
        ("total", 16, "<Q"),
        ("width", 24, "<I"),
        ("height", 28, "<I"),
        ("stride", 32, "<I"),
        ("generation", 48, "<Q"),
        ("active", 56, "<I"),
    ):
        (header[name],) = struct.unpack_from(fmt, data, offset)
    return header


def snapshot():
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
        if header2["generation"] != header["generation"] or header2["active"] != slot:
            continue  # writer advanced mid-read; retry
        i = y * header["stride"] + x * 4
        return tuple(pixels[i : i + 4])
    sys.exit("frame generation never stabilized")


got = snapshot()
if max(abs(g - e) for g, e in zip(got, expected)) > tol:
    sys.exit(
        "pixel (%d,%d) = BGRa %s, expected %s (tolerance %d)"
        % (x, y, ",".join(map(str, got)), ",".join(map(str, expected)), tol)
    )
print("ORACLE-OK pixel (%d,%d) = %s" % (x, y, ",".join(map(str, got))))
PY
}

# Case M3c-a: two image layers — a red fullscreen under a blue 40x22 layer
# centered at scene (60,34) (frame (140,79)). Samples: (10,10) -> (90,55)
# red (outside the mark's [40,100]x[23,45] rect); (60,34) and (70,40) blue.
call_daemon renderer.start "$(jq -cn --arg content "$m3c_ab_scene" \
    '{wallpaper_id:"scene-m3c-a",content_hash:"hash-m3c-a",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_a_status="$(wait_phase live)"
m3c_a_frame="$(jq -r '.result.frame_file' <<<"$m3c_a_status")"
scene_pixel_oracle "$m3c_a_frame" 90 55 "0,0,255,255" 1
scene_pixel_oracle "$m3c_a_frame" 140 79 "255,0,0,255" 1
scene_pixel_oracle "$m3c_a_frame" 150 85 "255,0,0,255" 1
echo "scene smoke passed (M3c a): two layers — fullscreen red under blue 40x22 at (60,34)"

# Case M3c-b: the src-over blend oracle. Opaque texel (64,103,142,255) at
# layer alpha 191/255 over a zero clear: the blend scales the straight
# source by the fragment alpha — stored (48,77,106,191) — and the readback
# premultiplies: R=48*191/255=36, G=77*191/255=58, B=106*191/255=79. The
# alpha channel blend factor is ONE (not SRC_ALPHA), so the stored alpha is
# 191, not 143.
call_daemon renderer.start "$(jq -cn --arg content "$m3c_b_scene" \
    '{wallpaper_id:"scene-m3c-b",content_hash:"hash-m3c-b",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_b_status="$(wait_phase live)"
m3c_b_frame="$(jq -r '.result.frame_file' <<<"$m3c_b_status")"
scene_pixel_oracle "$m3c_b_frame" 80 45 "79,58,36,191" 1
echo "scene smoke passed (M3c b): src-over blend — alpha 191/255 over zero clear -> (79,58,36,191)"

# Case M3c-c: draw order — the same two layers with the blue mark FIRST in
# scene.json: the fullscreen red layer draws last, so (60,34) is red now.
call_daemon renderer.start "$(jq -cn --arg content "$m3c_c_scene" \
    '{wallpaper_id:"scene-m3c-c",content_hash:"hash-m3c-c",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_c_status="$(wait_phase live)"
m3c_c_frame="$(jq -r '.result.frame_file' <<<"$m3c_c_status")"
scene_pixel_oracle "$m3c_c_frame" 140 79 "0,0,255,255" 1
echo "scene smoke passed (M3c c): draw order — scene.json order, later layers on top"

# Case M3c-d: a missing image skips its layer, never the scene: the daemon
# stays live, the valid layer renders, and the bounded one-time diagnostic
# names the layer.
call_daemon renderer.start "$(jq -cn --arg content "$m3c_d_scene" \
    '{wallpaper_id:"scene-m3c-d",content_hash:"hash-m3c-d",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_d_status="$(wait_phase live)"
m3c_d_frame="$(jq -r '.result.frame_file' <<<"$m3c_d_status")"
scene_pixel_oracle "$m3c_d_frame" 90 55 "0,0,255,255" 1
m3c_d_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$m3c_d_status")"
[[ "$m3c_d_tail" == *"event=renderer.scene.layer_skip layer=broken"* ]]
echo "scene smoke passed (M3c d): missing image -> layer skipped, scene live, diagnostic bounded"

# Case M3c-e: 257 image layers are over the 256-layer cap — the worker
# rejects the scene before the canary (exit 73), the daemon rolls back to
# the active base worker, and the failure detail names the cap.
m3c_e_params="$(jq -cn --arg content "$m3c_e_scene" \
    '{wallpaper_id:"scene-m3c-e",content_hash:"hash-m3c-e",width:160,height:90,fps:30,kind:"scene",content:$content}')"
call_daemon renderer.start "$m3c_e_params" >/dev/null
m3c_e_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.last_failure' <<<"$m3c_e_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$m3c_e_status")" == *"exit_code_73"* ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$m3c_e_status")" == *"over the 256 layer cap"* ]]
[[ "$(jq -r '.result.pid' <<<"$m3c_e_status")" == "$(jq -r '.result.pid' <<<"$m3c_d_status")" ]]
kill -0 "$(jq -r '.result.pid' <<<"$m3c_e_status")"
echo "scene smoke passed (M3c e): 257 layers -> worker exit 73 -> rolled_back with the layer cap"

# Case M3c-f: the SceneScript layer API — init() moves the blue layer via
# Scene.getLayer("mark"), changing origin AND size: the file places it at
# (10,10) with size 60x33 ([−20,40]x[−6.5,26.5] in scene units); the script
# re-centers it at (60,34) with size 40x22 ([40,100]x[23,45]). Samples:
# (60,34) -> (140,79) blue (the scripted center); (78,40) -> (158,85) blue
# (inside the scripted rect, outside the file rect); (10,10) -> (90,55) red
# (inside the file rect — the script moved the layer away from it).
call_daemon renderer.start "$(jq -cn --arg content "$m3c_f_scene" \
    '{wallpaper_id:"scene-m3c-f",content_hash:"hash-m3c-f",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_f_status="$(wait_phase live)"
m3c_f_frame="$(jq -r '.result.frame_file' <<<"$m3c_f_status")"
scene_pixel_oracle "$m3c_f_frame" 140 79 "255,0,0,255" 1
scene_pixel_oracle "$m3c_f_frame" 158 85 "255,0,0,255" 1
scene_pixel_oracle "$m3c_f_frame" 90 55 "0,0,255,255" 1
echo "scene smoke passed (M3c f): Scene.getLayer proxy — init() moved origin and size"

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
