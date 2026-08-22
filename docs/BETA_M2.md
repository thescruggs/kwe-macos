# BETA_M2 — Wallpaper renderer (CDP) milestone report

Status: **M2a complete** (spike + client + smoke); **M2b complete** (sandboxed
web renderer worker + supervision + smoke); **M2c complete** (daemon-owned
permission grants + manager wiring + smoke); **M2d complete** (sandbox
compromise suite + windowed preview fix + web preflight CLI); **M2e complete**
(web parity evidence: `--probe` capability manifest, `kwe diagnose` web lane,
renderer-dependent catalog state, FEATURE_COMPATIBILITY rows). Milestone
**done** — see §8 for the close-out and the M2 exit-gate summary.

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
- **The unit's `TasksMax` is a launch-class bound too (BETA B4,
  2026-08-22, chromium 151.0.7922.173).** The cgroup pids limit counts
  every thread of the daemon, the audio worker, bwrap and every chromium
  process; at `TasksMax=96` the browser's zygote cannot fork the renderer
  (`Zygote could not fork`, `pthread_create: Resource temporarily
  unavailable (11)` in the stderr tail) and the worker exits 73 at
  bootstrap. Measured with `systemd-run -p TasksMax=N kwe-web-renderer
  --probe`: 96 fails, 128+ passes, one probe alone peaks at ≥53 tasks
  sampled at 200 ms. The unit ships `TasksMax=512` and `kwe diagnose` runs
  the web probe under the unit's own `TasksMax` so the lane fails exactly
  when a supervised launch would. A probe from the shell is NOT evidence
  that the unit can launch the browser.

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
  fresh with a one-time log. Invalid siblings are pruned to the newest 8 by
  the shared `persist` helper (all three state stores benefit). Wallpaper
  ids follow the identity rule (1–128 ASCII letters, digits, `.`, `_`, `-`).
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
| test | `cargo test --workspace --all-targets` | pass (215 tests, 0 failures) |
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

## 7. M2d: the sandbox compromise suite, the preview fix, and web preflight

### 7.1 Goal

Three deliverables. (1) `scripts/smoke-web-compromise.sh`: a dedicated
compromise suite whose runtime-generated fixture page attempts four sandbox
escapes and paints one color-coded result box per attempt, driven through the
daemon twice — Scenario A with the default grants (network off), Scenario B
with a network grant set through `permissions.set` — and asserted with the
frame oracle AND with the actual sandbox argv read from
`/proc/<pid>/cmdline`. (2) The manager `WebPreview` fix: the old M2a
`chromium_command` (empty bwrap root, no system ro-binds, no `--no-sandbox`)
could not exec chromium at all; the preview now launches the same M2b sandbox
(shared `sandbox_prefix` builder) with WINDOWED chromium, and the network
grant is wired through `PermissionsClient` with a bounded relaunch on grant
change. (3) `kwe preflight --web <root>`: statically validate a web wallpaper
directory exactly like the existing `--path`/`--video` variants (exit 0 safe /
2 unsafe / 1 misuse).

### 7.2 The compromise matrix (attempt × scenario × expected → actual)

All results measured 2026-08-19 on Chromium 151.0.7922.137 through the
daemon pipeline, verified by `scripts/smoke-web-compromise.sh`. Attempt 1's
positive control is a host-side STALL listener (scratch loopback port whose
listener accepts every connection and never answers) — deterministic on
any machine, unlike an external address: scenario A's `--unshare-net`
namespace has NO loopback, so the connect fails fast; scenario B's shared
host netns connects to the listener and waits for an answer that never
comes (1.5 s abort).

| Attempt | Escape | Scenario A (default, no grant) | Scenario B (network grant) |
| --- | --- | --- | --- |
| 1 | network fetch to `http://127.0.0.1:<scratch>/` (host stall listener), 1.5 s abort bound | no loopback in the isolated netns: connect fails fast (~10 ms TypeError) → **green**; actual: green | connects to the host listener, hangs, aborts at the 1.5 s bound → **orange** (positive control); actual: **orange (255,139,0)** |
| 2 | cors-mode fetch of `file:///etc/passwd` + traversal XHR `file:///wallpaper/../../../../etc/passwd` | both fail → **green**; actual: green | same → **green**; actual: green |
| 3 | content-root reachability: `file:///wallpaper/index.html` | succeeds → **green**; actual: green | same → **green**; actual: green |
| 4 | allowed reads: localStorage + `navigator.userAgent` | succeed → **green**; actual: green | same → **green**; actual: green |

Honest reading of row 2: the boundary under test is the BROWSER's
file-scheme/CORS isolation — chromium 151 blocks cors-mode fetch/XHR to
every `file:` URL from a `file://` page (the page's own URL included), so
the traversal cannot distinguish a bound path from a non-bound one through
this attempt alone. The sandbox's non-bound-path contribution (only the
system paths and the content root are reachable — there IS no traversal
target) is asserted by the command-builder unit tests
(`webpreviewtest.cpp::isolationByDefault`, the kwe-core builder tests),
not by this case. The row's RED condition still holds: a resolution would
be a genuine host-file read by the browser's own code (the traversal
normalizes to `file:///etc/passwd`, which EXISTS inside the sandbox because
/etc is ro-bound).

The fixture colors are GREEN `#00c000` (sandbox held), ORANGE `#ff8c00`
(attempt left the sandbox — only the positive control may paint it), RED
`#ff0000` (escape/compromise — the suite fails), PENDING `#303030`. Probes
use the shared `scripts/frame-read.py` (JPEG q80 decode, per-channel
tolerance 60). The M2b screencast-geometry lesson applies (see §5.7): the
four boxes are painted in viewport X fractions spanning the FULL canvas
height and land at frame columns 0-40/40-80/80-120/120-160.

### 7.3 The argv-level proof

The grant→argv contract is asserted on the real process tree, not just on
painted pixels. `renderer.status`'s `pid` is the supervised worker
(`kwe-web-renderer`), whose only child is the bwrap process; both command
lines are read from `/proc/<pid>/cmdline` (NUL-separated, `tr '\0' ' '`).

| Process | Scenario A | Scenario B |
| --- | --- | --- |
| worker argv | no `--allow-network` (measured) | contains `--allow-network` (measured) |
| bwrap argv | contains `--unshare-net` (measured) | no `--unshare-net` (measured) |

Scenario B's painted ORANGE box proves the lack of `--unshare-net` is not a
cosmetic argv diff: the fetch CONNECTED to the host's listener and hung
until the abort — the namespace's network stack is genuinely the host's.

### 7.4 Measured deviations from the task text (all documented in the fixture)

1. **The abort reason is named `TimeoutError`**: `AbortSignal.timeout()`'s
   abort reason is a TimeoutError DOMException (spec behavior, confirmed
   through the daemon pipeline — the fixture's first `AbortError`-only check
   left scenario B green; the name is `TimeoutError` on chromium 151).
2. **Attempt 3 must use `no-cors`**: cors-mode fetch/XHR of ANY `file:` URL
   from a `file://` page is blocked — including the page's own URL (measured
   in M2d). A no-cors fetch resolves opaque exactly when the file exists, so
   it is the probe that proves attempt 2's failures are isolation (see the
   honest reading of row 2 in §7.2), not a broken content mount.
3. **`/etc/passwd` exists inside the sandbox** (a no-cors probe of it
   resolves): attempt 2's RED would be a genuine host-file read, not a
   missing-file artifact.
4. **Attempt 1's positive control is the 1.5 s abort, not a resolved
   response**: the stall listener never answers, so the fetch cannot
   resolve; the observable that proves the fetch LEFT the sandbox is the
   abort at the bound — ORANGE "network-present". This is deterministic
   (see §7.2), unlike the RFC 5737 probe of the first form, which depended
   on the host's route table (an offline or ICMP-refusing host painted
   GREEN and spurious-failed the suite).

### 7.5 The WebPreview fix

`apps/kwe-manager/src/webpreview.{h,cpp}` now builds the command from the
shared `sandbox_prefix(root, network_allowed)` in
`crates/kwe-core/src/websandbox.rs` (the M2b ro-bind set + `--unshare-net`
toggle, plus `web_preview_command()` as the pinned windowed form): bwrap
then `chromium --no-sandbox --disable-dev-shm-usage --no-first-run
--no-default-browser-check --disable-extensions
--user-data-dir=/tmp/kwe-preview-profile file:///wallpaper/index.html` —
no `--headless`, no CDP pipe, no screencast viewport. DISPLAY and
WAYLAND_DISPLAY are inherited (the preview is the user-facing window) AND
the session's display SOCKETS are bound into the namespace by
`display_binds()` (`web_preview_command` / `WebPreview::displayBinds`):
the namespace shadows /tmp with an empty tmpfs and leaves /run unbound, so
the inherited variables would otherwise point at sockets that do not exist
inside the sandbox and the window could never connect to any display. A
local X11 DISPLAY binds the /tmp/.X11-unix socket dir; a Wayland session
binds only its socket FILE under $XDG_RUNTIME_DIR — never the runtime dir
as a whole, which would leak kwallet/pipewire/ssh sockets to wallpaper JS;
an offscreen run (neither set) binds nothing. Binds whose source does not
exist are dropped (bwrap refuses a missing source).

The grant wiring: `WebPreview` holds a `PermissionsClient*`; on `play()`
it requests the wallpaper's permissions, snapshots `isGranted(network)`
into the launch, and on a `grantedChanged(network)` while the same
wallpaper is running relaunches the browser with the new argv. The
relaunch is ASYNC and pending-flagged: `QProcess::kill()` is async and
`start()` on a non-NotRunning process is a silent no-op (the first form
called start() immediately and silently DROPPED the correcting launch,
leaving the wrong network flag forever); the decision predicate
`wantsGrantRelaunch()` sets a pending flag, and the stateChanged handler
starts the new instance only once the old one is actually NotRunning —
one relaunch per change, and `launch()` re-reads the grant, so a second
toggle before the restart simply updates the value it starts with.
`stop()` cancels the pending flag so a user stop is never followed by an
unexpected relaunch.

Unit coverage without spawning anything: `webpreviewtest.cpp` pins the
isolation (ro-bind pairs, `--unshare-net` default and its removal under
grant), the windowed flags (no headless/CDP prefixes), the display binds
(selection for X11 local/remote, Wayland socket-only, neither, plus the
env-driven argumentsFor path), the `wantsGrantRelaunch` decision, and the
pre-spawn validation gates (non-local URL rejected, non-`index.html` file
rejected); the kwe-core builder tests pin the same command shape
(`web_preview_command_is_windowed_with_the_m2b_isolation`,
`display_binds_*`, `web_preview_command_binds_a_present_wayland_socket…`).

### 7.6 Web preflight

`kwe preflight --web <root>` mirrors the `--path`/`--video` variants:
`--web` takes a wallpaper directory, `--path` takes a scene dir — exactly
one is required (anything else exits 1). The entry check is
`preflight_web`'s real behavior, not a directory rule: there is NO
canonicalization and no requirement that root itself be a directory — only
`root/index.html` is validated (not a symlink, a regular file, ≤ 16 MiB,
readable, and containing an `<html` root, case-insensitive); anything else
exits 2 with the reason. Like the other variants it never launches a
renderer, and network stays disabled in the report (grants are the
daemon's job, M2c).

### 7.7 M2d acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (221 tests, 0 failures at the M2d gate; 222 with the M2e scan test) |
| cmake | `cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug && cmake --build build/cmake --parallel` | pass |
| ctest | `cd build/cmake && ctest --output-on-failure` | pass (7/7, incl. `kwe-web-preview-test`) |
| qmllint | `qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-manager apps/kwe-manager/qml/*.qml` | pass |
| smoke-web-compromise | `./scripts/smoke-web-compromise.sh` | pass (matrix above: scenario A boxes 1-4 green, scenario B box 1 orange / boxes 2-4 green; both argv proofs; plasmashell pid unchanged) |
| smoke-web | `./scripts/smoke-web.sh` | pass (11 cases, regression) |
| smoke-ui | `./scripts/smoke-ui.sh` | pass (regression) |

## 8. M2e: web parity evidence and renderer-dependent catalog state

### 8.1 Goal

Close the milestone's evidence gaps: the parity ladder's step 1
(capability-manifest entry) had no `content.web` answer, the catalog still
called web wallpapers `BackendMissing` (stale — the M2b/M2c worker renders
them), and FEATURE_COMPATIBILITY.md's web rows carried no ladder assessment.
M2e delivers: (1) a `kwe-web-renderer --probe` capability-manifest entry that
boots the real sandboxed browser and answers the CDP `Browser.getVersion`
query; (2) a `kwe diagnose` web lane mirroring the M1e video lane; (3) the
scan.rs web row flipped to `RendererDependent` with an honest detail string,
pinned by a new test; (4) FEATURE_COMPATIBILITY.md `content.web` and
`runtime.audio-web-64` rows moved to `partial (M2e)` with six-step ladder
assessments; (5) an empirical cookie/localStorage persistence check backing
the tmpfs-profile semantic difference; (6) this close-out section, including
the M2 exit-gate summary.

### 8.2 The probe (`kwe-web-renderer --probe`)

`crates/kwe-web-renderer/src/main.rs` adds `--probe` (the `--output` and
`--content` args become `required_unless_present = "probe"`):

- `probe_report()` writes a throwaway content root (an animated `PROBE_PAGE`
  as `index.html` under `$TMPDIR/kwe-web-probe-<pid>`), then
  `probe_browser_version(content)` spawns the real M2b sandbox
  (`spawn_browser` with `FrameSpec::new(160, 90)`, network off) and verifies
  three boot-class round trips on the CDP pipe: (1) `Browser.getVersion`
  (boot + pipe), reading the version from `result.product` — Chromium 151
  puts the version there (`"Chrome/151.0.7922.137"`), the legacy `browser`
  field is kept as a fallback; (2) a one-frame `Page.startScreencast`
  capture, received and acked with `Page.screencastFrameAck` (the §1.6
  contract) — proving the paint -> capture -> pipe -> ack path the worker
  runs on; the probe page animates because a page that paints identical
  pixels stops the compositor (§5.7); (3) `Runtime.evaluate("1+1")`
  answering `2` — the worker's own heartbeat probe (§5.3). The report
  carries the measured results (`screencast_frames`, `heartbeat_value`),
  so the manifest fields are exercised, not declared.
- Every path closes the CDP pipe and `reap_browser()`s the bwrap process
  group: a bounded try_wait loop to a 5 s deadline, then
  `libc::kill(-pid, SIGKILL)` on the group — a probe never leaves a browser
  behind.
- Backend reject = exit 73 with bounded diagnostics:
  `event=renderer.web.backend_reject detail=<reason>` and
  `event=renderer.web.backend_reject exit_code=73` — the same exit code the
  worker uses for backend rejection. The probe covers **boot-class
  failures** (missing bwrap/chromium, a sandbox that cannot boot, a browser
  that never answers the pipe, a capture round-trip that never produces a
  frame, a heartbeat that does not answer). The daemon's per-kind rlimit
  envelope (§5.4) is applied by the supervisor at spawn, not by the probe,
  so an rlimit-induced failure (e.g. the 128 GiB VA floor) can pass the
  probe yet fail a supervised launch — that gap is bounded by the envelope's
  own validation and the supervisor's failure budget. Measured
  (2026-08-19): missing bwrap on PATH → `detail=spawning bwrap`, exit 73; a
  cold probe completes in **≈0.6 s total**, far inside the 15 s budget
  `kwe diagnose` grants it. Backend versions pinned: Chromium 151.0.7922.137
  (this machine), bubblewrap **0.11.2** (`bwrap --version`).

Probe output (2026-08-19 run):

```json
{"backend":"chromium","browser_version":"Chrome/151.0.7922.137","heartbeat":true,"heartbeat_value":"2","protocol_version":"1.3","sandbox":"bwrap","screencast":"jpeg-q80","screencast_frames":1}
```

### 8.3 The `kwe diagnose` web lane

`crates/kwe-cli/src/main.rs` refactors the M1e video probe runner into a
shared `ProbeRun` outcome plus `run_renderer_probe(binary, deadline)` (the
video lane's `probe_video_backend()` becomes the first caller) and adds a
`probe_web_backend()` lane with the same Report/Missing/Failed{Hung} mapping.
A hung probe is killed after its deadline ("did not finish within 15 s;
killed") — the probe child is spawned into its own process group and the
kill targets that group (negative pid, mirroring the renderer's
`reap_browser` escalation), so a hung probe cannot leave a bwrap ->
chromium tree behind. Measured output (2026-08-19):

```
video backend: {"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}
web backend:   {"backend":"chromium","browser_version":"Chrome/151.0.7922.137","heartbeat":true,"heartbeat_value":"2","protocol_version":"1.3","sandbox":"bwrap","screencast":"jpeg-q80","screencast_frames":1}
```

Failure paths verified: the web renderer removed from beside the binary →
"kwe-web-renderer not found beside this binary; run it with --probe
manually"; bwrap absent from PATH → the probe itself exits 73 with its
bounded diagnostics (the lane reports Failed with exit 73).

### 8.4 Catalog state (scan.rs)

```rust
ProjectKind::Web => (
    Compatibility::RendererDependent,
    "sandboxed Chromium worker; network and audio off until granted",
),
```

The old `BackendMissing` value was stale: M2b/M2c's supervised sandboxed
Chromium worker renders web wallpapers, so the row is honest only as
renderer-dependent (a machine without bwrap/chromium genuinely cannot serve
them). The new `marks_web_projects_renderer_dependent` scan test scans a
synthetic web fixture (`project.json` `{"title":"Synthetic web","type":"web","file":"index.html"}`
plus an `index.html`) and asserts kind Web, `RendererDependent`, and the
exact detail string.

### 8.5 FEATURE_COMPATIBILITY.md rows and ladder assessment

`content.web` is now `partial (M2e)` and `runtime.audio-web-64` is now
`partial (M2e)` — full cells in FEATURE_COMPATIBILITY.md. Six-step ladder
assessment for `content.web` (ladder text in FEATURE_COMPATIBILITY.md,
"How parity is proven"):

| Step | Assessment | Citation |
| --- | --- | --- |
| 1. renderer/service capability-manifest entry | **met** | `kwe-web-renderer --probe` (§8.2) reports backend/version/protocol/sandbox/screencast/heartbeat; `kwe diagnose` prints the same lane (§8.3) |
| 2. original synthetic fixture exercising success and failure | **met** | smoke-web.sh (11 cases: canary promote, grant control and revocation, keepalive, pointer oracle, audio-grant gating, kill -9, missing root, busy-loop exit 73, quarantine, wedged heartbeat), smoke-web-compromise.sh (4 attempts x 2 scenarios, M2d §7.2), the scan fixture (§8.4) |
| 3. automated protocol/state tests and an image/event oracle | **met** | Rust: 222 tests, 0 failures (`cargo test --workspace --all-targets` — kwe-cdp fake-peer suite, daemon protocol tests, kwe-core websandbox builder tests, the M2e scan test); C++: 7/7 (`ctest` — kwe-daemon-activator, kwe-playlist-controller, kwe-permissions-client, kwe-web-preview); plus the `scripts/frame-read.py` oracle over the seqlock frame snapshot |
| 4. UI presentation for supported/partial/unavailable/failed states | **not met** | scoped to the M4 UI milestone, exactly as recorded for `content.video` in M1e |
| 5. backend/version/hardware evidence | **met** | probe report above; Chromium 151.0.7922.137, bubblewrap 0.11.2; V8 VA floor / 128 GiB default (M2b §5.4); decode latencies 60 µs / 1435 µs (M2b §5.5); `/proc` argv proofs (M2d §7.3) |
| 6. documentation of intentional semantic differences | **met** | FEATURE_COMPATIBILITY.md `content.web` cell: headless screencast geometry, keepalive-vs-empty frames, heartbeat, `audio_web` cadence and grant gating, file-scheme isolation boundary, tmpfs-profile cookie/localStorage behavior |

`runtime.audio-web-64`: the same five steps hold (producer evidence:
smoke-audio cases 1–3; delivery evidence: smoke-web case 4 — grant-gated
evaluation at ≤ 30/s, never evaluated without the grant); step 4 remains M4.

### 8.6 Cookie/localStorage persistence (empirical, tmpfs profile)

The FEATURE cell claims "no cookie persistence across runs". Verified with a
throwaway two-run supervised fixture (not committed): run 1 writes
`localStorage` and `document.cookie`; run 2 (fresh `--user-data-dir` tmpfs
profile) paints RED if either value survived; a within-run control box proves
the storage mechanism itself works on every run. Result (2026-08-19): both
runs stay red-free while the within-run control paints — localStorage never
survives a run boundary, so the tmpfs profile genuinely is throwaway. The
cookie half is stronger than the profile alone: `document.cookie` does not
round-trip on `file://` in Chromium 151 at all (set + read-back in the same
load returns empty, no exception), so the semantic-difference wording in
FEATURE_COMPATIBILITY.md states both facts.

### 8.7 M2 exit-gate summary

PROJECT_PLAN.md's M4 section sets the exit gate: *"renderer compromise tests
cannot read arbitrary home files or crash Plasma; disabling audio tears down
capture immediately."* Clause by clause:

| Clause | Evidence | Fully demonstrated? |
| --- | --- | --- |
| compromise tests cannot read arbitrary home files | smoke-web-compromise.sh attempt 2: cors-mode fetch and traversal XHR of `file:///etc/passwd` both fail — GREEN in scenario A and B (M2d §7.2); the sandbox ro-binds only `/usr` `/etc` `/lib` `/lib64` `/bin` `/sbin` plus the content root (M2b §5.2), so no home path exists in the namespace; the traversal target exists inside the sandbox (M2d §7.4.3), so a painted RED would be a genuine host-file read | **yes**, with the honest note from M2d §7.2: the observable boundary under test is the browser's file-scheme/CORS isolation (cors-mode file: fetches are blocked from a `file://` page, its own URL included); the sandbox's non-bound-path contribution is asserted by the command-builder tests (`webpreviewtest.cpp::isolationByDefault`; kwe-core `defaults_to_network_isolation_and_read_only_content`, `web_renderer_command_carries_the_pinned_flags`), not by page-observable behavior |
| cannot crash Plasma | plasmashell pid guard in the two suites that run the web renderer — smoke-web (its guard line is one of the 11 cases) and smoke-web-compromise — asserts the pid is identical and alive before and after, observed with the browser and its pages live; the remaining lanes (smoke-audio, smoke-video, smoke-supervisor) rely on the supervisor fault envelope (canary, promotion, failure budget, quarantine, forced kill and reap) and the structural rule that untrusted parsing/rendering/web/audio never runs inside plasmashell (README) | **yes** |
| disabling audio tears down capture immediately | smoke-audio case 3: `renderer.stop` while audio flows → silent latest-wins drops, only the rate-limited `event=audio.forward.dropped` note (1–10 lines over 2 s), zero client errors — grant-gated delivery is immediate; smoke-audio case 5: daemon SIGTERM → the worker logs `event=audio.worker.stopped` and its pid vanishes with no `forced_kill` line; the daemon's audio supervisor gives the worker exactly `STOP_GRACE = 1 s` (SIGTERM, grace, then SIGKILL of the process group — `crates/kwe-daemon/src/audio.rs`), so graceful capture teardown is bounded by 1 s by construction. Note: since M2c the audio grant gates delivery for every wallpaper kind, so smoke-audio case 2's video identity carries the grant (`permissions.set {"wallpaper_id":"audio-case2","audio":true}`) and its ack-advance assertions run on the granted path | **yes** for grant-gated delivery and daemon-shutdown teardown; honest limit: capture itself is global in M2 (one shared `kwe-audio-worker` while any wallpaper runs, per M2c §6.2 — the grant gates delivery, not capture), and a per-wallpaper capture switch belongs to the M4/M5 per-output work |

### 8.8 M2e acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (scan suite incl. `marks_web_projects_renderer_dependent`) |
| cmake | `cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug && cmake --build build/cmake --parallel` | pass |
| ctest | `cd build/cmake && ctest --output-on-failure` | pass |
| smoke-web | `./scripts/smoke-web.sh` | pass (11 cases, regression) |
| smoke-web-compromise | `./scripts/smoke-web-compromise.sh` | pass (regression) |
| smoke-audio | `./scripts/smoke-audio.sh` | pass (regression) |
| smoke-video | `./scripts/smoke-video.sh` | pass (regression) |
| smoke-supervisor | `./scripts/smoke-supervisor.sh` | pass (regression) |
| probe | `./target/debug/kwe-web-renderer --probe` | pass (≈0.6 s, report in §8.2) |
| diagnose | `./target/debug/kwe diagnose` | pass (web lane prints the report; Missing/Failed/Hung diagnostics distinct) |
