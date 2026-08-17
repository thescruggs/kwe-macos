# Architecture

## Reliability boundary

KDE documents a Plasma wallpaper as a QML plugin that draws the desktop. Code
loaded there shares `plasmashell`'s fate. The Plasma package in this project
must therefore remain a small display/input client; it may not parse Wallpaper
Engine files, compile shaders, play media, host web content, capture audio,
access Steam, schedule playlists, or own persistent state.

```text
 Steam client / Workshop
          |
   optional steam-bridge -----------+
          |                         |
          v                         v
  +----------------+         +-------------+
  | Kirigami UI    |<------->| user daemon |
  | browse/config  |  D-Bus  | state/policy|
  +----------------+         +------+------+
                                    |
                         supervised worker API
                         +----------+----------+
                         |          |          |
                    video worker scene worker web worker
                         +----------+----------+
                                    |
                         bounded SHM / DMA-BUF frames
                                    |
                         +----------v----------+
                         | thin Plasma QML/C++ |
                         | display/input bridge|
                         +---------------------+

 PipeWire capture -> audio worker -> normalized FFT bins -> renderer worker
 pointer events <- Plasma bridge -> daemon routing -> renderer worker
 watchdog/quarantine <- heartbeat, exit, resource and frame-health events
```

## Components

### `kwe-manager`

Qt 6/Kirigami application for gallery, Workshop, playlists, settings,
compatibility, and diagnostics. It talks only to versioned daemon APIs. Closing
the UI does not stop the active wallpaper.

### `kwe-daemon`

User service supervised by systemd. It owns SQLite state, library indexing,
playlist/policy decisions, worker lifecycle, rollback, and compatibility
records. All apply operations are transactions:

1. validate and classify content;
2. start an offscreen/hidden canary with resource limits;
3. require healthy frames and heartbeats for a bounded interval;
4. atomically promote the candidate;
5. retain the previous still frame and configuration until promotion is
   confirmed;
6. rollback and quarantine on failure.

### Plasma display bridge

A Plasma 6 wallpaper package plus a minimal Qt Quick item. It receives already
rendered frames, validates dimensions/format/modifier/fence metadata, and
forwards normalized input. Connection loss freezes the last good frame or
shows a static fallback. A CPU shared-memory path is required even if DMA-BUF
is the normal zero-copy path.

### Renderer workers

One independently killable process per active wallpaper or backend isolation
unit. Workers have bounded queues, heartbeats, deadlines, log rate limits, and
systemd resource controls. Scene and web parsers are treated as untrusted.

The scene renderer is an original Rust/Vulkan implementation using `ash`.
Open-source compatibility projects are concept and black-box behavior
references, not forks or runtime dependencies. See
`docs/adr/0001-original-vulkan-renderer.md`.

Alpha 0.1 implements the first two safe boundaries: defensive discovery in
`kwe-core`, an external `kwe-daemon`, a bounded v1 Unix-socket API, a Kirigami
manager, and an external Vulkan preflight worker. The Plasma bridge remains
absent, so Alpha cannot apply a wallpaper.

Alpha M1a adds an external generated test-pattern worker, the versioned
double-buffered mmap fallback in `kwe-frame-protocol`, and a standalone Qt Quick
consumer. The consumer copies only stable validated frames and visibly freezes
the last good image after a hang, exit, or corrupted header. This is the
executable contract for the future Plasma bridge; it is not yet loaded by
`plasmashell`. See `docs/FRAME_PROTOCOL_V1.md`.

Alpha M1b makes `kwe-daemon` the generated worker's lifecycle owner. A bounded
supervisor thread observes child exit and frame progress, performs bounded
terminate/kill/reap and restart, persists content-identity quarantine, writes a
static last-good PPM, and uses a Linux parent-death signal to prevent orphaned
workers. Control methods and status fields are documented in
`docs/SUPERVISOR_API_V1.md`. The renderer and mmap protocol are unchanged.

Alpha M1c adds a transactional candidate slot. The active worker remains the
only published display source during canary. Promotion increments a monotonic
display generation and retains the previous worker/mapping until a matching
acknowledgement or bounded timeout. A pre-ack failure restores that previous
worker and never advances the static fallback. See `docs/ALPHA_M1C.md`.

Alpha M1d-A applies address-space, file-size, descriptor, UID-scoped process,
and core-dump limits inside every renderer child before exec. The packaged
systemd user unit separately bounds aggregate resident memory, swap, CPU, and
tasks across the daemon and all concurrent worker slots. Resource denial uses
the same active-preserving retry and quarantine transaction as other candidate
failures. See `docs/ALPHA_M1D.md` and ADR 0003.

Alpha M1d-B adds a separate normalized input channel. The display client sends
passively observed positions with its current display generation; the daemon
routes them only to the promoted active worker through a nonblocking,
latest-event-wins pipe. Renderer acknowledgements are parsed with fixed byte
budgets. Frame protocol v1 remains unchanged. See `docs/INPUT_PROTOCOL_V1.md`.

Alpha M1e packages that boundary as the reusable `org.kde.kwe.display` QML
module and the `org.kde.kwe.wallpaper` Plasma 6 package. The module adds bounded
status polling and acknowledges a transactional display generation only after
the exact frame file is safely opened and validated. Its CPU fallback uses
bounded positioned reads instead of a mutable mapping in `plasmashell`, so
truncation is a normal rejected read rather than `SIGBUS`. Service loss disables
new input and retains the surface's private last-good frame. The package was
staged and exercised offscreen; it has not been installed or loaded into the
live Plasma session. See `docs/ALPHA_M1E.md` and ADR 0004.

### Steam bridge

Optional process that contains the Steamworks dependency and exposes only the
small operations the daemon needs. The first implementation may use
Steamworks.js to validate behavior, but the release decision should compare a
small native bridge against its Node runtime/packaging cost. Steam's local VDF
manifests remain the offline source of truth for installed content.

## Recovery state machine

`DISCOVERED -> PREFLIGHT_OK -> CANARY -> ACTIVE -> DEGRADED`

Any validation failure goes to `INCOMPATIBLE`. A crash, timeout, invalid frame,
or excessive resource use goes to `FAILED`; repeated equivalent failures move
the content hash to `QUARANTINED`. `FAILED` and `QUARANTINED` always activate
the last-known-good still image before notifying the UI.

The daemon never automatically retries an unchanged quarantined content hash.
A renderer update, changed content hash, settings change, or explicit user test
creates a new bounded canary attempt.

M1b implements the failure budget for the generated worker, M1c implements the
two-worker canary/promotion/rollback transaction, and M1d-A implements layered
process and systemd resource enforcement. M1d-B implements normalized pointer
position without button or touch grabs. M1e implements the reusable QML module
and thin Plasma package. Manager-owned installation consent, output assignment,
safe-mode restoration, and an explicitly authorized live `plasmashell`
survival matrix remain required before the project enables desktop replacement.

## Security defaults

- Web wallpaper network and filesystem access are off until explicitly
  granted.
- No worker receives Steam credentials, the full user environment, or broad
  home-directory access.
- Paths from project metadata are canonicalized beneath the content root;
  traversal and unsafe symlink targets are rejected.
- Wallpaper code cannot invoke shell commands or D-Bus arbitrarily.
- Log and crash-report exports redact usernames and paths where practical and
  never include wallpaper payloads.
- A safe-mode CLI can select KDE's normal image wallpaper without loading a
  suspect renderer.

## Upstream evidence behind the design

- KDE's documentation describes wallpaper plugins as QML that draws the
  desktop: <https://develop.kde.org/docs/plasma/>.
- The maintained Wallpaper Engine KDE fork documents that some scenes can
  crash KDE: <https://github.com/RainyPixel/wallpaper-engine-kde-plugin>.
- Waywallen Display demonstrates an external daemon plus DMA-BUF and a thin
  Plasma Qt Quick surface: <https://github.com/waywallen/waywallen-display>.
- Valve documents `ISteamUGC` subscription, download progress, and installed
  item APIs: <https://partner.steamgames.com/doc/features/workshop/implementation>.
