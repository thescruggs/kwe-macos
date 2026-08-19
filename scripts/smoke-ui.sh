#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
manager="$project_root/build/cmake/apps/kwe-manager/kwe-manager"
daemon_bin="$project_root/target/debug/kwe-daemon"
smoke_root="$(mktemp -d -t kwe-smoke.XXXXXX)"
daemon_pid=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

cd "$project_root"

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
"${KWE_DAEMON_BIN:?}" --socket "${KWE_DAEMON_SOCKET:?}" &
echo $! > "${KWE_DAEMON_PIDFILE:?}"
STUB
chmod +x "$activation_stub"
export KWE_DAEMON_BIN="$daemon_bin"
export KWE_DAEMON_PIDFILE="$smoke_root/case1/daemon.pid"
manager_exit=0
"$manager" --platform offscreen --socket "$socket_path" --smoke-test-ms 3000 \
    --daemon-activation-command "$activation_stub" || manager_exit=$?
if [[ -f "$smoke_root/case1/daemon.pid" ]]; then
    daemon_pid="$(cat "$smoke_root/case1/daemon.pid")"
fi
unset KWE_DAEMON_BIN KWE_DAEMON_PIDFILE
if [[ "$manager_exit" -ne 0 ]]; then
    echo "smoke-ui: manager failed in daemon-down case (exit $manager_exit)" >&2
    exit "$manager_exit"
fi
[[ -S "$socket_path" ]] || {
    echo "smoke-ui: manager did not activate the daemon (no socket)" >&2
    exit 1
}
kill -0 "$daemon_pid" 2>/dev/null || {
    echo "smoke-ui: activated daemon is not alive" >&2
    exit 1
}

# Case 2: daemon pre-running at manager start (original case).
socket_path="$smoke_root/case2/daemon.sock"
"$daemon_bin" --socket "$socket_path" &
daemon_pid=$!
for _attempt in {1..50}; do
    [[ -S "$socket_path" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        wait "$daemon_pid"
        exit 1
    }
    sleep 0.05
done

"$manager" --platform offscreen --socket "$socket_path" --smoke-test-ms 3000
