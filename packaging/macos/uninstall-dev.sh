#!/bin/sh
# Removes the developer LaunchAgent installed by install-dev.sh. Keeps
# ~/Library/Application Support/kwe (state, reports) unless --purge.
set -eu
uid="$(id -u)"
launchctl bootout "gui/$uid/org.kde.kwe.daemon" 2>/dev/null || true
rm -f "$HOME/Library/LaunchAgents/org.kde.kwe.daemon.plist"
if [ "${1:-}" = "--purge" ]; then
  rm -rf "$HOME/Library/Application Support/kwe" "$HOME/Library/Logs/kwe"
fi
echo "kwe-daemon LaunchAgent removed"
