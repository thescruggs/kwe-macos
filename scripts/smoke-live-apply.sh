#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# smoke-live-apply.sh: DESTRUCTIVE live wallpaper-apply smoke (BETA_M4d).
#
# Runs the real M4a apply transaction against the LIVE Plasma session on
# THIS machine (maintainer-authorized for BETA_M4; AGENTS.md's no-live-session
# rule is waived). Unlike smoke-apply.sh (read-only error cases), this suite
# actually switches the desktop: it applies a synthetic video wallpaper, a
# synthetic web wallpaper, and a hostile scene, then restores, and asserts
# every step is reversible and never disturbs plasmashell.
#
# Design:
#   * The smoke daemon binds the REAL runtime socket
#     ($XDG_RUNTIME_DIR/kwe/daemon-v1.sock) — the socket the installed
#     org.kde.kwe.wallpaper plugin's DisplaySession polls — so frames from
#     the smoke's renderer actually reach the live desktop. The system
#     kwe-daemon.service is stopped first (if running) and restarted by the
#     exit trap. This mirrors the real deployment exactly: the daemon the
#     plugin connects to is the daemon that applied the wallpaper.
#   * The wallpaper plugin/config is captured BEFORE anything changes with
#     the documented read-only evaluateScript probe (never desktopForScreen —
#     the known SIGSEGV hazard) and restored on EVERY exit path (trap), so
#     the desktop ends the run exactly as it began.
#   * "Frames reach the desktop" is proven by the plasmashell process holding
#     the smoke renderer's frame file open (/proc/<plasmashell-pid>/fd)
#     together with the frame sequence advancing.
#   * plasmashell is NEVER restarted or killed; the smoke asserts its PID is
#     identical before and after every destructive step.
#   * Fixtures are synthetic and generated at runtime (ffmpeg video, a
#     self-contained HTML page, and a structurally hostile scene.json) —
#     never Workshop content.
#
# Gated behind KWE_RUN_LIVE_APPLY_SMOKE=1 (the repo's KWE_RUN_*_SMOKE opt-in
# pattern; check.sh runs it under that env var). Without it: SKIPPED, exit 0.
#
# BETA_M4d decision (documented in docs/BETA_M4.md): smoke-apply.sh's
# KWE_LIVE_APPLY=1 lane stays the READ-ONLY live lane (enumeration +
# fail-closed error cases); this script is the destructive live lane. They
# are intentionally separate suites.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"

if [[ "${KWE_RUN_LIVE_APPLY_SMOKE:-0}" != "1" ]]; then
    echo "SKIPPED: smoke-live-apply.sh needs KWE_RUN_LIVE_APPLY_SMOKE=1;"
    echo "        it is a DESTRUCTIVE live-session smoke (BETA_M4d)"
    exit 0
fi

command -v jq >/dev/null
command -v ffmpeg >/dev/null
command -v qdbus6 >/dev/null
command -v systemctl >/dev/null
command -v ss >/dev/null

if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
    echo "FAILED: XDG_RUNTIME_DIR is not set; the live smoke needs the runtime socket" >&2
    exit 1
fi

live_socket="${XDG_RUNTIME_DIR}/kwe/daemon-v1.sock"

# The daemon's identity rule (supervisor.rs validate_identity_part): 1..=128
# ASCII letters/digits/'.'/'_'/'-'. Every interpolated plugin/config-group
# value in an evaluateScript payload must match it.
identity_part_ok() {
    [[ "$1" =~ ^[A-Za-z0-9._-]{1,128}$ ]]
}
smoke_root="$(mktemp -d -t kwe-live-apply-smoke.XXXXXX)"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
steam_root="$smoke_root/steam"

# --- Pre-test capture (idempotent; runs before ANY live change) ------------
# The desktop is in the state we find it: this is what must be restored.
plasmashell_pids=( $(pgrep -x plasmashell 2>/dev/null || true) )
if (( ${#plasmashell_pids[@]} != 1 )); then
    echo "FAILED: expected exactly one plasmashell process, found ${#plasmashell_pids[@]}" >&2
    exit 1
fi
plasmashell_pid="${plasmashell_pids[0]}"

system_daemon_was_running=0
if [[ "$(systemctl --user is-active kwe-daemon 2>/dev/null || true)" == "active" ]]; then
    system_daemon_was_running=1
fi
# Whether the smoke has taken over the live socket / stopped the system
# service; the trap only removes the socket once it owns it (never while the
# system daemon still serves it).
socket_taken=0
system_daemon_stopped=0

# Pre-flight (SIGKILL recovery): a live socket bound by a kwe-daemon that is
# NOT the system service means a previous smoke run was SIGKILLed before its
# trap ran, leaving an orphaned daemon holding the real socket. Starting the
# system service would then fail EADDRINUSE, and a re-run's capture would
# see the service inactive and never restart it. Fail closed with the
# recovery steps instead of silently displacing the orphan.
if [[ -S "$live_socket" && "$system_daemon_was_running" == "0" ]]; then
    owner="$(ss -xlp 2>/dev/null | grep -F "$live_socket" \
        | sed -n 's/.*users:(("\([^"]*\)",pid=\([0-9]*\),fd=[0-9]*)).*/\1 \2/p' | head -1)"
    if [[ -n "$owner" ]]; then
        owner_name="${owner%% *}"
        owner_pid="${owner##* }"
        if [[ "$owner_name" == "kwe-daemon" ]] && kill -0 "$owner_pid" 2>/dev/null; then
            echo "FAILED: the real socket $live_socket is held by an orphaned smoke daemon" >&2
            echo "        (a previous smoke-live-apply.sh run was SIGKILLed before its trap ran)." >&2
            echo "        Recovery: kill $owner_pid, then: systemctl --user start kwe-daemon" >&2
            exit 1
        fi
    fi
fi

# Read-only probe template (the documented M4a enumeration template, fixed;
# never desktopForScreen). Read back via print() as the shell's only output.
probe_script() {
    local connector="$1"
    local script
    script=$(cat <<'EOF'
var d = desktops();
var out = [];
for (var i = 0; i < d.length; i++) {
  var image = null;
  var wp = d[i].wallpaperPlugin;
  if (/^[A-Za-z0-9._-]+$/.test(wp)) {
    var g = d[i].currentConfigGroup;
    try {
      d[i].currentConfigGroup = ["Wallpaper", wp, "General"];
      image = d[i].readConfig("Image");
    } catch (e) { }
    d[i].currentConfigGroup = g;
  }
  out.push({index: i, id: d[i].id, screen: d[i].screen, wp: wp, image: image, group: g});
}
EOF
)
    script+=$'\n'"var c = {\"${connector}\": screenForConnector(\"${connector}\")};"$'\n'
    script+=$'print(JSON.stringify({desktops: out, connectors: c}));'
    printf '%s' "$script"
}

run_probe() {
    local connector="$1"
    local script
    script="$(probe_script "$connector")"
    # Bounded: the shell may be wedged; never poll it forever.
    timeout 10 qdbus6 org.kde.plasmashell /PlasmaShell evaluateScript "$script"
}

# Target output = the first enabled and connected connector (kscreen-doctor,
# ANSI-stripped). Connector names are identity-validated before interpolation.
target_output="$(kscreen-doctor -o 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | awk '
    function emit(o, e, c) { if (o != "" && e == 1 && c == 1) { print o; exit } }
    /^Output: / { emit(out, en, co); out = $3; en = 0; co = 0; next }
    /^[[:space:]]*enabled[[:space:]]*$/ { en = 1; next }
    /^[[:space:]]*connected[[:space:]]*$/ { co = 1; next }
    END { emit(out, en, co) }
')"
if [[ -z "$target_output" || ! "$target_output" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "FAILED: could not find an enabled, connected output (got '$target_output')" >&2
    exit 1
fi

pretest_probe="$(run_probe "$target_output")"
target_screen="$(jq -r --arg out "$target_output" '.connectors[$out]' <<<"$pretest_probe")"
if [[ "$target_screen" == "null" || "$target_screen" == "-1" ]]; then
    echo "FAILED: output $target_output has no live desktop containment (screen='$target_screen')" >&2
    exit 1
fi
pretest_desktop_index="$(jq -r --argjson screen "$target_screen" \
    '.desktops[] | select(.screen == $screen) | .index' <<<"$pretest_probe" | head -1)"
pretest_plugin="$(jq -r --argjson screen "$target_screen" \
    '.desktops[] | select(.screen == $screen) | .wp' <<<"$pretest_probe" | head -1)"
pretest_image="$(jq -r --argjson screen "$target_screen" \
    '.desktops[] | select(.screen == $screen) | .image // ""' <<<"$pretest_probe" | head -1)"
# The desktop's actual config group (the daemon's probe captures it the same
# way); restore replays these members, never a hardcoded shape. Falls back to
# the synthesized wallpaper group when the probe reports none.
pretest_group_json="$(jq -c --argjson screen "$target_screen" \
    '.desktops[] | select(.screen == $screen) | .group' <<<"$pretest_probe" | head -1)"
if [[ "$pretest_group_json" == "null" || -z "$pretest_group_json" ]]; then
    pretest_group_json="$(jq -cn --arg plugin "$pretest_plugin" '["Wallpaper", $plugin, "General"]')"
fi
identity_part_ok "$pretest_plugin" || {
    echo "FAILED: target desktop has no usable wallpaper plugin ('$pretest_plugin')" >&2
    exit 1
}
while IFS= read -r _member; do
    identity_part_ok "$_member" || {
        echo "FAILED: captured config-group member is not a valid identity part: '$_member'" >&2
        exit 1
    }
done <<<"$(jq -r '.[]' <<<"$pretest_group_json")"

echo "live smoke: target output $target_output (desktop index $pretest_desktop_index,"
echo "            pre-test plugin $pretest_plugin, image '${pretest_image:-none}')"
echo "live smoke: plasmashell PID $plasmashell_pid; system kwe-daemon $([ "$system_daemon_was_running" == 1 ] && echo running || echo stopped)"

# --- Trap: restore the EXACT pre-test state on EVERY exit path -------------
# Idempotent and bounded: safe to re-run after a mid-run crash (a fresh
# capture then reflects the interrupted state, and the restore replays the
# same script). plasmashell is never touched; only the wallpaper plugin and
# the daemon service are reverted.
build_restore_script() {
    # Pure builder mirroring apply.rs restore_script: the plugin and every
    # config-group member are identity parts (validated); the Image value is
    # JSON-escaped via jq (the daemon's six-character escape set); wallpaper
    # content never reaches the script.
    local member literal="" image_clause="" image_literal
    while IFS= read -r member; do
        identity_part_ok "$member" || return 1
        literal="${literal:+$literal, }\"$member\""
    done <<<"$(jq -r '.[]' <<<"$pretest_group_json")"
    if [[ -n "${pretest_image:-}" ]]; then
        image_literal="$(jq -n --arg v "$pretest_image" '$v')"
        image_clause="d.writeConfig(\"Image\", $image_literal);"$'\n'
    fi
    printf 'var d = desktops()[%s];\nif (!d) throw "no desktop %s";\nd.currentConfigGroup = [%s];\n%s d.wallpaperPlugin = "%s";' \
        "$pretest_desktop_index" "$pretest_desktop_index" "$literal" "$image_clause" "$pretest_plugin"
}

restore_live_wallpaper() {
    # Rebuild the exact pre-test wallpaper config, then verify with a fresh
    # probe instead of trusting evaluateScript's exit 0. Best-effort and
    # bounded: the trap must never hang on a wedged shell.
    local script post post_plugin post_image post_group
    script="$(build_restore_script)" || {
        echo "WARNING: could not build the pre-test restore script" >&2
        return 1
    }
    if ! timeout 10 qdbus6 org.kde.plasmashell /PlasmaShell evaluateScript "$script" >/dev/null 2>&1; then
        echo "WARNING: the pre-test wallpaper restore evaluateScript failed" >&2
        return 1
    fi
    post="$(run_probe "$target_output" 2>/dev/null || true)"
    if [[ -z "$post" ]]; then
        echo "WARNING: could not re-probe the desktop to verify the restore" >&2
        return 1
    fi
    post_plugin="$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .wp' <<<"$post" | head -1)"
    post_image="$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .image // ""' <<<"$post" | head -1)"
    post_group="$(jq -c --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .group' <<<"$post" | head -1)"
    if [[ "$post_plugin" != "$pretest_plugin" \
        || "$post_image" != "${pretest_image:-}" \
        || "$post_group" != "$pretest_group_json" ]]; then
        echo "WARNING: pre-test wallpaper restore verification failed:" >&2
        echo "        plugin $post_plugin (want $pretest_plugin); image '$post_image'" >&2
        echo "        (want '${pretest_image:-}'); group $post_group (want $pretest_group_json)" >&2
        return 1
    fi
    echo "live smoke: pre-test wallpaper restored and verified (plugin $post_plugin, group $post_group)"
    return 0
}

cleanup() {
    # Stop the smoke daemon first so the plugin no longer polls it, then put
    # back the pre-test wallpaper and the system daemon service.
    if [[ -n "${smoke_daemon_pid:-}" ]]; then
        kill "$smoke_daemon_pid" 2>/dev/null || true
        wait "$smoke_daemon_pid" 2>/dev/null || true
        smoke_daemon_pid=""
    fi
    # Remove the socket only once the smoke owns it (or the system service
    # was stopped): never unlink the live system daemon's socket on a failure
    # that happened before the takeover.
    if [[ "$socket_taken" == "1" || "$system_daemon_stopped" == "1" ]]; then
        rm -f -- "$live_socket"
    fi
    if [[ "${pretest_captured:-0}" == "1" ]]; then
        restore_live_wallpaper || true
    fi
    if [[ "${system_daemon_was_running:-0}" == "1" ]] \
        && [[ "$(systemctl --user is-active kwe-daemon 2>/dev/null || true)" != "active" ]]; then
        systemctl --user start kwe-daemon >/dev/null 2>&1 || true
        # Bounded wait; a start that stays inactive needs manual recovery.
        for _attempt in {1..50}; do
            [[ "$(systemctl --user is-active kwe-daemon 2>/dev/null || true)" == "active" ]] && break
            sleep 0.1
        done
        if [[ "$(systemctl --user is-active kwe-daemon 2>/dev/null || true)" != "active" ]]; then
            echo "WARNING: kwe-daemon.service did not come back up." >&2
            echo "WARNING: recovery: systemctl --user start kwe-daemon" >&2
        fi
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM
pretest_captured=1

# --- Helpers -----------------------------------------------------------------
fail() {
    echo "FAILED: $1" >&2
    if [[ -n "${smoke_daemon_pid:-}" ]]; then
        echo "--- smoke daemon log tail ---" >&2
        sed -n '1,160p' "$smoke_root/daemon.log" >&2
    fi
    exit 1
}

call_daemon() {
    local method="$1"
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    "$target_dir/debug/kwe" daemon-call --socket "$live_socket" --method "$method" --params "$params"
}

assert_pid_unchanged() {
    local label="$1"
    local -a now
    now=( $(pgrep -x plasmashell 2>/dev/null || true) )
    if (( ${#now[@]} != 1 )); then
        fail "plasmashell process count changed after $label (was exactly 1 at pid $plasmashell_pid, now ${#now[@]}: ${now[*]:-none})"
    fi
    [[ "${now[0]}" == "$plasmashell_pid" ]] \
        || fail "plasmashell PID changed after $label (was $plasmashell_pid, now ${now[0]})"
    kill -0 "$plasmashell_pid" 2>/dev/null || fail "plasmashell $plasmashell_pid is no longer alive after $label"
}

wait_for_plugin_frame() {
    # The live plugin (inside plasmashell) holds the renderer's frame file
    # open: proof the frames actually reach the desktop.
    local pid="$1" frame_file="$2" label="$3"
    local base
    base="$(basename -- "$frame_file")"
    for _attempt in {1..200}; do
        if ls -l "/proc/$pid/fd" 2>/dev/null | grep -qF -- "$base"; then
            echo "live smoke: plasmashell is consuming frames from $base ($label)"
            return 0
        fi
        sleep 0.05
    done
    echo "timed out waiting for plasmashell ($pid) to open frame file $base ($label)" >&2
    return 1
}

assert_frames_flowing() {
    # The frame sequence advances: the renderer is publishing, not frozen.
    local label="$1"
    local first second
    first="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
    sleep 0.5
    second="$(jq -r '.result.sequence' <<<"$(call_daemon renderer.status)")"
    [[ -n "$first" && -n "$second" && "$second" -gt "$first" ]] \
        || fail "$label frames are not advancing ($first -> $second)"
}

# --- Fixture steam root (synthetic, runtime-generated; never Workshop) ------
mkdir -p "$steam_root/steamapps/workshop/content/431960/1"
mkdir -p "$steam_root/steamapps/workshop/content/431960/2"
mkdir -p "$steam_root/steamapps/workshop/content/431960/3"
echo '"LibraryFolders" { }' >"$steam_root/steamapps/libraryfolders.vdf"
cat >"$steam_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "1" "1" "2" "1" "3" "1" } }
EOF

# Video item 1: a flat #3366CC mp4 (the M1e oracle color) at the apply target.
# 30 s so the clip cannot end mid-assertion on a slow machine (a solid color
# compresses tiny, so the file stays small).
video_content="$steam_root/steamapps/workshop/content/431960/1/fixture.mp4"
cat >"$steam_root/steamapps/workshop/content/431960/1/project.json" <<'EOF'
{"title":"Synthetic Live Video","type":"video","file":"fixture.mp4","tags":[]}
EOF
ffmpeg -loglevel error -f lavfi -i "color=c=0x3366CC:s=320x180:r=30" -t 30 \
    -c:v libx264 -pix_fmt yuv420p "$video_content" -y

# Web item 2: a self-contained animated page (no network) over a solid base.
web_content="$steam_root/steamapps/workshop/content/431960/2"
cat >"$steam_root/steamapps/workshop/content/431960/2/project.json" <<'EOF'
{"title":"Synthetic Live Web","type":"web","tags":[]}
EOF
cat >"$web_content/index.html" <<'HTML'
<!DOCTYPE html>
<html><head><style>body{margin:0;background:#2266AA}</style></head>
<body><canvas id="c" width="320" height="180"></canvas>
<script>
  var c = document.getElementById('c');
  var ctx = c.getContext('2d');
  var x = 0;
  function paint() {
    ctx.fillStyle = '#2266AA'; ctx.fillRect(0, 0, 320, 180);
    ctx.fillStyle = '#FF8800'; ctx.fillRect(x % 280, 10, 20, 20);
    x++;
    requestAnimationFrame(paint);
  }
  requestAnimationFrame(paint);
</script></body></html>
HTML

# Bad scene item 3: passes preflight (a JSON object root) but the renderer
# rejects it structurally ("general" must be an object) and exits 73 before
# any frame — the hostile-content containment case.
bad_content="$steam_root/steamapps/workshop/content/431960/3/scene.json"
cat >"$steam_root/steamapps/workshop/content/431960/3/project.json" <<'EOF'
{"title":"Synthetic Hostile Scene","type":"scene","file":"scene.json","tags":[]}
EOF
echo '{"general": 42}' >"$bad_content"

cd "$project_root"
cargo build --workspace >/dev/null

# --- Take the live socket: stop the system daemon, run the smoke daemon -----
if [[ "$system_daemon_was_running" == "1" ]]; then
    systemctl --user stop kwe-daemon
    system_daemon_stopped=1
    for _attempt in {1..100}; do
        [[ -S "$live_socket" ]] || break
        sleep 0.05
    done
fi
mkdir -p "$(dirname -- "$live_socket")"
chmod 700 "$(dirname -- "$live_socket")" 2>/dev/null || true
rm -f -- "$live_socket"

start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$live_socket" \
        --steam-root "$steam_root" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 3000 \
        --renderer-video-startup-timeout-ms 8000 \
        --renderer-web-startup-timeout-ms 10000 \
        --renderer-frame-timeout-ms 1500 \
        --renderer-stop-grace-ms 500 \
        --renderer-restart-delay-ms 100 \
        --renderer-canary-ms 300 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --apply-promotion-timeout-ms 20000 \
        --apply-probe-timeout-ms 5000 >"$smoke_root/daemon.log" 2>&1 &
    smoke_daemon_pid=$!
    for _attempt in {1..200}; do
        if [[ -S "$live_socket" ]]; then
            socket_taken=1
            return 0
        fi
        kill -0 "$smoke_daemon_pid" 2>/dev/null || {
            echo "smoke daemon exited during startup" >&2
            sed -n '1,120p' "$smoke_root/daemon.log" >&2
            return 1
        }
        sleep 0.05
    done
    echo "smoke daemon socket did not appear at $live_socket" >&2
    sed -n '1,120p' "$smoke_root/daemon.log" >&2
    return 1
}
start_daemon
call_daemon health >/dev/null
echo "live smoke: smoke daemon bound the real socket $live_socket"

# --- Case 1: VIDEO apply (destructive, live) --------------------------------
apply_params="$(jq -cn --arg out "$target_output" --arg content "$video_content" \
    '{output:$out,wallpaper_id:"1",kind:"video",content:$content,width:320,height:180,fps:30}')"
video_result="$(call_daemon wallpaper.apply "$apply_params" || true)"
[[ "$(jq -r '.ok' <<<"$video_result")" == "true" ]] \
    || fail "video apply failed: $(jq -c . <<<"$video_result")"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r --arg out "$target_output" '.result.outputs[$out].wallpaper_id' <<<"$assignments")" == "1" ]] \
    || fail "assignment did not persist wallpaper 1 on $target_output"
[[ "$(jq -r --arg out "$target_output" '.result.outputs[$out].kind' <<<"$assignments")" == "video" ]] \
    || fail "assignment kind must be video"
live_probe="$(run_probe "$target_output")"
[[ "$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .wp' <<<"$live_probe")" == "org.kde.kwe.wallpaper" ]] \
    || fail "desktop plugin must be org.kde.kwe.wallpaper after video apply"
status="$(call_daemon renderer.status)"
frame_file="$(jq -r '.result.frame_file' <<<"$status")"
[[ -n "$frame_file" && -f "$frame_file" ]] || fail "video apply produced no frame file"
wait_for_plugin_frame "$plasmashell_pid" "$frame_file" "video apply" \
    || fail "plasmashell did not open the video frame file"
assert_frames_flowing "video"
python3 "$project_root/scripts/frame-read.py" "$frame_file" probe 160 90 1 1 51 102 204 6 \
    || fail "video frame pixel does not match the #3366CC fixture"
assert_pid_unchanged "video apply"
echo "live smoke: case 1 video apply passed (live frames on the desktop)"

# --- Case 2: WEB apply (destructive, live) ----------------------------------
apply_params="$(jq -cn --arg out "$target_output" --arg content "$web_content" \
    '{output:$out,wallpaper_id:"2",kind:"web",content:$content,width:320,height:180,fps:30}')"
web_result="$(call_daemon wallpaper.apply "$apply_params" || true)"
[[ "$(jq -r '.ok' <<<"$web_result")" == "true" ]] \
    || fail "web apply failed: $(jq -c . <<<"$web_result")"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r --arg out "$target_output" '.result.outputs[$out].wallpaper_id' <<<"$assignments")" == "2" ]] \
    || fail "assignment did not persist wallpaper 2 on $target_output"
live_probe="$(run_probe "$target_output")"
[[ "$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .wp' <<<"$live_probe")" == "org.kde.kwe.wallpaper" ]] \
    || fail "desktop plugin must be org.kde.kwe.wallpaper after web apply"
status="$(call_daemon renderer.status)"
frame_file="$(jq -r '.result.frame_file' <<<"$status")"
[[ -n "$frame_file" && -f "$frame_file" ]] || fail "web apply produced no frame file"
wait_for_plugin_frame "$plasmashell_pid" "$frame_file" "web apply" \
    || fail "plasmashell did not open the web frame file"
assert_frames_flowing "web"
assert_pid_unchanged "web apply"
echo "live smoke: case 2 web apply passed (live frames on the desktop)"

# --- Case 3: BAD SCENE apply (hostile content containment) ------------------
apply_params="$(jq -cn --arg out "$target_output" --arg content "$bad_content" \
    '{output:$out,wallpaper_id:"3",kind:"scene",content:$content,width:320,height:180,fps:30}')"
bad_result="$(call_daemon wallpaper.apply "$apply_params" || true)"
[[ "$(jq -r '.ok' <<<"$bad_result")" == "false" ]] \
    || fail "the hostile scene apply must fail, got: $(jq -c . <<<"$bad_result")"
[[ "$(jq -r '.result.error' <<<"$bad_result")" == "apply_failed" ]] \
    || fail "the hostile scene failure must map to apply_failed, got $(jq -r '.result.error' <<<"$bad_result")"
# Containment: the desktop is still on the kwe plugin, the renderer is not
# live/hung, the previous (web) assignment survived the rollback, and the
# shell never crashed.
live_probe="$(run_probe "$target_output")"
[[ "$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .wp' <<<"$live_probe")" == "org.kde.kwe.wallpaper" ]] \
    || fail "desktop plugin must remain org.kde.kwe.wallpaper after a failed apply"
renderer_phase="$(jq -r '.result.phase' <<<"$(call_daemon renderer.status)")"
[[ "$renderer_phase" != "live" && "$renderer_phase" != "awaiting_ack" ]] \
    || fail "the hostile renderer must not be live (phase $renderer_phase)"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r --arg out "$target_output" '.result.outputs[$out].wallpaper_id' <<<"$assignments")" == "2" ]] \
    || fail "rollback must preserve the previous (web) assignment after the failed apply"
assert_pid_unchanged "bad scene apply"
echo "live smoke: case 3 hostile scene containment passed (apply_failed, desktop operable)"

# --- Case 4: RESTORE (reversible) -------------------------------------------
restore_result="$(call_daemon wallpaper.restore "{\"output\":\"$target_output\"}")"
[[ "$(jq -r '.ok' <<<"$restore_result")" == "true" ]] \
    || fail "wallpaper.restore failed: $(jq -c . <<<"$restore_result")"
[[ "$(jq -r '.result.mode' <<<"$restore_result")" == "assignment" ]] \
    || fail "restore must run in assignment mode, got $(jq -r '.result.mode' <<<"$restore_result")"
[[ "$(jq -r '.result.restored.wallpaper_plugin' <<<"$restore_result")" == "$pretest_plugin" ]] \
    || fail "restore returned plugin $(jq -r '.result.restored.wallpaper_plugin' <<<"$restore_result"), expected $pretest_plugin"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r --arg out "$target_output" '.result.outputs[$out]' <<<"$assignments")" == "null" ]] \
    || fail "assignment store must be cleared after restore"
final_probe="$(run_probe "$target_output")"
final_plugin="$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .wp' <<<"$final_probe" | head -1)"
final_image="$(jq -r --argjson screen "$target_screen" '.desktops[] | select(.screen == $screen) | .image // ""' <<<"$final_probe" | head -1)"
[[ "$final_plugin" == "$pretest_plugin" ]] || fail "final plugin $final_plugin != pre-test $pretest_plugin"
[[ "$final_image" == "$pretest_image" ]] || fail "final image '$final_image' != pre-test '$pretest_image'"
assert_pid_unchanged "restore"
echo "live smoke: case 4 restore passed (pre-test plugin/config back, store cleared)"

# --- Case 5: end state ------------------------------------------------------
[[ "$(pgrep -x plasmashell | head -1)" == "$plasmashell_pid" ]] \
    || fail "plasmashell PID changed across the whole run"
echo "live smoke: case 5 end state passed — plasmashell PID $plasmashell_pid unchanged throughout"
echo "live smoke: pre-test probe: $pretest_probe"
echo "live smoke: final  probe:  $final_probe"
echo "all live apply smoke cases passed"
