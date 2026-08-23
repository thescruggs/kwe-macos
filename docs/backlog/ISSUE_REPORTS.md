# Feature: "Report rendering issue" (F4)

- **Requested:** 2026-08-23 (maintainer: "add a button for me to record
  rendering errors so that the next time we have a session we can pull debug
  logs with my notes on specific wallpapers").
- **Status:** IMPLEMENTED 2026-08-23 (manager button + dialog; `IssueReporter`
  in `apps/kwe-manager/src/issuereporter.{h,cpp}`; optional `kwe reports`
  index in `crates/kwe-cli`).

## What it does

The maintainer applies a wallpaper, notices a rendering problem, clicks
**Report rendering issue…** on the wallpaper's detail page, types a short
note ("black layer", "wrong colours", "missing effect", "offset", "slow"),
and clicks **Save report**. `IssueReporter::record()` then writes one bundle
to disk and reports the saved folder path back in the UI (selectable, with a
Copy path action). **Nothing is uploaded anywhere** — this is purely local,
for the next debugging session.

## Where it is captured

`~/.local/share/kwe/reports/<YYYYMMDD-HHMMSS>-<wallpaperId>/` (honours
`XDG_DATA_HOME`; the directory is created `0700`). Each report bundle holds:

| File | Contents |
| --- | --- |
| `report.md` | Wallpaper id/title/kind, the note (verbatim, capped at 4 KiB), the recorded timestamp, the package version (`pacman -Q kde-wallpaper-engine`, falling back to the app version), a **Renderer diagnostics** section (see below), and an **Artefacts** list recording what was captured vs. skipped/failed and why. |
| `renderer-status.json` | Raw `kwe daemon-call --method renderer.status` output — phase, failures, `last_failure_detail`, `stderr_tail` (shader fallbacks, effect counts, model skips). |
| `assignments.json` | Raw `kwe daemon-call --method wallpaper.assignments` output. |
| `health.json` | Raw `kwe daemon-call --method health` output. |
| `journal.txt` | `journalctl --user -u kwe-daemon -n 400 --no-pager` (last 400 lines). If unavailable (no session, no matching unit, `journalctl` missing), the file holds the best available diagnostic instead — its absence is never treated as an error. |
| `frame.png` | The newest `~/.local/state/kwe/last-good-*.ppm` the renderer published, decoded and re-encoded as PNG, downscaled to ≤ 1280 px wide. Skipped (noted in `report.md`) when no such frame exists. |

The **Renderer diagnostics** section in `report.md` pulls the
`event=renderer.scene.*` / `renderer.web.*` / `renderer.video.*` lines out of
`renderer-status.json`'s `stderr_tail` (capped at 60 lines) so the
maintainer's note and the evidence sit together instead of requiring a
second file open.

## How to read it next session

```sh
kwe reports
```

lists every report directory under `~/.local/share/kwe/reports/`, newest
first, with the first line of each note — a quick index before opening a
specific `report.md`.

## Bounded and best-effort by design

Every subprocess (`kwe daemon-call` × 3, `journalctl`) runs with a 5 s
timeout and capped output (1 MiB stdout per artefact); the note is truncated
to 4 KiB. A failed artefact (daemon unreachable, `journalctl` missing, no
frame on disk) is recorded as a line in `report.md`'s Artefacts section and
never aborts the rest of the report — a maintainer capturing evidence for a
broken renderer should not be blocked by one more thing being broken too.
`record()` only fails outright (surfaced via `IssueReporter::errorMessage`)
when the report directory itself, or `report.md` within it, cannot be
written. The wallpaper id is sanitized before it becomes a path component, so
a hostile or unexpected id can never write outside the reports directory.

## Not done / notes

- No automatic redaction of the note or the captured journal/renderer output
  — this is a local, maintainer-only tool, and the bundle is never
  transmitted anywhere by this feature.
- No report browsing UI in the manager; `kwe reports` and opening the folder
  directly are the intended next-session workflow for now.
- No size cap on the number of report directories retained; pruning old
  reports is a manual step if the directory grows large.
