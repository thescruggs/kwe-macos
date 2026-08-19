#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Supervised sandboxed-web-renderer smoke suite (BETA_M2b; grants lane
# BETA_M2c).
# Mirrors scripts/smoke-video.sh: isolated smoke root, daemon with fast
# bounded supervisor timings, and jq assertions on the local JSON API. The web
# fixtures (animated dot, static page, pointer oracle, busy loop) are
# generated at runtime and never committed. SKIPPED (exit 0) when chromium or
# bwrap is missing, so machines without the sandbox runtime can still run the
# other acceptance lanes.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-web-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
fixture_animated="$smoke_root/fixtures/animated"
fixture_static="$smoke_root/fixtures/static"
fixture_oracle="$smoke_root/fixtures/oracle"
fixture_busy="$smoke_root/fixtures/busy"
fixture_wedge="$smoke_root/fixtures/wedge"
# Port for the sandbox-integrity probe. Negative case: --unshare-net removes
# the sandbox's own loopback, so 127.0.0.1:$probe_port is unreachable and the
# fetch fails fast. Positive case (BETA_M2c): a local python http.server
# binds it on the host loopback and the per-wallpaper network grant (set
# through permissions.set) makes the daemon append --allow-network to the
# worker's argv, so the fetch resolves.
probe_port=$((18080 + ($$ % 2000)))
daemon_pid=""
probe_server_pid=""

cleanup() {
    if [[ -n "$probe_server_pid" ]]; then
        kill "$probe_server_pid" 2>/dev/null || true
        wait "$probe_server_pid" 2>/dev/null || true
    fi
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

# The web lane needs the default stop grace (500 ms) for the worker's bounded
# teardown (close the CDP pipes, reap, escalate to SIGTERM/SIGKILL on the bwrap
# group), so --renderer-stop-grace-ms stays at the daemon default instead of
# the video lane's 80 ms. No address-space or process-ceiling override is
# passed: since M2b the web kind carries its own defaults (131072 MiB for the
# V8-sandbox virtual reservations; 32768 for the kernel RLIMIT_NPROC ceiling
# that otherwise kills the bwrap fork on a desktop session) — this lane
# running without any override IS the proof (docs/BETA_M2.md M2b).
start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 500 \
        --renderer-web-startup-timeout-ms 10000 \
        --renderer-frame-timeout-ms 1000 \
        --renderer-stop-grace-ms 500 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --renderer-web-heartbeat-ms 1000 \
        --renderer-web-heartbeat-max-failures 2 \
        >"$smoke_root/daemon.log" 2>&1 &
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
    for _attempt in {1..900}; do
        output="$(call_daemon renderer.status)"
        if [[ "$(jq -r '.result.phase' <<<"$output")" == "$expected" ]]; then
            printf '%s\n' "$output"
            return
        fi
        sleep 0.05
    done
    echo "timed out waiting for renderer phase $expected" >&2
    printf '%s\n' "$output" >&2
    return 1
}

start_web() {
    local wallpaper_id="$1"
    local content_hash="$2"
    local content="$3"
    local params
    params="$(jq -cn \
        --arg wallpaper_id "$wallpaper_id" \
        --arg content_hash "$content_hash" \
        --arg content "$content" \
        '{wallpaper_id:$wallpaper_id,content_hash:$content_hash,width:160,height:90,fps:30,kind:"web",content:$content}')"
    call_daemon renderer.start "$params"
}

# Pixel probe over the shared frame file via scripts/frame-read.py (the shared
# bounded parser): exits 0 when at least one pixel in the box matches the
# target RGB within the per-channel tolerance, 1 otherwise. The fixtures use a
# dark #101214 palette; the yellow dot (#ffdd00) and the network marker (red,
# painted last) cannot confound the two probes. Tolerances are JPEG-sized:
# the frames are decoded q80 screencasts (see the geometry note above: the
# slot rows duplicate one JPEG row, so a probe box effectively samples its
# columns; the marker is solid red across every row at its columns, and the
# yellow dot reads (255,221,0) at its columns). The decoded interiors run
# roughly (245..255, 5..15, 5..15) for the red block and (245..255,
# 210..230, 0..20) for the yellow dot — a 60/50 tolerance still discriminates
# (the yellow dot's green channel is ~215+ and the dark background's red
# channel is ~16, so neither can match a red target at 60, and the dark
# background cannot match the yellow dot at 50).
probe_frame() {
    local frame_file="$1"
    local x="$2" y="$3" w="$4" h="$5"
    local r="$6" g="$7" b="$8" tol="$9"
    python3 "$project_root/scripts/frame-read.py" "$frame_file" \
        probe "$x" "$y" "$w" "$h" "$r" "$g" "$b" "$tol"
}

# All fixtures share the dark palette and the loopback probe marker: once an
# HTTP fetch to http://127.0.0.1:$2/probe resolves (1.5 s abort timeout; the
# positive-control server answers with Access-Control-Allow-Origin for the
# file:// opaque origin), __kwe_net is set and kwe_marker() paints a red
# block whose FRAME columns are 6..18 -- the probe box (10,10,4,4) sits
# solidly inside them. The block is positioned in VIEWPORT X FRACTIONS of
# the spec frame (x*6/160, w*12/160) and spans the full canvas height, NOT
# fixed pixels, because of how the screencast geometry collapses on this
# stack (measured, not assumed; see kwe_marker() in the template below):
# headless=new ignores --window-size and runs a 500x90 window with a 500x3
# layout viewport; the screencast aspect-fits that surface into
# maxWidth=160/maxHeight=90, which is a 160x1 JPEG (one row averaging all
# three canvas rows), and the worker's bounded slot fill duplicates that
# single row across all 90 frame rows. Consequences: (a) the frame's Y
# axis carries no information at all -- only columns matter; (b) a marker
# painted on a subset of canvas rows is diluted by the row average into a
# dim smear that fails the probe tolerance (measured: a row-0-only marker
# reads back as (85,13,14) instead of red); (c) a fixed-coordinate marker
# is fragile at best and off-canvas at worst. The X-fractional, full-height
# design is invariant to the actual surface size: the block lands at frame
# columns 6..18 and every frame row is red there, for any viewport, which
# is exactly the discrimination the case needs.
# The animated/oracle bodies REPAINT the block on every frame while the
# flag is set, so the marker is persistent once granted: the probe box
# stays red instead of surviving one compositor frame (a fetch that resolves
# while the page idles paints exactly once, and a later frame -- or the
# keepalive re-publication -- would clear it; a one-shot marker makes the
# positive control a capture-race lottery). The marker depends on the fetch,
# NOT on host paths: /etc is ro-bound inside the sandbox, so a page-side
# fetch of /etc/passwd is blocked by Chromium's scheme isolation regardless
# of the network namespace -- only a network-dependent marker discriminates
# the netns. The marker must paint red when the browser shares the network
# namespace (positive control, case 1b) and must never paint under
# --unshare-net (the sandbox's loopback does not exist then; the fetch fails
# fast with an unreachable host, and the abort timeout bounds any
# misbehaving path). fixture-animated also draws the dot on the pointer when
# a mouse event arrives; fixture-oracle idles with a pulsing corner dot (so
# the compositor keeps producing frames) and draws the dot at the pointer
# only after mousedown -- the pointer oracle's baseline probe box is empty
# of yellow before the event and must contain it after.
make_fixture() {
    local dir="$1"
    local probe_port="$2"
    local body="$3"
    mkdir -p "$dir"
    cat >"$dir/index.html" <<HTML
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;padding:0;overflow:hidden;background:#101214}
canvas{display:block}
</style></head><body>
<canvas id="c"></canvas>
<script>
var cv = document.getElementById('c');
var ctx = cv.getContext('2d');
var __kwe_net = 0;
function resize() { cv.width = innerWidth; cv.height = innerHeight; }
resize(); addEventListener('resize', resize);
window.audio_web = function (bands) { window.__kwe_last = bands.length; };
// Network marker in viewport X fractions (see the header comment: the
// screencast maps the whole surface onto the spec frame, so fractional
// placement lands at the same frame columns for any surface size). The
// marker spans the FULL canvas height: measured on this stack the headless
// surface is 500x3 (outerWidth=500 is chromium's minimum headless window,
// innerHeight=3 is a layout quirk), and the screencast aspect-fits the
// surface into maxWidth=160/maxHeight=90 -> a 160x1 JPEG -- a single row
// that is the area-average of all three canvas rows -- which the worker's
// bounded slot fill then duplicates across all 90 frame rows. A marker
// that leaves any canvas row dark at its columns gets its red diluted by
// the row average (measured: rows (208,3,3)/(63,14,16)/(16,18,20) average
// to a dim (85,13,14) that fails the probe tolerance); covering every row
// keeps the average solid red. The block is painted LAST in every frame --
// after the dot -- so the dot can never cover it; #ff0000 is used by
// nothing else in the fixtures.
function kwe_marker() {
  if (!__kwe_net) return;
  ctx.fillStyle = "#ff0000";
  ctx.fillRect(innerWidth * 6 / 160, 0,
               Math.max(1, innerWidth * 12 / 160), cv.height);
}
fetch('http://127.0.0.1:${probe_port}/probe', {signal: AbortSignal.timeout(1500)}).then(function () {
  __kwe_net = 1;
  kwe_marker();
}).catch(function () {});
$body
</script></body></html>
HTML
}

make_fixture "$fixture_animated" "$probe_port" '
var mouseDot = null;
addEventListener("mousedown", function (e) { mouseDot = { x: e.clientX, y: e.clientY, r: 32 }; });
addEventListener("mousemove", function (e) { if (mouseDot) { mouseDot.x = e.clientX; mouseDot.y = e.clientY; } });
function frame(t) {
  ctx.fillStyle = "#101214";
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.fillStyle = "#ffdd00";
  if (mouseDot) {
    ctx.beginPath(); ctx.arc(mouseDot.x, mouseDot.y, mouseDot.r, 0, Math.PI * 2); ctx.fill();
  } else {
    var x = cv.width * (0.5 + 0.45 * Math.sin(t / 300));
    var y = cv.height * (0.5 + 0.4 * Math.cos(t / 230));
    ctx.beginPath(); ctx.arc(x, y, 28, 0, Math.PI * 2); ctx.fill();
  }
  // Persistent network marker: repaint the sandbox-integrity block every
  // frame once the fetch resolved. Painted LAST, so the yellow dot cannot
  // cover it; the marker frame columns 6..18 (full height) are disjoint
  // from the pointer/oracle probe areas.
  kwe_marker();
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);
'

# The late-wedge page paints and animates for ~30 frames (so the canary
# promotes it to live — a timeout-based wedge could fire before the first
# frame on a cold start and turn the case into an ordinary canary failure),
# then wedges its renderer main thread: no more CDP answers, no more frames,
# and the keepalive re-publication keeps the supervisor's frame timeout from
# ever tripping. Without the page-independent heartbeat the dead stream would
# be masked forever (BETA_M2b case 9). The dot MOVES (rather than a static
# center dot) because a static canvas stops the compositor from producing
# new frames, which stops rAF callbacks entirely — the painted counter would
# never cross 30 and the busy loop would never start (the same stale-frame
# quirk that forced the animated fixtures to repaint per frame).
mkdir -p "$fixture_wedge"
cat >"$fixture_wedge/index.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;padding:0;overflow:hidden;background:#101214}
canvas{display:block}
</style></head><body>
<canvas id="c"></canvas>
<script>
var cv = document.getElementById('c');
var ctx = cv.getContext('2d');
cv.width = innerWidth; cv.height = innerHeight;
var painted = 0;
function frame(t) {
  ctx.fillStyle = '#101214';
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.fillStyle = '#ffdd00';
  ctx.beginPath();
  ctx.arc(cv.width * (0.5 + 0.45 * Math.sin(t / 300)), cv.height / 2, 24, 0, Math.PI * 2);
  ctx.fill();
  if (++painted > 30) { setTimeout(function () { while (true) {} }, 0); }
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);
</script></body></html>
HTML

# The static fixture has no animation at all: no screencast frames flow after
# the first paint, and only the keepalive re-publication keeps the sequence
# advancing (BETA_M2b case 2).
make_fixture "$fixture_static" "$probe_port" ''

make_fixture "$fixture_oracle" "$probe_port" '
var dot = null;
addEventListener("mousedown", function (e) { dot = { x: e.clientX, y: e.clientY, r: 32 }; });
addEventListener("mousemove", function (e) { if (dot) { dot.x = e.clientX; dot.y = e.clientY; } });
function frame(t) {
  ctx.fillStyle = "#101214";
  ctx.fillRect(0, 0, cv.width, cv.height);
  ctx.fillStyle = "#ffdd00";
  // Idle: a pulsing corner dot keeps the compositor producing frames without
  // ever entering the probe box (the pointer target sits around the center).
  var pr = 26 + 4 * Math.sin(t / 200);
  ctx.beginPath(); ctx.arc(40, 40, pr, 0, Math.PI * 2); ctx.fill();
  if (dot) {
    ctx.beginPath(); ctx.arc(dot.x, dot.y, dot.r, 0, Math.PI * 2); ctx.fill();
  }
  // Persistent network marker (see make_fixture): the marker frame columns
  // 6..18 (full height) are disjoint from the pointer baseline/probe boxes
  // and from the pulsing corner dot; painted last every frame.
  kwe_marker();
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);
'

# The busy-loop page passes the static preflight but never paints a first
# frame (and its wedged renderer main thread stalls the CDP session request),
# so the worker rejects the backend with exit 73 (BETA_M2b case 7).
mkdir -p "$fixture_busy"
cat >"$fixture_busy/index.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"></head><body><script>while(true){}</script></body></html>
HTML
echo "web smoke: fixtures generated"

command -v jq >/dev/null
command -v python3 >/dev/null

# SKIPPED-exit-0: this lane is meaningless without the sandbox runtime; the
# web renderer's own preflight covers the same prerequisite for the daemon.
if ! command -v chromium >/dev/null || ! command -v bwrap >/dev/null; then
    echo "web smoke skipped: chromium/bwrap not installed"
    exit 0
fi

# The web renderer spawns a whole browser under the desktop session; record
# plasmashell's pid before anything runs and assert it is untouched (and
# alive) after every case. An absent plasmashell (headless CI) records nothing
# and the guard is skipped.
plasma_before="$(pgrep -x plasmashell | head -1 || true)"

cd "$project_root"
cargo build --workspace >/dev/null
start_daemon
call_daemon health >/dev/null

# Case 1: the animated fixture promotes through the canary to live with
# kind/content identity, the sequence advances, no failures, and the sandbox
# holds: the loopback probe marker never paints red. The marker depends on an
# HTTP fetch, so its failure proves the netns isolation (--unshare-net: the
# sandbox's own loopback does not exist, the fetch to
# http://127.0.0.1:$probe_port fails fast) — not Chromium's scheme isolation,
# which would block a /etc/passwd fetch regardless of the netns. The web
# kind's per-lane budgets are asserted from the live status (they are the M2b
# defaults, not passed as overrides).
animated_params='{"wallpaper_id":"web","content_hash":"hash-web","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_animated"'"}'
call_daemon renderer.start "$animated_params" >/dev/null
live_status="$(wait_phase live)"
[[ "$(jq -r '.result.kind' <<<"$live_status")" == "web" ]]
[[ "$(jq -r '.result.content_hash' <<<"$live_status")" == "hash-web" ]]
live_pid="$(jq -r '.result.pid' <<<"$live_status")"
[[ "$(jq -r '.result.resource_limits.address_space_mib' <<<"$live_status")" == "131072" ]]
[[ "$(jq -r '.result.resource_limits.processes' <<<"$live_status")" == "32768" ]]
sequence_first="$(jq -r '.result.sequence' <<<"$live_status")"
sleep 1
sequence_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$sequence_second" -gt "$sequence_first" ]]
[[ "$(jq -r '.result.failures' <<<"$live_status")" == "0" ]]
animated_frame="$(jq -r '.result.frame_file' <<<"$(call_daemon renderer.status)")"
[[ -n "$animated_frame" && -f "$animated_frame" ]]
if probe_frame "$animated_frame" 10 10 4 4 255 0 0 60; then
    echo "sandbox leak: network probe marker painted red" >&2
    exit 1
fi
last_good_file="$(jq -r '.last_good.file' "$state_dir/supervisor-v1.json")"
[[ -s "$state_dir/$last_good_file" ]]
head -c 2 "$state_dir/$last_good_file" | cmp -s - <(printf 'P6')
echo "web smoke passed: canary promote kind=web, sequence advances, sandbox holds, last-good P6"

# Case 1b (grants lane, BETA_M2c): the per-wallpaper network grant is the
# ONLY path to --allow-network — the M2b per-request allow_network test hook
# is removed (its param is now rejected as an unknown field), and the daemon
# runs without --allow-test-faults in this lane. Assert the documented
# defaults through permissions.get first (a wallpaper without a record has
# network off, audio off, pointer on), then grant network for a fresh
# identity (permissions.set patches only the provided field; the answer is
# the new effective record) and start the same animated fixture supervised:
# the marker must paint red through the real grant mechanism. Then revoke
# (network false) and restart the same identity: the marker must stay away
# while the probe server keeps running — connectivity is unchanged, so the
# grant alone is the discriminator. The marker is painted only while the
# page-side __kwe_net flag is set (set by the resolved fetch), so "no red"
# is the observable for "no __kwe_net". The server adds
# Access-Control-Allow-Origin because a file:// page has an opaque ("null")
# origin: a plain python http.server answers the request but the fetch
# would reject on CORS, masking the network result. The marker is
# persistent (repainted every frame once the fetch resolves), so the probe
# hits a solidly red box instead of racing a one-frame repaint; and it runs
# supervised rather than as a direct spawn because the frame file is
# created by the supervisor (SharedFrameWriter create_new) and the daemon
# path is the path the sandbox actually runs under.
python3 - "$probe_port" "$smoke_root" <<'PY' >"$smoke_root/http-server.log" 2>&1 &
import http.server
import sys

port, root = int(sys.argv[1]), sys.argv[2]

class CorsHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=root, **kwargs)

    def end_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        super().end_headers()

    def log_message(self, _format, *_args):
        pass

http.server.HTTPServer(("127.0.0.1", port), CorsHandler).serve_forever()
PY
probe_server_pid=$!
sleep 0.5
kill -0 "$probe_server_pid" || {
    echo "case 1b: probe http server failed to start" >&2
    sed -n '1,40p' "$smoke_root/http-server.log" >&2
    exit 1
}
# No record exists for the case-1 identity, so the effective record is the
# documented default policy: network off, audio off, pointer on.
defaults_status="$(call_daemon permissions.get '{"wallpaper_id":"web"}')"
[[ "$(jq -r '.result.granted.network' <<<"$defaults_status")" == "false" ]]
[[ "$(jq -r '.result.granted.audio' <<<"$defaults_status")" == "false" ]]
[[ "$(jq -r '.result.granted.pointer' <<<"$defaults_status")" == "true" ]]
# Grant network for a fresh identity; the unset fields keep their defaults.
grant_status="$(call_daemon permissions.set '{"wallpaper_id":"web-grant","network":true}')"
[[ "$(jq -r '.result.granted.network' <<<"$grant_status")" == "true" ]]
[[ "$(jq -r '.result.granted.audio' <<<"$grant_status")" == "false" ]]
[[ "$(jq -r '.result.granted.pointer' <<<"$grant_status")" == "true" ]]
granted_params='{"wallpaper_id":"web-grant","content_hash":"hash-web-grant","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_animated"'"}'
call_daemon renderer.start "$granted_params" >/dev/null
granted_status="$(wait_phase live)"
granted_frame="$(jq -r '.result.frame_file' <<<"$granted_status")"
[[ -n "$granted_frame" && -f "$granted_frame" ]]
granted_painted=1
for _attempt in {1..60}; do
    if probe_frame "$granted_frame" 10 10 4 4 255 0 0 60; then
        granted_painted=0
        break
    fi
    sleep 0.25
done
[[ "$granted_painted" == "0" ]]
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
# Revocation takes effect on the next renderer.start: the restarted worker
# must spawn with --unshare-net again. The probe server is still up, so a
# red marker here could only mean the revocation did not reach the argv.
revoke_status="$(call_daemon permissions.set '{"wallpaper_id":"web-grant","network":false}')"
[[ "$(jq -r '.result.granted.network' <<<"$revoke_status")" == "false" ]]
call_daemon renderer.start "$granted_params" >/dev/null
wait_phase live >/dev/null
revoked_clean=1
for _attempt in {1..12}; do
    revoked_frame="$(jq -r '.result.frame_file' <<<"$(call_daemon renderer.status)")"
    if [[ -n "$revoked_frame" && -f "$revoked_frame" ]] \
        && probe_frame "$revoked_frame" 10 10 4 4 255 0 0 60; then
        revoked_clean=0
        break
    fi
    sleep 0.25
done
[[ "$revoked_clean" == "1" ]]
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
kill "$probe_server_pid"
wait "$probe_server_pid" 2>/dev/null || true
probe_server_pid=""
echo "web smoke passed: network grant paints red through the daemon; revocation restores the sandbox"

# Case 2: a static page paints once and then produces no screencast frames;
# the keepalive re-publication must keep the sequence advancing over 1.5 s
# with zero failures and no decode diagnostics (the keepalive path never
# fabricates frames -- it re-publishes the last decoded one).
static_params='{"wallpaper_id":"web-static","content_hash":"hash-web-static","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_static"'"}'
call_daemon renderer.start "$static_params" >/dev/null
static_status="$(wait_phase live)"
static_first="$(jq -r '.result.sequence' <<<"$static_status")"
sleep 1.5
static_second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
[[ "$static_second" -gt "$static_first" ]]
[[ "$(jq -r '.result.failures' <<<"$(call_daemon renderer.status)")" == "0" ]]
static_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$static_tail" != *"decode_failure"* ]]
echo "web smoke passed: static page keepalive advances the sequence, no failures, no decode diagnostics"

# Case 3: pointer oracle. Send enter+move+down at the normalized center; the
# worker dispatches CDP mouse events at the viewport-scaled position and the
# fixture paints its dot there. The baseline probe box must be empty of yellow
# and the post-down box must contain it. The pointer acks round-trip into
# input_ack_sequence.
oracle_params='{"wallpaper_id":"web-oracle","content_hash":"hash-web-oracle","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_oracle"'"}'
call_daemon renderer.start "$oracle_params" >/dev/null
oracle_status="$(wait_phase live)"
oracle_frame="$(jq -r '.result.frame_file' <<<"$oracle_status")"
oracle_generation="$(jq -r '.result.display_generation' <<<"$oracle_status")"
# Dot drawn at normalized (0.5, 0.5) lands at frame (80, 45) regardless of the
# headless viewport: probe a 16x16 box around it (x y w h), JPEG tolerance 50.
if probe_frame "$oracle_frame" 72 37 16 16 255 221 0 50; then
    echo "pointer oracle baseline is not clean" >&2
    exit 1
fi
call_daemon renderer.input \
    "$(jq -cn --argjson g "$oracle_generation" '{generation:$g,phase:"enter",x:0.5,y:0.5}')" >/dev/null
call_daemon renderer.input \
    "$(jq -cn --argjson g "$oracle_generation" '{generation:$g,phase:"move",x:0.5,y:0.5}')" >/dev/null
call_daemon renderer.input \
    "$(jq -cn --argjson g "$oracle_generation" '{generation:$g,phase:"down",x:0.5,y:0.5,button:"primary"}')" >/dev/null
pointer_acked=0
for _attempt in {1..50}; do
    pointer_acked="$(jq -r '.result.input_ack_sequence' <<<"$(call_daemon renderer.status)")"
    [[ "$pointer_acked" != "0" ]] && break
    sleep 0.1
done
[[ "$pointer_acked" != "0" ]]
# The worker acks on decode but dispatches the CDP event on the next tick, and
# the page paints on the following compositor frame: retry the probe until the
# dot lands (bounded; a clean baseline was already proven above).
oracle_painted=1
for _attempt in {1..20}; do
    if probe_frame "$oracle_frame" 72 37 16 16 255 221 0 50; then
        oracle_painted=0
        break
    fi
    sleep 0.25
done
[[ "$oracle_painted" == "0" ]]
echo "web smoke passed: pointer oracle paints the dot at the normalized position, acked"

# Case 4: audio injection without --audio-capture. Forward 64-band frames via
# the direct audio.forward daemon-call. BETA_M2c: delivery is gated by the
# per-wallpaper audio grant — without it frames are dropped silently
# (latest-wins, counted in audio_grant_dropped, bounded-rate log) and the
# worker never acks; with the grant the worker acks each frame with the wire
# sequence (the display generation). The case asserts both sides: the
# ungranted drop first (ack sequence unmoved, drop counter advanced), then a
# live grant — no worker restart — and acks advancing to the live generation
# with zero protocol errors and no evaluate diagnostics. Runs on a fresh
# worker so the ack counter starts at 0.
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
audio_params='{"wallpaper_id":"web-audio","content_hash":"hash-web-audio","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_animated"'"}'
call_daemon renderer.start "$audio_params" >/dev/null
audio_status="$(wait_phase live)"
audio_generation="$(jq -r '.result.display_generation' <<<"$audio_status")"
[[ "$audio_generation" != "0" ]]
ack_before="$(jq -r '.result.input_ack_sequence' <<<"$audio_status")"
dropped_before="$(jq -r '.result.audio_grant_dropped' <<<"$audio_status")"
# No audio grant yet: the daemon drops the frames before they reach the
# worker pipe, so the acks cannot move and the drop counter must advance.
for _i in 1 2 3; do
    call_daemon audio.forward \
        "$(jq -cn --argjson g "$audio_generation" \
            '{generation:$g,frame:{left:[range(64)|0.5],right:[range(64)|0.25]}}')" >/dev/null
    sleep 0.3
done
dropped_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.input_ack_sequence' <<<"$dropped_status")" == "$ack_before" ]]
[[ "$(jq -r '.result.audio_grant_dropped' <<<"$dropped_status")" -gt "$dropped_before" ]]
# Grant audio: delivery resumes immediately (no restart) and the worker acks
# each forwarded frame with the wire sequence (the display generation).
call_daemon permissions.set '{"wallpaper_id":"web-audio","audio":true}' >/dev/null
for _i in 1 2 3; do
    call_daemon audio.forward \
        "$(jq -cn --argjson g "$audio_generation" \
            '{generation:$g,frame:{left:[range(64)|0.5],right:[range(64)|0.25]}}')" >/dev/null
    sleep 0.3
done
audio_after="$(jq -r '.result.input_ack_sequence' <<<"$(call_daemon renderer.status)")"
[[ "$audio_after" != "$ack_before" ]]
[[ "$audio_after" == "$audio_generation" ]]
[[ "$(jq -r '.result.input_protocol_errors' <<<"$(call_daemon renderer.status)")" == "0" ]]
audio_tail="$(jq -r '.result.stderr_tail | join("\n")' <<<"$(call_daemon renderer.status)")"
[[ "$audio_tail" != *"audio_evaluate_error"* ]]
echo "web smoke passed: audio grant gates delivery (dropped without, acks advance with), zero protocol errors"

# Case 5: kill -9 the active worker; the daemon records one failure during the
# restart window and auto-restarts. The last-good still image survives the
# kill, and the first successful canary promotes the new worker (failures
# reset by promotion -- no quarantine on a single failure).
kill_pid="$(jq -r '.result.pid' <<<"$(call_daemon renderer.status)")"
[[ "$kill_pid" != "null" ]]
kill -9 "$kill_pid"
# The restarting phase itself lasts only the restart delay (~20 ms), too short
# to poll reliably; the failure record is the durable signal — it appears on
# the next tick and survives until the restart's promotion clears it.
failed_status=""
for _attempt in {1..200}; do
    status="$(call_daemon renderer.status)"
    if [[ "$(jq -r '.result.failures' <<<"$status")" == "1" ]]; then
        failed_status="$status"
        break
    fi
    sleep 0.02
done
[[ -n "$failed_status" ]]
[[ "$(jq -r '.result.last_failure' <<<"$failed_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$failed_status")" == *"signal_9"* ]]
[[ "$(jq -r '.result.last_good_file' <<<"$failed_status")" != "null" ]]
kill_restart_status="$(wait_phase live)"
[[ "$(jq -r '.result.failures' <<<"$kill_restart_status")" == "0" ]]
[[ "$(jq -r '.result.pid' <<<"$kill_restart_status")" != "$kill_pid" ]]
echo "web smoke passed: kill -9 recorded once with last-good preserved, auto-restarted, not quarantined"

# Case 6: a missing content root is rejected before any worker spawns
# (invalid_params naming the preflight reason).
missing_params='{"wallpaper_id":"web-missing","content_hash":"hash-web-missing","width":160,"height":90,"fps":30,"kind":"web","content":"/nonexistent/kwe-m2b-web"}'
if call_daemon renderer.start "$missing_params" >/dev/null 2>&1; then
    echo "missing content root was accepted" >&2
    exit 1
fi
echo "web smoke passed: missing content root rejected with invalid_params"

# Case 7: the busy-loop page passes the static preflight but the worker
# rejects the backend (exit 73, CDP bootstrap deadlock); the active base
# worker stays live and rolled_back names exit_code_73.
base_pid="$(jq -r '.result.pid' <<<"$(call_daemon renderer.status)")"
busy_params='{"wallpaper_id":"web-busy","content_hash":"hash-web-busy","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_busy"'"}'
call_daemon renderer.start "$busy_params" >/dev/null
rollback_status="$(wait_phase rolled_back)"
[[ "$(jq -r '.result.pid' <<<"$rollback_status")" == "$base_pid" ]]
[[ "$(jq -r '.result.last_failure' <<<"$rollback_status")" == "process_exit" ]]
[[ "$(jq -r '.result.last_failure_detail' <<<"$rollback_status")" == *"exit_code_73"* ]]
kill -0 "$base_pid"
echo "web smoke passed: busy-loop content -> worker exit 73 -> rolled_back with exit_code_73"

# Case 8: repeated kill -9s with no intervening success hit the three-failure
# budget -> quarantined, and renderer.start for the same identity is refused
# with the quarantine phase (mirrors smoke-video case 6).
# Start a fresh worker first: case 7's busy-loop identity is quarantined
# (the status "failures" field reads the *requested* identity's record, and
# after the rollback the requested identity is still the busy one, so its
# leftover 3 failures would short-circuit the loop). A fresh promotion
# clears the animated identity's record, so the budget starts at 0.
call_daemon renderer.start "$animated_params" >/dev/null
fresh_status="$(wait_phase live)"
[[ "$(jq -r '.result.failures' <<<"$fresh_status")" == "0" ]]
# The kill must land during the bootstrap window (candidate_pid non-null):
# a kill of the promoted live worker restarts cleanly and promotion clears
# the failure record, so the budget would never accumulate (the "restarting"
# phase itself lasts only the ~20 ms restart delay and is not a reliable
# poll target). Killing each fresh candidate before it can promote lets the
# record persist: failures 1 -> 2 -> 3 -> quarantine.
for _attempt in {1..8}; do
    target=""
    for _poll in {1..100}; do
        status="$(call_daemon renderer.status)"
        target="$(jq -r '.result.candidate_pid // .result.pid // empty' <<<"$status")"
        [[ -n "$target" ]] && break
        sleep 0.02
    done
    if [[ -z "$target" ]]; then
        echo "case 8: timed out waiting for a kill target" >&2
        exit 1
    fi
    kill -9 "$target" 2>/dev/null || true
    for _poll in {1..250}; do
        status="$(call_daemon renderer.status)"
        failures="$(jq -r '.result.failures // 0' <<<"$status")"
        phase="$(jq -r '.result.phase' <<<"$status")"
        [[ "$failures" -ge "$_attempt" ]] && break
        [[ "$phase" == "quarantined" ]] && break
        sleep 0.02
    done
    if [[ "$failures" -ge "$_attempt" ]]; then
        echo "  case 8: kill #$_attempt (pid $target) recorded, failures=$failures phase=$phase"
        [[ "$failures" -ge 3 ]] && break
    elif [[ "$phase" == "quarantined" ]]; then
        echo "  case 8: kill #$_attempt (pid $target) -> quarantined"
        break
    else
        echo "case 8: kill #$_attempt (pid $target) never recorded a failure (phase=$phase, failures=$failures)" >&2
        exit 1
    fi
done
quarantined_status="$(wait_phase quarantined)"
[[ "$(jq -r '.result.failures' <<<"$quarantined_status")" == "3" ]]
[[ "$(jq -r '.result.pid' <<<"$quarantined_status")" == "null" ]]
refused_status="$(call_daemon renderer.start "$animated_params")"
[[ "$(jq -r '.result.phase' <<<"$refused_status")" == "quarantined" ]]
[[ "$(jq -r '.result.pid' <<<"$refused_status")" == "null" ]]
echo "web smoke passed: three failures quarantine and refuse the identity"

# Case 9: the late-wedge page promotes to live (the canary sees ~30 frames of
# animation), then wedges its renderer main thread. Screencast acks stop
# answering, no new frames flow, and the keepalive re-publication keeps the
# supervisor's frame timeout from ever tripping — without the heartbeat the
# dead stream would be masked forever. The session-scoped probe (smoke
# override: 1000 ms interval, max 2) must time out twice and the worker exits
# 73 with the heartbeat diagnostic; the daemon records it (the dead worker's
# stderr tail rides into last_failure_detail) and restarts. The wedge repeats
# on every restart — but the daemon's failure budget is pre-promotion by
# design (a promotion clears the record: a worker that reached live is
# trusted, so post-promotion exits restart cleanly rather than quarantining —
# see case 8's note), so the case asserts the dead stream is NEVER masked:
# repeated exit-73 restarts observed across consecutive cycles, not
# quarantine.
wedge_params='{"wallpaper_id":"web-wedge","content_hash":"hash-web-wedge","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_wedge"'"}'
call_daemon renderer.start "$wedge_params" >/dev/null
wait_phase live >/dev/null
wedge_failed=""
for _attempt in {1..400}; do
    status="$(call_daemon renderer.status)"
    if [[ "$(jq -r '.result.failures // 0' <<<"$status")" -ge 1 ]] \
        && [[ "$(jq -r '.result.last_failure_detail' <<<"$status")" == *"exit_code_73"* ]]; then
        wedge_failed="$status"
        break
    fi
    sleep 0.05
done
[[ -n "$wedge_failed" ]]
[[ "$(jq -r '.result.last_failure' <<<"$wedge_failed")" == "process_exit" ]]
# The heartbeat diagnostic is folded into the failure detail with the dead
# worker's stderr ring tail.
[[ "$(jq -r '.result.last_failure_detail' <<<"$wedge_failed")" == *"heartbeat_failed"* ]]
# Each restart re-promotes and clears the record (failures returns to 0), so
# count the 0 -> 1 transitions carrying the exit-73 detail: one per wedge
# cycle. Three consecutive cycles prove the heartbeat catches the wedge
# every time — the dead stream is never masked.
wedge_exits_73=0
prev_failures=0
for _attempt in {1..60}; do
    status="$(call_daemon renderer.status)"
    failures="$(jq -r '.result.failures // 0' <<<"$status")"
    detail="$(jq -r '.result.last_failure_detail // ""' <<<"$status")"
    if [[ "$prev_failures" == "0" && "$failures" -ge 1 && "$detail" == *"exit_code_73"* ]]; then
        wedge_exits_73=$((wedge_exits_73 + 1))
    fi
    prev_failures="$failures"
    [[ "$wedge_exits_73" -ge 3 ]] && break
    sleep 0.5
done
[[ "$wedge_exits_73" -ge 3 ]]
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
echo "web smoke passed: wedged page caught by the heartbeat -> repeated exit-73 restarts, never masked"

# Final stop: the daemon stops cleanly and stays healthy; plasmashell's pid is
# untouched and alive (the browser sandbox never reached the live session).
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
call_daemon health >/dev/null
plasma_after="$(pgrep -x plasmashell | head -1 || true)"
if [[ -n "$plasma_before" ]]; then
    [[ "$plasma_after" == "$plasma_before" ]]
    kill -0 "$plasma_before"
    echo "web smoke passed: plasmashell pid unchanged (${plasma_before})"
else
    echo "web smoke passed: no plasmashell running, session guard skipped"
fi
echo "all web smoke cases passed"
