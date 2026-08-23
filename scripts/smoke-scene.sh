#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Supervised SceneScript smoke suite (BETA_M3a..M3g).
# Mirrors scripts/smoke-video.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. The
# scene.json + script.js fixtures (and the solid-PNG images) are generated
# at runtime and never committed, and the SceneScript engine is exercised
# end-to-end: the frame oracle proves update() drives rendering (the clear
# color is scripted, not the scene.json default), a throwing script stays
# contained, and a final standalone llvmpipe lane runs the worker directly
# under the software rasterizer with --device llvmpipe (docs/BETA_M3.md).
# The M3c cases (a)-(f) exercise the compositor with pixel oracles: two-
# layer composites, the src-over blend math, draw order, missing-image
# skips, the 256-layer cap, and script-driven layer transforms via
# Scene.getLayer. The M3e text lanes are structural (the resolved font is
# machine-dependent). The M3f particle cases (a)-(e) pin the deterministic
# simulation (motion trail, gravity differential, the spawn cap with its
# bounded diagnostic, the instance.count factor from script, and the
# blend-mode differential) through region/gravity/max oracles. The M3g
# video cases (a)-(d) poll a runtime-generated two-color clip for playback
# advance, native-size substitution, the concurrency cap, and an
# unresolved source; they are skipped, never failed, without ffmpeg. The
# standalone llvmpipe lanes repeat the scripted-color oracle AND the M3c
# composite/blend layer oracles AND the M3f particle oracles AND the M3g
# video oracles, so a driver-dependent readback orientation (mirrored
# frames) or a broken quad is caught on the CI-friendly lane.
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
        --renderer-scene-startup-timeout-ms 6000 \
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

media_state() {
    local generation="$1"
    local playback="$2"
    local params
    params="$(jq -cn --argjson generation "$generation" --arg playback "$playback" \
        '{generation:$generation,playback:$playback}')"
    call_daemon media.state "$params" >/dev/null
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
# Pre-existing staleness fixed while running this suite for S1 (unrelated
# to S1 itself): B4 classifies exit_code_73/74 candidate failures as
# FailureKind::Refused ("refused"), not ProcessExit — this and the other
# three exit-73 rollback cases below were still asserting the pre-B4
# value.
[[ "$(jq -r '.result.last_failure' <<<"$nested_rollback")" == "refused" ]]
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
[[ "$(jq -r '.result.last_failure' <<<"$rollback_status")" == "refused" ]]
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
[[ "$(jq -r '.result.last_failure' <<<"$missing_rollback_status")" == "refused" ]]
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

# ---------------------------------------------------------------------------
# M3d fixtures: blend modes + color effects (BETA_M3d). One fullscreen solid
# texel (64,103,142) over an opaque solid clear (102,64,26) — hand-computed
# composites per the researched WE semantics (docs/SCENE_FORMAT_V1.md, M3d
# section; the exact formulas were verified against llvmpipe before writing
# the oracles below):
#   normal   src-over, opaque texel            -> (64,103,142)
#   multiply texel*bg/255                      -> (26,26,14)
#   add      min(255, texel+bg)                -> (166,167,168)
#   screen   255-(255-texel)(255-bg)/255       -> (140,141,154)
#   subtract max(0, bg-texel)                  -> (38,0,0)
# plus an add-mode alpha=128 case over a transparent clear (the readback
# premultiplies: R=(64*128+127)/255=32, G=(103*128+127)/255=52,
# B=(142*128+127)/255=71, A=128), a translucent multiply case (layer alpha
# 0.5 over a 0.5-alpha clear: the mode acts on the color, the ALPHA
# composites src-over — 0.5 + (128/255)*0.5 = 191.5 -> 192, and the
# readback premultiplies: R=(26*192+127)/255=20, G=20, B=(14*192+127)/255=11
# — pins the review-fixed alpha policy), an effects case (brightness 2.0,
# tint (1,0.4,0.5): R=128, G=103*2*0.4=82.4->82, B=142), a colorBlendMode=11
# clamp case (unimplemented -> normal + bounded one-time diagnostic), and a
# script-driven case that switches blendMode at runtime.
m3d_texel="$smoke_root/m3d-texel.png"
m3d_normal_scene="$smoke_root/m3d-normal.json"
m3d_multiply_scene="$smoke_root/m3d-multiply.json"
m3d_add_scene="$smoke_root/m3d-add.json"
m3d_screen_scene="$smoke_root/m3d-screen.json"
m3d_subtract_scene="$smoke_root/m3d-subtract.json"
m3d_add128_scene="$smoke_root/m3d-add128.json"
m3d_multiply128_scene="$smoke_root/m3d-multiply128.json"
m3d_effects_scene="$smoke_root/m3d-effects.json"
m3d_clamp_scene="$smoke_root/m3d-clamp11.json"
m3d_js_scene="$smoke_root/m3d-js.json"
m3d_js_script="$smoke_root/m3d-js.js"
python3 - "$m3d_texel" "$m3d_normal_scene" "$m3d_multiply_scene" "$m3d_add_scene" "$m3d_screen_scene" "$m3d_subtract_scene" "$m3d_add128_scene" "$m3d_multiply128_scene" "$m3d_effects_scene" "$m3d_clamp_scene" "$m3d_js_scene" "$m3d_js_script" <<'PY'
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


open(sys.argv[1], "wb").write(png_solid(64, 103, 142))


def scene(objects, clear=(0.4, 0.25, 0.1, 1.0), script=None):
    general = {"clearcolor": list(clear), "resolution": [160, 90], "fps": 30}
    if script is not None:
        general["script"] = script
    return {"general": general, "objects": objects}


layer = {"name": "layer", "image": "m3d-texel.png", "origin": [0.0, 0.0], "size": [160.0, 90.0], "alpha": 1.0, "visible": True}

# The alpha=255 blend oracles: one scene per mode, opaque clear.
json.dump(scene([layer]), open(sys.argv[2], "w"))
for mode, path in ((1, sys.argv[3]), (6, sys.argv[4]), (7, sys.argv[5]), (9, sys.argv[6])):
    l = dict(layer, colorBlendMode=mode)
    json.dump(scene([l]), open(path, "w"))
# The alpha=128 add case: layer alpha 0.5 over a fully transparent clear —
# the blend stores the straight (64,103,142,128) and the readback
# premultiplies exactly once (the M3c blend oracle pattern).
l = dict(layer, alpha=0.5, colorBlendMode=6)
json.dump(scene([l], clear=(0.0, 0.0, 0.0, 0.0)), open(sys.argv[7], "w"))
# The translucent multiply case: layer alpha 0.5 over a 0.5-alpha clear —
# the alpha policy (the mode acts on the color, the alpha composites
# src-over) pinned byte-exact; see the M3d-10 case below.
l = dict(layer, alpha=0.5, colorBlendMode=1)
json.dump(scene([l], clear=(0.4, 0.25, 0.1, 0.5)), open(sys.argv[8], "w"))
# The effects case: brightness 2.0 with a tint (1, 0.4, 0.5) — applied to
# the sampled texel BEFORE blending, so over the opaque clear the composite
# is the effect-scaled texel (128, 82, 142).
l = dict(layer, brightness=2.0, tint=[1.0, 0.4, 0.5])
json.dump(scene([l]), open(sys.argv[9], "w"))
# The clamp case: colorBlendMode 11 (known-unimplemented, recorded
# undecoded) renders src-over with a bounded one-time diagnostic.
l = dict(layer, colorBlendMode=11)
json.dump(scene([l]), open(sys.argv[10], "w"))
# The script-driven case: update() switches the layer's blendMode from
# add (6) to multiply (1) at t = 3 s; the oracle samples both sides.
json.dump(scene([layer], script="m3d-js.js"), open(sys.argv[11], "w"))
open(sys.argv[12], "w").write(
    "var t = 0;\n"
    "function update(dt) {\n"
    "  t += dt;\n"
    "  var l = Scene.getLayer(\"layer\");\n"
    "  if (l === null) throw new Error(\"layer not registered\");\n"
    "  l.blendMode = t < 3.0 ? 6 : 1;\n"
    "}\n"
)
PY
echo "scene smoke: M3d fixtures generated"

# ---------------------------------------------------------------------------
# M3e fixtures: text layers (BETA_M3e). Every text scene is one text layer
# over a fullscreen opaque blue background (the M3c blue texel), centered
# on the frame (scene (0,0) = frame (80,45)); text layers pin size (1,1)
# and render glyphs at pointsize*4 px per em (default 12pt -> 48 px), so a
# pointsize 10 -> 40 px em, well inside the sampled region below. The
# region oracles are STRUCTURAL (foreground-pixel count, differing-pixel
# count, mean color) — never byte-pins: the exact glyph pixels depend on
# which font the resolver lands on, which varies per machine (the M3e
# acceptance records the actual values observed). Case (a) requests the
# Noto Sans family (the resolver's first fallback candidate); if the
# machine lacks it the resolver falls back to another system font and the
# structural oracle still holds.
m3e_a_scene="$smoke_root/m3e-a.json"
m3e_b_scene="$smoke_root/m3e-b.json"
m3e_b_script="$smoke_root/m3e-b.js"
m3e_c_scene="$smoke_root/m3e-c.json"
m3e_c_script="$smoke_root/m3e-c.js"
m3e_d_scene="$smoke_root/m3e-d.json"
python3 - "$m3e_a_scene" "$m3e_b_scene" "$m3e_b_script" "$m3e_c_scene" "$m3e_c_script" "$m3e_d_scene" <<'PY'
import json
import os
import struct
import sys


def scene(objects, script=None):
    general = {"clearcolor": [0.0, 0.0, 0.0, 1.0], "resolution": [160, 90], "fps": 30}
    if script is not None:
        general["script"] = script
    return {"general": general, "objects": objects}


bg = {"name": "bg", "image": "m3c-blue.png", "origin": [0.0, 0.0], "size": [160.0, 90.0], "alpha": 1.0, "visible": True}


def text_layer(name, string, **extra):
    layer = {
        "name": name,
        "text": string,
        "font": "Noto Sans",
        "pointsize": 10.0,  # 40 px em
        "color": [1.0, 0.0, 0.0, 1.0],
        "origin": [0.0, 0.0],
        "alpha": 1.0,
        "visible": True,
    }
    layer.update(extra)
    return layer


# (a): fixed string + known family, red on blue — the structural region
# oracle (count + mean color, no byte-pins).
json.dump(scene([bg, text_layer("txt", "SMOKE")]), open(sys.argv[1], "w"))
# (b): the script swaps layer.text from 4 wide glyphs to 1 at t=3s — the
# foreground count drops ~4x and the two frames differ.
json.dump(scene([bg, text_layer("txt", "WWWW")], script="m3e-b.js"), open(sys.argv[2], "w"))
open(sys.argv[3], "w").write(
    "var t = 0;\n"
    "function update(dt) {\n"
    "  t += dt;\n"
    "  var l = Scene.getLayer(\"txt\");\n"
    "  if (l === null) throw new Error(\"layer not registered\");\n"
    "  if (t >= 3.0) l.text = \"W\";\n"
    "}\n"
)
# (c): the pointsize clamp — JSON 9999 clamps to 512 px, script writes
# clamp the same way (9999 -> 512, -5 -> 4); the script probes the clamped
# values through console.log (captured in the daemon's stderr tail).
json.dump(scene([bg, text_layer("txt", "CLAMP", pointsize=9999.0)], script="m3e-c.js"), open(sys.argv[4], "w"))
open(sys.argv[5], "w").write(
    "function init() {\n"
    "  var l = Scene.getLayer(\"txt\");\n"
    "  if (l === null) throw new Error(\"layer not registered\");\n"
    "  console.log(\"M3E-POINTSIZE-JSON \" + l.pointsize);\n"
    "  l.pointsize = 9999;\n"
    "  console.log(\"M3E-POINTSIZE-SET \" + l.pointsize);\n"
    "  l.pointsize = -5;\n"
    "  console.log(\"M3E-POINTSIZE-NEG \" + l.pointsize);\n"
    "}\n"
)
# (d): an unknown family — the resolver falls back (the chain, then any
# font) and the layer still renders; the one-time diagnostic names the
# requested family and the resolution. White default color.
json.dump(
    scene([bg, text_layer("txt", "FALLBACK", font="DefinitelyNotAFontFamily_M3E", color=[1.0, 1.0, 1.0, 1.0])]),
    open(sys.argv[6], "w"),
)
PY
echo "scene smoke: M3e fixtures generated"

# ---------------------------------------------------------------------------
# M3f fixtures: particle systems (BETA_M3f). Cases (a)-(f) cover the
# deterministic motion trail, the gravity differential, the spawn cap
# (particles_capped), the instance.count factor from script, the
# blend-mode differential, and the cross-kind draw order (a particle
# system under an image listed after it in the file). Every scene is one
# or two particle systems over a fullscreen clear; scene (0,0) = frame
# (80,45) (the M3c convention); draws interleave in the FILE's object
# order across kinds (merged_draws — case (f) pins it). The particle
# texture is the runtime-generated 8x8 solid PNG (m3f-white.png /
# m3f-gray.png / m3f-red.png) — the same png_solid writer as M3c. All
# cases use opaque textures and system alpha 1, so the readback
# premultiply is the identity (the M3c/M3d convention): Normal (0) =
# src-over = the texture color, Add (6) = min(255, texture + bg) per the
# researched WE semantics.
m3f_white="$smoke_root/m3f-white.png"
m3f_gray="$smoke_root/m3f-gray.png"
m3f_red="$smoke_root/m3f-red.png"
m3f_a_scene="$smoke_root/m3f-a.json"
m3f_b_scene="$smoke_root/m3f-b.json"
m3f_c_scene="$smoke_root/m3f-c.json"
m3f_d_scene="$smoke_root/m3f-d.json"
m3f_d_script="$smoke_root/m3f-d.js"
m3f_e_scene="$smoke_root/m3f-e.json"
m3f_f_scene="$smoke_root/m3f-f.json"
python3 - "$m3f_white" "$m3f_gray" "$m3f_red" "$m3f_a_scene" "$m3f_b_scene" "$m3f_c_scene" "$m3f_d_scene" "$m3f_d_script" "$m3f_e_scene" "$m3f_f_scene" <<'PY'
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


white, gray, red = sys.argv[1], sys.argv[2], sys.argv[3]
open(white, "wb").write(png_solid(255, 255, 255))
open(gray, "wb").write(png_solid(76, 76, 76))
open(red, "wb").write(png_solid(255, 0, 0))


def scene(objects, clear=(0.0, 0.0, 0.0, 1.0), script=None):
    general = {"clearcolor": list(clear), "resolution": [160, 90], "fps": 30}
    if script is not None:
        general["script"] = script
    return {"general": general, "objects": objects}


def system(name, material, origin=(0.0, 0.0), **particle):
    # The shared props (blendMode, visible, alpha, brightness — the
    # M3c/M3d path) live on the OBJECT, not inside the "particle" dict:
    # WE serializes them beside `particle` (corpus: colorBlendMode on
    # image objects), and the parser reads them from there. Only the
    # particle-definition keys stay in the dict.
    object_props = {}
    for key in ("blendMode", "colorBlendMode", "visible"):
        if key in particle:
            object_props[key] = particle.pop(key)
    spec = {
        "spawnRate": 100.0,
        "life": 1.0,
        "speed": 60.0,
        "spread": 6.283185,  # 2pi, clamps to TAU
        "sizeStart": 8.0,
        "sizeEnd": 8.0,
        "colorStart": [1.0, 1.0, 1.0, 1.0],
        "colorEnd": [1.0, 1.0, 1.0, 1.0],
        "alphaStart": 1.0,
        "alphaEnd": 1.0,
        "material": material,
    }
    spec.update(particle)
    obj = {"name": name, "particle": spec, "origin": list(origin)}
    obj.update(object_props)
    return obj


# (a): one deterministic trail — 100/s, life 1 s, speed 60, direction 0,
# spread 0. Steady state: 100 particles, one per px, x in [1,60] scene px
# (frame x 81..140), y exactly 0 (frame y 45); the 8px quads tile the band
# [76,144]x[41,49] with no gaps (a fully covered 69x8 = 552 px rectangle;
# 536 measured — the head and tail quads straddle the band edge).
json.dump(
    scene(
        [
            system(
                "dust",
                "m3f-white.png",
                spawnRate=100.0,
                life=1.0,
                speed=60.0,
                direction=0.0,
                spread=0.0,
            )
        ]
    ),
    open(sys.argv[4], "w"),
)
# (b): gravity differential — blue with gravity [0,80] falls from the
# origin; red without stays at the center. Red drawn LAST so its 8x8
# square at the origin stays visible (fresh blue particles pass through
# it). After 2 s: blue steady-state trail falls y = 40 t^2 (t <= 2 ->
# y up to 160 px, clipped at the frame bottom), mean frame-y well below
# the red 45.
json.dump(
    scene(
        [
            system(
                "blue",
                "m3f-white.png",
                spawnRate=60.0,
                life=2.0,
                speed=0.0,
                spread=0.0,
                gravity=[0.0, 80.0],
                colorStart=[0.0, 0.0, 1.0, 1.0],
                colorEnd=[0.0, 0.0, 1.0, 1.0],
            ),
            system(
                "red",
                "m3f-white.png",
                spawnRate=60.0,
                life=2.0,
                speed=0.0,
                spread=0.0,
                gravity=[0.0, 0.0],
                colorStart=[1.0, 0.0, 0.0, 1.0],
                colorEnd=[1.0, 0.0, 0.0, 1.0],
            ),
        ]
    ),
    open(sys.argv[5], "w"),
)
# (c): the spawn cap — 4096/s would fill 20480 over one life (5 s) but
# maxCount 4096 (the hard cap) clamps the population; excess spawns are
# dropped (never evicting live particles) with the one-time
# particles_capped diagnostic. The drop policy (spawn -> integrate ->
# retain, floored accumulator) suppresses ALL births while the cap is
# full and no particle has died, so the population is ONE sliding cohort
# of 4096 whose age spread stays exactly maxCount/spawnRate = 1 s. The
# exact-step cycle (period = life = 5 s, verified by sim):
#   [0,1]   ramp: a uniform-age disc, radius 0 -> 30 px (white 0 -> ~3.9k)
#   [1,~3]  the cohort is a SOLID annulus [30(t-1), 30t] sweeping outward
#           at 30 px/s: fully in frame near t = 2 (max ~8.7-8.8k white px,
#           the annulus area pi(60^2-30^2) = 8482 + quad overhang);
#           frame-white >= 4k px for ~40% of every cycle
#   [~3.7,5] the annulus leaves the 160x90 frame (inner radius > 92): 0 px
#   [5,6]   the cohort dies at 4096/s while fresh births replace it 1:1:
#           the disc regrows to ~3.9k at t = 6 = t = 1 (mod 5)
# The whole-frame poll converges on the first >= 4k crossing (~1.0 s).
json.dump(
    scene(
        [
            system(
                "dust",
                "m3f-white.png",
                spawnRate=4096.0,
                life=5.0,
                speed=30.0,
                maxCount=4096,  # the WE key is an integer (floats reject)
            )
        ]
    ),
    open(sys.argv[6], "w"),
)
# (d): the instance.count factor from script — two identical systems at
# origins (-50,0) and (50,0) (frame x 30 and 130), spawnRate 100, life
# 0.5, speed 60, spread 2pi, size 4: 50 live particles each -> a ~700 px
# sparse disc (box [0,60]x[15,75]). The script multiplies pb's count by 8
# at t=2 s: 400 particles saturate the same disc (~2800 px), so the white
# count ratio pb/pa climbs past 3.
json.dump(
    scene(
        [
            system(
                "pa",
                "m3f-white.png",
                origin=(-50.0, 0.0),
                spawnRate=100.0,
                life=0.5,
                speed=60.0,
                sizeStart=4.0,
                sizeEnd=4.0,
            ),
            system(
                "pb",
                "m3f-white.png",
                origin=(50.0, 0.0),
                spawnRate=100.0,
                life=0.5,
                speed=60.0,
                sizeStart=4.0,
                sizeEnd=4.0,
            ),
        ],
        script="m3f-d.js",
    ),
    open(sys.argv[7], "w"),
)
open(sys.argv[8], "w").write(
    "var t = 0, logged = false;\n"
    "function update(dt) {\n"
    "  t += dt;\n"
    "  if (t >= 2.0 && !logged) {\n"
    "    var pb = Scene.getParticleSystem(\"pb\");\n"
    "    if (pb === null) throw new Error(\"particle system not registered\");\n"
    "    pb.instance.count = 8;\n"
    "    console.log(\"M3F-COUNT-SET \" + pb.instance.count);\n"
    "    logged = true;\n"
    "  }\n"
    "}\n"
)
# (e): blend-mode differential over an opaque mid-gray clear (30,30,30):
# Normal (0) is src-over — an opaque gray (76,76,76) texture draws 76
# regardless of overlap; Add (6) is min(255, texture + bg) = 106 single,
# 182 double-overlapped, up to 255 — the add disc's max channel value
# clearly exceeds the normal disc's. Origins (-52,0) and (52,0) (frame x
# 28 / 132), discs r=45 (speed 30, life 1.5): add's right edge at frame
# x 73 and normal's left edge at 87 keep 7 px margins from the
# [80,160] box seam, and the boxes' max-R gates (>= 150 / <= 100)
# separate the modes. blendMode is an OBJECT prop (the M3c/M3d shared
# path), hoisted by the system() helper.
json.dump(
    scene(
        [
            system(
                "add",
                "m3f-gray.png",
                origin=(-52.0, 0.0),
                spawnRate=30.0,
                life=1.5,
                speed=30.0,
                blendMode=6.0,
            ),
            system(
                "normal",
                "m3f-gray.png",
                origin=(52.0, 0.0),
                spawnRate=30.0,
                life=1.5,
                speed=30.0,
                blendMode=0.0,
            ),
        ],
        clear=(30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0),
    ),
    open(sys.argv[9], "w"),
)
# (f): the cross-kind draw order — the file's objects array is [particle,
# image]. The particle system (dust, at objects[0]) saturates to a SOLID
# uniform-age disc of radius 30 (4096/s x life 1, cap 4096 — the disc
# phase of the capped steady state: ages uniform [0,1], radii [0,30],
# ~3.9k white px, persistent because births = deaths 1:1 from t = 1 on).
# The opaque 30x30 red image (overlay, at objects[1]) sits at the same
# origin and must draw ON TOP: with merged_draws the frame center reads
# red; the old draws.extend() bug painted every particle draw last and
# the center read white (the smoke regression this case pins).
json.dump(
    scene(
        [
            system(
                "dust",
                "m3f-white.png",
                spawnRate=4096.0,
                life=1.0,
                speed=30.0,
                maxCount=4096,
            ),
            {
                "name": "overlay",
                "image": "m3f-red.png",
                "origin": [0.0, 0.0],
                "size": [30.0, 30.0],
            },
        ]
    ),
    open(sys.argv[10], "w"),
)
PY
echo "scene smoke: M3f fixtures generated"

# M3g fixtures: video layers (BETA_M3g). The corpus carries NO video
# layers at all — not one of the 60 packages has a `video` object key —
# so every M3g fixture is synthetic and generated at runtime, never
# committed (the M3c/M3f convention). The clip is 64x64, 2 s at 30 fps:
# one second of flat #3366CC, then one second of flat #CC6633. Flat
# frames are the deterministic oracle (the smoke-video.sh convention) —
# a flat color survives yuv420p chroma subsampling and the YUV round
# trip without edge error, measured back as (49,100,201) and
# (202,100,49), ~3 off nominal — so the probes run at tolerance 20 while
# the two colors stay 153 apart in both R and B. Cases: (a) playback
# advances (color A then color B on one fullscreen layer), (b) the
# native-size substitution when `size` is absent, (c) the concurrency
# cap diagnostic, (d) an unresolved source skipping only its own layer.
# The whole block is guarded on ffmpeg: without it the M3g cases are
# skipped, never failed.
m3g_ready=0
m3g_clip="$smoke_root/m3g-clip.mp4"
m3g_a_scene="$smoke_root/m3g-a.json"
m3g_b_scene="$smoke_root/m3g-b.json"
m3g_c_scene="$smoke_root/m3g-c.json"
m3g_d_scene="$smoke_root/m3g-d.json"
m3g_pkg="$smoke_root/m3g-video.pkg"
m3g_bad_pkg="$smoke_root/m3g-corrupt-video.pkg"
if command -v ffmpeg >/dev/null; then
    ffmpeg -loglevel error -y \
        -f lavfi -i "color=c=0x3366CC:s=64x64:r=30:d=1" \
        -f lavfi -i "color=c=0xCC6633:s=64x64:r=30:d=1" \
        -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0[v]" -map "[v]" \
        -pix_fmt yuv420p "$m3g_clip"
    python3 - "$m3g_a_scene" "$m3g_b_scene" "$m3g_c_scene" "$m3g_d_scene" "$m3g_pkg" "$m3g_bad_pkg" <<'PY'
import json
import os
import struct
import sys


def scene(objects, clear=(0.0, 0.0, 0.0, 1.0)):
    return {
        "general": {"clearcolor": list(clear), "resolution": [160, 90], "fps": 30},
        "objects": objects,
    }


def video(name, source="m3g-clip.mp4", **props):
    # The `video` key carries the source reference and classifies the
    # object; `loop` and `rate` sit BESIDE it on the object, like the
    # shared props (origin, size, alpha) the M3c/M3d path reads.
    spec = {"name": name, "video": source, "origin": [0.0, 0.0], "loop": True}
    spec.update(props)
    return spec


# (a): one fullscreen looping layer. The declared size stretches the
# 64x64 clip over the whole 160x90 frame, so the center band samples the
# video wherever playback has reached.
json.dump(scene([video("clip", size=[160.0, 90.0])]), open(sys.argv[1], "w"))

# (b): the same layer with `size` ABSENT — open_video_layers substitutes
# the decoder's own 64x64 (the semantics an image layer gets from its
# decoded texture), so the layer covers frame [48,112] x [13,77] and
# everything outside keeps the clear. The clear is opaque green, a color
# the clip never contains, so an "outside" sample is unambiguous.
json.dump(
    scene([video("clip")], clear=(0.0, 1.0, 0.0, 1.0)),
    open(sys.argv[2], "w"),
)

# (c): MAX_VIDEO_LAYERS + 1 = 3 video layers. The parse clears the third
# layer's source and counts one skip rather than rejecting the scene; the
# first two still open, so the frame keeps showing the clip.
json.dump(
    scene([video("clip%d" % index, size=[160.0, 90.0]) for index in range(3)]),
    open(sys.argv[3], "w"),
)

# (d): an unresolved source (no such file under the content root) skips
# ONLY its own layer — video-source-unavailable — while the layer beside
# it still draws. The 30x30 red square is the M3f 8x8 red png generated
# above, scaled by the layer size.
json.dump(
    scene(
        [
            video("broken", source="m3g-missing.mp4", size=[160.0, 90.0]),
            {
                "name": "overlay",
                "image": "m3f-red.png",
                "origin": [0.0, 0.0],
                "size": [30.0, 30.0],
            },
        ]
    ),
    open(sys.argv[4], "w"),
)

def build_pkg(entries, version="0001"):
    out = bytearray(struct.pack("<I", 8) + b"PKGV" + version.encode())
    out += struct.pack("<I", len(entries))
    offset = 0
    table = bytearray()
    for path, payload in entries:
        encoded = path.encode()
        table += struct.pack("<I", len(encoded)) + encoded
        table += struct.pack("<I", offset) + struct.pack("<I", len(payload))
        offset += len(payload)
    out += table
    for _, payload in entries:
        out += payload
    return bytes(out)

pkg_scene = scene([video("pkg-clip", size=[160.0, 90.0])])
pkg_json = json.dumps(pkg_scene, separators=(",", ":")).encode()
clip = open(os.path.join(os.path.dirname(sys.argv[1]), "m3g-clip.mp4"), "rb").read()
open(sys.argv[5], "wb").write(build_pkg([("scene.json", pkg_json), ("m3g-clip.mp4", clip)]))
bad_scene = scene([video("corrupt", source="m3g-corrupt.mp4", size=[160.0, 90.0])])
bad_json = json.dumps(bad_scene, separators=(",", ":")).encode()
open(sys.argv[6], "wb").write(build_pkg([("scene.json", bad_json), ("m3g-corrupt.mp4", b"not a video")]))
PY
    m3g_ready=1
    echo "scene smoke: M3g fixtures generated"
else
    echo "scene smoke: skipping the M3g video cases (ffmpeg not installed)"
fi

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

# A standalone worker can publish the KWEFRM1 canary before its first real
# compositor frame. Poll the pixel oracle for a bounded interval so a valid
# asynchronous startup is not mistaken for a zeroed frame; dump the lane log
# when the expected pixel never arrives.
scene_pixel_wait() {
    local frame_file="$1" x="$2" y="$3" expected="$4" tolerance="$5" log="$6"
    for _attempt in {1..120}; do
        if scene_pixel_oracle "$frame_file" "$x" "$y" "$expected" "$tolerance" >/dev/null 2>&1; then
            scene_pixel_oracle "$frame_file" "$x" "$y" "$expected" "$tolerance"
            return 0
        fi
        sleep 0.05
    done
    echo "standalone pixel oracle timed out: frame=$frame_file pixel=($x,$y) expected=$expected tolerance=$tolerance" >&2
    scene_pixel_oracle "$frame_file" "$x" "$y" "$expected" "$tolerance" || true
    sed -n '1,160p' "$log" >&2
    return 1
}

# wait_first_frame: block until the worker has PUBLISHED a frame, not
# merely created the mapping. The frame file gets its KWEFRM1 magic when
# the mapping is created, which is before font loading and the first
# render, so a lane that starts sampling on the magic alone can read
# generation 0 — an all-zero slot — and fail an oracle that would have
# passed a moment later. (Observed on the llvmpipe M3e lane, where the
# 900+ font scan sits inside that window.) Lanes with their own polling
# oracles do not need this; a one-shot region oracle does.
wait_first_frame() {
    local frame_file="$1" log="$2"
    for _attempt in {1..400}; do
        local generation
        generation="$(python3 -c "
import struct, sys
try:
    with open('$frame_file', 'rb') as f:
        data = f.read(64)
    print(struct.unpack_from('<Q', data, 48)[0] if len(data) >= 64 else 0)
except OSError:
    print(0)
")"
        if (( generation > 0 )); then
            return 0
        fi
        sleep 0.05
    done
    echo "no frame published: $frame_file" >&2
    sed -n '1,160p' "$log" >&2
    return 1
}

# M3e region oracles: structural assertions over a rectangle of the shared
# frame file — never byte-pins, because the exact glyph pixels depend on
# which font the resolver lands on (machine-dependent). Each embeds the
# stable even-generation snapshot of the pixel oracle. Arguments are
# "B,G,R,A" colors in memory order, like scene_pixel_oracle.
#
# scene_region_oracle: counts the region pixels within tol_text of the
# text color ("foreground"), counts the pixels differing from the
# background by more than tol_bg ("differing"), and requires the mean
# color of the differing pixels to sit inside per-channel bounds (glyph
# interiors are the exact text color; antialiased edges lean toward the
# background). Prints the actual counts and mean for the acceptance
# record. Args: frame x0 y0 w h bg text_color tol_bg tol_text min_foreground
# min_differing mean_r_min mean_r_max mean_g_min mean_g_max mean_b_min
# mean_b_max.
scene_region_oracle() {
    local frame_file="$1"
    python3 - "$@" <<'PY'
import struct
import sys

path = sys.argv[1]
x0, y0, w, h = (int(sys.argv[i]) for i in (2, 3, 4, 5))
bg = tuple(int(v) for v in sys.argv[6].split(","))
text_color = tuple(int(v) for v in sys.argv[7].split(","))
tol_bg, tol_text = int(sys.argv[8]), int(sys.argv[9])
min_fg, min_diff = int(sys.argv[10]), int(sys.argv[11])
mean_r_min, mean_r_max = float(sys.argv[12]), float(sys.argv[13])
mean_g_min, mean_g_max = float(sys.argv[14]), float(sys.argv[15])
mean_b_min, mean_b_max = float(sys.argv[16]), float(sys.argv[17])


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
    # The region's BGRA bytes at a stable even generation.
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
        region = bytearray()
        for yy in range(y0, y0 + h):
            i = yy * header["stride"] + x0 * 4
            region += pixels[i : i + w * 4]
        return bytes(region)
    sys.exit("frame generation never stabilized")


region = snapshot()
foreground = 0
differing = 0
mean_r = mean_g = mean_b = 0.0
for p in range(0, len(region), 4):
    b, g, r, a = region[p], region[p + 1], region[p + 2], region[p + 3]
    if max(abs(r - text_color[2]), abs(g - text_color[1]), abs(b - text_color[0])) <= tol_text:
        foreground += 1
    if max(abs(r - bg[2]), abs(g - bg[1]), abs(b - bg[0])) > tol_bg:
        differing += 1
        mean_r += r
        mean_g += g
        mean_b += b
if differing == 0:
    sys.exit("region has no pixels differing from the background")
mean_r /= differing
mean_g /= differing
mean_b /= differing
if foreground < min_fg:
    sys.exit("foreground %d < %d" % (foreground, min_fg))
if differing < min_diff:
    sys.exit("differing %d < %d" % (differing, min_diff))
if not (
    mean_r_min <= mean_r <= mean_r_max
    and mean_g_min <= mean_g <= mean_g_max
    and mean_b_min <= mean_b <= mean_b_max
):
    sys.exit("mean (R %.1f G %.1f B %.1f) outside bounds" % (mean_r, mean_g, mean_b))
print(
    "ORACLE-OK region foreground=%d differing=%d mean=(R %.1f G %.1f B %.1f)"
    % (foreground, differing, mean_r, mean_g, mean_b)
)
PY
}

# scene_region_probe: prints the foreground count only (no assertions) —
# the poll primitive for the runtime-change case. Exits 1 when the frame
# never stabilizes. Args: frame x0 y0 w h text_color tol_text.
scene_region_probe() {
    local frame_file="$1"
    python3 - "$@" <<'PY'
import struct
import sys

path = sys.argv[1]
x0, y0, w, h = (int(sys.argv[i]) for i in (2, 3, 4, 5))
text_color = tuple(int(v) for v in sys.argv[6].split(","))
tol_text = int(sys.argv[7])


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
            continue
        slot = header["active"]
        offset = 64 + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if header2["generation"] != header["generation"] or header2["active"] != slot:
            continue
        region = bytearray()
        for yy in range(y0, y0 + h):
            i = yy * header["stride"] + x0 * 4
            region += pixels[i : i + w * 4]
        return bytes(region)
    sys.exit("frame generation never stabilized")


region = snapshot()
foreground = 0
for p in range(0, len(region), 4):
    b, g, r, a = region[p], region[p + 1], region[p + 2], region[p + 3]
    if max(abs(r - text_color[2]), abs(g - text_color[1]), abs(b - text_color[0])) <= tol_text:
        foreground += 1
print("foreground=%d" % foreground)
PY
}

# scene_region_diff: two stable snapshots `interval` seconds apart; counts
# the region pixels that changed (any channel, tolerance 1 for driver
# float rounding) and both foreground counts. Requires the difference >=
# min_differing and the second foreground <= half the first (the
# runtime text swap shrinks the drawn text ~4x). Prints the actuals.
# Args: frame x0 y0 w h interval min_differing min_foreground_a
# text_color tol_text.
scene_region_diff() {
    local frame_file="$1"
    python3 - "$@" <<'PY'
import struct
import sys
import time

path = sys.argv[1]
x0, y0, w, h = (int(sys.argv[i]) for i in (2, 3, 4, 5))
interval = float(sys.argv[6])
min_diff, min_fg_a = int(sys.argv[7]), int(sys.argv[8])
text_color = tuple(int(v) for v in sys.argv[9].split(","))
tol_text = int(sys.argv[10])


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
            continue
        slot = header["active"]
        offset = 64 + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if header2["generation"] != header["generation"] or header2["active"] != slot:
            continue
        region = bytearray()
        for yy in range(y0, y0 + h):
            i = yy * header["stride"] + x0 * 4
            region += pixels[i : i + w * 4]
        return bytes(region)
    sys.exit("frame generation never stabilized")


def foreground(region):
    count = 0
    for p in range(0, len(region), 4):
        b, g, r, a = region[p], region[p + 1], region[p + 2], region[p + 3]
        if max(abs(r - text_color[2]), abs(g - text_color[1]), abs(b - text_color[0])) <= tol_text:
            count += 1
    return count


a = snapshot()
fg_a = foreground(a)
time.sleep(interval)
b = snapshot()
fg_b = foreground(b)
changed = 0
for p in range(0, len(a), 4):
    if any(abs(a[p + i] - b[p + i]) > 1 for i in range(3)):
        changed += 1
if fg_a < min_fg_a:
    sys.exit("foreground_a %d < %d" % (fg_a, min_fg_a))
if changed < min_diff:
    sys.exit("changed %d < %d" % (changed, min_diff))
if fg_b > fg_a / 2:
    sys.exit("foreground_b %d did not drop below half of foreground_a %d" % (fg_b, fg_a))
print(
    "ORACLE-OK diff changed=%d foreground_a=%d foreground_b=%d"
    % (changed, fg_a, fg_b)
)
PY
}

# M3f oracle: the gravity differential (case b) — counts the pixels within
# tol of each of two colors in the whole frame and requires the mean FRAME
# y of the falling system to sit below the stationary one by at least
# mean_delta px (scene +y = frame +y, the M3c convention). Prints the
# actuals for the acceptance record. Args: frame red_color blue_color tol
# red_min blue_min mean_delta.
scene_gravity_oracle() {
    local frame_file="$1"
    python3 - "$@" <<'PY'
import struct
import sys

path = sys.argv[1]
red_color = tuple(int(v) for v in sys.argv[2].split(","))
blue_color = tuple(int(v) for v in sys.argv[3].split(","))
tol, red_min, blue_min = (int(sys.argv[i]) for i in (4, 5, 6))
mean_delta = float(sys.argv[7])


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
            continue
        slot = header["active"]
        offset = 64 + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if header2["generation"] != header["generation"] or header2["active"] != slot:
            continue
        return bytes(pixels), header
    sys.exit("frame generation never stabilized")


pixels, header = snapshot()
red_count = blue_count = 0
red_y = blue_y = 0.0
for yy in range(header["height"]):
    i = yy * header["stride"]
    for xx in range(header["width"]):
        b, g, r, a = pixels[i : i + 4]
        if max(abs(r - red_color[2]), abs(g - red_color[1]), abs(b - red_color[0])) <= tol:
            red_count += 1
            red_y += yy
        elif max(abs(r - blue_color[2]), abs(g - blue_color[1]), abs(b - blue_color[0])) <= tol:
            blue_count += 1
            blue_y += yy
        i += 4
if red_count < red_min:
    sys.exit("red %d < %d" % (red_count, red_min))
if blue_count < blue_min:
    sys.exit("blue %d < %d" % (blue_count, blue_min))
red_mean = red_y / red_count
blue_mean = blue_y / blue_count
if red_mean - blue_mean < mean_delta:
    sys.exit("gravity gap %.1f px < %s (red_mean_y %.1f, blue_mean_y %.1f)" % (red_mean - blue_mean, mean_delta, red_mean, blue_mean))
print("ORACLE-OK gravity red=%d blue=%d red_mean_y=%.1f blue_mean_y=%.1f" % (red_count, blue_count, red_mean, blue_mean))
PY
}

# M3f oracle: the maximum channel value in a box among pixels differing
# from the background by more than tol — the blend-mode differential pin
# (case e). Gray particle textures make all three channels equal, so the
# R bounds alone carry the assertion. Prints the max for the acceptance
# record. Args: frame x0 y0 w h bg tol min_r max_r.
scene_region_max() {
    local frame_file="$1"
    python3 - "$@" <<'PY'
import struct
import sys

path = sys.argv[1]
x0, y0, w, h = (int(sys.argv[i]) for i in (2, 3, 4, 5))
bg = tuple(int(v) for v in sys.argv[6].split(","))
tol, min_r, max_r = int(sys.argv[7]), int(sys.argv[8]), int(sys.argv[9])


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
            continue
        slot = header["active"]
        offset = 64 + slot * header["stride"] * header["height"]
        pixels = data[offset : offset + header["stride"] * header["height"]]
        with open(path, "rb") as f:
            data2 = f.read()
        header2 = read_header(data2)
        if header2["generation"] != header["generation"] or header2["active"] != slot:
            continue
        region = bytearray()
        for yy in range(y0, y0 + h):
            i = yy * header["stride"] + x0 * 4
            region += pixels[i : i + w * 4]
        return bytes(region)
    sys.exit("frame generation never stabilized")


region = snapshot()
best = 0
for p in range(0, len(region), 4):
    b, g, r, a = region[p], region[p + 1], region[p + 2], region[p + 3]
    if max(abs(r - bg[2]), abs(g - bg[1]), abs(b - bg[0])) > tol:
        best = max(best, r)
if not (min_r <= best <= max_r):
    sys.exit("max R %d outside [%d, %d]" % (best, min_r, max_r))
print("ORACLE-OK max R=%d" % best)
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
# layer alpha 191/255 over a zero clear: the shader outputs straight color
# and the color blend factor is ONE, so the attachment stores the STRAIGHT
# composite (64,103,142,191); the readback premultiplies exactly once:
# R=64*191/255=48, G=103*191/255=77, B=142*191/255=106 — BGRA memory order
# (106,77,48,191). (A color factor of SRC_ALPHA would have stored an
# already-premultiplied composite and the readback would premultiply AGAIN,
# the double-darkened (79,58,36,191) — that was the M3c review finding.)
# The alpha channel blend factor is ONE (not SRC_ALPHA), so the stored
# alpha is 191, not 143.
call_daemon renderer.start "$(jq -cn --arg content "$m3c_b_scene" \
    '{wallpaper_id:"scene-m3c-b",content_hash:"hash-m3c-b",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3c_b_status="$(wait_phase live)"
m3c_b_frame="$(jq -r '.result.frame_file' <<<"$m3c_b_status")"
scene_pixel_oracle "$m3c_b_frame" 80 45 "106,77,48,191" 1
echo "scene smoke passed (M3c b): src-over blend — alpha 191/255 over zero clear -> (106,77,48,191)"

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
[[ "$(jq -r '.result.last_failure' <<<"$m3c_e_status")" == "refused" ]]
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

# ---------------------------------------------------------------------------
# M3d cases: blend modes + color effects (BETA_M3d). One fullscreen solid
# texel (64,103,142) over an opaque solid clear (102,64,26), sampled at the
# frame center (80,45). Expected values are the hand-computed composites per
# the researched WE semantics (docs/SCENE_FORMAT_V1.md, M3d section), in
# BGRA memory order. All cases are alpha=255 except the pinned alpha=128
# add case, so the readback premultiply is the identity; the attachment
# stores the STRAIGHT composite (the M3c convention: a color factor of ONE
# keeps straight color, the readback premultiplies exactly once). Daemon
# lane samples use tolerance 1 for driver float rounding; the standalone
# llvmpipe lanes below repeat every value as an EXACT byte oracle.

# Case M3d-1: Normal blend (colorBlendMode 0, the default) — src-over with
# an opaque texel is the identity over the opaque clear: (142,103,64,255).
call_daemon renderer.start "$(jq -cn --arg content "$m3d_normal_scene" \
    '{wallpaper_id:"scene-m3d-normal",content_hash:"hash-m3d-normal",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_normal_status="$(wait_phase live)"
m3d_normal_frame="$(jq -r '.result.frame_file' <<<"$m3d_normal_status")"
scene_pixel_oracle "$m3d_normal_frame" 80 45 "142,103,64,255" 1
echo "scene smoke passed (M3d 1): normal blend — opaque texel over opaque clear"

# Case M3d-2: Multiply (colorBlendMode 1) — texel*bg/255 per channel:
# B=142*26/255=14, G=103*64/255=26, R=64*102/255=26.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_multiply_scene" \
    '{wallpaper_id:"scene-m3d-multiply",content_hash:"hash-m3d-multiply",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_multiply_status="$(wait_phase live)"
m3d_multiply_frame="$(jq -r '.result.frame_file' <<<"$m3d_multiply_status")"
scene_pixel_oracle "$m3d_multiply_frame" 80 45 "14,26,26,255" 1
echo "scene smoke passed (M3d 2): multiply blend — texel*bg/255 per channel"

# Case M3d-3: Add (colorBlendMode 6) — min(255, texel+bg):
# B=142+26=168, G=103+64=167, R=64+102=166.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_add_scene" \
    '{wallpaper_id:"scene-m3d-add",content_hash:"hash-m3d-add",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_add_status="$(wait_phase live)"
m3d_add_frame="$(jq -r '.result.frame_file' <<<"$m3d_add_status")"
scene_pixel_oracle "$m3d_add_frame" 80 45 "168,167,166,255" 1
echo "scene smoke passed (M3d 3): add blend — min(255, texel+bg)"

# Case M3d-4: Screen (colorBlendMode 7) — 255-(255-texel)(255-bg)/255:
# B=142*(1-26/255)+26=153.5->154, G=103*(1-64/255)+64=141, R=64*(1-102/255)+102=140.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_screen_scene" \
    '{wallpaper_id:"scene-m3d-screen",content_hash:"hash-m3d-screen",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_screen_status="$(wait_phase live)"
m3d_screen_frame="$(jq -r '.result.frame_file' <<<"$m3d_screen_status")"
scene_pixel_oracle "$m3d_screen_frame" 80 45 "154,141,140,255" 1
echo "scene smoke passed (M3d 4): screen blend — 255-(255-texel)(255-bg)/255"

# Case M3d-5: Subtract (colorBlendMode 9) — max(0, bg-texel) (the researched
# WE semantics: REVERSE_SUBTRACT, the background minus the texel):
# B=max(0,26-142)=0, G=max(0,64-103)=0, R=102-64=38.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_subtract_scene" \
    '{wallpaper_id:"scene-m3d-subtract",content_hash:"hash-m3d-subtract",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_subtract_status="$(wait_phase live)"
m3d_subtract_frame="$(jq -r '.result.frame_file' <<<"$m3d_subtract_status")"
scene_pixel_oracle "$m3d_subtract_frame" 80 45 "0,0,38,255" 1
echo "scene smoke passed (M3d 5): subtract blend — max(0, bg-texel)"

# Case M3d-6: color effects — brightness 2.0 with tint (1,0.4,0.5) scale
# the sampled texel BEFORE blending: (64,103,142) -> R=128, G=82.4->82,
# B=142, then normal src-over over the opaque clear is the identity.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_effects_scene" \
    '{wallpaper_id:"scene-m3d-effects",content_hash:"hash-m3d-effects",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_effects_status="$(wait_phase live)"
m3d_effects_frame="$(jq -r '.result.frame_file' <<<"$m3d_effects_status")"
scene_pixel_oracle "$m3d_effects_frame" 80 45 "142,82,128,255" 1
echo "scene smoke passed (M3d 6): brightness 2.0 + tint (1,0.4,0.5) before blending"

# Case M3d-7: add mode at layer alpha 0.5 over a transparent clear — the
# single-premultiplication pin: the attachment stores the STRAIGHT
# composite (64,103,142,128) — alpha 0.5*255=127.5 rounds to 128 — and the
# readback premultiplies exactly once: B=(142*128+127)/255=71,
# G=(103*128+127)/255=52, R=(64*128+127)/255=32.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_add128_scene" \
    '{wallpaper_id:"scene-m3d-add128",content_hash:"hash-m3d-add128",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_add128_status="$(wait_phase live)"
m3d_add128_frame="$(jq -r '.result.frame_file' <<<"$m3d_add128_status")"
scene_pixel_oracle "$m3d_add128_frame" 80 45 "71,52,32,128" 1
echo "scene smoke passed (M3d 7): add at alpha 128 — straight composite premultiplied once"

# Case M3d-10: the translucent multiply alpha policy — layer alpha 0.5 over
# a 0.5-alpha clear. The mode acts on the color (the attachment stores the
# hard multiply B=142*26/255=14, G=103*64/255=26, R=64*102/255=26) while
# the ALPHA channel composites src-over: 0.5 + (128/255)*0.5 = 0.75098 ->
# 191.5 -> 192 (the dst alpha is the quantized 128, which pushes the tie to
# 192). Readback premultiplies exactly once: B=(14*192+127)/255=11,
# G=(26*192+127)/255=20, R=20, A=192. This pins that the layer's own
# opacity survives (the review-fixed (ZERO, ONE) delivered the backdrop's
# (7,13,13,128) instead, discarding the layer's 0.5 entirely).
call_daemon renderer.start "$(jq -cn --arg content "$m3d_multiply128_scene" \
    '{wallpaper_id:"scene-m3d-multiply128",content_hash:"hash-m3d-multiply128",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_multiply128_status="$(wait_phase live)"
m3d_multiply128_frame="$(jq -r '.result.frame_file' <<<"$m3d_multiply128_status")"
scene_pixel_oracle "$m3d_multiply128_frame" 80 45 "11,20,20,192" 1
echo "scene smoke passed (M3d 10): translucent multiply — alpha src-over, layer opacity survives"

# Case M3d-8: colorBlendMode 11 is a recorded-but-undecoded value that no
# fixed-function Vulkan factor can express, so it clamps to normal with a
# bounded one-time diagnostic naming the layer and mode.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_clamp_scene" \
    '{wallpaper_id:"scene-m3d-clamp11",content_hash:"hash-m3d-clamp11",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_clamp_status="$(wait_phase live)"
m3d_clamp_frame="$(jq -r '.result.frame_file' <<<"$m3d_clamp_status")"
scene_pixel_oracle "$m3d_clamp_frame" 80 45 "142,103,64,255" 1
m3d_clamp_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$m3d_clamp_status")"
[[ "$m3d_clamp_tail" == *"event=renderer.scene.blend_mode_clamped layer=layer mode=11"* ]]
echo "scene smoke passed (M3d 8): colorBlendMode 11 -> clamped to normal with the one-time diagnostic"

# Case M3d-9: the JS-driven blend mode — update() writes layer.blendMode:
# add (6) until t=3s, multiply (1) after. Two samples of the same live
# frame file: the first while t<3 (the add composite), the second after the
# script's t crosses 3 (the multiply composite) — two frames, one layer,
# both oracles. The first sample POLLS until the add composite is observed
# (a slow lane could otherwise sample before the first update() applied the
# scripted mode, or even after the t=3s switch); only then does the 3.5s
# wait start, so the second sample always lands past the switch.
call_daemon renderer.start "$(jq -cn --arg content "$m3d_js_scene" \
    '{wallpaper_id:"scene-m3d-js",content_hash:"hash-m3d-js",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3d_js_status="$(wait_phase live)"
m3d_js_frame="$(jq -r '.result.frame_file' <<<"$m3d_js_status")"
m3d_js_add_observed=0
for _attempt in {1..120}; do
    if scene_pixel_oracle "$m3d_js_frame" 80 45 "168,167,166,255" 1 >/dev/null 2>&1; then
        m3d_js_add_observed=1
        break
    fi
    sleep 0.25
done
[[ "$m3d_js_add_observed" == "1" ]]
sleep 3.5
scene_pixel_oracle "$m3d_js_frame" 80 45 "14,26,26,255" 1
echo "scene smoke passed (M3d 9): scripted blendMode switch — add at t<3, multiply at t>3"

# ---------------------------------------------------------------------------
# M3e cases (BETA_M3e): text layers. The daemon lanes cannot receive
# --font-dir (the daemon spawns workers with fixed args), so every M3e lane
# resolves REAL system fonts; on a machine with none the lanes are skipped
# with a message. The worker's own "text_font_none" diagnostic is the
# authoritative fallback signal when a lane still finds nothing usable.
# The resolver scans exactly these paths (skipping missing ones); a missing
# start point makes find exit 1, so `|| true` keeps the probe non-fatal.
m3e_any_font="$(find /usr/share/fonts /usr/local/share/fonts -maxdepth 4 -type f \( -iname '*.ttf' -o -iname '*.otf' -o -iname '*.ttc' \) -print -quit 2>/dev/null || true)"
if [[ -n "$m3e_any_font" ]]; then

# Case M3e-b: the script swaps layer.text from "WWWW" to "W" at t=3s — the
# two frames differ (the region changes over the switch) and the drawn
# glyph area drops ~4x. The first probe POLLS until the 4-glyph text is
# observed (foreground >= 450), then waits 1s so snapshot A is taken well
# before t=3 even on a slow lane; scene_region_diff's 2.5s span crosses
# the switch, so snapshot B shows the single glyph.
call_daemon renderer.start "$(jq -cn --arg content "$m3e_b_scene" \
    '{wallpaper_id:"scene-m3e-b",content_hash:"hash-m3e-b",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3e_b_status="$(wait_phase live)"
m3e_b_frame="$(jq -r '.result.frame_file' <<<"$m3e_b_status")"
m3e_b_c1=0
for _attempt in {1..120}; do
    if m3e_b_probe="$(scene_region_probe "$m3e_b_frame" 30 18 100 54 "0,0,255,255" 30 2>/dev/null)"; then
        m3e_b_fg="${m3e_b_probe#foreground=}"
        if (( m3e_b_fg >= 450 )); then
            m3e_b_c1="$m3e_b_fg"
            break
        fi
    fi
    sleep 0.25
done
[[ "$m3e_b_c1" -ge 450 ]]
sleep 1
scene_region_diff "$m3e_b_frame" 30 18 100 54 2.5 150 450 "0,0,255,255" 30
echo "scene smoke passed (M3e b): scripted text swap — frames differ, foreground $m3e_b_c1 -> ~/4"

# Case M3e-c: the pointsize clamp — scene.json 9999 (-> 512 px), script
# writes 9999 (-> 512) and -5 (-> 4). The script probes the clamped values
# through console.log, captured in the daemon's stderr tail.
call_daemon renderer.start "$(jq -cn --arg content "$m3e_c_scene" \
    '{wallpaper_id:"scene-m3e-c",content_hash:"hash-m3e-c",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3e_c_status="$(wait_phase live)"
m3e_c_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$m3e_c_status")"
[[ "$m3e_c_tail" == *"M3E-POINTSIZE-JSON 512"* ]]
[[ "$m3e_c_tail" == *"M3E-POINTSIZE-SET 512"* ]]
[[ "$m3e_c_tail" == *"M3E-POINTSIZE-NEG 4"* ]]
echo "scene smoke passed (M3e c): pointsize clamped — JSON 9999 -> 512, script 9999 -> 512, -5 -> 4"

# Case M3e-d: an unknown font family — the resolver falls back (the
# documented chain, then any font) and the layer still renders; the
# one-time diagnostic names the requested family and the resolution.
call_daemon renderer.start "$(jq -cn --arg content "$m3e_d_scene" \
    '{wallpaper_id:"scene-m3e-d",content_hash:"hash-m3e-d",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3e_d_status="$(wait_phase live)"
m3e_d_frame="$(jq -r '.result.frame_file' <<<"$m3e_d_status")"
m3e_d_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$m3e_d_status")"
if [[ "$m3e_d_tail" == *"event=renderer.scene.text_font_none"* ]]; then
    echo "scene smoke SKIP (M3e d): no usable system fonts for the fallback — text lane needs real fonts"
else
    [[ "$m3e_d_tail" == *"event=renderer.scene.text_font_fallback layer=txt requested=DefinitelyNotAFontFamily_M3E"* ]]
    scene_region_oracle "$m3e_d_frame" 30 18 100 54 "255,0,0,255" "255,255,255,255" 20 30 300 400 180 255 180 255 180 255
    echo "scene smoke passed (M3e d): unknown family -> fallback font, layer renders, diagnostic names the request"
fi

else
    echo "scene smoke SKIP (M3e b/c/d): no system fonts under /usr/share/fonts — text lanes need real fonts"
fi

# ---------------------------------------------------------------------------
# M3f cases (BETA_M3f): particle systems. Same lane conventions as M3c/M3d:
# daemon lanes sample the live shared frame file (tolerance 1); the
# standalone llvmpipe lanes below repeat the oracles against the worker's
# own frame file. Scene (0,0) = frame (80,45), particles draw after the
# clear in object order; opaque textures and system alpha 1 keep the
# readback premultiply the identity, so Normal = the texture color and
# Add = min(255, texture + bg).

# Case M3f-a: the deterministic trail — 100 particles, one per scene px,
# x in [81,140] frame, y exactly 45: the 8px quads tile the band
# [76,144]x[41,49] with no gaps (536 white px measured — the head and
# tail quads straddle the band edge). The region
# [70,35,80,20] must hold >= 450 full-white pixels with a pure-white
# mean. Polls until the trail fills (life 1 s steady state), then asserts.
call_daemon renderer.start "$(jq -cn --arg content "$m3f_a_scene" \
    '{wallpaper_id:"scene-m3f-a",content_hash:"hash-m3f-a",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_a_status="$(wait_phase live)"
m3f_a_frame="$(jq -r '.result.frame_file' <<<"$m3f_a_status")"
m3f_a_fg=0
for _attempt in {1..120}; do
    if m3f_a_probe="$(scene_region_probe "$m3f_a_frame" 70 35 80 20 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_a_fg="${m3f_a_probe#foreground=}"
        (( m3f_a_fg >= 450 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_a_fg" -ge 450 ]]
scene_region_oracle "$m3f_a_frame" 70 35 80 20 "0,0,0,255" "255,255,255,255" 20 30 450 450 250 255 250 255 250 255
echo "scene smoke passed (M3f a): deterministic trail — $m3f_a_fg white px in the 69x8 band at frame y 45"

# Case M3f-b: the gravity differential — the stationary red square (no
# gravity) stays at frame y 45; the blue system (gravity [0,80]) falls
# y = 40 t^2, so its on-screen mean frame-y sits ~15 px lower. Polls for
# the blue column below the origin (>= 100 px), then the mean-gap oracle
# (red_mean_y > blue_mean_y + 3, both counts bounded below). Colors are
# in the suite's memory (B,G,R,A) order like every pixel oracle: visual
# blue is "255,0,0,255" (B=255), visual red is "0,0,255,255" (R=255).
call_daemon renderer.start "$(jq -cn --arg content "$m3f_b_scene" \
    '{wallpaper_id:"scene-m3f-b",content_hash:"hash-m3f-b",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_b_status="$(wait_phase live)"
m3f_b_frame="$(jq -r '.result.frame_file' <<<"$m3f_b_status")"
m3f_b_blue=0
for _attempt in {1..120}; do
    if m3f_b_probe="$(scene_region_probe "$m3f_b_frame" 60 50 40 40 "255,0,0,255" 30 2>/dev/null)"; then
        m3f_b_blue="${m3f_b_probe#foreground=}"
        (( m3f_b_blue >= 100 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_b_blue" -ge 100 ]]
scene_gravity_oracle "$m3f_b_frame" "255,0,0,255" "0,0,255,255" 30 25 40 3
echo "scene smoke passed (M3f b): gravity differential — blue fell, mean frame-y below the stationary red by > 3 px"

# Case M3f-c: the spawn cap — spawnRate 4096/s would fill 20480 over one
# life (5 s) but maxCount 4096 (the hard cap) caps the population; excess
# spawns are dropped (live particles are never evicted) and the bounded
# one-time diagnostic fires at the first drop (~1 s sim). The drop policy
# (spawn -> integrate -> retain, floored accumulator) suppresses ALL
# births while the cap is full and nothing has died, so the population is
# ONE sliding cohort of 4096 with age spread maxCount/spawnRate = 1 s.
# Exact-step sim (period = life = 5 s): a uniform-age disc phase (radius
# 0->30, white 0->~3.9k), then the cohort as a SOLID annulus
# [30(t-1), 30t] sweeping outward at 30 px/s — max ~8.7-8.8k white px at
# t~2 (annulus area pi(60^2-30^2) = 8482 + quad overhang), frame-white
# >= 4k for ~40% of every cycle — then the annulus leaves the frame
# (~3.7 s, 0 px), and the cohort dies 1:1 into fresh births ([5,6],
# regrowing the disc). The poll converges on the first >= 4k crossing
# (~1.0 s: 4637 measured — a ramp value, not the cycle max).
call_daemon renderer.start "$(jq -cn --arg content "$m3f_c_scene" \
    '{wallpaper_id:"scene-m3f-c",content_hash:"hash-m3f-c",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_c_status="$(wait_phase live)"
m3f_c_frame="$(jq -r '.result.frame_file' <<<"$m3f_c_status")"
m3f_c_fg=0
for _attempt in {1..120}; do
    if m3f_c_probe="$(scene_region_probe "$m3f_c_frame" 0 0 160 90 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_c_fg="${m3f_c_probe#foreground=}"
        (( m3f_c_fg >= 4000 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_c_fg" -ge 4000 ]]
# The bounded one-time diagnostic fires at the first dropped spawn (step
# 60, ~1.0 s sim — well before the whole-frame poll above finishes), so
# it is re-queried after the poll, not off the first live status.
m3f_c_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3f_c_tail" == *"event=renderer.scene.particles_capped system=dust"* ]]
echo "scene smoke passed (M3f c): spawn cap — 4096 of 4096/s live, particles_capped diagnostic, $m3f_c_fg px in the annulus"

# Case M3f-d: the instance.count factor from script — pb.instance.count =
# 8 at t=2 s multiplies pb's spawn rate: its disc saturates (~2800 px)
# while pa's stays sparse (~700 px). The poll first requires pa's sparse
# disc (>= 300 white), then the pb/pa white ratio past 3 (the switch lands
# at t=2; a slow lane just waits longer). The script logs the clamped
# factor through console.log — re-queried via renderer.status so the
# t=2 log line is in the tail.
call_daemon renderer.start "$(jq -cn --arg content "$m3f_d_scene" \
    '{wallpaper_id:"scene-m3f-d",content_hash:"hash-m3f-d",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_d_status="$(wait_phase live)"
m3f_d_frame="$(jq -r '.result.frame_file' <<<"$m3f_d_status")"
m3f_d_ratio=0
for _attempt in {1..120}; do
    if m3f_d_pa="$(scene_region_probe "$m3f_d_frame" 0 15 60 60 "255,255,255,255" 30 2>/dev/null)" \
        && m3f_d_pb="$(scene_region_probe "$m3f_d_frame" 100 15 60 60 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_d_ca="${m3f_d_pa#foreground=}"
        m3f_d_cb="${m3f_d_pb#foreground=}"
        if (( m3f_d_ca >= 300 && m3f_d_cb > 3 * m3f_d_ca )); then
            m3f_d_ratio=$(( m3f_d_cb * 10 / m3f_d_ca ))
            break
        fi
    fi
    sleep 0.25
done
[[ "$m3f_d_ratio" -ge 30 ]] || {
    echo "M3f-d failure: ratio x10=$m3f_d_ratio (pa=$m3f_d_ca pb=$m3f_d_cb) frame=$m3f_d_frame" >&2
    call_daemon renderer.status | jq -r '.result | "phase=\(.phase) frame_file=\(.frame_file) last_failure_detail=\(.last_failure_detail)", (.stderr_tail | join("\n"))' >&2
    exit 1
}
m3f_d_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3f_d_tail" == *"M3F-COUNT-SET 8"* ]] || {
    echo "M3f-d failure: M3F-COUNT-SET 8 not in the stderr tail; tail was:" >&2
    printf '%s\n' "$m3f_d_tail" >&2
    exit 1
}
echo "scene smoke passed (M3f d): instance.count from script — pb/pa white ratio $(( m3f_d_ratio / 10 )).$(( m3f_d_ratio % 10 )) (> 3 after count=8)"

# Case M3f-e: the blend-mode differential over an opaque mid-gray clear
# (30,30,30) — the gray (76,76,76) texture: Normal (0) is src-over, an
# opaque texture draws 76 regardless of overlap; Add (6) is
# min(255, texture + bg): 106 single, 182 double-overlapped, up to 255.
# The add disc's max R sits well above the normal disc's; each box
# [0,80]x[15,75] / [80,80]x[15,75] holds one disc (r=45 at frame x 28/132,
# 7 px margins from the seam).
call_daemon renderer.start "$(jq -cn --arg content "$m3f_e_scene" \
    '{wallpaper_id:"scene-m3f-e",content_hash:"hash-m3f-e",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_e_status="$(wait_phase live)"
m3f_e_frame="$(jq -r '.result.frame_file' <<<"$m3f_e_status")"
m3f_e_add_observed=0
for _attempt in {1..120}; do
    if scene_region_max "$m3f_e_frame" 0 15 80 60 "30,30,30,255" 20 150 255 >/dev/null 2>&1; then
        m3f_e_add_observed=1
        break
    fi
    sleep 0.25
done
[[ "$m3f_e_add_observed" == "1" ]]
scene_region_max "$m3f_e_frame" 0 15 80 60 "30,30,30,255" 20 150 255
scene_region_max "$m3f_e_frame" 80 15 80 60 "30,30,30,255" 20 0 100
echo "scene smoke passed (M3f e): blend differential — add disc max R >= 150, normal disc max R <= 100"

# Case M3f-f: the draw order across kinds — the file's objects array is
# [particle, image]; with the merged painter order (merged_draws) the
# opaque 30x30 red image (objects[1]) draws ON TOP of the solid white
# particle disc (objects[0], a capped 4096/s x life 1 steady disc of
# radius 30 — the center pixel is always particle-covered). The old
# draws.extend() bug painted every particle draw LAST: the frame center
# read white instead of red. The poll waits for red at the center, then
# checks a disc-only pixel left of the image is still white (the
# particles really are under the image, not missing).
call_daemon renderer.start "$(jq -cn --arg content "$m3f_f_scene" \
    '{wallpaper_id:"scene-m3f-f",content_hash:"hash-m3f-f",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3f_f_status="$(wait_phase live)"
m3f_f_frame="$(jq -r '.result.frame_file' <<<"$m3f_f_status")"
m3f_f_red_seen=0
for _attempt in {1..120}; do
    if m3f_f_probe="$(scene_region_probe "$m3f_f_frame" 80 45 1 1 "0,0,255,255" 30 2>/dev/null)"; then
        m3f_f_fg="${m3f_f_probe#foreground=}"
        (( m3f_f_fg >= 1 )) && { m3f_f_red_seen=1; break; }
    fi
    sleep 0.25
done
[[ "$m3f_f_red_seen" == "1" ]] || {
    echo "M3f-f failure: frame center never read red (particles drawing over the image?) frame=$m3f_f_frame" >&2
    call_daemon renderer.status | jq -r '.result | "phase=\(.phase) frame_file=\(.frame_file) last_failure_detail=\(.last_failure_detail)", (.stderr_tail | join("\n"))' >&2
    exit 1
}
m3f_f_white_seen=0
for _attempt in {1..120}; do
    if m3f_f_white="$(scene_region_probe "$m3f_f_frame" 50 45 1 1 "255,255,255,255" 30 2>/dev/null)"; then
        (( ${m3f_f_white#foreground=} >= 1 )) && { m3f_f_white_seen=1; break; }
    fi
    sleep 0.25
done
[[ "$m3f_f_white_seen" == "1" ]] || {
    echo "M3f-f failure: disc probe (50,45) never read white (particle disc missing under the image?) probe=$m3f_f_white frame=$m3f_f_frame" >&2
    call_daemon renderer.status | jq -r '.result | "phase=\(.phase) frame_file=\(.frame_file) last_failure_detail=\(.last_failure_detail)", (.stderr_tail | join("\n"))' >&2
    exit 1
}
echo "scene smoke passed (M3f f): draw order — particle system under the image listed after it (center red, disc white at x 50)"

# M3g poll primitive: wait for at least `want` pixels in the box to match
# the color, printing the last count seen (so a failure reports how close
# it got). Args: frame x0 y0 w h "B,G,R,A" tol want attempts.
m3g_wait_color() {
    local frame="$1" x0="$2" y0="$3" w="$4" h="$5" color="$6" tol="$7" want="$8" attempts="$9"
    local probe count=0
    for _attempt in $(seq 1 "$attempts"); do
        if probe="$(scene_region_probe "$frame" "$x0" "$y0" "$w" "$h" "$color" "$tol" 2>/dev/null)"; then
            count="${probe#foreground=}"
            if (( count >= want )); then
                echo "$count"
                return 0
            fi
        fi
        sleep 0.25
    done
    echo "$count"
    return 1
}

if [[ "$m3g_ready" == "1" ]]; then

# M3g cases (BETA_M3g): video layers decoded by libmpv and uploaded into
# the same layer texture every frame. Same lane conventions as M3f: the
# daemon lanes sample the live shared frame file, the standalone llvmpipe
# lanes below repeat the oracles against the worker's own frame file.
# Every assertion is a POLL, not a single sample — a video layer's frame
# is whatever playback has reached, so the oracle waits for a state to
# appear rather than pinning one instant.

# Case M3g-a: playback advances. One fullscreen looping layer over the
# 2 s clip: the 8x8 center box must read flat #3366CC (BGRA 204,102,51)
# at some poll and flat #CC6633 (BGRA 51,102,204) at a later one. A
# decoder that opened but never advanced — the failure this case exists
# for, since a static first frame still renders and still logs
# video_open — passes the first probe and hangs on the second. 200
# attempts x 0.25 s = 50 s, 25 clip loops.
call_daemon renderer.start "$(jq -cn --arg content "$m3g_a_scene" \
    '{wallpaper_id:"scene-m3g-a",content_hash:"hash-m3g-a",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_a_status="$(wait_phase live)"
m3g_a_frame="$(jq -r '.result.frame_file' <<<"$m3g_a_status")"
m3g_a_generation="$(jq -r '.result.display_generation' <<<"$m3g_a_status")"
m3g_a_first="$(m3g_wait_color "$m3g_a_frame" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-a failure: the first clip color never reached the frame center (best=$m3g_a_first of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_a_second="$(m3g_wait_color "$m3g_a_frame" 76 41 8 8 "51,102,204,255" 20 60 200)" || {
    echo "M3g-a failure: the second clip color never arrived — playback is not advancing (best=$m3g_a_second of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_a_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_a_tail" == *"event=renderer.scene.video_open layer=clip size=64x64"* ]]
echo "scene smoke passed (M3g a): playback advances — both clip colors reached the frame center ($m3g_a_first then $m3g_a_second of 64 px)"

# Media transport is latest-wins and ack-only at the protocol boundary. The
# scene worker fans the command out to every open decoder; while paused or
# stopped it must keep publishing the last frame so the supervisor sees a
# live worker. The input ack sequence proves the state reached this worker,
# and the sequence comparisons prove keepalive continued.
media_state "$m3g_a_generation" paused
sleep 0.5
m3g_paused_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.input_ack_sequence' <<<"$m3g_paused_status")" != "0" ]]
m3g_paused_first="$(jq -r '.result.sequence' <<<"$m3g_paused_status")"
sleep 0.75
m3g_paused_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_paused_second" -gt "$m3g_paused_first" ]]
media_state "$m3g_a_generation" playing
media_state "$m3g_a_generation" stopped
sleep 0.5
m3g_stopped_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.phase' <<<"$m3g_stopped_status")" == "live" ]]
m3g_stopped_first="$(jq -r '.result.sequence' <<<"$m3g_stopped_status")"
sleep 0.75
m3g_stopped_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_stopped_second" -gt "$m3g_stopped_first" ]]
echo "scene smoke passed (M3g media): paused/playing/stopped acked and keepalive advanced"

# Case M3g-b: the native-size substitution. The scene declares no `size`,
# so open_video_layers fills it from the decoder (64x64) before the
# script engine is built: the layer covers frame [48,112] x [13,77]. The
# center reads the clip; the box at x 120 — outside the layer, inside a
# fullscreen stretch — must stay the opaque green clear. A substitution
# that fell back to the frame size, or to [0,0] and a degenerate quad,
# fails one of the two.
call_daemon renderer.start "$(jq -cn --arg content "$m3g_b_scene" \
    '{wallpaper_id:"scene-m3g-b",content_hash:"hash-m3g-b",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_b_status="$(wait_phase live)"
m3g_b_frame="$(jq -r '.result.frame_file' <<<"$m3g_b_status")"
m3g_b_center="$(m3g_wait_color "$m3g_b_frame" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-b failure: the clip never reached the frame center (best=$m3g_b_center of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_b_outside="$(scene_region_probe "$m3g_b_frame" 120 41 8 8 "0,255,0,255" 20)"
[[ "${m3g_b_outside#foreground=}" == "64" ]]
m3g_b_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_b_tail" == *"event=renderer.scene.video_open layer=clip size=64x64"* ]]
echo "scene smoke passed (M3g b): native-size substitution — clip at the center, clear still green at x 120"

# Case M3g-c: the concurrency cap. Three video layers, cap 2: the parse
# clears the third source and counts one skip; the scene still loads and
# the two opened layers still draw. The diagnostic is emitted once at
# load, so it is already in the tail by the time the frame is live.
call_daemon renderer.start "$(jq -cn --arg content "$m3g_c_scene" \
    '{wallpaper_id:"scene-m3g-c",content_hash:"hash-m3g-c",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_c_status="$(wait_phase live)"
m3g_c_frame="$(jq -r '.result.frame_file' <<<"$m3g_c_status")"
m3g_c_center="$(m3g_wait_color "$m3g_c_frame" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-c failure: the capped scene never drew the clip (best=$m3g_c_center of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_c_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_c_tail" == *"event=renderer.scene.video_layer_skip count=1 (cap is 2)"* ]]
# The parse clears the third source, so the loader diagnoses that layer by
# name too: the cap is visible per layer, not just as a count.
[[ "$m3g_c_tail" == *"layer_skip layer=clip2 detail=video-source-unavailable"* ]]
echo "scene smoke passed (M3g c): concurrency cap — 3 video layers, 1 skipped, the scene still renders"

# Case M3g-d: an unresolved source degrades ONE layer. The video layer is
# listed first and its source does not exist; the 30x30 red square beside
# it must still draw at the center, and the skip must name the layer and
# carry the RESOLVER's own detail (the video-source-unavailable slug is
# the cleared-source case, which (c) covers).
call_daemon renderer.start "$(jq -cn --arg content "$m3g_d_scene" \
    '{wallpaper_id:"scene-m3g-d",content_hash:"hash-m3g-d",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_d_status="$(wait_phase live)"
m3g_d_frame="$(jq -r '.result.frame_file' <<<"$m3g_d_status")"
m3g_d_red="$(m3g_wait_color "$m3g_d_frame" 76 41 8 8 "0,0,255,255" 20 60 120)" || {
    echo "M3g-d failure: the healthy image layer never drew beside the broken video (best=$m3g_d_red of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_d_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$m3g_d_tail" == *'layer_skip layer=broken detail=video "m3g-missing.mp4" is missing or unreadable'* ]]
echo "scene smoke passed (M3g d): unresolved source — only the video layer skipped, the image still drew"

# Case M3g-e: a runtime-generated package embeds the same synthetic clip.
# The package lane must extract it into the worker HOME before libmpv opens
# it, then remove that private directory after teardown.
call_daemon renderer.start "$(jq -cn --arg content "$m3g_pkg" \
    '{wallpaper_id:"scene-m3g-pkg",content_hash:"hash-m3g-pkg",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_pkg_status="$(wait_phase live)"
m3g_pkg_frame="$(jq -r '.result.frame_file' <<<"$m3g_pkg_status")"
m3g_pkg_center="$(m3g_wait_color "$m3g_pkg_frame" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-e failure: packaged clip never reached the frame center (best=$m3g_pkg_center of 64)" >&2
    jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)" >&2
    exit 1
}
m3g_pkg_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
if [[ "$m3g_pkg_tail" != *"event=renderer.scene.pkg entries=2 script_entry=false"* || \
      "$m3g_pkg_tail" != *"event=renderer.scene.video_open layer=pkg-clip size=64x64"* ]]; then
    echo "M3g-e failure: package diagnostics missing" >&2
    printf '%s\n' "$m3g_pkg_tail" >&2
    exit 1
fi
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
if find "$runtime_dir" -type d -name 'kwe-scene-video-*' -print -quit | grep -q .; then
    echo "M3g-e failure: extracted package video directory survived worker teardown" >&2
    find "$runtime_dir" -type d -name 'kwe-scene-video-*' -print >&2
    exit 1
fi
echo "scene smoke passed (M3g e): package video decoded, worker-owned extraction cleaned up"

# Case M3g-f: a corrupt package video is a bad layer, not a scene/process
# failure. The valid package structure reaches live, libmpv rejects only the
# layer, and the daemon remains able to stop it cleanly.
call_daemon renderer.start "$(jq -cn --arg content "$m3g_bad_pkg" \
    '{wallpaper_id:"scene-m3g-badpkg",content_hash:"hash-m3g-badpkg",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
m3g_bad_status="$(wait_phase live)"
m3g_bad_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$m3g_bad_status")"
if [[ "$m3g_bad_tail" != *"event=renderer.scene.pkg entries=2 script_entry=false"* || \
      "$m3g_bad_tail" != *"event=renderer.scene.layer_skip layer=corrupt detail=video-open-failed"* ]]; then
    echo "M3g-f failure: corrupt-package diagnostics missing" >&2
    printf '%s\n' "$m3g_bad_tail" >&2
    exit 1
fi
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
echo "scene smoke passed (M3g f): corrupt package video degraded one layer and stayed live"

fi

# ---------------------------------------------------------------------------
# B2 cases: a scene that draws nothing must never be applied as a healthy
# wallpaper (docs/bugs/SCENE_APPLY_BLANK_CLEAR_COLOR.md). The gate is the
# shared classification, applied twice: statically at preflight here
# (invalid_params, no worker spawns), and inside the worker itself for a
# scene that never went through preflight (the standalone B2-d lane below).
# One drawable object is enough to apply — degraded is not blank.
b2_models_scene="$smoke_root/b2-models.json"
b2_partial_scene="$smoke_root/b2-partial.json"
python3 - "$b2_models_scene" "$b2_partial_scene" <<'B2PY'
import json
import sys


def scene(objects):
    return {
        "general": {"clearcolor": [0.7, 0.7, 0.7, 1.0], "resolution": [160, 90], "fps": 30},
        "objects": objects,
    }


# (a) every object is a feature this build cannot render: two model layers
# (scene3d, M3h) and a particle system whose definition is an external file
# — the exact shape of the reported Workshop scene.
json.dump(
    scene([
        {"name": "bg", "image": "models/bg.json"},
        {"name": "fg", "image": "models/fg.json"},
        {"name": "sparkle", "image": None, "particle": "particles/presets/magic_sparkle.json"},
    ]),
    open(sys.argv[1], "w"),
)
# (b) the same scene plus one drawable image layer: degraded, not blank, so
# it must still apply.
json.dump(
    scene([
        {"name": "bg", "image": "models/bg.json"},
        {"name": "real", "image": "m3c-red.png", "origin": [0.0, 0.0], "size": [160.0, 90.0]},
    ]),
    open(sys.argv[2], "w"),
)
B2PY

# Case B2-a: model-only scene -> preflight invalid_params, no worker spawns.
b2_reject="$(call_daemon renderer.start "$(jq -cn --arg content "$b2_models_scene" \
    '{wallpaper_id:"scene-b2-models",content_hash:"hash-b2-models",width:160,height:90,fps:30,kind:"scene",content:$content}')" || true)"
[[ "$(jq -r '.ok' <<<"$b2_reject")" == "false" ]]
[[ "$(jq -r '.result.error' <<<"$b2_reject")" == "invalid_params" ]]
[[ "$(jq -r '.result.detail' <<<"$b2_reject")" == *"scene preflight rejected"* ]]
[[ "$(jq -r '.result.detail' <<<"$b2_reject")" == *"draws nothing in this build"* ]]
# S1: the model-only reason text changed from "scene3d" to the honest
# resolution failure (no --wallpaper-engine-assets is configured for this
# daemon instance, so neither model layer's material texture resolves).
[[ "$(jq -r '.result.detail' <<<"$b2_reject")" == *"material textures could not be resolved"* ]]
[[ "$(jq -r '.result.detail' <<<"$b2_reject")" == *"external particle files"* ]]
echo "scene smoke passed (B2 a): model-only scene -> preflight invalid_params, never applied"

# Case B2-b: one drawable layer is enough — a degraded scene still applies
# (fullscreen red at the frame center), and the worker reports the model
# layer it could not render.
call_daemon renderer.start "$(jq -cn --arg content "$b2_partial_scene" \
    '{wallpaper_id:"scene-b2-partial",content_hash:"hash-b2-partial",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
b2_partial_status="$(wait_phase live)"
b2_partial_frame="$(jq -r '.result.frame_file' <<<"$b2_partial_status")"
scene_pixel_oracle "$b2_partial_frame" 80 45 "0,0,255,255" 1
b2_partial_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$b2_partial_status")"
# S1: the model layer's own texture-resolution diagnostic (no assets
# configured, so "bg" cannot resolve) replaces the old blanket skip.
[[ "$b2_partial_tail" == *"event=renderer.scene.model_texture_skip count=1"* ]]
echo "scene smoke passed (B2 b): one drawable layer -> applied, model texture skip reported"

# ---------------------------------------------------------------------------
# S1 case: a model layer whose material texture is a real TEXV0005 (raw
# ARGB8888) container, packaged entirely inside a scene.pkg — model.json,
# material.json, and the .tex asset are all pkg entries, so this case needs
# no --wallpaper-engine-assets configured (pkg-entries-first resolution
# finds everything). The model must resolve, decode, and draw its solid
# colour through the daemon lane end to end, exactly the headline S1
# behaviour: a model layer draws its base texture as a textured quad.
s1_model_pkg="$smoke_root/s1-model.pkg"
python3 - "$s1_model_pkg" <<'S1PY'
import struct
import sys


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


def texv_argb8888(width, height, rgba):
    out = bytearray()
    out += b"TEXV0005\0"
    out += b"TEXI0001\0"
    out += struct.pack("<I", 0)  # format ARGB8888
    out += struct.pack("<I", 0)  # flags
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # ignored
    out += b"TEXB0003\0"
    out += struct.pack("<I", 1)  # image count
    out += struct.pack("<i", -1)  # FIF_UNKNOWN
    out += struct.pack("<I", 1)  # mipmap count
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # compression = 0
    pixels = bytes(rgba) * (width * height)
    out += struct.pack("<i", len(pixels))  # uncompressedSize
    out += struct.pack("<i", len(pixels))  # compressedSize
    out += pixels
    return bytes(out)


scene_json = (
    b'{"general": {"clearcolor": [0.1, 0.1, 0.1, 1.0], "resolution": [160, 90], "fps": 30},'
    b' "objects": [{"name": "solid", "image": "models/solid.json",'
    b' "origin": [0.0, 0.0], "size": [160.0, 90.0]}]}'
)
model_json = b'{"material": "materials/solid.json"}'
material_json = b'{"passes": [{"shader": "genericimage2", "textures": ["solid"]}]}'
texture = texv_argb8888(4, 4, (255, 200, 0, 255))

open(sys.argv[1], "wb").write(
    build_pkg(
        [
            ("scene.json", scene_json),
            ("models/solid.json", model_json),
            ("materials/solid.json", material_json),
            ("materials/solid.tex", texture),
        ]
    )
)
S1PY

call_daemon renderer.start "$(jq -cn --arg content "$s1_model_pkg" \
    '{wallpaper_id:"scene-s1-model",content_hash:"hash-s1-model",width:160,height:90,fps:30,kind:"scene",content:$content}')" >/dev/null
s1_model_status="$(wait_phase live)"
s1_model_frame="$(jq -r '.result.frame_file' <<<"$s1_model_status")"
# scene_pixel_oracle expects BGRA memory order (its own doc comment);
# the texture pixel is R=255,G=200,B=0 (the pkg fixture builder's literal
# ARGB8888 payload bytes, expand_raw's identity copy), so in the frame's
# B8G8R8A8_UNORM order that is B,G,R,A = 0,200,255,255.
scene_pixel_oracle "$s1_model_frame" 80 45 "0,200,255,255" 1
s1_model_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$s1_model_status")"
[[ "$s1_model_tail" != *"model_texture_skip"* ]]
echo "scene smoke passed (S1): pkg model layer with a real TEXV0005 texture -> resolves, decodes, and draws its colour"

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
stop_standalone() {
    local pid="$1" log="$2" label="$3"
    kill -TERM "$pid" 2>/dev/null || true
    local status=0
    wait "$pid" || status=$?
    if [[ "$status" != "0" ]]; then
        echo "$label exited with status $status" >&2
        sed -n '1,160p' "$log" >&2
        return "$status"
    fi
}

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
stop_standalone "$standalone_pid" "$smoke_root/standalone.log" "standalone renderer"
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

# The M3c layer oracles on the llvmpipe lane: the composite case (draw
# order + layer transform + orientation — a mirrored framebuffer or a
# broken quad would move the mark's pixels and fail these samples) and the
# blend oracle (the ONE-factor premultiplied value). Same frame pixel
# oracle as the daemon lane, against the worker's own frame file.
standalone_m3c="$smoke_root/standalone-m3c.bin"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$standalone_m3c" --width 160 --height 90 --fps 30 \
    --content "$m3c_ab_scene" --device llvmpipe >"$smoke_root/standalone-m3c.log" 2>&1 &
standalone_m3c_pid=$!
for _attempt in {1..400}; do
    [[ -f "$standalone_m3c" ]] && head -c 8 "$standalone_m3c" | grep -q KWEFRM1 && break
    kill -0 "$standalone_m3c_pid" 2>/dev/null || {
        echo "standalone M3c renderer exited early" >&2
        sed -n '1,120p' "$smoke_root/standalone-m3c.log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$standalone_m3c" | grep -q KWEFRM1
scene_pixel_wait "$standalone_m3c" 90 55 "0,0,255,255" 1 "$smoke_root/standalone-m3c.log"
scene_pixel_wait "$standalone_m3c" 140 79 "255,0,0,255" 1 "$smoke_root/standalone-m3c.log"
scene_pixel_wait "$standalone_m3c" 150 85 "255,0,0,255" 1 "$smoke_root/standalone-m3c.log"
stop_standalone "$standalone_m3c_pid" "$smoke_root/standalone-m3c.log" "standalone M3c renderer"
echo "scene smoke passed: standalone llvmpipe lane — M3c two-layer composite oracles"

standalone_blend="$smoke_root/standalone-blend.bin"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$standalone_blend" --width 160 --height 90 --fps 30 \
    --content "$m3c_b_scene" --device llvmpipe >"$smoke_root/standalone-blend.log" 2>&1 &
standalone_blend_pid=$!
for _attempt in {1..400}; do
    [[ -f "$standalone_blend" ]] && head -c 8 "$standalone_blend" | grep -q KWEFRM1 && break
    kill -0 "$standalone_blend_pid" 2>/dev/null || {
        echo "standalone blend renderer exited early" >&2
        sed -n '1,120p' "$smoke_root/standalone-blend.log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$standalone_blend" | grep -q KWEFRM1
scene_pixel_wait "$standalone_blend" 80 45 "106,77,48,191" 1 "$smoke_root/standalone-blend.log"
stop_standalone "$standalone_blend_pid" "$smoke_root/standalone-blend.log" "standalone M3c blend renderer"
echo "scene smoke passed: standalone llvmpipe lane — M3c blend oracle (106,77,48,191)"

if [[ "$m3g_ready" == "1" ]]; then
    # Standalone M3g lane: the same synthetic clip is opened directly by the
    # worker under llvmpipe. This catches teardown/status failures that the
    # daemon's child supervision could otherwise hide.
    standalone_m3g="$smoke_root/standalone-m3g.bin"
    standalone_m3g_log="$smoke_root/standalone-m3g.log"
    VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
        --output "$standalone_m3g" --width 160 --height 90 --fps 30 \
        --content "$m3g_a_scene" --device llvmpipe >"$standalone_m3g_log" 2>&1 &
    standalone_m3g_pid=$!
    for _attempt in {1..1200}; do
        [[ -f "$standalone_m3g" ]] && head -c 8 "$standalone_m3g" | grep -q KWEFRM1 && break
        kill -0 "$standalone_m3g_pid" 2>/dev/null || {
            echo "standalone M3g renderer exited early" >&2
            sed -n '1,160p' "$standalone_m3g_log" >&2
            exit 1
        }
        sleep 0.05
    done
    head -c 8 "$standalone_m3g" | grep -q KWEFRM1
    m3g_standalone_first="$(m3g_wait_color "$standalone_m3g" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
        echo "standalone M3g failure: first clip color missing (best=$m3g_standalone_first)" >&2
        sed -n '1,160p' "$standalone_m3g_log" >&2
        exit 1
    }
    m3g_standalone_second="$(m3g_wait_color "$standalone_m3g" 76 41 8 8 "51,102,204,255" 20 60 200)" || {
        echo "standalone M3g failure: second clip color missing (best=$m3g_standalone_second)" >&2
        sed -n '1,160p' "$standalone_m3g_log" >&2
        exit 1
    }
    stop_standalone "$standalone_m3g_pid" "$standalone_m3g_log" "standalone M3g renderer"
    echo "scene smoke passed: standalone llvmpipe lane — M3g video playback and teardown"
fi

# The M3d blend/effect oracles on the llvmpipe lane: seven single-sample
# lanes plus the scripted switch. Every value is pinned EXACTLY (tolerance
# 0): the daemon lane above proved the composites with tolerance 1 and the
# device unit tests proved them byte-exact on this same llvmpipe driver, so
# the smoke lane pins the same bytes. lane_start waits for the canary
# (KWEFRM1 header) with the early-exit guard; lane_stop SIGTERMs and waits
# for exit 0. The canary window is 1200 x 0.05 s = 60 s: llvmpipe's first
# frame can take 20+ s under load (measured — the last lane of a full
# suite crossing a 20 s window), and the window only delays failure
# detection, never the passing path.
lane_start() {
    local output="$1" content="$2" log="$3"
    VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
        --output "$output" --width 160 --height 90 --fps 30 \
        --content "$content" --device llvmpipe >"$log" 2>&1 &
    lane_pid=$!
    lane_log="$log"
    for _attempt in {1..1200}; do
        [[ -f "$output" ]] && head -c 8 "$output" | grep -q KWEFRM1 && break
        kill -0 "$lane_pid" 2>/dev/null || {
            echo "standalone lane exited early" >&2
            sed -n '1,120p' "$log" >&2
            exit 1
        }
        sleep 0.05
    done
    head -c 8 "$output" | grep -q KWEFRM1
}

lane_stop() {
    stop_standalone "$lane_pid" "$lane_log" "standalone lane"
}

# Normal: the src-over identity over the opaque clear.
lane_start "$smoke_root/standalone-m3d-normal.bin" "$m3d_normal_scene" "$smoke_root/standalone-m3d-normal.log"
scene_pixel_wait "$smoke_root/standalone-m3d-normal.bin" 80 45 "142,103,64,255" 0 "$smoke_root/standalone-m3d-normal.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d normal byte oracle"

# Multiply: texel*bg/255.
lane_start "$smoke_root/standalone-m3d-multiply.bin" "$m3d_multiply_scene" "$smoke_root/standalone-m3d-multiply.log"
scene_pixel_wait "$smoke_root/standalone-m3d-multiply.bin" 80 45 "14,26,26,255" 0 "$smoke_root/standalone-m3d-multiply.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d multiply byte oracle"

# Add: min(255, texel+bg).
lane_start "$smoke_root/standalone-m3d-add.bin" "$m3d_add_scene" "$smoke_root/standalone-m3d-add.log"
scene_pixel_wait "$smoke_root/standalone-m3d-add.bin" 80 45 "168,167,166,255" 0 "$smoke_root/standalone-m3d-add.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d add byte oracle"

# Screen: 255-(255-texel)(255-bg)/255.
lane_start "$smoke_root/standalone-m3d-screen.bin" "$m3d_screen_scene" "$smoke_root/standalone-m3d-screen.log"
scene_pixel_wait "$smoke_root/standalone-m3d-screen.bin" 80 45 "154,141,140,255" 0 "$smoke_root/standalone-m3d-screen.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d screen byte oracle"

# Subtract: max(0, bg-texel).
lane_start "$smoke_root/standalone-m3d-subtract.bin" "$m3d_subtract_scene" "$smoke_root/standalone-m3d-subtract.log"
scene_pixel_wait "$smoke_root/standalone-m3d-subtract.bin" 80 45 "0,0,38,255" 0 "$smoke_root/standalone-m3d-subtract.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d subtract byte oracle"

# Add at alpha 128: the single-premultiplication pin — the straight
# composite (64,103,142,128) premultiplied once at readback. The alpha 0.5
# stores 128 (0.5*255=127.5 rounds to nearest even), and the RGB follows:
# B=(142*128+127)/255=71, G=(103*128+127)/255=52, R=(64*128+127)/255=32.
lane_start "$smoke_root/standalone-m3d-add128.bin" "$m3d_add128_scene" "$smoke_root/standalone-m3d-add128.log"
scene_pixel_wait "$smoke_root/standalone-m3d-add128.bin" 80 45 "71,52,32,128" 0 "$smoke_root/standalone-m3d-add128.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d add-at-128 byte oracle"

# Effects: brightness 2.0, tint (1,0.4,0.5) — (128,82,142) over opaque.
lane_start "$smoke_root/standalone-m3d-effects.bin" "$m3d_effects_scene" "$smoke_root/standalone-m3d-effects.log"
scene_pixel_wait "$smoke_root/standalone-m3d-effects.bin" 80 45 "142,82,128,255" 0 "$smoke_root/standalone-m3d-effects.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d effects byte oracle"

# Translucent multiply: the alpha-policy pin — layer alpha 0.5 over a
# 0.5-alpha clear, delivered (11,20,20,192).
lane_start "$smoke_root/standalone-m3d-multiply128.bin" "$m3d_multiply128_scene" "$smoke_root/standalone-m3d-multiply128.log"
scene_pixel_wait "$smoke_root/standalone-m3d-multiply128.bin" 80 45 "11,20,20,192" 0 "$smoke_root/standalone-m3d-multiply128.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d translucent multiply byte oracle"

# The scripted switch: update() writes blendMode add (6) until t=3s then
# multiply (1) — two exact samples of the same live frame file. The first
# POLLS until the add composite is observed (a slow lane must not sample
# before the first update() or after the t=3s switch); the 3.5s wait starts
# from the observation.
lane_start "$smoke_root/standalone-m3d-js.bin" "$m3d_js_scene" "$smoke_root/standalone-m3d-js.log"
m3d_js_lane_add_observed=0
for _attempt in {1..120}; do
    if scene_pixel_oracle "$smoke_root/standalone-m3d-js.bin" 80 45 "168,167,166,255" 0 >/dev/null 2>&1; then
        m3d_js_lane_add_observed=1
        break
    fi
    sleep 0.25
done
[[ "$m3d_js_lane_add_observed" == "1" ]]
sleep 3.5
scene_pixel_wait "$smoke_root/standalone-m3d-js.bin" 80 45 "14,26,26,255" 0 "$smoke_root/standalone-m3d-js.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3d scripted blendMode switch (exact)"

# The M3e text oracle on the llvmpipe lane: a fixed string ("SMOKE") in a
# known family (Noto Sans, the resolver's first fallback candidate — if
# the machine lacks it, the fallback chain resolves and the structural
# oracle still holds), red on opaque blue. Structural assertions, not
# byte-pins: the region must hold >= 300 pixels matching the text color
# and >= 400 pixels differing from the background, and the mean color of
# the differing pixels must be the text color within per-channel bounds.
# The actual counts and mean are printed for the acceptance record. A
# fontless machine skips with a message (the resolver's text_font_none
# diagnostic is the signal).
if [[ -n "$m3e_any_font" ]]; then
    standalone_m3e_a="$smoke_root/standalone-m3e-a.bin"
    VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
        --output "$standalone_m3e_a" --width 160 --height 90 --fps 30 \
        --content "$m3e_a_scene" --device llvmpipe >"$smoke_root/standalone-m3e-a.log" 2>&1 &
    standalone_m3e_a_pid=$!
    for _attempt in {1..400}; do
        [[ -f "$standalone_m3e_a" ]] && head -c 8 "$standalone_m3e_a" | grep -q KWEFRM1 && break
        kill -0 "$standalone_m3e_a_pid" 2>/dev/null || {
            echo "standalone M3e renderer exited early" >&2
            sed -n '1,120p' "$smoke_root/standalone-m3e-a.log" >&2
            exit 1
        }
        sleep 0.05
    done
    head -c 8 "$standalone_m3e_a" | grep -q KWEFRM1
    wait_first_frame "$standalone_m3e_a" "$smoke_root/standalone-m3e-a.log"
    if grep -q "event=renderer.scene.text_font_none" "$smoke_root/standalone-m3e-a.log"; then
        echo "scene smoke SKIP (M3e a): no usable system fonts — text lane needs real fonts"
        stop_standalone "$standalone_m3e_a_pid" "$smoke_root/standalone-m3e-a.log" "standalone M3e renderer" || true
    else
        # Mean bounds are RED-DOMINANCE, not tight per-channel: glyph
        # interiors are pure text color, but the antialiased edges lean
        # toward the blue background, so the differing-pixel mean carries
        # a blue cast (measured on this lane: R 234.0, G 0.0, B 128.9).
        scene_region_oracle "$standalone_m3e_a" 30 18 100 54 "255,0,0,255" "0,0,255,255" 20 30 300 400 150 255 0 80 0 210
        stop_standalone "$standalone_m3e_a_pid" "$smoke_root/standalone-m3e-a.log" "standalone M3e renderer"
        echo "scene smoke passed: standalone llvmpipe lane — M3e text region oracle (fixed string, known family)"
    fi
else
    echo "scene smoke SKIP (M3e a): no system fonts under /usr/share/fonts — text lane needs real fonts"
fi

# The M3f oracles on the llvmpipe lane: the same six cases, run directly
# against the worker's own frame file. The canary wait (lane_start) covers
# startup; each lane then polls its steady-state signal exactly like the
# daemon lane above (tolerances identical — the region/gravity/max oracles
# are structural, never byte-pins, because particle positions are
# frame-time dependent).

# (a): the deterministic trail.
lane_start "$smoke_root/standalone-m3f-a.bin" "$m3f_a_scene" "$smoke_root/standalone-m3f-a.log"
m3f_a_lane_fg=0
for _attempt in {1..120}; do
    if m3f_a_lane_probe="$(scene_region_probe "$smoke_root/standalone-m3f-a.bin" 70 35 80 20 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_a_lane_fg="${m3f_a_lane_probe#foreground=}"
        (( m3f_a_lane_fg >= 450 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_a_lane_fg" -ge 450 ]]
scene_region_oracle "$smoke_root/standalone-m3f-a.bin" 70 35 80 20 "0,0,0,255" "255,255,255,255" 20 30 450 450 250 255 250 255 250 255
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f a trail oracle ($m3f_a_lane_fg white px)"

# (b): the gravity differential.
lane_start "$smoke_root/standalone-m3f-b.bin" "$m3f_b_scene" "$smoke_root/standalone-m3f-b.log"
m3f_b_lane_blue=0
for _attempt in {1..120}; do
    if m3f_b_lane_probe="$(scene_region_probe "$smoke_root/standalone-m3f-b.bin" 60 50 40 40 "255,0,0,255" 30 2>/dev/null)"; then
        m3f_b_lane_blue="${m3f_b_lane_probe#foreground=}"
        (( m3f_b_lane_blue >= 100 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_b_lane_blue" -ge 100 ]]
scene_gravity_oracle "$smoke_root/standalone-m3f-b.bin" "255,0,0,255" "0,0,255,255" 30 25 40 3
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f b gravity oracle"

# (c): the spawn cap — the diagnostic fires in the lane's own log (first
# dropped spawn, ~1.0 s sim — after the whole-frame poll below, like the
# daemon lane). The poll converges on the first >= 4k crossing: the
# sliding 1-s cohort's solid annulus phase (max ~8.7-8.8k px at t ~ 2, on
# frame >= 4k for ~40% of every 5 s cycle — see the (c) fixture comment).
lane_start "$smoke_root/standalone-m3f-c.bin" "$m3f_c_scene" "$smoke_root/standalone-m3f-c.log"
m3f_c_lane_fg=0
for _attempt in {1..120}; do
    if m3f_c_lane_probe="$(scene_region_probe "$smoke_root/standalone-m3f-c.bin" 0 0 160 90 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_c_lane_fg="${m3f_c_lane_probe#foreground=}"
        (( m3f_c_lane_fg >= 4000 )) && break
    fi
    sleep 0.25
done
[[ "$m3f_c_lane_fg" -ge 4000 ]]
grep -q "event=renderer.scene.particles_capped system=dust" "$smoke_root/standalone-m3f-c.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f c cap oracle ($m3f_c_lane_fg px, particles_capped diag)"

# (d): the instance.count factor from script.
lane_start "$smoke_root/standalone-m3f-d.bin" "$m3f_d_scene" "$smoke_root/standalone-m3f-d.log"
m3f_d_lane_ratio=0
for _attempt in {1..120}; do
    if m3f_d_lane_pa="$(scene_region_probe "$smoke_root/standalone-m3f-d.bin" 0 15 60 60 "255,255,255,255" 30 2>/dev/null)" \
        && m3f_d_lane_pb="$(scene_region_probe "$smoke_root/standalone-m3f-d.bin" 100 15 60 60 "255,255,255,255" 30 2>/dev/null)"; then
        m3f_d_lane_ca="${m3f_d_lane_pa#foreground=}"
        m3f_d_lane_cb="${m3f_d_lane_pb#foreground=}"
        if (( m3f_d_lane_ca >= 300 && m3f_d_lane_cb > 3 * m3f_d_lane_ca )); then
            m3f_d_lane_ratio=$(( m3f_d_lane_cb * 10 / m3f_d_lane_ca ))
            break
        fi
    fi
    sleep 0.25
done
[[ "$m3f_d_lane_ratio" -ge 30 ]]
grep -q "M3F-COUNT-SET 8" "$smoke_root/standalone-m3f-d.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f d instance.count oracle (ratio $(( m3f_d_lane_ratio / 10 )).$(( m3f_d_lane_ratio % 10 )))"

# (e): the blend-mode differential.
lane_start "$smoke_root/standalone-m3f-e.bin" "$m3f_e_scene" "$smoke_root/standalone-m3f-e.log"
m3f_e_lane_add_observed=0
for _attempt in {1..120}; do
    if scene_region_max "$smoke_root/standalone-m3f-e.bin" 0 15 80 60 "30,30,30,255" 20 150 255 >/dev/null 2>&1; then
        m3f_e_lane_add_observed=1
        break
    fi
    sleep 0.25
done
[[ "$m3f_e_lane_add_observed" == "1" ]]
scene_region_max "$smoke_root/standalone-m3f-e.bin" 0 15 80 60 "30,30,30,255" 20 150 255
scene_region_max "$smoke_root/standalone-m3f-e.bin" 80 15 80 60 "30,30,30,255" 20 0 100
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f e blend differential oracle"

# (f): the cross-kind draw order — red at the frame center (the image
# listed after the particle system draws on top of its disc) plus white
# left of the image (the disc itself).
lane_start "$smoke_root/standalone-m3f-f.bin" "$m3f_f_scene" "$smoke_root/standalone-m3f-f.log"
m3f_f_lane_red_seen=0
for _attempt in {1..120}; do
    if m3f_f_lane_probe="$(scene_region_probe "$smoke_root/standalone-m3f-f.bin" 80 45 1 1 "0,0,255,255" 30 2>/dev/null)"; then
        m3f_f_lane_fg="${m3f_f_lane_probe#foreground=}"
        (( m3f_f_lane_fg >= 1 )) && { m3f_f_lane_red_seen=1; break; }
    fi
    sleep 0.25
done
if [[ "$m3f_f_lane_red_seen" != "1" ]]; then
    echo "M3f-f failure: standalone lane frame center never read red (particles drawing over the image?) frame=$smoke_root/standalone-m3f-f.bin" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3f-f.log" >&2
    exit 1
fi
m3f_f_lane_white_seen=0
for _attempt in {1..120}; do
    if m3f_f_lane_white="$(scene_region_probe "$smoke_root/standalone-m3f-f.bin" 50 45 1 1 "255,255,255,255" 30 2>/dev/null)"; then
        (( ${m3f_f_lane_white#foreground=} >= 1 )) && { m3f_f_lane_white_seen=1; break; }
    fi
    sleep 0.25
done
if [[ "$m3f_f_lane_white_seen" != "1" ]]; then
    echo "M3f-f failure: standalone lane disc probe (50,45) never read white (disc missing?) probe=$m3f_f_lane_white" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3f-f.log" >&2
    exit 1
fi
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3f f draw-order oracle (image over particle system)"


# The M3g oracles on the llvmpipe lane: the same four cases, run directly
# against the worker's own frame file and its own stderr log. Video
# decoding is libmpv on the CPU (hwdec=no), so nothing here depends on
# the device — but the UPLOAD path does: refresh_layer writes the decoded
# frame into the live image every tick, and a driver that needed an
# explicit flush would show a frozen first frame, which case (a) catches.
if [[ "$m3g_ready" == "1" ]]; then

# (a): playback advances — both clip colors reach the center.
lane_start "$smoke_root/standalone-m3g-a.bin" "$m3g_a_scene" "$smoke_root/standalone-m3g-a.log"
m3g_a_lane_first="$(m3g_wait_color "$smoke_root/standalone-m3g-a.bin" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-a failure: standalone lane never showed the first clip color (best=$m3g_a_lane_first of 64)" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3g-a.log" >&2
    exit 1
}
m3g_a_lane_second="$(m3g_wait_color "$smoke_root/standalone-m3g-a.bin" 76 41 8 8 "51,102,204,255" 20 60 200)" || {
    echo "M3g-a failure: standalone lane playback is not advancing (best=$m3g_a_lane_second of 64)" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3g-a.log" >&2
    exit 1
}
grep -q "event=renderer.scene.video_open layer=clip size=64x64" "$smoke_root/standalone-m3g-a.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3g a playback oracle ($m3g_a_lane_first then $m3g_a_lane_second of 64 px)"

# (b): the native-size substitution — clip at the center, clear outside.
lane_start "$smoke_root/standalone-m3g-b.bin" "$m3g_b_scene" "$smoke_root/standalone-m3g-b.log"
m3g_b_lane_center="$(m3g_wait_color "$smoke_root/standalone-m3g-b.bin" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-b failure: standalone lane never showed the clip at the center (best=$m3g_b_lane_center of 64)" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3g-b.log" >&2
    exit 1
}
m3g_b_lane_outside="$(scene_region_probe "$smoke_root/standalone-m3g-b.bin" 120 41 8 8 "0,255,0,255" 20)"
[[ "${m3g_b_lane_outside#foreground=}" == "64" ]]
grep -q "event=renderer.scene.video_open layer=clip size=64x64" "$smoke_root/standalone-m3g-b.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3g b native-size oracle"

# (c): the concurrency cap — one skip counted, the scene still renders.
lane_start "$smoke_root/standalone-m3g-c.bin" "$m3g_c_scene" "$smoke_root/standalone-m3g-c.log"
m3g_c_lane_center="$(m3g_wait_color "$smoke_root/standalone-m3g-c.bin" 76 41 8 8 "204,102,51,255" 20 60 200)" || {
    echo "M3g-c failure: standalone lane capped scene never drew the clip (best=$m3g_c_lane_center of 64)" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3g-c.log" >&2
    exit 1
}
grep -q "event=renderer.scene.video_layer_skip count=1 (cap is 2)" "$smoke_root/standalone-m3g-c.log"
grep -q "layer_skip layer=clip2 detail=video-source-unavailable" "$smoke_root/standalone-m3g-c.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3g c concurrency-cap oracle"

# (d): the unresolved source — only that layer skipped, with the
# resolver's detail.
lane_start "$smoke_root/standalone-m3g-d.bin" "$m3g_d_scene" "$smoke_root/standalone-m3g-d.log"
m3g_d_lane_red="$(m3g_wait_color "$smoke_root/standalone-m3g-d.bin" 76 41 8 8 "0,0,255,255" 20 60 120)" || {
    echo "M3g-d failure: standalone lane image layer never drew (best=$m3g_d_lane_red of 64)" >&2
    sed -n '1,120p' "$smoke_root/standalone-m3g-d.log" >&2
    exit 1
}
grep -qF 'layer_skip layer=broken detail=video "m3g-missing.mp4" is missing or unreadable' "$smoke_root/standalone-m3g-d.log"
lane_stop
echo "scene smoke passed: standalone llvmpipe lane — M3g d unresolved-source oracle"

fi

# B2 backstop lane: the worker's own no-drawable-content guard. The daemon
# lane cannot reach it (preflight refuses the same scene first), so the
# model-only scene runs standalone: it must exit 74 before publishing a
# single frame, naming what it could not render.
b2_standalone_log="$smoke_root/standalone-b2.log"
b2_standalone_status=0
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$smoke_root/standalone-b2.bin" --width 160 --height 90 --fps 30 \
    --content "$b2_models_scene" --device llvmpipe >"$b2_standalone_log" 2>&1 || \
    b2_standalone_status=$?
if [[ "$b2_standalone_status" != "74" ]]; then
    echo "B2 backstop failure: expected exit 74, got $b2_standalone_status" >&2
    sed -n '1,120p' "$b2_standalone_log" >&2
    exit 1
fi
# S1: both model layers still fail to resolve standalone (no
# --assets-dir given here either), so the worker's own texture-skip
# diagnostic fires with the same count the old model_layer_skip did.
grep -q "event=renderer.scene.model_texture_skip count=2" "$b2_standalone_log"
grep -q "event=renderer.scene.no_drawable_content objects=3" "$b2_standalone_log"
grep -q "event=renderer.scene.unsupported exit_code=74" "$b2_standalone_log"
[[ ! -f "$smoke_root/standalone-b2.bin" ]]
echo "scene smoke passed (B2 d): standalone model-only scene -> exit 74, no frame published"

# S2: a model layer whose material names a custom shader (not one of the
# real Wallpaper Engine asset shaders) — the test writes a tiny synthetic
# shaders/ tree into its own --assets-dir and the material pipeline must
# preprocess, compile, and draw through it: a deterministic solid colour,
# independent of the texture (the fragment shader ignores v_TexCoord),
# proving the full shaderpre -> shaderc -> Vulkan material-pipeline path
# end to end rather than just falling back to the S1 base-texture quad.
s2_assets_dir="$smoke_root/s2-assets"
mkdir -p "$s2_assets_dir/shaders"
cat >"$s2_assets_dir/shaders/smoketest.vert" <<'VERT'
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform mat4 g_ModelViewProjectionMatrix;
varying vec2 v_TexCoord;
void main() {
	gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);
	v_TexCoord = a_TexCoord;
}
VERT
cat >"$s2_assets_dir/shaders/smoketest.frag" <<'FRAG'
varying vec2 v_TexCoord;
void main() {
	gl_FragColor = vec4(0.2, 0.6, 0.8, 1.0) + 0.0 * vec4(v_TexCoord, 0.0, 0.0);
}
FRAG

s2_material_pkg="$smoke_root/s2-material.pkg"
python3 - "$s2_material_pkg" <<'S2PY'
import struct
import sys


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


def texv_argb8888(width, height, rgba):
    out = bytearray()
    out += b"TEXV0005\0"
    out += b"TEXI0001\0"
    out += struct.pack("<I", 0)  # format ARGB8888
    out += struct.pack("<I", 0)  # flags
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # ignored
    out += b"TEXB0003\0"
    out += struct.pack("<I", 1)  # image count
    out += struct.pack("<i", -1)  # FIF_UNKNOWN
    out += struct.pack("<I", 1)  # mipmap count
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # compression = 0
    pixels = bytes(rgba) * (width * height)
    out += struct.pack("<i", len(pixels))  # uncompressedSize
    out += struct.pack("<i", len(pixels))  # compressedSize
    out += pixels
    return bytes(out)


# The texture colour (255, 0, 0, 255) is deliberately NOT the shader's
# output colour: the oracle below only matches the shader's hard-coded
# vec4(0.2, 0.6, 0.8, 1.0), so a S2 regression that quietly fell back to
# drawing the base texture as a flat quad (the S1 path) would sample red,
# not the shader's colour, and the oracle would catch it.
scene_json = (
    b'{"general": {"clearcolor": [0.0, 0.0, 0.0, 1.0], "resolution": [160, 90], "fps": 30},'
    b' "objects": [{"name": "solid", "image": "models/solid.json",'
    b' "origin": [0.0, 0.0], "size": [160.0, 90.0]}]}'
)
model_json = b'{"material": "materials/solid.json"}'
material_json = b'{"passes": [{"shader": "smoketest", "textures": ["solid"]}]}'
texture = texv_argb8888(4, 4, (255, 0, 0, 255))

open(sys.argv[1], "wb").write(
    build_pkg(
        [
            ("scene.json", scene_json),
            ("models/solid.json", model_json),
            ("materials/solid.json", material_json),
            ("materials/solid.tex", texture),
        ]
    )
)
S2PY

s2_standalone_log="$smoke_root/standalone-s2-material.log"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$smoke_root/standalone-s2-material.bin" --width 160 --height 90 --fps 30 \
    --content "$s2_material_pkg" --assets-dir "$s2_assets_dir" --device llvmpipe \
    >"$s2_standalone_log" 2>&1 &
s2_standalone_pid=$!
for _attempt in {1..400}; do
    [[ -f "$smoke_root/standalone-s2-material.bin" ]] && head -c 8 "$smoke_root/standalone-s2-material.bin" | grep -q KWEFRM1 && break
    kill -0 "$s2_standalone_pid" 2>/dev/null || {
        echo "standalone S2 material renderer exited early" >&2
        sed -n '1,120p' "$s2_standalone_log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$smoke_root/standalone-s2-material.bin" | grep -q KWEFRM1
# The KWEFRM1 magic is written when the frame mapping is created — well
# before material-shader compilation runs (main.rs step 5a), let alone the
# first render — so the "shaders compiled=" log line is not guaranteed to
# exist yet at this point. `scene_pixel_wait` polls until the shader has
# actually drawn its colour (compilation is necessarily done by then), so
# check the log line AFTER it rather than racing the worker's own startup
# order.
# B8G8R8A8 memory order: B=0.8*255=204, G=0.6*255=153, R=0.2*255=51.
scene_pixel_wait "$smoke_root/standalone-s2-material.bin" 80 45 "204,153,51,255" 1 "$s2_standalone_log"
grep -q "event=renderer.scene.shaders compiled=1 fallback=0" "$s2_standalone_log"
stop_standalone "$s2_standalone_pid" "$s2_standalone_log" "standalone S2 material renderer"
echo "scene smoke passed (S2): pkg model layer with a custom material shader -> compiles and draws through the material pipeline, not the S1 base texture"

# S3: a model layer with a resolvable but visually-irrelevant base
# material, plus one resolved `effects[]` entry naming a synthetic
# effect file that declares one FBO (`_rt_Solid`) and two material
# passes: pass 0 renders a deterministic hard-coded colour INTO that FBO
# (`target: "_rt_Solid"`); pass 1 has no target (upstream's "draws
# directly onto the compositor" case, which this renderer folds into the
# layer's own material — see `EffectChainPlan`'s doc comment in main.rs)
# and samples `_rt_Solid` via its OWN material.json `textures` array,
# passing the sampled colour straight through. The oracle proves the
# whole S3 chain end to end on a real scene load (not just the device
# test in vulkan.rs): FBO creation+clear, the intermediate pass actually
# rendering into it, and the final pass sampling it by name.
s3_assets_dir="$smoke_root/s3-assets"
mkdir -p "$s3_assets_dir/shaders"
cat >"$s3_assets_dir/shaders/s3base.vert" <<'VERT'
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform mat4 g_ModelViewProjectionMatrix;
varying vec2 v_TexCoord;
void main() {
	gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);
	v_TexCoord = a_TexCoord;
}
VERT
cat >"$s3_assets_dir/shaders/s3base.frag" <<'FRAG'
varying vec2 v_TexCoord;
void main() {
	gl_FragColor = vec4(1.0, 1.0, 1.0, 1.0) + 0.0 * vec4(v_TexCoord, 0.0, 0.0);
}
FRAG
cp "$s3_assets_dir/shaders/s3base.vert" "$s3_assets_dir/shaders/s3solid.vert"
cat >"$s3_assets_dir/shaders/s3solid.frag" <<'FRAG'
varying vec2 v_TexCoord;
void main() {
	// A deliberately distinctive hard-coded colour, unrelated to any
	// sampled texture -- proves this pass's OWN draw ran, not a
	// coincidental default.
	gl_FragColor = vec4(0.2, 0.5, 0.9, 1.0) + 0.0 * vec4(v_TexCoord, 0.0, 0.0);
}
FRAG
cp "$s3_assets_dir/shaders/s3base.vert" "$s3_assets_dir/shaders/s3sample.vert"
cat >"$s3_assets_dir/shaders/s3sample.frag" <<'FRAG'
uniform sampler2D g_Texture0;
varying vec2 v_TexCoord;
void main() {
	gl_FragColor = texSample2D(g_Texture0, v_TexCoord);
}
FRAG

s3_effects_pkg="$smoke_root/s3-effects.pkg"
python3 - "$s3_effects_pkg" <<'S3PY'
import struct
import sys


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


def texv_argb8888(width, height, rgba):
    out = bytearray()
    out += b"TEXV0005\0"
    out += b"TEXI0001\0"
    out += struct.pack("<I", 0)  # format ARGB8888
    out += struct.pack("<I", 0)  # flags
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # ignored
    out += b"TEXB0003\0"
    out += struct.pack("<I", 1)  # image count
    out += struct.pack("<i", -1)  # FIF_UNKNOWN
    out += struct.pack("<I", 1)  # mipmap count
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # compression = 0
    pixels = bytes(rgba) * (width * height)
    out += struct.pack("<i", len(pixels))
    out += struct.pack("<i", len(pixels))
    out += pixels
    return bytes(out)


scene_json = (
    b'{"general": {"clearcolor": [0.0, 0.0, 0.0, 1.0], "resolution": [160, 90], "fps": 30},'
    b' "objects": [{"name": "fx", "image": "models/fx.json",'
    b' "origin": [0.0, 0.0], "size": [160.0, 90.0],'
    b' "effects": [{"file": "effects/test.json", "visible": true, "passes": [{}]}]}]}'
)
model_json = b'{"material": "materials/fx.json", "fullscreen": true}'
# The base material's ONLY texture is the `_rt_FullFrameBuffer` runtime
# target (the real corpus's `copybackground` pattern,
# `materials/util/fullscreenlayer.json`) -- deliberately, not a real
# `.tex` asset: compile_material_layers only lets an effect chain's own
# final untargeted pass replace a layer's base material when the base
# material has nothing real of its own to lose (main.rs's
# `texture_slots_are_bare_render_target_only` safety decision, added
# after the corpus regression sweep found a REAL base photo's effect
# chain discarding the photo entirely). Testing that exact safe-boundary
# case here, not a real-photo-plus-effect case this slice does not yet
# handle.
material_json = b'{"passes": [{"shader": "s3base", "textures": ["_rt_FullFrameBuffer"]}]}'

effect_json = (
    b'{"name": "test",'
    b' "fbos": [{"name": "_rt_Solid", "scale": 1.0, "format": "rgba8888"}],'
    b' "passes": ['
    b'  {"material": "materials/effects/solid.json", "target": "_rt_Solid"},'
    b'  {"material": "materials/effects/sample.json"}'
    b' ]}'
)
solid_material_json = b'{"passes": [{"shader": "s3solid"}]}'
sample_material_json = b'{"passes": [{"shader": "s3sample", "textures": ["_rt_Solid"]}]}'

open(sys.argv[1], "wb").write(
    build_pkg(
        [
            ("scene.json", scene_json),
            ("models/fx.json", model_json),
            ("materials/fx.json", material_json),
            ("effects/test.json", effect_json),
            ("materials/effects/solid.json", solid_material_json),
            ("materials/effects/sample.json", sample_material_json),
        ]
    )
)
S3PY

s3_standalone_log="$smoke_root/standalone-s3-effects.log"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$smoke_root/standalone-s3-effects.bin" --width 160 --height 90 --fps 30 \
    --content "$s3_effects_pkg" --assets-dir "$s3_assets_dir" --device llvmpipe \
    >"$s3_standalone_log" 2>&1 &
s3_standalone_pid=$!
for _attempt in {1..400}; do
    [[ -f "$smoke_root/standalone-s3-effects.bin" ]] && head -c 8 "$smoke_root/standalone-s3-effects.bin" | grep -q KWEFRM1 && break
    kill -0 "$s3_standalone_pid" 2>/dev/null || {
        echo "standalone S3 effects renderer exited early" >&2
        sed -n '1,120p' "$s3_standalone_log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$smoke_root/standalone-s3-effects.bin" | grep -q KWEFRM1
# B8G8R8A8 memory order: B=0.9*255=229.5, G=0.5*255=127.5, R=0.2*255=51.
scene_pixel_wait "$smoke_root/standalone-s3-effects.bin" 80 45 "230,128,51,255" 2 "$s3_standalone_log"
grep -q "event=renderer.scene.effects objects=1 passes=1 fallback=0" "$s3_standalone_log"
grep -q "event=renderer.scene.shaders compiled=1 fallback=0" "$s3_standalone_log"
stop_standalone "$s3_standalone_pid" "$s3_standalone_log" "standalone S3 effects renderer"
echo "scene smoke passed (S3): pkg model layer with a resolved effect chain -> a targeted pass renders a deterministic colour into an FBO, a second (untargeted) pass samples it and becomes the layer's own material draw"

# S4: a model layer whose material shader declares FOUR attributes —
# `a_Position`, `a_TexCoord`, `a_Normal`, `a_Color` — the
# `genericimage3`/`genericimage4`-family image-object shape S4 newly
# supports (S1/S2/S3 only ever fed `a_Position`+`a_TexCoord` through the
# widened `MATERIAL_UNIT_QUAD` buffer; `a_Normal` is always `+Z` and
# `a_Color` is always opaque white on this flat quad — see
# `docs/SCENE_FORMAT_V1.md`). The fragment shader multiplies `a_Color`
# by `a_Normal` (`v_Color * vec4(v_Normal, 1.0)`), so a correct draw is
# pure opaque blue ((1,1,1,1) * (0,0,1,1) = (0,0,1,1)); any misrouted
# attribute (wrong offset, wrong format, an unmodified S1 base-texture
# fallback) would draw the base texture's red instead.
s4_assets_dir="$smoke_root/s4-assets"
mkdir -p "$s4_assets_dir/shaders"
cat >"$s4_assets_dir/shaders/smoketest4.vert" <<'VERT'
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
attribute vec3 a_Normal;
attribute vec4 a_Color;
uniform mat4 g_ModelViewProjectionMatrix;
varying vec4 v_Color;
varying vec3 v_Normal;
void main() {
	gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);
	v_Color = a_Color;
	v_Normal = a_Normal;
}
VERT
cat >"$s4_assets_dir/shaders/smoketest4.frag" <<'FRAG'
varying vec4 v_Color;
varying vec3 v_Normal;
void main() {
	gl_FragColor = v_Color * vec4(v_Normal, 1.0);
}
FRAG

s4_material_pkg="$smoke_root/s4-material.pkg"
python3 - "$s4_material_pkg" <<'S4PY'
import struct
import sys


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


def texv_argb8888(width, height, rgba):
    out = bytearray()
    out += b"TEXV0005\0"
    out += b"TEXI0001\0"
    out += struct.pack("<I", 0)  # format ARGB8888
    out += struct.pack("<I", 0)  # flags
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # ignored
    out += b"TEXB0003\0"
    out += struct.pack("<I", 1)  # image count
    out += struct.pack("<i", -1)  # FIF_UNKNOWN
    out += struct.pack("<I", 1)  # mipmap count
    out += struct.pack("<I", width)
    out += struct.pack("<I", height)
    out += struct.pack("<I", 0)  # compression = 0
    pixels = bytes(rgba) * (width * height)
    out += struct.pack("<i", len(pixels))  # uncompressedSize
    out += struct.pack("<i", len(pixels))  # compressedSize
    out += pixels
    return bytes(out)


# The texture colour (255, 0, 0, 255) is deliberately NOT the shader's
# output colour, same reasoning as the S2 case above: the oracle only
# matches the shader's real a_Color/a_Normal-derived blue, so a
# regression that quietly fell back to the S1 base-texture quad (or that
# fed a_Normal/a_Color garbage instead of the documented (0,0,1)/(1,1,1,1)
# constants) would sample red or something other than pure blue, and the
# oracle would catch it.
scene_json = (
    b'{"general": {"clearcolor": [0.0, 0.0, 0.0, 1.0], "resolution": [160, 90], "fps": 30},'
    b' "objects": [{"name": "solid", "image": "models/solid.json",'
    b' "origin": [0.0, 0.0], "size": [160.0, 90.0]}]}'
)
model_json = b'{"material": "materials/solid.json"}'
material_json = b'{"passes": [{"shader": "smoketest4", "textures": ["solid"]}]}'
texture = texv_argb8888(4, 4, (255, 0, 0, 255))

open(sys.argv[1], "wb").write(
    build_pkg(
        [
            ("scene.json", scene_json),
            ("models/solid.json", model_json),
            ("materials/solid.json", material_json),
            ("materials/solid.tex", texture),
        ]
    )
)
S4PY

s4_standalone_log="$smoke_root/standalone-s4-material.log"
VK_ICD_FILENAMES="$lvp_icd" "$target_dir/debug/kwe-scene-renderer" \
    --output "$smoke_root/standalone-s4-material.bin" --width 160 --height 90 --fps 30 \
    --content "$s4_material_pkg" --assets-dir "$s4_assets_dir" --device llvmpipe \
    >"$s4_standalone_log" 2>&1 &
s4_standalone_pid=$!
for _attempt in {1..400}; do
    [[ -f "$smoke_root/standalone-s4-material.bin" ]] && head -c 8 "$smoke_root/standalone-s4-material.bin" | grep -q KWEFRM1 && break
    kill -0 "$s4_standalone_pid" 2>/dev/null || {
        echo "standalone S4 material renderer exited early" >&2
        sed -n '1,120p' "$s4_standalone_log" >&2
        exit 1
    }
    sleep 0.05
done
head -c 8 "$smoke_root/standalone-s4-material.bin" | grep -q KWEFRM1
# B8G8R8A8 memory order, opaque pure blue: B=255, G=0, R=0, A=255.
scene_pixel_wait "$smoke_root/standalone-s4-material.bin" 80 45 "255,0,0,255" 1 "$s4_standalone_log"
grep -q "event=renderer.scene.shaders compiled=1 fallback=0" "$s4_standalone_log"
stop_standalone "$s4_standalone_pid" "$s4_standalone_log" "standalone S4 material renderer"
echo "scene smoke passed (S4): pkg model layer with a_Normal/a_Color-declaring material shader -> compiles through the widened vertex-attribute pipeline and draws its real colour, not the S1 base texture"

echo "all scene smoke cases passed"
