# Scene format v1 and SceneScript API

Status: **implemented (M3a)** for the items marked below, with the
**scene.pkg archive reader (M3b)** added for the items marked M3b;
everything else in the table is planned and reserved, not implemented. This
document describes what the worker (`kwe-scene-renderer`, the daemon's
`scene` kind) accepts and what script surface it runs. The scene entry
format is the foundation slice of the original SceneScript engine per ADR
0001.

## Provenance

This is an original implementation. The scene format and script API are
inspired by the behavior of open-wallpaper-engine and linux-wallpaperengine
(and through them, the original wallpaper engine scene format), which are
consulted **as behavior references only** — for the shape of `scene.json`
and the script entry points `init` / `update` / `resized`. No code is
copied from either project or from wallpaper engine itself: the schema
parser, the QuickJS engine wrapper, and the Vulkan compositor are written
for this crate (SPDX Apache-2.0 headers). The GPL-licensed reference
projects contribute no code to this repository (see THIRD_PARTY.yml for the
actual third-party components: rquickjs 0.12.2 + vendored quickjs-ng, both
MIT).

## scene.json

A UTF-8 JSON file, at most **16 MiB** (`MAX_SCENE_JSON_BYTES`). The root
must be an object. Parse failures — unreadable file, invalid JSON, wrong
shape, out-of-range values, or a `script` reference that is missing,
non-string, escapes the content root, is not a `.js` file, or exceeds the
2 MiB cap — are backend rejections: a bounded stderr diagnostic
(`event=renderer.scene.backend_reject kind=... detail=...`) and **exit 73**
before the canary, so the supervisor records `exit_code_73` and rolls back.

```json
{
  "general": {
    "clearcolor": [0.1, 0.1, 0.1, 1.0],
    "resolution": [1920, 1080],
    "fps": 30,
    "script": "script.js"
  }
}
```

| Field | Type | Default | Meaning in M3a |
|---|---|---|---|
| `general` | object (optional) | `{}` | Scene-wide settings. Must be an object when present. |
| `general.clearcolor` | `[r, g, b, a]` of finite floats in `0.0..=1.0`, exactly 4 entries, **or** the string form `"r g b"` (three space-separated finite floats in `0.0..=1.0`, alpha implied `1.0`) | `[0, 0, 0, 1]` | The color the worker clears every frame — unless the script writes `Engine.clearcolor`, which is read back after every `update()`. The string form is what Wallpaper Engine actually writes: **59 of 60** corpus scene.json entries use it (e.g. `"clearcolor": "0.7 0.7 0.7"`, one with five-digit precision); the property-wrapped object form `{"user": ..., "value": ...}` (1 of 60) stays rejected until user properties arrive in M3c+. |
| `general.resolution` | `[w, h]` of integers in `1..=8192`, exactly 2 entries (optional) | none | Parsed and validated, but **non-binding in M3a**: the worker always renders at the daemon-requested `--width`/`--height`. A mismatch is logged once (`event=renderer.scene.resolution scene=... requested=...`), not an error. |
| `general.fps` | finite float in `(0.0, 240.0]` (optional) | none | Same: parsed and validated, non-binding hint; a mismatch is logged (`event=renderer.scene.fps`), not an error. The pacing always comes from the daemon's `--fps`. |
| `general.script` | string (optional) | none | A path **relative to the scene.json's directory** (file scenes) or **an entry path inside the package** (pkg scenes, M3b). File scenes: resolved against the canonicalized content root so symlinks cannot escape it; must end in `.js` (a `.pkg` reference is rejected — the archive itself is only consumable through the M3b reader), must exist, be a regular file, and be at most **2 MiB** (`MAX_SCRIPT_BYTES`). Pkg scenes: see "scene.pkg" below. |

Anything else in `general` or at the root is ignored (future slices: layers,
effects, properties). Unknown top-level structure never fails the parse.

## scene.pkg

**Implemented (M3b)** in `kwe-core` (`crates/kwe-core/src/pkg.rs`) and wired
into the worker's `--content` path: a `.pkg` content is opened by
`PkgReader`, its unique `scene.json` entry is parsed in memory, and — when
`general.script` names a package entry — that entry is extracted into a
private `kwe-scene-script-<pid>` directory under the worker's HOME (mode
0700) and loaded like a file scene's script. Textures, models, and other
assets are **M3c+** and are deliberately not extracted; the renderer logs
`event=renderer.scene.pkg entries=N script_entry=...`.

### Verified layout

Triple-confirmed against byte-level inspection of ~60 real Workshop scene
packages (20 distinct `PKGV` versions, 3128 entries), the public QuickBMS
extractor script (0.1a), and the BSD-3-licensed RePKG implementation
(behavior references only per ADR 0001 — no code copied):

```text
u32 LE  magic-string length in bytes (8 on the corpus)
bytes   magic string: b"PKGV" + 4 ASCII digits, e.g. "PKGV0001"
u32 LE  entry count
  per entry:
    u32 LE  path length in bytes
    bytes   UTF-8 path, e.g. "scene.json"
    u32 LE  payload offset, relative to the start of the data section
    u32 LE  payload size in bytes
data section: raw concatenated payloads
```

Offsets are relative to the data section (right after the table), and
payloads are stored **raw**: the corpus contains no compressed entry
(JSON descriptors, TEXV0005 textures, and raw pixel data only — verified
with an independent pure-Python LZ4 block decoder), and none of the
reference implementations decompresses anything. The QuickBMS script notes
"PKGV0001, PKGV0006 and so on are all the same format"; the layout is
version-independent, so any `PKGV` + 4 digits is accepted.

### The LZ4 question (honest variant note)

The M3b brief described "LZ4-compressed payloads in the commonly documented
format". The evidence above **disproves that premise** for every package we
can see, so raw is the primary path. To cover the possibility that some
publisher-side tool produced frame-compressed packages, the reader
additionally recognizes the LZ4 frame magic (`04 22 4D 18`) at a payload's
start and decompresses it — with the output cap (64 MiB per entry)
enforced **during** decompression (the frame decoder is wrapped in
`take(cap + 1)`; a declared content size or a bomb can never allocate past
the cap). A payload whose first four bytes are not the frame magic is
returned verbatim. The `compressed` flag on `PkgEntry` reports which path a
given entry takes.

### Bounds and validation

| Bound | Value |
|---|---|
| package size | 512 MiB |
| entry count | 65 536 |
| entry path | 512 bytes |
| entry payload | 64 MiB (read-time cap, before and during decompression) |
| total payload | 512 MiB (checked while parsing the table) |

The whole table is validated **at open**, before any payload is touched:
magic/version shape (structured `unsupported version` error for a
PKGV-prefixed magic that is not 4 digits), entry count, per-path length and
UTF-8, ranges (`offset + size` inside the data section, checked overflow),
total payload sum, and path safety. The open is TOCTOU-safe like every
other read in kwe-core: lstat (reject symlinks), `O_NOFOLLOW` open, fstat
re-check on the fd, parse from the fd, size re-check after parsing. All
reads stay pinned to the fd.

### Path-traversal policy (documented decision)

M3b ships `read_entry` only — no extract-to-disk API — so a hostile entry
path cannot write outside the package on this slice. The table is still
validated at open: **empty paths, NUL bytes, backslashes, absolute paths,
and `..` components are rejected** (`PathTraversal`), so a future
extractor cannot inherit a hostile table. Callers that resolve entry paths
(the worker's script extraction) additionally confine resolution: the
script reference must be relative, `.js`, and match exactly one entry
(case-insensitive, literal or `/<name>`-suffixed); the extracted file is
always written as `script.js` under a pid-unique 0700 directory the worker
owns. Nothing from a package is ever resolved against the host file
system.

### Worker behavior

* `--content` ending in `.pkg` (case-insensitive) selects the archive path.
* Exactly one entry ending in `scene.json` (case-insensitive) is required:
  zero matches with a `scene.pkg` entry present means a **nested archive**
  (`event=renderer.scene.backend_reject kind=Pkg detail="nested scene.pkg
  inside the package is not supported (M3b)"`) — nested packages are
  refused, not recursed; zero matches otherwise, or several matches, are
  likewise exit 73.
* The scene.json entry is read bounded to 16 MiB and parsed by the same
  core as file scenes (unknown keys tolerated, `general` rules identical).
* `general.script` resolves against the package table; the entry is read
  bounded to 2 MiB, extracted, and loaded by the script engine. A script
  reference that is empty, `.pkg`, non-`.js`, absolute, traversing, missing
  from the table, or ambiguous is a backend rejection (exit 73).
* Archive failures (corrupt magic/table, truncated data, bounds, traversal
  entries) are backend rejections: `kind=Pkg`, exit 73 before the canary,
  so the supervisor records `exit_code_73` and rolls back. Preflight
  (kwe-core `preflight_scene`) runs the same structural validation for a
  `.pkg` content and rejects a corrupt archive before the worker spawns
  (this closes M1 finding G12, which previously let any `.pkg` through).

## Script execution model

One QuickJS runtime + context per worker (rquickjs 0.12.2, MIT, vendored
quickjs-ng 0.15.1 — THIRD_PARTY.yml). Bounds:

| Bound | Value | Behavior |
|---|---|---|
| heap | 64 MiB (`Runtime::set_memory_limit`) | JS "Out of memory" exception → bounded `memory_limit` diag → **exit 71** (the renderer never survives an OOM; it cannot render meaningfully) |
| stack | 4 MiB (`Runtime::set_max_stack_size`) | runaway recursion → contained exception, renderer keeps the last state |
| per-update budget | 8 ms soft / 33 ms hard (wall clock in the interrupt callback — rquickjs exposes no step counts; docs/BETA_M3.md risk 1) | soft: frame skipped, bounded `event=renderer.scene.script_timeout kind=soft`; hard: uncatchable exception, counted as `hard_timeout`; the renderer always keeps publishing the last good frame |
| `dt` | clamped to `[0.0, 1.0]` | a hung producer cannot feed a huge dt downstream |
| console | 30 lines per 10 s window, 512 bytes per line | `event=renderer.scene.console`, `console_dropped` on overflow |

Script exceptions are contained: caught, counted (`script_errors` in
`event=renderer.complete`), logged at most once per error class per 30 s
window, and never kill the renderer. `Engine.clearcolor` reads that fall
back on the current color keep a throwing `update()` from corrupting state.

## SceneScript API coverage matrix

Status key: **implemented (M3a)** — in this slice and covered by tests;
*planned* — reserved for M3b–M3k; **not in scope** — explicitly out of the
beta scope. API items follow the behavior of the reference implementations
(see Provenance), restricted to what a worker can do with no window and no
image compositing in M3a.

### Globals

| API | Status | Notes |
|---|---|---|
| `Engine.frametime` | **implemented (M3a)** | seconds since the previous update, per update (number) |
| `Engine.fps` | **implemented (M3a)** | the pacing the daemon asked for, fixed (number) |
| `Engine.resolution` | **implemented (M3a)** | `{x, y}` — the pixel size the worker renders at, fixed (read-only object) |
| `Engine.clearcolor` | **implemented (M3a)** | `{r, g, b, a}` (0..1 floats). **M3a-only bridge, not a wallpaper-engine API**: the worker reads it back after every `update()` and clears the frame to it. Planned to move to `thisScene.clearcolor` once scene objects arrive. Read-back falls back to the current color on non-finite/missing values. |
| `console.log / info / warn / error` | **implemented (M3a)** | rate-bounded, truncated to 512 bytes, surfaced on the worker's stderr ring |
| `thisScene` | *planned* | with scene objects (M3c–M3k) |
| wallpaper-engine globals beyond `Engine.*` | *planned* / **not in scope** | nothing else exists in M3a; each API joins via the coverage matrix here when implemented |

### Entry points

| API | Status | Notes |
|---|---|---|
| `init()` | **implemented (M3a)** | called once at script load, after evaluation; exceptions are contained (the script is still driven) |
| `update(dt)` | **implemented (M3a)** | called once per paced step, `dt` in seconds, clamped to `[0.0, 1.0]`; the return value is ignored (the renderer reads `Engine.clearcolor` back); a missing `update` renders the current color forever |
| `resized(w, h)` | **implemented (M3a)** | called once at script load with the daemon-provided size; dimensions are fixed in M3a — there is no live-resize path (docs/BETA_M3.md risk 7) |

### Scene objects and render model

| API | Status | Notes |
|---|---|---|
| layers, effects, text, particles, 3D models, properties | *planned* (M3c–M3k) | the parse tolerates extra keys but renders none of them |
| `.pkg` archives | **implemented (M3b)** | scene.json entry parsed in memory; script entry extracted to a private HOME dir; nested archives refused; textures/models are M3c+ |
| image assets | *planned* | no asset loading in M3a; the clear pass is the only draw |
| audio/pointer/media input in script | *planned* | the worker receives and acks the wire inputs (M1a plumbing, unchanged) but exposes none of them to the script in M3a |

## Output

Frames are premultiplied BGRA8888 through the shared frame mapping
(docs/FRAME_PROTOCOL_V1.md): a 64-byte `KWEFRM1` header, two BGRA8888
slots, generation-toggle publishing, keepalive re-publish so a script that
never changes the color cannot trip the supervisor's frame timeout. The
Vulkan attachment is `B8G8R8A8_UNORM` when supported (both validated
drivers) with an `R8G8B8A8_UNORM` fallback; the channel conversion is
identity for `B8G8R8A8` readback (bytes are already B,G,R,A) and a
[2,1,0] permutation for `R8G8B8A8`, both premultiplied with
`(v*a+127)/255` rounding — byte-exact per unit test.

## See also

- docs/BETA_M3.md — the M3a slice: goal, acceptance evidence, exit codes,
  open risks (interrupt-budget deviation, llvmpipe determinism, reader
  staleness, loader lifetime).
- docs/adr/0001-original-vulkan-renderer.md — the architecture this slice
  implements (ADR 0001 is binding).
