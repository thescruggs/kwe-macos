# Alpha local protocol v1

The manager talks to `kwe-daemon` over a user-owned Unix stream socket. Each
connection carries exactly one UTF-8 JSON request and response, each terminated
by a newline.

Request:

```json
{"version":1,"id":"caller-value","method":"health"}
```

Response:

```json
{"version":1,"id":"caller-value","ok":true,"result":{"status":"ready"}}
```

Methods are `health`, `catalog`, `rescan`, the `renderer.*` control methods,
and the `playlist.*` methods below. Requests are capped at 64 KiB — except
`playlist.import`, which accepts a whole legacy playlist blob up to 4 MiB plus
1 KiB slack — responses at the manager boundary at 32 MiB, and read/write
deadlines are five seconds. Unknown versions and methods return structured
errors. The socket is created with mode `0600`; the daemon refuses to replace
a symlink or regular file at the requested path.

Playlist methods (additive, API version unchanged):

- `playlist.list` → `{"playlists":[...]}` — all definitions, or
  `playlist_store_unavailable` when the definitions store is corrupt.
- `playlist.put` `{"playlist":{...}}` — validated create-or-replace upsert
  (manager is the single writer); echoes the stored playlist.
- `playlist.remove` `{"id":"..."}` → `{"removed":"..."}`.
- `playlist.activate` `{"id":"..."}` or `{"id":null}` — selects the active
  session (restoring the persisted position when one exists) and returns
  the session status.
- `playlist.status` — `{active, playlist_id, decision, unavailable_ids,
  definitions:{count, store_health}, clock_skipped_ms}`; `decision` uses
  the `PlaylistDecision` shape (`started|waiting|advanced|paused|
  exhausted|no_eligible`).
- `playlist.import` `{"playlists":[{title, entries, shuffle, repeat,
  duration_seconds, transition, transition_seconds}]}` — legacy migration;
  merges only into an empty store (`playlist_import_blocked` otherwise),
  derives bounded ids from titles, and reports `{imported, rejected}`.
- `playlist.debug-clock-skip` `{"ms":N}` — test-only suspend simulation
  (1..=3 600 000 ms), rejected with `test_faults_disabled` unless the
  daemon runs with `--allow-test-faults`.

This is a development transport, not a stable public API. Catalog schema and
transport versions are separate so either can evolve explicitly.

