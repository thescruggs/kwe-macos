# BETA_M2 — Wallpaper renderer (CDP) milestone report

Status: **M2a complete** (spike + client + smoke); **M2b complete** (sandboxed
web renderer worker + supervision + smoke); **M2c complete** (daemon-owned
permission grants + manager wiring + smoke).

## M2a goal

Pin the Chromium DevTools Protocol wire contract over `--remote-debugging-pipe`
empirically on the installed browser, then ship a small, bounded CDP client
(`crates/kwe-cdp`) that M2b's renderer worker will use to capture wallpaper
frames headlessly. Everything in this document was measured against
Chromium 151.0.7922.137 (Arch Linux, `--headless=new`) on this machine
(2026-08-19), and re-verified by `scripts/smoke-cdp.sh`.

Deliverables: spike report (this file), `crates/kwe-cdp` (library + fake-peer
unit tests, 35 passing), `crates/kwe-cdp/examples/spike.rs` (chromium-driving
probe), `scripts/smoke-cdp.sh` (jq assertions on the probe's JSON summary).

## 1. The wire, empirically

### 1.1 File descriptors

`--remote-debugging-pipe` does **not** use stdin/stdout. Chromium reads CDP
requests from **fd 3** and writes responses/events to **fd 4**
(`content::DevToolsAgentHost::kReadFD = 3, kWriteFD = 4`). At startup it runs
`fcntl(3, F_GETFL)` + `fcntl(4, F_GETFL)` and refuses to start with
`Remote debugging pipe file descriptors are not open.` if either fails — so
the renderer must hand the browser two real, open descriptors at those
numbers before exec.

The client owns the opposite ends: two independent unidirectional channels
(pipes or socketpairs) per direction — never a shared socketpair end, which
would loop the client's own writes back into its reads.

### 1.2 Framing: ASCIIZ

Each CDP message is exactly **one JSON document followed by a single NUL byte
(0x00)**, in both directions (`devtools_pipe_handler.cc`,
PipeReaderASCIIZ/PipeWriterASCIIZ). No `Content-Length` headers, no HTTP.
Trying `Content-Length: N\r\n` framing yields silence (measured). The
`--remote-debugging-pipe=cbor` variant switches to CBOR and is **not** used.

Observed request (as written to fd 3):

```json
{"id":1,"method":"Target.getTargets","params":{}}
```
followed by `0x00`.

### 1.3 Envelopes

```
request : {"id":<u32>,"method":"...","params":{...}[,"sessionId":"..."]}
response: {"id":<u32>[,"result":{...}|"error":{code,message}][,"sessionId":"..."]}
event   : {"method":"...","params":{...}[,"sessionId":"..."]}
```

- The **browser session** (getTargets, attachToTarget, ...) carries **no**
  sessionId.
- `Target.attachToTarget {targetId, flatten:true}` returns
  `{"sessionId":"<hex>"}`; with `flatten:true` every subsequent response and
  event of that session carries the top-level `sessionId` (measured; also
  visible on the `Target.attachedToTarget` event, whose sessionId sits inside
  `params`).
- Error shape (measured, unknown method):
  `{"code":-32601,"message":"'Bogus.method' wasn't found"}` — the response
  still carries the request `id`, so correlation is unaffected.
- Unsolicited responses (late answers to timed-out requests) are dropped by
  the client, never buffered.

### 1.4 Bootstrap sequence (measured, ~250 ms total)

```
Target.getTargets                     -> find type:"page" (poll; the initial
                                        pre-navigation target appears first,
                                        retry until the fixture URL shows)
Target.attachToTarget {flatten:true}  -> sessionId
Page.enable                           -> {}
Page.startScreencast {format:"jpeg",quality:80,
                      maxWidth:160,maxHeight:90,everyNthFrame:1}
```

### 1.5 Screencast event shape (measured, exact)

```json
{"method":"Page.screencastFrame",
 "params":{
   "data":"<base64 jpeg>",
   "metadata":{
     "deviceScaleFactor":1,"pageScaleFactor":1,"offsetTop":0,"offsetLeft":0,
     "deviceWidth":780,"deviceHeight":437,"scrollOffsetX":0,"scrollOffsetY":0,
     "timestamp":<epoch seconds float>},
   "sessionId":<int>},
 "sessionId":"<attached session>"}
```

- `params.sessionId` is the **screencast-session counter** (increments per
  `startScreencast` call, source: `page_handler.cc` `session_id_`), **not** the
  attached-session id. The ack must echo this value.
- `metadata.deviceWidth/Height` are the full-page layout dimensions
  (`--window-size` is ignored by headless=new, measured); `maxWidth/maxHeight`
  alone control the delivered jpeg size (measured: 160x90 regardless of page).

### 1.6 The ack contract (the one thing that can hang you)

- The producer stalls **exactly 3 frames** after the last ack:
  `kMaxScreencastFramesInFlight = 2` (source: `page_handler.cc`), i.e. the
  third unacked frame is delivered and then **hard silence** — measured at
  cold start (no acks ever) **and** mid-stream (3 additional frames after the
  acks stop), in every run.
- Each frame must be acked with
  `Page.screencastFrameAck {"sessionId": <params.sessionId>}` in the attached
  session. With per-frame acks the stream runs indefinitely (~30 fps,
  33 ms cadence measured on the 780x437 page; ~14 ms at 160x90 — the
  compositor delivers faster than the nominal cadence on tiny viewports).
- A late ack after silence resumes capture (observed during development, not re-verified by the smoke).
- **Deviation from the M2a task text**: the spec assumed "frames stop (≤1
  additional)" after acks cease. The measured and source-verified behavior is
  **exactly 3 additional frames** (2 in flight + 1 being produced).
  `scripts/smoke-cdp.sh` therefore asserts a 1..=3 band plus hard silence.

### 1.7 Timings and sizes (measured)

| Quantity | Value |
| --- | --- |
| spawn -> first frame (cold browser) | 428–679 ms (older runs), 207–229 ms (current run set) |
| startScreencast response -> first frame | 20–53 ms (34–37 ms current) |
| frame cadence with acks | ~33 ms @780x437; ~14 ms @160x90 |
| jpeg size, 160x90 q80 (dark animated page) | 464–469 B (552–558 B current fixture) |
| full wire envelope per frame event | ~913 B (466 B jpeg + JSON + base64 overhead) |
| unacked frames before stall | exactly 3 (both cold start and mid-stream) |
| pipe close -> chromium exit | rc=0, prompt (observed ~50 ms during development) |

### 1.8 Quirks and pitfalls (all measured)

1. **fd 3/4 are required**: closing them pre-exec or leaving FD_CLOEXEC set on
   them makes chromium refuse to start (silently, if stderr is not drained —
   the zygotes keep the pipe open, so the client sees timeout, not EOF).
   `dup2(old, old)` does **not** clear FD_CLOEXEC — clear it explicitly.
2. **User-profile leakage**: without `--user-data-dir=<fresh>`,
   `--disable-extensions`, `--no-first-run`, getTargets shows the user's real
   profile (extensions, newtab). The smoke always uses a throwaway profile.
3. **Pre-navigation target**: getTargets at T+0.19 s catches a newtab target;
   poll until the fixture URL appears (or a lone page target).
4. **GCM noise on stderr** ("DEPRECATED_ENDPOINT") is harmless.
5. Closing the client's pipe ends is the teardown signal (exit rc=0, prompt;
   ~50 ms observed during development); no CDP "close" message exists.
6. `--headless=new` is required (old headless lacks the pipe path).

## 2. Pinned contract for M2b (the renderer will assert this)

```text
flags:  --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage
        --disable-extensions --no-first-run --no-default-browser-check
        --remote-debugging-pipe --user-data-dir=<fresh tmp> <file://fixture>
wire:   fds 3/4, ASCIIZ (JSON + NUL) framing, 4 MiB per-message bound
setup:  Target.getTargets -> Target.attachToTarget{flatten:true}
        -> Page.enable -> Page.startScreencast{format:"jpeg",quality:80,
           maxWidth,maxHeight,everyNthFrame:1}
frames: Page.screencastFrame events in the attached session; ack each with
        Page.screencastFrameAck{sessionId:<params.sessionId>} or the stream
        hard-stalls after exactly 3 frames
timing: first frame < 10 s after spawn (measured: < 700 ms)
teardown: close both pipe ends; chromium exits rc=0 promptly (~50 ms observed)
```

## 3. crates/kwe-cdp

An original, minimal, bounded CDP client; no async runtime, no threads in the
library. Modules: `codec` (ASCIIZ framing + 4 MiB bound decoder), `transport`
(nonblocking pipe pump with `poll(2)` deadlines, bounded write backlog),
`connection` (id correlation, monotonic u32 ids, bounded event queue 64
drop-oldest with a drop counter), `client` (request_browser/request_session,
5 s default timeout). Errors: `Timeout`, `ParseError`, `OversizedMessage`,
`Io`. All 38 unit tests run against in-memory socketpair fake peers — no
browser needed. The spike example drives real chromium and prints a JSON
summary; the smoke script asserts on it.

## 4. M2a acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (38 kwe-cdp) |
| smoke | `./scripts/smoke-cdp.sh` | pass (7 jq assertions) |

Smoke evidence (2026-08-19 run):
`stall_frames=3, silence_confirmed=true` (phase A);
`frames=8, first_frame_after_start_ms=34, additional_after_ack_stop=3,
silence_confirmed=true, bytes_per_frame_avg=552` (phase B); both chromium
instances exited rc=0.

## 5. M2b: the sandboxed web renderer worker

### 5.1 Goal

`crates/kwe-web-renderer` turns one sandboxed headless Chromium into a
supervised wallpaper renderer: it spawns the browser inside a bwrap sandbox,
captures the page over the CDP screencast pipe pinned in §1–§2, publishes
BGRA8888 frames through the same bounded frame protocol the video/test
workers use, and answers the daemon's pointer and audio lines from its
control stream. The supervisor treats it like any other worker — canary,
promotion, failure budget, quarantine — so a crashing or malicious page can
never escape the worker's fault envelope.

Deliverables: `crates/kwe-web-renderer` (worker binary, ~1500 lines),
`web_renderer_command()` in `crates/kwe-core/src/websandbox.rs` (the bwrap
sandbox builder), `scripts/smoke-web.sh` (9 cases), THIRD_PARTY.yml entries,
this section.

### 5.2 The sandbox

M2a's `chromium_command` produced a bare flag string; M2b replaces it with a
real bwrap command (this was the M2a gap: bwrap's root namespace starts
empty, and nothing would bind the browser's system paths in). The sandbox:

- read-only binds `/usr`, `/etc`, `/lib`, `/lib64`, `/bin`, `/sbin`
  (verified: chromium 151 launches and answers the CDP pipe through this);
- overlays the wallpaper content root at `/wallpaper` (read-only);
- gives `/tmp` as a writable tmpfs for the throwaway profile
  (`/tmp/kwe-profile`, fresh per launch), `/proc` and `/dev`;
- runs `--unshare-net` unless the content permission set grants network
  (M1a default OFF; grants land in M2c);
- `--die-with-parent --new-session`, then `chromium --headless=new
  --no-sandbox --disable-gpu --disable-dev-shm-usage --no-first-run
  --no-default-browser-check --disable-extensions --remote-debugging-pipe
  --window-size=<spec> --user-data-dir=/tmp/kwe-profile
  file:///wallpaper/index.html`.

The worker itself runs with the supervisor's stripped environment (no
`DISPLAY`, no session bus; see SUPERVISOR_API_V1.md) and hands the browser
the CDP pipe on fds 3/4 exactly as pinned in §1.1/§2.

### 5.3 Worker contract

- **Frames**: `Page.screencastFrame` events (jpeg q80, delivered at the spec
  size) are acked per frame (the §1.6 contract), base64-decoded by a
  hand-rolled bounded decoder, JPEG-decoded through the `image` crate with
  `Limits` capped at **8192 px per dimension / 64 MiB alloc / 16 777 216
  pixels**, converted to opaque BGRA8888, and published through the shared
  frame protocol at the pacing deadline (fps). A frame that fails to decode
  or exceeds the caps is counted (`event=renderer.web.decode_failure`) and
  skipped, never published. (`MAX_DECODE_DIMENSION = 8192`,
  `MAX_DECODE_ALLOC_BYTES = 64 MiB`, `MAX_DECODED_PIXELS = 16_777_216` in
  `crates/kwe-web-renderer/src/main.rs` — the doc and the code agree.)
- **Keepalive**: a static page produces no screencast frames, so the last
  decoded frame is re-published at each pacing deadline; the supervisor's
  frame timeout can never trip on a page that painted once.
- **Heartbeat**: the keepalive cannot distinguish a still page from a
  *wedged* page — a page whose renderer main thread hangs after first paint
  stops answering CDP (acks included) while the browser process survives and
  the keepalive keeps the sequence advancing forever. A page-independent
  probe therefore runs every `--web-heartbeat-ms` (default 5000): a
  session-scoped `Runtime.evaluate("1+1")` sent through the non-blocking CDP
  API (`Client::send_session` + `take_response` — a blocking probe would
  stall the publish pipeline past the supervisor's frame timeout, so the
  daemon would reap the worker before this path could fire). An unanswered
  probe within its one-interval deadline counts as one consecutive failure;
  `--web-heartbeat-max-failures` (default 3) consecutive failures/timeouts
  emit `event=renderer.web.heartbeat_failed` and exit 73. A healthy static
  page answers every probe, so only genuinely unresponsive pages trip it.
- **Scale policy**: the frame slot is fixed-size, so letterboxing is
  unavailable; a delivered frame whose dimensions differ from the spec
  (compositor aspect rounding, e.g. 160x89 for a 160x90 spec) is stretched
  with a bounded nearest-neighbor scale (`y*src_h/dst_h` integer ratio) that
  fills the slot exactly. Output is always exactly spec-sized, so the
  stretch is O(spec pixels) — bounded and cheap (measured below).
- **Input**: pointer lines from the control stream are dispatched to the
  page as CDP `Input.dispatchMouseEvent` in layout CSS pixels (normalized
  u16/65535 mapped to the screencast viewport; a held-button bitfield is
  carried on every event); `audio.forward` frames are evaluated as
  `window.audio_web([...])` at most 30/s (rate-limit diagnostics
  `event=renderer.web.audio_rate_limited`); media-state messages are
  ack-only. Each valid message is echoed with its wire sequence (the daemon
  acks). Evaluation failures are counted and diagnosed
  (`event=renderer.web.audio_evaluate_error`), never fatal.
- **Faults**: `--exit-after N`, `--memory-after N` exercise the same fault
  block as the test/video workers, with the same exit codes: 70
  (`--exit-after` fired), 71 (memory denied under rlimit), 72 (memory
  unexpectedly succeeded), 73 (backend rejected — the browser failed to
  answer the CDP pipe, the sandbox never came up, or the page failed the
  heartbeat; the daemon folds the chromium stderr tail into the failure
  detail).
- **Teardown**: closing the CDP pipe ends is the bounded shutdown signal
  (chromium exits rc=0 within ~50 ms, §1.7); the bwrap process group is
  SIGTERMed after a grace, SIGKILLed past it, and reaped with a bound.

### 5.4 Supervision deltas (the 128 GiB address-space decision)

Empirically (all measured on this machine, Chromium 151.0.7922.137):

- The V8 sandbox reserves ~53 GiB of virtual address space per browser
  process at exec (VmSize ≈ 55 449 708 kB per process; RSS stays ~250 MB,
  36 threads — the reservation is pure VA, no resident cost).
- With `RLIMIT_AS` at 16384 MiB the browser SIGTRAPs at exec, silently
  (rc=133, empty stderr tail); 64 GiB renders fine but the DevTools pipe
  bootstrap never answers (silent timeout — the V8 sandbox floor is
  ~98 GiB: 96 GiB fails, 100352 MiB works); the daemon's default is
  **131072 MiB (128 GiB)**, clearing the floor with margin.
- The old global 1024-process ceiling kills the bwrap fork: the kernel's
  `RLIMIT_NPROC` counts every thread of the uid (`user->processes`), this
  session alone runs ~1265 threads, and `spawning bwrap` then fails with
  EAGAIN (3× → quarantine). The web kind therefore defaults to a
  **32768-process ceiling** and 1024 open files, like the M1e video fix.
- Resident protection comes from the supervisor timeouts plus the
  containing unit's systemd `MemoryMax`, not from `RLIMIT_AS`.

The status `resource_limits` for a web worker reports
`{address_space_mib: 131072, open_files: 1024, processes: 32768}`.

### 5.5 Decode latencies (measured)

Offline microbenchmark of the exact decode path (hand-rolled base64 +
`image` ImageReader with the worker's `Limits` + `into_rgb8` + the
nearest-neighbor convert; release build, image 0.25.10), on a q80 jpeg
matching the wire size class:

| Spec | jpeg bytes | decode+convert, median (min–max) |
| --- | --- | --- |
| 160x90 | 1504 (wire q80 dark pages: ~555) | 60 µs (58–132) |
| 960x540 | 17650 | 1435 µs (1402–2121) |
| 160x89 stretch path | 1504 | 59 µs (58–121) |

The 160x90 decode is ~0.2 % of the 33 ms pacing budget; even 960x540 costs
~1.4 ms. The stretch policy is free at wallpaper sizes.

### 5.6 M2b acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (201 tests, 0 failures) |
| smoke-web | `./scripts/smoke-web.sh` | pass (11 cases, plasmashell pid unchanged) |
| smoke-cdp | `./scripts/smoke-cdp.sh` | pass (M2a regression) |
| smoke-video | `./scripts/smoke-video.sh` | pass (M2a regression) |
| smoke-supervisor | `./scripts/smoke-supervisor.sh` | pass (M2a regression) |

Smoke-web case evidence (2026-08-19 run): canary promote with the 128 GiB
budget applied (sequence advances, sandbox holds, last-good P6 persisted).
The sandbox-integrity case is network-dependent, not scheme-isolation-based:
the fixture fetches `http://127.0.0.1:<port>/probe` (1.5 s abort timeout)
and paints a red marker covering the probe box (10,10,4,4) in the captured
frame on success; under `--unshare-net` the sandbox's own loopback does not
exist and the fetch fails fast, so the marker never paints and the probe
box stays empty — while the positive control (the same fixture started
through the daemon with the per-request `allow_network` test hook, gated
behind `--allow-test-faults`, plus a loopback CORS `python3 -m
http.server`) paints the marker, proving the negative comes from the
network namespace, not from a marker that could never paint. The marker is
positioned in viewport X fractions and spans the full canvas height
because of the screencast geometry (see §5.7): the headless surface is
500x3, the screencast aspect-fits it to a 160x1 JPEG, and the slot fill
duplicates that single row across all 90 frame rows — the marker must
dominate the row average, or it decodes as a dim smear that fails the
probe (measured root cause of the earlier positive-control failures).
Further cases: static-page keepalive (sequence advances, 0 failures, no
decode diagnostics); pointer oracle (baseline clean, dot at normalized
(0.5, 0.5) painted and acked, probe verified); audio.forward (acks advance
to the display generation, 0 protocol errors); kill -9 (recorded once,
last-good preserved, auto-restarted, not quarantined); missing content root
rejected `invalid_params`; busy-loop page (worker exit 73, rolled back with
`exit_code_73`); three candidate-window kills → quarantine (`failures=3`,
pid null) and `renderer.start` refused with the quarantine phase; late-wedge
page (promotes to live, then wedges its renderer main thread: the keepalive
masks the dead stream, the heartbeat times out twice under the smoke
override and the worker exits 73 with `event=renderer.web.heartbeat_failed`
in the failure detail, the supervisor records the exit and restarts, and the
wedge repeats — the case observes three consecutive exit-73 cycles, never
masked. Note the wedge does NOT quarantine: the failure budget is
pre-promotion by design (a promotion clears the record — a worker that
reached live is trusted, so post-promotion exits restart cleanly; the same
rule that makes case-8's candidate kills accumulate). plasmashell's pid was
identical and alive before and after the suite.

### 5.7 Open risks

- **VA floor may move with chromium versions**: 128 GiB clears the current
  98 GiB floor with margin, but a future browser with a bigger V8 sandbox
  reservation fails *silently* (no stderr) below its own floor. The daemon
 's upper bound is 256 GiB (`RendererResourceLimits::validate`).
- **Bwrap fork under NPROC**: the 32768 ceiling is per-uid-wide process
  accounting; a session running >32768 threads (or a kernel with tighter
  `kernel.threads-max`) would hit EAGAIN at spawn, surfacing as an opaque
  `exit_code_73`/backend-reject with an empty stderr tail. Mitigation is
  the failure budget + the sandbox holding no secrets.
- **Screencast geometry (measured root cause of the early positive-control
  failures, 2026-08-19)**: headless=new ignores `--window-size` — the
  window is 500x90 with a 500x3 layout viewport — and
  `Page.startScreencast` aspect-fits the surface into maxWidth/maxHeight,
  producing a **160x1 JPEG** whose single row is the area-average of the
  three canvas rows; the worker's bounded slot fill then duplicates that
  row across all 90 frame rows (`y*src_h/dst_h` is 0 for a 1-row source),
  so the frame's y-axis carries no information at all. Consequences: a
  marker painted on a subset of canvas rows decodes as a dim full-height
  smear (measured (85,13,14) from rows (208,3,3)/(63,14,16)/(16,18,20)),
  and a fixed-coordinate marker is fragile at best, off-canvas at worst.
  Fixtures handle it by painting the marker in viewport X fractions
  spanning the full canvas height — invariant to the surface size, landing
  at frame columns 6..18 on any geometry. Relatedly, a page that paints
  identical pixels every frame stops the compositor from producing new
  frames, which stops rAF callbacks entirely (this was the earlier
  "1-frame quirk" in isolated captures): animations must change pixels per
  frame to keep frames flowing. The keepalive re-publish masks still pages;
  a page that genuinely stops painting is indistinguishable from one that
  paints once by the frame path alone — the heartbeat bounds that blind
  spot (a stopped-painting page whose main thread still answers probes is
  fine; a wedged one exits 73).
- **Heartbeat interplay with the frame timeout**: the probe deadline is one
  full interval (default 5 s) and a healthy-but-busy page can take longer
  than that to answer under extreme load — the worker would then exit 73
  even though the page is alive. The interval/failure budget is
  configurable (`--web-heartbeat-ms` / `--web-heartbeat-max-failures`) and
  the daemon defaults (5 s / 3) sit far above the measured <100 ms answer
  time; a page that stalls the renderer main thread for >15 s is wedged in
  every practical sense. The probe is strictly non-blocking, so it never
  competes with the pacing deadline for the worker's single thread.
- **Decode budget**: worst-case 960x540 decode ≈1.4 ms is fine at 30 fps;
  a future 4K spec (3840x2160) would cost ~20 ms/frame in software decode —
  the caps keep it bounded, but the pacing budget shrinks.

## 6. M2c: daemon-owned permission grants

### 6.1 Goal

Move per-wallpaper capability decisions (network, audio, pointer) from the
M2b per-request test hook into a daemon-owned, persisted, bounded grant
store, and wire it through the manager UI. Grants are the production
mechanism: a wallpaper's record in `permissions-v1.json` decides whether
`kwe-web-renderer` spawns with `--allow-network` and whether forwarded
audio reaches the worker. The M2b `allow_network` hook is removed — the
parameter is now rejected as an unknown field, the daemon no longer runs
with `--allow-test-faults` in the smoke lane, and `smoke-web.sh` proves the
positive and negative controls through the grant path alone.

### 6.2 Design

- **Store** (`crates/kwe-daemon/src/grants.rs`): `permissions-v1.json` in
  the private state directory beside `supervisor-v1.json` —
  `{"schema_version": 1, "grants": {"<wallpaper_id>": {"network": bool,
  "audio": bool, "pointer": bool}}}`. Bounded (≤ 256 records, 1 MiB),
  written atomically through `persist.rs::atomic_write`, loaded with
  `deny_unknown_fields`; a corrupt file is renamed aside
  (`permissions-v1.json.invalid-<seconds>-<nanos>`) and the store starts
  fresh with a one-time log. Wallpaper ids follow the identity rule (1–128
  ASCII letters, digits, `.`, `_`, `-`).
- **Default policy**: network off, audio off, **pointer on** (interactivity
  is core; the pointer grant is reserved for future stricter modes and is
  not enforced yet).
- **RPC** (one request per connection, through the supervisor's bounded
  command channel): `permissions.get` → the effective record (defaults when
  no record exists); `permissions.set` → patch semantics, the answer is the
  new effective record; `permissions.list` → all records. Unknown fields
  and out-of-bounds ids are rejected (`invalid_params`); the 257th record
  fails (`permissions_failed` naming the safety limit).
- **Enforcement**:
  - *Network*: at spawn the supervisor appends `--allow-network` to the web
    worker argv only when the wallpaper's grant record allows it; otherwise
    bwrap runs `--unshare-net`. Revocation takes effect on the next
    `renderer.start` for that identity.
  - *Audio*: capture stays global (`kwe-audio-worker` keeps running), but
    the grant gates delivery — `audio.forward` frames for a wallpaper
    without the audio grant are dropped silently (latest-wins,
    bounded-rate logging), counted in `audio_grant_dropped`
    (renderer.status).
  - *Pointer*: pass-through stays enabled by default; not enforced yet.
- **Manager** (`apps/kwe-manager`): `PermissionsClient`
  (`permissionsclient.{h,cpp}`) mirrors the CatalogClient/PlaylistClient
  QLocalSocket pattern with a bounded retrying queue and per-id pending
  state; `WallpaperDetail.qml` toggles read and write the daemon record
  through it. The QML-local QSettings permission state in CatalogModel is
  removed (catalog stats stay).

### 6.3 Acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (214 tests, 0 failures) |
| cmake | `cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug && cmake --build build/cmake --parallel` | pass |
| ctest | `cd build/cmake && ctest --output-on-failure` | pass (5/5, incl. `kwe-permissions-client-test`) |
| qmllint | `qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-manager apps/kwe-manager/qml/*.qml` | pass |
| smoke-web | `./scripts/smoke-web.sh` | pass (grants lane: defaults asserted, grant paints red, revocation restores the sandbox) |
| smoke-video | `./scripts/smoke-video.sh` | pass (regression) |
| smoke-supervisor | `./scripts/smoke-supervisor.sh` | pass (regression) |

The M2b §5.6 evidence above describes the hook-era positive control and
stays as history; the grants lane (`smoke-web.sh` case 1b) now drives the
same fixture through `permissions.set` → `renderer.start` (marker paints
red) and revocation → restart (marker stays away while the probe server
still runs — the grant, not connectivity, is the discriminator).
