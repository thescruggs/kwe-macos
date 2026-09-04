#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Web wallpaper on macOS, end to end and offscreen: kwe-daemon supervises
# kwe-web-renderer, which runs Google Chrome / Chromium headless under
# sandbox-exec, drives it over the CDP pipe, and publishes frames. Runs the
# lane twice — with the Seatbelt profile (default) and with
# KWE_WEB_SANDBOX=off — and reports both, so a CI log answers the open
# "does (deny network*) break the browser's own IPC?" question directly.
# Exit status: 0 when the sandboxed lane passes; 2 when only the
# unsandboxed lane passes (sandbox profile needs work); 1 when neither does.
#
#   scripts/macos/smoke-web-macos.sh
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
case "$(uname -s)" in Darwin) ;; *) echo "macOS only" >&2; exit 2;; esac
cargo build -q -p kwe-daemon -p kwe-web-renderer -p kwe-cli

smoke_root="$(mktemp -d -t kwe-web-smoke)"
fixture="$smoke_root/fixture"
mkdir -p "$fixture"
cat >"$fixture/index.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;padding:0;overflow:hidden;background:#101214}canvas{display:block}
</style></head><body><canvas id="c"></canvas><script>
var cv=document.getElementById('c'),ctx=cv.getContext('2d'),t=0;
function resize(){cv.width=innerWidth;cv.height=innerHeight;}
resize();addEventListener('resize',resize);
(function frame(){t++;ctx.fillStyle='hsl('+(t*7%360)+',80%,50%)';ctx.fillRect(0,0,cv.width,cv.height);requestAnimationFrame(frame);})();
</script></body></html>
HTML

daemon_pid=""
cleanup() {
  if [[ -n "$daemon_pid" ]]; then kill "$daemon_pid" 2>/dev/null || true; wait "$daemon_pid" 2>/dev/null || true; fi
  rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

run_lane() {
  local label="$1"; shift
  local socket="$smoke_root/$label.sock"
  local log="$smoke_root/$label-daemon.log"
  env "$@" target/debug/kwe-daemon \
    --socket "$socket" \
    --renderer-runtime-dir "$smoke_root/$label-runtime" \
    --state-dir "$smoke_root/$label-state" \
    --renderer-web-startup-timeout-ms 30000 \
    --renderer-frame-timeout-ms 5000 \
    --renderer-canary-ms 150 \
    --renderer-handoff-timeout-ms 5000 \
    --renderer-max-failures 1 >"$log" 2>&1 &
  daemon_pid=$!
  for _ in $(seq 1 100); do [[ -S "$socket" ]] && break; sleep 0.05; done
  local call="target/debug/kwe daemon-call --socket $socket"
  $call --method renderer.start --params "{\"wallpaper_id\":\"web-$label\",\"content_hash\":\"web-$label\",\"width\":160,\"height\":90,\"fps\":30,\"kind\":\"web\",\"content\":\"$fixture\"}" >/dev/null
  local phase="" status=""
  for _ in $(seq 1 400); do
    status="$($call --method renderer.status)"
    phase="$(jq -r '.result.phase' <<<"$status")"
    [[ "$phase" == "live" || "$phase" == "quarantined" || "$phase" == "stopped" ]] && break
    sleep 0.1
  done
  local result=1
  if [[ "$phase" == "live" ]]; then
    local s1 s2
    s1="$(jq -r '.result.sequence' <<<"$status")"; sleep 1
    s2="$($call --method renderer.status | jq -r '.result.sequence')"
    if (( s2 > s1 )); then result=0; fi
    echo "web[$label]: phase=live sequence $s1 -> $s2"
  else
    echo "web[$label]: phase=$phase"
    jq -r '.result | "  last_failure=\(.last_failure) detail=\(.last_failure_detail)\n  stderr_tail:\n" + ((.stderr_tail // []) | map("    " + .) | join("\n"))' <<<"$status" || true
    tail -n 20 "$log" | sed 's/^/  daemon: /' || true
  fi
  $call --method renderer.stop >/dev/null 2>&1 || true
  kill "$daemon_pid" 2>/dev/null || true; wait "$daemon_pid" 2>/dev/null || true; daemon_pid=""
  return $result
}

sandboxed=1; bare=1
run_lane sandboxed KWE_WEB_SANDBOX=on && sandboxed=0 || true
run_lane bare KWE_WEB_SANDBOX=off && bare=0 || true
echo "summary: sandbox-exec lane $([[ $sandboxed == 0 ]] && echo PASS || echo FAIL); unsandboxed lane $([[ $bare == 0 ]] && echo PASS || echo FAIL)"
if [[ $sandboxed == 0 ]]; then exit 0; fi
if [[ $bare == 0 ]]; then exit 2; fi
exit 1
