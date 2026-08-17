# Alpha M3e — renderer status visibility

M3e adds a read-only manager client for the daemon's existing
`renderer.status` endpoint. The UI polls independently of catalog refresh and
shows a clear quarantine banner with the wallpaper ID and bounded last-failure
detail. It does not start, stop, or retry workers, keeping recovery an explicit
daemon/CLI action while the Plasma bridge is still in alpha.
