# S1 adversarial review (TEXV + model/material port)

Worktree `kwe-s1`, branch `beta-s1-texv-models`, commits `4b9a181`, `4f47bab`,
`dc5da3d` on `beta-b4-apply-quarantine`. Originally a read-only review;
findings below are each marked with a **Resolution** line recording what
was fixed and how, applied in a follow-up commit
(`fix(s1): adversarial review findings`) on the same branch.

Counts: **3 MUST-FIX, 3 RECOMMENDED, 4 NIT** — all 10 addressed (see each
finding's Resolution line).

---

## MUST-FIX

### 1. `TEXB0004` mips skip the declared-size check → reachable panic

**Resolution: fixed.** `expected_mip_size` is now computed *inside*
`parse_mipmap` from the mip's own width/height, right after they're read —
uniformly across all four container versions, `TEXB0004` included; the
external 8-byte peek/rewind hack is gone. Added a defense-in-depth length
check (`rgba_full.len() >= mip0.width * mip0.height * 4`) before
`crop_top_left` runs, returning `TexvError` instead of trusting the
invariant transitively. New tests: `texb0004_happy_path_decodes_like_texb0003`,
`texb0004_short_lying_payload_is_refused_not_panicked` (isolates the
`parse_mipmap`-level check), `texb0004_short_payload_with_padding_does_not_panic_on_crop`
(the literal crash PoC from this finding, confirmed it now returns `Err`
instead), `texb0004_truncated_buffers_never_panic`. All 16 `texv::` tests
pass.

`crates/kwe-scene-renderer/src/texv.rs:640-663` (the peek-and-rewind that
computes `expected_size` before calling `parse_mipmap`) sets
`json_probe_pos = None` whenever `header.container_version ==
ContainerVersion::Texb0004`, which forces `expected = None` for every mip
of a `TEXB0004` container. Inside `parse_mipmap` (texv.rs:408-419), the
declared-uncompressed-size-vs-format/dimensions consistency check only
runs `if let Some(expected) = expected_size`, so for `TEXB0004` it never
runs at all — only the pixel-count-derived `alloc_cap` upper bound
(texv.rs:420-428) still applies, and that bound has no floor.

An attacker can therefore ship a `TEXB0004`/`FIF_UNKNOWN`/`ARGB8888` (or
`R8`/`RG88`) mip whose declared `uncompressedSize` is far *smaller* than
`width*height*bpp` (trivially satisfies `alloc_cap`), with
`compression == 0` so `parse_mipmap` returns exactly that many bytes
(texv.rs:436-438). `expand_raw` (texv.rs:532-542) then copies that short
payload verbatim (`payload.to_vec()` for ARGB8888; `chunks_exact`/`iter`
for R8/RG88, neither of which validates length). `crop_top_left`
(texv.rs:585-598) is then called with `mip0.width`/`mip0.height` as
`src_w`/`src_h` (the *declared*, not payload-derived, dimensions) and
indexes `rgba[row_start..row_end]` on every row when `real_w/h < src_w/h`
— the common case for a padded pow2-vs-real texture. With a short buffer
this is `rgba[16..272]` on a 16-byte `Vec` → **Rust panics on the slice
range**, crashing the scene-renderer worker process outright.

This directly contradicts the module's own claim ("nothing here panics on
hostile input... see the `tests` module for the fuzz-ish coverage") and
AGENTS.md's "parse untrusted metadata without exceptions escaping a
service boundary." It also matters more than an ordinary crash: the
worker's clean B2 refusal path (exit 73/74) is classified
`FailureKind::Refused` (no strike, `supervisor.rs:1638-1659`), but a panic
exits some other way and is classified `FailureKind::ProcessExit`
(`record_failure`, strike-counted) — repeated hostile/corrupt `.tex`
content down this path can accumulate strikes toward quarantine, which is
exactly the failure mode B4 was written to fix.

Note: BC/block formats do **not** share this exact crash — the
`texture2ddecoder` crate's generated decoders bounds-check `data.len()`
against `num_blocks_x*num_blocks_y*raw_block_size` and return `Err`
(`texture2ddecoder-0.1.2/src/macros.rs:14-16`) rather than panicking, so
`decode_block` (texv.rs:549-579) is safe even with a short payload. The
raw-format path (`expand_raw` → `crop_top_left`) is not similarly guarded.

Also note: the whole test module (texv.rs:742-1100) never constructs a
`TEXB0004` fixture — every test uses `TEXB0003`. The one container version
with the missing check has zero test coverage.

**Minimal fix**: compute `expected_mip_size` from *inside* `parse_mipmap`,
right after it reads the mip's own `width`/`height` (it already has
`header` in scope there) — this removes the fragile external
peek/8-byte-rewind hack in `decode_texv` entirely and makes the check
apply uniformly across all four container versions, `TEXB0004` included.
As defense in depth, also make `crop_top_left`/`expand_raw` check
`rgba_full.len() >= src_w as usize * src_h as usize * 4` and return a
`TexvError` instead of assuming the caller-supplied dimensions match the
buffer length.

### 2. Preflight and worker do not actually agree on model drawability

**Resolution: fixed.** New `crates/kwe-core/src/texvheader.rs` re-parses
the same fixed-size header fields `texv::parse_header` reads (magic,
format enum, container version, dimensions, image count) — no mip chain,
no LZ4, no BC decode, so it stays inside `kwe-core`'s dependency
boundary. `scenemodel::resolve_model` now runs this check on any resolved
texture bytes carrying the TEXV0005 magic and rejects a corrupt/truncated
header, an unimplemented format (when not FIF-tagged), or a texture whose
real dimensions alone would exceed a 256 MiB single-texture budget. New
tests: `scenemodel::resolvable_but_undecodable_texture_is_refused` plus 6
`texvheader::` unit tests (valid header, unimplemented format,
FIF-tagged-format-is-irrelevant, oversized dimensions, truncated/garbage
buffers, wrong sub-container magic). Existing fixtures using the
non-decodable placeholder `b"TEXV0005fake"` were updated to a real
structurally-valid minimal header (`texvheader::valid_minimal_texv`,
`#[cfg(test)] pub(crate)`, shared across `scenemodel.rs`/`pkg.rs`/`preflight.rs`
tests). Not fully closed: a corrupt LZ4 stream or a wrong per-mip
declared size deeper in the chain still only surfaces as a worker-side
`model_texture_skip` — the header check catches the common, cheap-to-detect
failure modes, as the review's own minimal-fix suggestion scoped it.

`crates/kwe-core/src/scenemodel.rs:123-197` (`resolve_model`) counts a
model layer as resolved once `lookup(texture_ref)` returns *some bytes* —
it never decodes them, and structurally cannot: the TEXV decoder lives in
a different crate (`kwe-scene-renderer`), not `kwe-core`. The worker's own
gate, `load_model_textures` (`crates/kwe-scene-renderer/src/main.rs:1385-1425`),
additionally requires `texv::decode_model_texture` to succeed *and* the
shared texture-memory budget (`texture_budget_allows`, main.rs:1408) to
accept it before counting the layer as drawable.

Concretely: a scene with one model layer whose resolved `.tex` bytes exist
but are corrupt, truncated, or an unimplemented format (`RGB888`,
`RGB565`, `RG1616f`, `R16f`, `RGBA1010102`, `RGBA16161616f`,
`RGB161616f` — `TextureFormat::block_bytes`/`raw_bytes_per_pixel` return
`None` for all of these, texv.rs:126-143) passes preflight
(`summarize_scene_objects_resolved`, `crates/kwe-core/src/sceneobjects.rs:242-274`,
calls `resolve_model(...).is_ok()` → `models_resolved += 1` →
`drawable() > 0` → `report.safe = true`). `wallpaper.apply` accepts the
apply and spawns the worker. The worker parses the identical scene,
`load_model_textures` fails to decode the same bytes, `drawable_objects`
stays 0 with `declared_objects > 0`, and the worker refuses
(`EXIT_NO_DRAWABLE_CONTENT`, main.rs ~840-855) — the apply transaction
rolls back and the previous wallpaper stays on screen. The user sees an
"accepted" apply silently do nothing.

The module doc for `scenemodel.rs` states this file exists so "preflight
and the scene worker... agree on whether a model layer can draw anything
before any pixel decode happens (B2 honesty contract)" — that claim is
not true for any texture that resolves-but-fails-to-decode. (This is a
documented trade-off, not a hidden one — `docs/SCENE_FORMAT_V1.md`'s
"Honesty (B2 contract)" section says outright that "preflight only needs
to know a texture's bytes exist, never decode them" — but the trade-off
is exactly the preflight/worker disagreement the review was asked to
check for, and it reopens the accepted-then-silently-rolled-back UX class
B2/B4 exist to prevent.)

**Minimal fix**: give preflight a cheap, decode-free check that still
predicts the worker's outcome for the common failure modes — at minimum,
parse the TEXV *header* (magic + format enum + dimension sanity, no
LZ4/BC work) in `resolve_model`'s texture step and reject unsupported
formats/malformed containers there, sharing that header-only logic
between the two crates. Short of that, narrow the doc claim so it stops
asserting an agreement that doesn't hold, and make sure the resulting
worker-side refusal is cheap and always lands cleanly as `Refused` (never
a panic — see #1).

### 3. Unbounded per-object filesystem I/O during preflight (daemon-side DoS)

**Resolution: fixed.** `summarize_scene_objects_resolved` now stops
attempting resolution once it has tried `MAX_MODEL_RESOLUTIONS` (256,
mirrors `kwe-scene-renderer::layers::MAX_LAYERS`) model objects — objects
beyond the cap are left unresolved rather than attempted. Separately,
`confined_read`'s contract changed to require an already-canonicalized
root (documented precondition), and both lookup-closure constructors
(`preflight::file_lane_asset_lookup`, `pkg::pkg_lane_asset_lookup`) now
canonicalize the scene-directory/assets-root paths exactly once when the
closure is built, not per model object resolved. New test
`sceneobjects::model_resolution_attempts_are_capped` builds 306
deliberately-resolvable model objects and asserts both that
`models_resolved` stops at exactly 256 and that the lookup closure is
never called more than `256 * 3` times.

`summarize_scene_objects_resolved` (`crates/kwe-core/src/sceneobjects.rs:242-274`)
calls `resolve_model` for *every* object classified `SceneObjectKind::Model`
in the scene, with no cap on how many such objects a `scene.json`/`scene.pkg`
may declare — only the pre-existing whole-file size caps
(`MAX_SCENE_JSON_BYTES` / `MAX_PKG_ENTRY_BYTES`, ~16 MiB) apply, which still
permit on the order of 10^5–10^6 minimal objects (e.g.
`{"name":"m","image":"models/m.json"}` is ~40 bytes). Each `resolve_model`
call does up to three `confined_read` calls (model.json, material.json,
texture), and each `confined_read` (`scenemodel.rs:207-237`) does two
`canonicalize()` calls plus a `symlink_metadata()` — several stat syscalls
— before it can fail. This all runs **synchronously inside the trusted
`kwe-daemon` process**, as part of validating a single `wallpaper.apply`/
`renderer.start` request, before any subprocess/sandbox boundary is
involved.

A crafted scene.json with ~300k minimal, unresolvable model objects turns
one apply request into hundreds of thousands of blocking stat/open
syscalls executed inline in the daemon's request path — a straightforward
CPU/IO amplification DoS against the control-plane process itself, not the
sandboxed renderer. The pre-S1 static classifier
(`summarize_scene_objects`) has no such cost (pure in-memory, O(1)/object);
this I/O-per-object cost is new in S1.

**Minimal fix**: cap the number of model objects
`summarize_scene_objects_resolved` will actually attempt to resolve (e.g.
the same cap `crates/kwe-scene-renderer/src/scene.rs`'s `parse_objects`
already enforces on the layer-registration side, `MAX_LAYERS`), so
preflight's worst-case cost is bounded independent of how many objects a
hostile scene.json declares.

---

## RECOMMENDED

### 4. Per-mip decompression cap is not a per-texture cap

**Resolution: fixed, as part of #1.** `parse_mipmap` now takes a `keep:
bool`; only image-0/mip-0 (the one mip ever kept) is LZ4-decompressed or
copied. Every other mip is still structurally validated (dimensions,
declared-size consistency, allocation cap) but its payload bytes are
advanced past with `Reader::take` and discarded — no `lz4_flex::decompress`
call, no owned-buffer allocation. A 256-mip container can no longer force
more LZ4 work than the one mip that's actually used.

`decode_texv` (texv.rs:606-720) fully LZ4-decompresses (or raw-copies)
*every* mip of *every* image — up to `MAX_IMAGE_COUNT * MAX_MIPMAP_COUNT`
= 256 mips (texv.rs:43-47) — even though only image-0/mip-0 is ever kept
(`first_image_mip0`, texv.rs:625-668); every other decompressed `Mipmap`
is allocated and immediately dropped at the end of its loop iteration.
Each mip's uncompressed size is bounded by `alloc_cap` (~`MAX_MIP_PIXELS
* 4 + 4096` ≈ 67 MB, texv.rs:420-428), so a single crafted container with
all 256 mips near that cap can force **up to ~17 GB of cumulative LZ4
decompression work** from a file that can be small on disk (highly
compressible payloads, e.g. runs of zeros, reach large LZ4 ratios). Bounded
per-mip, but not bounded in total — a real decompression-bomb amplifier
across mips/images that the module's own doc comment claims to defend
against ("a lying declared size is exactly the decompression-bomb vector
this bound exists for") without actually capping the sum. Lower severity
than #1-#3 because it runs inside the resource-limited, supervised
scene-renderer worker (a wedge here should eventually hit the supervisor's
own timeouts), but it is wasted, attacker-controlled CPU work that AGENTS.md's
"bound... allocations, retries" rule is meant to prevent.

**Minimal fix**: skip mips other than image-0/mip-0 entirely —
`reader.take(compressed_size)` to advance past them without invoking
`lz4_flex::decompress`, and skip the per-mip consistency check for any mip
that won't be kept. This pairs naturally with the fix for #1 (only
image-0/mip-0 ever needs `expected_size` computed at all).

### 5. Raw `renderer.start` preflight uses `assets_dir=None` but the spawned worker gets the real one

**Resolution: fixed.** `StartSpec::try_from` no longer calls
`into_validated` internally (field-mapping only, unvalidated); the
`"renderer.start"`/`"renderer.retry"` RPC arm now calls
`spec.into_validated(apply.and_then(|handle| handle.scene_assets_dir()))`,
reading the daemon's configured assets root through a new
`pub fn ApplyHandle::scene_assets_dir(&self) -> Option<&Path>` accessor —
the same value `spawn_worker` already forwards to the worker
unconditionally. New test
`renderer_start_scene_preflight_honors_the_configured_assets_dir` proves
the same `StartSpec` refuses with `assets_dir: None` and accepts with the
configured assets root, exercising the exact two-step the RPC arm now
runs.

`crates/kwe-daemon/src/main.rs:1078-1193`
(`TryFrom<RendererStartParams> for StartSpec`) calls
`spec.into_validated(None)` unconditionally, so scene preflight run
through the low-level `renderer.start` RPC never sees the daemon's
configured `--wallpaper-engine-assets`. But `SupervisorRuntime`'s spawn
path (`crates/kwe-daemon/src/supervisor.rs:1172-1178`) unconditionally
forwards `self.config.scene_assets_dir` to the worker regardless of which
RPC validated the spec. Net effect: a model-layer scene that would
actually resolve and draw fine at runtime can be needlessly rejected at
preflight when started through `renderer.start` (as opposed to the
primary `wallpaper.apply` path in `apply.rs`, which does thread the real
assets dir through both preflight and spawn). Fails closed, not open, so
lower severity than 1-3, but it's an avoidable inconsistency between the
two entry points into the same supervisor.

**Fix**: thread the daemon's `scene_assets_dir` through this `TryFrom`
conversion the same way `apply.rs` does.

### 6. Silent no-op for a misconfigured explicit `--assets-dir`

**Resolution: fixed**, scoped to daemon/CLI startup as the review's fix
suggested. `kwe-daemon`'s `--wallpaper-engine-assets` and `kwe preflight
--assets-dir` now check `.is_dir()` on an explicit value and log a
warning (`event=daemon.config.invalid_assets_dir path=... detail=not-a-directory`
/ a `warning:` line on stderr) instead of silently treating it as
`None`; the auto-detected default was already validated
(`default_wallpaper_engine_assets_dir` only returns existing directories).
The scene worker's own `--assets-dir` (fed by the already-validated
daemon config, or set directly for standalone/manual invocations) is left
unvalidated — out of the review's stated scope.

`default_wallpaper_engine_assets_dir` (`crates/kwe-core/src/scan.rs:132-140`)
only validates the *auto-discovered* candidate with `.is_dir()`. An
operator-supplied `--assets-dir` (kwe-cli) / `--wallpaper-engine-assets`
(kwe-daemon) is passed straight through with no existence/directory check;
if it's wrong, `confined_read`'s `root.canonicalize()` just fails on every
lookup and every model layer silently never resolves, with no log line
naming why. Not a containment issue (canonicalize+`starts_with` still
holds), purely an operability gap.

**Fix**: validate (exists, is a directory, canonicalize) the explicit
value at daemon/cli startup and log a warning if it doesn't check out.

---

## NIT

### 7. Dead symlink check in `confined_read`

**Resolution: fixed**, incidentally while reworking `confined_read` for
finding #3 (the already-canonical-root precondition). The dead
`symlink_metadata(&canonical).file_type().is_symlink()` check is removed;
the doc comment now states the `starts_with` check is the sole
containment defense.

`crates/kwe-core/src/scenemodel.rs:230` checks
`metadata.file_type().is_symlink()` on `symlink_metadata(&canonical)`,
but `canonical` is already the fully symlink-resolved output of
`candidate.canonicalize()` (scenemodel.rs:225) — it can never itself be a
symlink, so this check is always false and does nothing. The actual (and
sufficient) defense is the preceding `canonical.starts_with(&root_canonical)`
check (scenemodel.rs:226), confirmed by the existing
`confined_read_stays_inside_root_and_rejects_traversal_and_symlinks` test.
Not unsafe, just misleading — either drop the dead check or fix the
doc comment (scenemodel.rs:199-206) so it doesn't imply this line does
the symlink defense.

### 8. Stale doc comment on `SceneObjectKind::Model`

**Resolution: fixed.** The doc comment now describes the S1 resolve →
draw behavior and the skip-never-reject contract for a malformed model
object, replacing the pre-S1 "scene3d, BETA_M3h... skipped before any
validation" text.

`crates/kwe-core/src/sceneobjects.rs:26-29` still reads "image references
a `.json` model instance: scene3d, BETA_M3h... Skipped by the renderer
before any validation." That hasn't been true since this slice —
`parse_model_layer` (`crates/kwe-scene-renderer/src/scene.rs:117-145`) now
registers a layer for every well-formed model object. Not touched by this
diff; worth updating alongside it.

### 9. `TEXB0004` has zero test coverage

**Resolution: fixed, as part of #1.** `texb0004_happy_path_decodes_like_texb0003`,
`texb0004_short_lying_payload_is_refused_not_panicked`,
`texb0004_short_payload_with_padding_does_not_panic_on_crop`, and
`texb0004_truncated_buffers_never_panic` added to `texv.rs`'s test
module.

Every fixture builder in `texv.rs`'s `#[cfg(test)] mod tests`
(texv.rs:742-1100) constructs a `TEXB0003` container; none exercises
`TEXB0004`'s extra ignored fields + bounded editor-JSON string + trailing
`u32` (texv.rs:367-372), which is exactly the container version affected
by #1. Add a `TEXB0004` fixture to the truncation/garbage sweep and a
size-lie case specific to that version.

### 10. Duplicated 64 MiB asset-read cap

**Resolution: fixed.** A single `pub const scenemodel::MODEL_ASSET_READ_CAP`
now backs both `preflight::file_lane_asset_lookup` and
`pkg::pkg_lane_asset_lookup`; the two separate `const` definitions are
gone.

`MODEL_ASSET_READ_CAP` (`crates/kwe-core/src/preflight.rs:62`) and the
pkg-lane `READ_CAP` (`crates/kwe-core/src/pkg.rs:816`) are two separate
`const` definitions of the same 64 MiB value rather than one shared
constant. They agree today; a future edit to one lane and not the other
would silently desync the file-lane and pkg-lane caps.
