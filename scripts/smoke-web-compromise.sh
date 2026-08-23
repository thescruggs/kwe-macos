#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Sandbox-compromise smoke suite (BETA_M2d). A runtime-generated fixture
# page attempts four sandbox escapes in order — (1) a network fetch to a
# host STALL listener (a scratch loopback port whose listener accepts every
# connection and never answers: deterministic on any machine, unlike an
# external address), (2) a cors-mode fetch of a HOST canary file that is
# not bound into the namespace plus a traversal XHR to the same file through
# /wallpaper/../.. (BETA B6: the renderer runs chromium with
# --allow-file-access-from-files, so file: reads are no longer blocked by
# the browser — the BOUND set is the boundary), (3) a cors-mode fetch of
# file:///wallpaper/index.html proving the content root is readable (the
# control for attempt 2: reads work, so its rejections are isolation) and
# reachable, (4) localStorage and userAgent reads — and paints one
# color-coded result box per attempt. The suite runs the fixture through the
# daemon pipeline twice (Scenario A: default grants, network off; Scenario
# B: network grant set through permissions.set), asserting the painted boxes
# with the frame oracle (scripts/frame-read.py) AND the actual sandbox argv:
# /proc/<pid>/cmdline of the supervised worker (the daemon's grant->argv
# --allow-network contract) and of its bwrap child (--unshare-net presence).
# SKIPPED (exit 0) when chromium or bwrap is missing, so machines without
# the sandbox runtime can still run the other acceptance lanes.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-web-compromise.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
fixture="$smoke_root/fixture"
stall_port_file="$smoke_root/stall.port"
daemon_pid=""
stall_pid=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ -n "$stall_pid" ]]; then
        kill "$stall_pid" 2>/dev/null || true
        wait "$stall_pid" 2>/dev/null || true
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

# Same web-lane daemon timings as smoke-web.sh (the default 500 ms stop
# grace; the web kind's own address-space/process-ceiling defaults).
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

# Pixel probe over the shared frame file via scripts/frame-read.py (the
# shared bounded parser): exits 0 when at least one pixel in the box matches
# the target RGB within the per-channel tolerance.
probe_frame() {
    local frame_file="$1"
    local x="$2" y="$3" w="$4" h="$5"
    local r="$6" g="$7" b="$8" tol="$9"
    python3 "$project_root/scripts/frame-read.py" "$frame_file" \
        probe "$x" "$y" "$w" "$h" "$r" "$g" "$b" "$tol"
}

# The compromise fixture (runtime-generated, never committed). Each of the
# four attempts paints one result box, in viewport X fractions spanning the
# FULL canvas height — the M2b screencast-geometry lesson (smoke-web.sh):
# headless=new runs a 500x3 layout viewport, the screencast aspect-fits it
# into the 160x90 spec frame as a 160x1 JPEG, and the slot fill duplicates
# that single row, so only columns carry information. X fractions map
# linearly to frame columns: the four boxes land at frame columns
# 0-40/40-80/80-120/120-160, each solid across every row. Colors:
# GREEN #00c000 = the sandbox held (expected), ORANGE #ff8c00 = the network
# attempt left the sandbox (the positive control fires), RED #ff0000 = the
# attempt escaped the sandbox (the suite fails), PENDING #303030 = not
# settled yet. The fixture paints as the attempts settle and repaints once
# more after 2 s (attempt 1 aborts at 1.5 s), so the last compositor frame
# carries the final state; the suite polls the frame file with bounded
# retries.
# A host-side stall listener on a scratch loopback port: it ACCEPTS every
# connection and never answers. This is the deterministic positive control
# for the network grant — no dependence on external addresses or the host
# route table. The fixture fetch targets 127.0.0.1:$STALL_PORT; scenario A
# (--unshare-net) has no loopback inside the namespace, so the connect
# fails fast, while scenario B (shared host netns) connects to the listener
# and waits for an answer that never comes (abort at the 1.5 s bound).
start_stall_listener() {
    cat >"$smoke_root/stall_listener.py" <<'PY'
import socket
import sys

port_file = sys.argv[1]
listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(16)
with open(port_file, "w") as handle:
    handle.write(str(listener.getsockname()[1]) + "\n")
    handle.flush()
while True:
    conn, _ = listener.accept()
    try:
        while True:
            if not conn.recv(65536):
                break
    except OSError:
        pass
PY
    python3 "$smoke_root/stall_listener.py" "$stall_port_file" >"$smoke_root/stall.log" 2>&1 &
    stall_pid=$!
    for _attempt in {1..100}; do
        [[ -s "$stall_port_file" ]] && return
        kill -0 "$stall_pid" 2>/dev/null || {
            echo "stall listener exited during startup" >&2
            sed -n '1,40p' "$smoke_root/stall.log" >&2
            return 1
        }
        sleep 0.05
    done
    echo "stall listener port did not appear" >&2
    return 1
}

make_fixture() {
    mkdir -p "$fixture"
    # The host canary for attempt 2: a real, readable file on the host that
    # is NOT bound into the sandbox. A resolution from inside is a genuine
    # host-file read through a hole in the namespace.
    echo "kwe-host-canary $$" >"$smoke_root/host-canary.txt"
    cat >"$fixture/index.html" <<HTML
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;padding:0;overflow:hidden;background:#101214}
canvas{display:block}
</style></head><body>
<canvas id="c"></canvas>
<script>
var cv = document.getElementById('c');
var ctx = cv.getContext('2d');
function resize() { cv.width = innerWidth; cv.height = innerHeight; }
resize(); addEventListener('resize', resize);
var GREEN = '#00c000', ORANGE = '#ff8c00', RED = '#ff0000', PENDING = '#303030';
var states = [PENDING, PENDING, PENDING, PENDING];
function paintBoxes() {
  ctx.fillStyle = '#101214';
  ctx.fillRect(0, 0, cv.width, cv.height);
  for (var i = 0; i < 4; i++) {
    ctx.fillStyle = states[i];
    ctx.fillRect(i * innerWidth / 4, 0, innerWidth / 4, cv.height);
  }
}
function settle(i, color) { states[i] = color; paintBoxes(); }
// Attempt 1 (network): fetch to the host's STALL listener (scratch
// loopback port $STALL_PORT — accepts every connection, never answers).
// Scenario A (--unshare-net) has no loopback inside the namespace: the
// connect fails fast with a TypeError in ~10 ms -> green. Scenario B
// shares the host netns: the connect succeeds (the listener accepted) and
// the response never comes — the fetch aborts at the 1.5 s bound: the
// positive control, deterministic on any machine. The abort reason is the
// AbortSignal.timeout() DOMException, named 'TimeoutError' (measured on
// chromium 151 through the daemon pipeline; 'AbortError' is accepted too
// for engines that reject with the generic name). The elapsed guard is
// belt-and-braces — the signal timeout is the only abort source, so the
// abort cannot fire before the 1.5 s bound. A resolved response would
// mean the listener answered — or a full network compromise (red).
var netStart = performance.now();
fetch('http://127.0.0.1:$STALL_PORT/', {signal: AbortSignal.timeout(1500)}).then(
  function () { settle(0, RED); },
  function (e) {
    settle(0, ((e.name === 'TimeoutError' || e.name === 'AbortError')
      && performance.now() - netStart >= 1000) ? ORANGE : GREEN);
  });
// Attempt 2 (host-file reads): cors-mode fetch of a host canary file that
// exists on the host ($smoke_root/host-canary.txt) but is NOT bound into
// the namespace, and a traversal XHR to the same file through
// /wallpaper/../.. (normalizes to the host path). Both must FAIL. Since
// BETA B6 chromium runs with --allow-file-access-from-files — file: reads
// are NOT blocked by the browser any more (attempt 3 proves it) — so a
// resolution here is a real read of a host file through the namespace:
// red. (/etc/passwd is deliberately not the target: /etc is ro-bound and
// IS readable by wallpaper JS under B6; narrowing that bind is the
// recorded follow-up.)
var fsDone = 0;
function fsSettle(color) {
  if (++fsDone >= 2) settle(1, color);
}
fetch('file://$smoke_root/host-canary.txt').then(
  function (r) { if (r.ok) { settle(1, RED); } else { fsSettle(GREEN); } },
  function () { fsSettle(GREEN); });
try {
  var xhr = new XMLHttpRequest();
  xhr.open('GET', 'file:///wallpaper/../..$smoke_root/host-canary.txt', true);
  xhr.onload = function () { if (xhr.status === 200 && xhr.responseText.indexOf('kwe-host-canary') === 0) { settle(1, RED); } else { fsSettle(GREEN); } };
  xhr.onerror = function () { fsSettle(GREEN); };
  xhr.send();
} catch (e) { fsSettle(GREEN); }
// Attempt 3 (content-root reachability, the attempt-2 control): a
// cors-mode fetch of the page's own file must RESOLVE with its bytes —
// B6 made same-directory reads legal, and this is exactly what WebGL
// textures and XHR-loaded assets in real wallpapers need. A rejection here
// means the flag is missing (and attempt 2's greens would be meaningless).
fetch('file:///wallpaper/index.html').then(
  function (r) { return r.text(); }).then(
  function (t) { settle(2, t.indexOf('kwe-compromise') >= 0 ? GREEN : RED); },
  function () { settle(2, RED); });
// Attempt 4 (allowed reads): localStorage (per-profile, inside the tmpfs)
// and the user agent string must keep working.
try {
  localStorage.setItem('kwe-compromise', '1');
  if (localStorage.getItem('kwe-compromise') === '1' && navigator.userAgent.length > 0) {
    settle(3, GREEN);
  } else {
    settle(3, RED);
  }
} catch (e) { settle(3, RED); }
paintBoxes();
// A final repaint after everything settled (attempt 1 aborts at 1.5 s).
setTimeout(paintBoxes, 2000);
</script></body></html>
HTML
}

# The argv-level proof: renderer.status's pid is the supervised worker
# (kwe-web-renderer), whose only child is the bwrap process. The worker
# argv carries --allow-network exactly when the daemon's grant record
# allows network; the bwrap argv carries --unshare-net exactly when it
# does not. Both are read from /proc/<pid>/cmdline (NUL-separated).
worker_argv() {
    local worker_pid="$1"
    tr '\0' ' ' <"/proc/$worker_pid/cmdline"
}
bwrap_argv() {
    local worker_pid="$1"
    local bwrap_pid="" pid="" cmd=""
    for _attempt in {1..200}; do
        for pid in $(pgrep -P "$worker_pid" 2>/dev/null || true); do
            cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
            if [[ "$cmd" == bwrap* ]]; then
                bwrap_pid="$pid"
                break
            fi
        done
        [[ -n "$bwrap_pid" ]] && break
        sleep 0.05
    done
    [[ -n "$bwrap_pid" ]]
    tr '\0' ' ' <"/proc/$bwrap_pid/cmdline"
}

# Box probes on the 160x90 spec frame: box i occupies frame columns
# i*40..(i+1)*40 (full height), probe the center. JPEG q80 decoded colors:
# green (0,192,0) reads roughly (0..40, 150..255, 0..40), orange (255,140,0)
# reads (240..255, 100..180, 0..40) — a 60 tolerance discriminates both
# from each other and from the dark background (16,18,20).
GREEN_R=0 GREEN_G=192 GREEN_B=0
ORANGE_R=255 ORANGE_G=140 ORANGE_B=0
TOL=60
BOX_Y=20 BOX_W=4 BOX_H=50

probe_box() {
    local frame_file="$1" x="$2" r="$3" g="$4" b="$5"
    probe_frame "$frame_file" "$x" "$BOX_Y" "$BOX_W" "$BOX_H" "$r" "$g" "$b" "$TOL"
}

# Wait until all four boxes show the expected colors on the worker's frame
# file. The fixture paints as the attempts settle (attempt 1 aborts at
# 1.5 s in scenario B), and the earliest captured frames may be blank or
# partial — poll with bounded retries, then fail loudly on a miss.
wait_boxes() {
    local frame_file="$1"
    shift
    local expected_r="$1" expected_g="$2" expected_b="$3"
    for _attempt in {1..80}; do
        if probe_box "$frame_file" 20 "$expected_r" "$expected_g" "$expected_b" \
            && probe_box "$frame_file" 60 "$GREEN_R" "$GREEN_G" "$GREEN_B" \
            && probe_box "$frame_file" 100 "$GREEN_R" "$GREEN_G" "$GREEN_B" \
            && probe_box "$frame_file" 140 "$GREEN_R" "$GREEN_G" "$GREEN_B"; then
            return 0
        fi
        sleep 0.25
    done
    echo "timed out waiting for expected boxes on $frame_file" >&2
    return 1
}

command -v jq >/dev/null
command -v python3 >/dev/null

# SKIPPED-exit-0: this lane is meaningless without the sandbox runtime.
if ! command -v chromium >/dev/null || ! command -v bwrap >/dev/null; then
    echo "web compromise smoke skipped: chromium/bwrap not installed"
    exit 0
fi

# The fixture fetch targets the stall listener's port, so the listener must
# be up (and its port known) before the fixture is generated.
start_stall_listener
STALL_PORT="$(cat "$stall_port_file")"
make_fixture
echo "web compromise smoke: fixtures generated (stall listener on 127.0.0.1:$STALL_PORT)"

# The suite spawns a whole browser under the desktop session; record
# plasmashell's pid before anything runs and assert it is untouched (and
# alive) afterwards. An absent plasmashell (headless CI) records nothing and
# the guard is skipped.
plasma_before="$(pgrep -x plasmashell | head -1 || true)"

cd "$project_root"
cargo build --workspace >/dev/null
start_daemon
call_daemon health >/dev/null

# Scenario A: default grants — the fixture's attempt 1 must be blocked fast
# (box 1 green), the host-file reads must fail (box 2 green), the content
# root must stay reachable (box 3 green) and the allowed reads must work
# (box 4 green). The argv proof: the worker runs WITHOUT --allow-network
# and its bwrap child runs WITH --unshare-net.
defaults_status="$(call_daemon permissions.get '{"wallpaper_id":"web-comp-a"}')"
[[ "$(jq -r '.result.granted.network' <<<"$defaults_status")" == "false" ]]
call_daemon renderer.start \
    "$(jq -cn --arg content "$fixture" '{wallpaper_id:"web-comp-a",content_hash:"hash-web-comp-a",width:160,height:90,fps:30,kind:"web",content:$content}')" \
    >/dev/null
scenario_a="$(wait_phase live)"
a_pid="$(jq -r '.result.pid' <<<"$scenario_a")"
[[ "$a_pid" != "null" ]]
a_worker_argv="$(worker_argv "$a_pid")"
a_bwrap_argv="$(bwrap_argv "$a_pid")"
[[ "$a_worker_argv" != *"--allow-network"* ]]
[[ "$a_bwrap_argv" == *"--unshare-net"* ]]
a_frame="$(jq -r '.result.frame_file' <<<"$scenario_a")"
[[ -n "$a_frame" && -f "$a_frame" ]]
wait_boxes "$a_frame" "$GREEN_R" "$GREEN_G" "$GREEN_B"
echo "web compromise smoke passed: scenario A boxes 1-4 green; worker argv without --allow-network, bwrap argv with --unshare-net"

# Scenario B: the network grant (permissions.set, patch semantics — the
# answer is the new effective record) is the ONLY path to a network-enabled
# sandbox. Attempt 1 must now show the positive control (box 1 orange: the
# fetch connected to the host's stall listener and hung until the 1.5 s
# abort — the netns is shared, deterministically), the isolation attempts
# must still fail (boxes 2-4 green), and the bwrap argv must carry NO
# --unshare-net while the worker argv carries --allow-network.
grant_status="$(call_daemon permissions.set '{"wallpaper_id":"web-comp-b","network":true}')"
[[ "$(jq -r '.result.granted.network' <<<"$grant_status")" == "true" ]]
call_daemon renderer.start \
    "$(jq -cn --arg content "$fixture" '{wallpaper_id:"web-comp-b",content_hash:"hash-web-comp-b",width:160,height:90,fps:30,kind:"web",content:$content}')" \
    >/dev/null
scenario_b="$(wait_phase live)"
b_pid="$(jq -r '.result.pid' <<<"$scenario_b")"
[[ "$b_pid" != "null" ]]
b_worker_argv="$(worker_argv "$b_pid")"
b_bwrap_argv="$(bwrap_argv "$b_pid")"
[[ "$b_worker_argv" == *"--allow-network"* ]]
[[ "$b_bwrap_argv" != *"--unshare-net"* ]]
b_frame="$(jq -r '.result.frame_file' <<<"$scenario_b")"
[[ -n "$b_frame" && -f "$b_frame" ]]
wait_boxes "$b_frame" "$ORANGE_R" "$ORANGE_G" "$ORANGE_B"
echo "web compromise smoke passed: scenario B box 1 orange (positive control), boxes 2-4 green; worker argv with --allow-network, bwrap argv without --unshare-net"

# Final stop: the daemon stops cleanly and stays healthy; plasmashell's pid
# is untouched and alive (the browser sandbox never reached the live
# session).
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
call_daemon health >/dev/null
plasma_after="$(pgrep -x plasmashell | head -1 || true)"
if [[ -n "$plasma_before" ]]; then
    [[ "$plasma_after" == "$plasma_before" ]]
    kill -0 "$plasma_before"
    echo "web compromise smoke passed: plasmashell pid unchanged (${plasma_before})"
else
    echo "web compromise smoke passed: no plasmashell running, session guard skipped"
fi
echo "all web compromise smoke cases passed"
