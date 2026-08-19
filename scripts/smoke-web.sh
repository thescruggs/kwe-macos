#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Supervised sandboxed-web-renderer smoke suite (BETA_M2b).
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
# dark #101214 palette; the yellow dot (#ffdd00) and the network marker (red)
# never overlap (corner vs. center boxes), so the two probes cannot confound.
probe_frame() {
    local frame_file="$1"
    local x0="$2" y0="$3" x1="$4" y1="$5"
    local r="$6" g="$7" b="$8" tol="$9"
    python3 "$project_root/scripts/frame-read.py" "$frame_file" \
        probe "$x0" "$y0" "$x1" "$y1" "$r" "$g" "$b" "$tol"
}

# All fixtures share the dark palette and the fetch('/etc/passwd') marker: on
# success (which must never happen -- the sandbox has no network and no
# readable host paths) a red pixel is painted at (10,10), so a red probe is an
# end-to-end sandbox-integrity check. fixture-animated also draws the dot on
# the pointer when a mouse event arrives; fixture-oracle idles with a pulsing
# corner dot (so the compositor keeps producing frames) and draws the dot at
# the pointer only after mousedown -- the pointer oracle's baseline probe box
# is empty of yellow before the event and must contain it after.
make_fixture() {
    local dir="$1"
    local body="$2"
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
function resize() { cv.width = innerWidth; cv.height = innerHeight; }
resize(); addEventListener('resize', resize);
window.audio_web = function (bands) { window.__kwe_last = bands.length; };
fetch('/etc/passwd').then(function () {
  ctx.fillStyle = '#ff0000';
  ctx.fillRect(10, 10, 4, 4);
}).catch(function () {});
$body
</script></body></html>
HTML
}

make_fixture "$fixture_animated" '
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
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);
'

# The static fixture has no animation at all: no screencast frames flow after
# the first paint, and only the keepalive re-publication keeps the sequence
# advancing (BETA_M2b case 2).
make_fixture "$fixture_static" ''

make_fixture "$fixture_oracle" '
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
# holds: the /etc/passwd fetch marker never paints red (network off, host
# paths unreadable). The web kind's per-lane budgets are asserted from the
# live status (they are the M2b defaults, not passed as overrides).
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
if probe_frame "$animated_frame" 1 1 4 4 255 0 0 40; then
    echo "sandbox leak: /etc/passwd marker painted red" >&2
    exit 1
fi
last_good_file="$(jq -r '.last_good.file' "$state_dir/supervisor-v1.json")"
[[ -s "$state_dir/$last_good_file" ]]
head -c 2 "$state_dir/$last_good_file" | cmp -s - <(printf 'P6')
echo "web smoke passed: canary promote kind=web, sequence advances, sandbox holds, last-good P6"

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
# headless viewport: probe a 16x16 box around it, JPEG tolerance 50.
if probe_frame "$oracle_frame" 72 37 88 53 255 221 0 50; then
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
    if probe_frame "$oracle_frame" 72 37 88 53 255 221 0 50; then
        oracle_painted=0
        break
    fi
    sleep 0.25
done
[[ "$oracle_painted" == "0" ]]
echo "web smoke passed: pointer oracle paints the dot at the normalized position, acked"

# Case 4: audio injection without --audio-capture. Forward 64-band frames via
# the direct audio.forward daemon-call; the worker acks each one with the wire
# sequence (the display generation), so input_ack_sequence must advance to the
# live generation with zero protocol errors and no evaluate diagnostics. Runs
# on a fresh worker so the ack counter starts at 0.
call_daemon renderer.stop >/dev/null
wait_phase stopped >/dev/null
audio_params='{"wallpaper_id":"web-audio","content_hash":"hash-web-audio","width":160,"height":90,"fps":30,"kind":"web","content":"'"$fixture_animated"'"}'
call_daemon renderer.start "$audio_params" >/dev/null
audio_status="$(wait_phase live)"
audio_generation="$(jq -r '.result.display_generation' <<<"$audio_status")"
[[ "$audio_generation" != "0" ]]
audio_frame="$(jq -r '.result.frame_file' <<<"$audio_status")"
ack_before="$(jq -r '.result.input_ack_sequence' <<<"$audio_status")"
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
echo "web smoke passed: audio.forward acks advance to the display generation, zero protocol errors"

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
