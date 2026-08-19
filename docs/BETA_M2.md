# BETA_M2 — Wallpaper renderer (CDP) milestone report

Status: **M2a complete** (spike + client + smoke); M2b (renderer worker) not started.

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
- A late ack after silence resumes capture (measured).
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
| pipe close -> chromium exit | rc=0 within ~50 ms |

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
5. Closing the client's pipe ends is the teardown signal (exit rc=0, ~50 ms);
   no CDP "close" message exists.
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
teardown: close both pipe ends; chromium exits rc=0 within ~50 ms
```

## 3. crates/kwe-cdp

An original, minimal, bounded CDP client; no async runtime, no threads in the
library. Modules: `codec` (ASCIIZ framing + 4 MiB bound decoder), `transport`
(nonblocking pipe pump with `poll(2)` deadlines, bounded write backlog),
`connection` (id correlation, monotonic u32 ids, bounded event queue 64
drop-oldest with a drop counter), `client` (request_browser/request_session,
5 s default timeout). Errors: `Timeout`, `ParseError`, `OversizedMessage`,
`Io`. All 35 unit tests run against in-memory socketpair fake peers — no
browser needed. The spike example drives real chromium and prints a JSON
summary; the smoke script asserts on it.

## 4. M2a acceptance

| Gate | Command | Result |
| --- | --- | --- |
| fmt | `cargo fmt --all -- --check` | pass |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| test | `cargo test --workspace --all-targets` | pass (35 kwe-cdp) |
| smoke | `./scripts/smoke-cdp.sh` | pass (5 assertions) |

Smoke evidence (2026-08-19 run):
`stall_frames=3, silence_confirmed=true` (phase A);
`frames=8, first_frame_after_start_ms=34, additional_after_ack_stop=3,
silence_confirmed=true, bytes_per_frame_avg=552` (phase B); both chromium
instances exited rc=0.
