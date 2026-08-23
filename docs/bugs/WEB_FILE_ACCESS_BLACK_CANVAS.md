# Bug: web wallpapers render black (WebGL textures / XHR from file://)

- **Reported:** 2026-08-22 (user, right after B4: "It is applying now, but
  I'm mostly getting black on the wallpaper, it is not rendering correctly")
- **Severity:** High user-facing — the apply succeeds and the desktop shows
  a black canvas where the wallpaper should be.
- **Status:** FIXED 2026-08-22 on `beta-b4-apply-quarantine` (B6), with a
  deliberate, maintainer-approved isolation trade-off and a follow-up.

## Symptom

Web wallpaper 2646399969 ("All-in-one Raindrops Night") promotes, the
supervisor logs only keepalive re-publishes, and both `last-good-*.ppm`
frames are 99.9 % black. 1747779570 ("2AM Cyberpunk City") is mostly black
too.

## Evidence

- Inside the production bwrap argv with `--screenshot`, 2646399969 renders
  its bottom nav over a blurred photo and a BLACK canvas above; console:
  `WebGL: INVALID_VALUE: enableVertexAttribArray: index out of range` —
  the effect's program never linked. WebGL itself is available (SwiftShader
  via ANGLE, with or without `--disable-gpu`; a minimal WebGL clear-red
  page screenshots red under every flag variant), so the canvas is not a
  WebGL-availability problem.
- With `--allow-file-access-from-files` the same screenshot shows the full
  rain effect (mean luminance 0.14 → 0.45). A `file://` page has an opaque
  origin: its own images are cross-origin for `texImage2D` (SecurityError,
  caught by the page → shader setup skipped) and same-directory XHR/fetch
  is blocked by CORS (`Access to XMLHttpRequest at
  'file:///wallpaper/translations.json' from origin 'null' has been
  blocked`, seen on 1747779570). Wallpaper Engine serves content from a
  same-origin scheme, so Workshop pages assume local reads work.
- 1747779570 stays mostly black with the flag: it is an audio-reactive LED
  wallpaper with `hideWhenSilent` on by default and a near-black background
  colour; without the audio grant there is nothing to show. Not a bug.
- Console also says: `Automatic fallback to software WebGL has been
  deprecated. Please use the --enable-unsafe-swiftshader flag` — WebGL
  currently works through a fallback chromium intends to remove.

## What the flag costs (measured)

With `--allow-file-access-from-files`, XHR from the page inside the
sandbox reads `file:///wallpaper/index.html` (200), `/etc/hostname` (200),
`/etc/passwd` (200, 2369 bytes), `/proc/self/status` (200, empty) and the
throwaway profile's `Local State` (200). Without it every one is
`NetworkError`. Nothing under the user's home exists in the namespace (only
`/usr /etc /lib /lib64 /bin /sbin` and the content root are bound; `/tmp`
is the sandbox's own tmpfs), and leaving the sandbox still needs the
per-wallpaper network grant (off by default). The M2d compromise suite's
attempt 2 asserted the old browser-level block and had to be re-targeted.

## Decision (maintainer, 2026-08-22)

"Enable now + narrow /etc later": wallpapers must render; the readable set
is bounded by the namespace; the network grant is the exfiltration gate.
Follow-up (not started): bind a minimal `/etc` (fontconfig, `ld.so.cache`,
`localtime`, ssl certs, whatever chromium actually opens — measure with
strace) so the readable system set shrinks to what the browser needs.

## Fix

- `crates/kwe-core/src/websandbox.rs`: `web_renderer_command` and
  `web_preview_command` add `--allow-file-access-from-files` and
  `--enable-unsafe-swiftshader`; the pinned-flag tests assert both.
- `scripts/smoke-web-compromise.sh` attempt 2 now targets a HOST canary
  (`<smoke_root>/host-canary.txt`, real, not bound) by cors-mode fetch and
  by traversal through `/wallpaper/../..`; both must reject (RED on a 200
  with the canary's bytes). Attempt 3 becomes the control: a cors-mode
  fetch of the page's own file must resolve with its bytes. Both scenarios
  green on this machine.
- Docs: BETA_M2.md §2/§5 pinned flags and §7.2 matrix + honest reading,
  FEATURE_COMPATIBILITY.md `content.web` cell.
- Package `pkgrel` 7.

## Verify

Install `-7`, `systemctl --user daemon-reload && systemctl --user restart
kwe-daemon`, apply 2646399969: the rain effect renders. For 1747779570
grant audio in the wallpaper's permissions and play something.
