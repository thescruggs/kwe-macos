#!/bin/sh
# Removes the developer LaunchAgents installed by install-dev.sh. Keeps
# ~/Library/Application Support/kwe (state, reports) unless --purge.
set -eu
uid="$(id -u)"
for label in org.kde.kwe.display org.kde.kwe.daemon; do
  launchctl bootout "gui/$uid/$label" 2>/dev/null || true
  rm -f "$HOME/Library/LaunchAgents/$label.plist"
done
if [ "${1:-}" = "--purge" ]; then
  rm -rf "$HOME/Library/Application Support/kwe" "$HOME/Library/Logs/kwe"
fi
echo "kwe LaunchAgents removed"
