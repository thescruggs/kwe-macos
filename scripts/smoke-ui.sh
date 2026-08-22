#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
manager="$project_root/build/cmake/apps/kwe-manager/kwe-manager"
daemon_bin="$project_root/target/debug/kwe-daemon"
smoke_root="$(mktemp -d -t kwe-smoke.XXXXXX)"
daemon_pids=()

cleanup() {
    # Kill every smoke daemon, in any order; `wait` is fine for the ones this
    # script started directly and harmless for the ones the stub started.
    for pid in "${daemon_pids[@]:-}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf -- "$smoke_root"
}
# TERM/INT must stop the run, not resume it: a trap that continues executing
# after cleanup would leave a smoke daemon holding the pipeline open.
trap 'cleanup; exit 143' TERM INT
trap cleanup EXIT

cd "$project_root"

# Qt routes qWarning/QML diagnostics to journald when it is available, which
# is exactly how a UI full of broken bindings passed this smoke: the messages
# never reached the log this script reads. Force them to stderr and capture
# them per case.
export QT_FORCE_STDERR_LOGGING=1

# The UI smoke used to check only "Ready + item count", so a QML error that
# killed every binding (0.1.0-alpha.1 shipped with the C++ client types
# unregistered: the whole apply lane was dead) still passed. A QML diagnostic
# in the manager's output now fails the smoke. This is a backstop, not the
# primary gate: the offscreen platform never exposes the window, so bindings
# that only evaluate on a real render stay quiet here. scripts/qml-typecheck.sh
# is what actually catches an unregistered or unimported type.
assert_no_qml_errors() {
    local case_name="$1" log="$2"
    local hits
    hits="$(grep -nE "ReferenceError|TypeError|is not a type|Unable to assign|Cannot assign" \
        "$log" || true)"
    if [[ -n "$hits" ]]; then
        echo "smoke-ui: QML errors in the $case_name case (every such binding is dead):" >&2
        echo "$hits" >&2
        exit 1
    fi
}

# Case 1: daemon down at manager start. The manager must activate the user
# daemon through its injectable activation command. The stub plays the role
# of `systemctl --user start kwe-daemon` WITHOUT touching the user's real
# unit: it starts the smoke daemon on the smoke socket and records its pid.
# The manager never sees the socket when it launches, so reaching Ready
# within the smoke window proves the activation path worked.
socket_path="$smoke_root/case1/daemon.sock"
mkdir -p "$(dirname "$socket_path")"
activation_stub="$smoke_root/case1/activate-daemon.sh"
cat > "$activation_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
# Detach the daemon's stdio: the daemon outlives this stub, and any pipe it
# keeps open would hold the manager's activation channel (and with it the
# smoke pipeline) open past the manager's exit. The smoke script exports
# KWE_DAEMON_SOCKET (the manager's contract), KWE_DAEMON_LOG and
# KWE_DAEMON_PIDFILE.
"${KWE_DAEMON_BIN:?}" --socket "${KWE_DAEMON_SOCKET:?}" \
    </dev/null >>"${KWE_DAEMON_LOG:?}" 2>&1 &
echo $! > "${KWE_DAEMON_PIDFILE:?}"
STUB
chmod +x "$activation_stub"
export KWE_DAEMON_BIN="$daemon_bin"
export KWE_DAEMON_LOG="$smoke_root/case1/daemon.log"
export KWE_DAEMON_PIDFILE="$smoke_root/case1/daemon.pid"
manager_exit=0
manager_log="$smoke_root/case1/manager.log"
"$manager" --platform offscreen --socket "$socket_path" --smoke-test-ms 3000 \
    --daemon-activation-command "$activation_stub" >"$manager_log" 2>&1 || manager_exit=$?
cat "$manager_log"
# Capture the pid right away so the cleanup trap covers every failure path
# below, not just the happy one.
if [[ -f "$smoke_root/case1/daemon.pid" ]]; then
    daemon_pids+=("$(cat "$smoke_root/case1/daemon.pid")")
fi
unset KWE_DAEMON_BIN KWE_DAEMON_LOG KWE_DAEMON_PIDFILE
if [[ "$manager_exit" -ne 0 ]]; then
    echo "smoke-ui: manager failed in daemon-down case (exit $manager_exit)" >&2
    exit "$manager_exit"
fi
assert_no_qml_errors "daemon-down" "$manager_log"
[[ -S "$socket_path" ]] || {
    echo "smoke-ui: manager did not activate the daemon (no socket)" >&2
    exit 1
}
kill -0 "${daemon_pids[-1]}" 2>/dev/null || {
    echo "smoke-ui: activated daemon is not alive" >&2
    exit 1
}

# Case 2: daemon pre-running at manager start (original case). Its stdio is
# redirected to a log for the same reason as the stub's daemon: it outlives
# the script and must not hold the script's stdout/stderr open.
socket_path="$smoke_root/case2/daemon.sock"
mkdir -p "$(dirname "$socket_path")"
"$daemon_bin" --socket "$socket_path" </dev/null >>"$smoke_root/case2/daemon.log" 2>&1 &
daemon_pids+=("$!")
for _attempt in {1..50}; do
    [[ -S "$socket_path" ]] && break
    kill -0 "${daemon_pids[-1]}" 2>/dev/null || {
        wait "${daemon_pids[-1]}"
        exit 1
    }
    sleep 0.05
done

manager_log="$smoke_root/case2/manager.log"
"$manager" --platform offscreen --socket "$socket_path" --smoke-test-ms 3000 \
    >"$manager_log" 2>&1
cat "$manager_log"
assert_no_qml_errors "daemon-up" "$manager_log"
