# SR-2 — asset VFS, typed IR, module split (decomposition)

Parent epic: `docs/Scene-Rendering-Plan.md` §8 SR-2. Started 2026-08-28,
immediately after the SR-1 epic closed (trunk @ `924b1ac`).

Child order: SR-2a → SR-2b → SR-2c → SR-2d+ → SR-2z. Each child is one
mergeable slice with its own implementation and adversarial-review passes,
following the same template as `docs/SR0.md`/`docs/SR1.md`.

- **SR-2a — the confined VFS** (this doc's filled contract below). Adds
  `crates/kwe-core/src/vfs.rs`: one type (`Vfs`) unifying the
  pkg-entries → scene-dir → assets-root lookup chain every asset family
  implements separately today. **Introduced without migrating any caller**
  — zero existing `resolve_*` call site changes in this slice.
- **SR-2b — IR core / unknown bag.** The typed intermediate representation
  scene loading is expected to produce (plan §8 SR-2's "typed IR"): a
  structured scene tree plus an explicit "unknown fields" bag so a
  not-yet-modeled scene.json property is preserved (not silently dropped)
  through to serialization, the way `sceneobjects::summarize_scene_objects`
  already tracks unknown keys/types for the taxonomy today, generalized
  into the IR itself.
- **SR-2c — differential adapter + first loader family.** Wires the SR-2a
  `Vfs` into ONE real call site (the family with the simplest existing
  resolver — image layers are the leading candidate, per this doc's
  semantics table) behind a differential test that runs the OLD resolver
  and the NEW `Vfs`-backed path over the same corpus fixtures and asserts
  identical outcomes (or documents+approves each divergence this doc's
  table already predicts, e.g. the componentwise symlink strengthening).
  Establishes the migration PATTERN the remaining families in 2d+ repeat.
- **SR-2d+ — per-family migrations.** One child per remaining asset family
  (shader source, video, model/material, particle files, scripts),
  following SR-2c's differential pattern exactly. Each child's own
  differential-test corpus run is that family's proof; no family's
  behavior is unified with another's without a differential test showing
  they were already the same, or a conductor decision recording the
  chosen behavior when they were not (mirrors SR-1c's own capability-gate
  precedent for "document the difference, do not average it").
- **SR-2z — orchestration/backend module extraction.** Once every family
  reads through the VFS, split `kwe-scene-renderer/src/main.rs`'s
  5000+-line load/resolve/orchestrate body (today's single file this
  epic's whole semantics table below cites almost entirely) into the
  smaller, IR/VFS-shaped modules SR-2b's typed IR made possible — the
  actual "module split" plan §8 SR-2 names, deferred to the END of the
  epic on purpose: splitting before every family is behind the same two
  seams (`Vfs`, the typed IR) would just move today's per-family drift
  into more files instead of removing it.

## Current resolver semantics (the SR-2a/2c+ differential baseline)

Every existing asset-resolution path in the codebase as of SR-2a, with its
exact rules and file:line citation. This table is authoritative for what
`Vfs`'s contract had to be at least as strict as (conductor decision (b));
where two resolvers disagree, both rows are listed and the disagreement is
called out rather than averaged.

### Source priority order

Every multi-source family (image/model/particle-file/shader) resolves in
the same fixed order: **pkg entry table → the pkg's own parent directory
(the "scene dir" for a packaged scene) → the Wallpaper Engine assets
root**, trying each in turn and taking the first source with ANY answer —
including a BAD one: today's code silently falls through past a
pkg-matched-but-oversized entry, or a scene-dir hit that fails its own
confinement check, to try the next source (`crates/kwe-scene-renderer/src/
main.rs:1274-1287,1292-1303,2859-2874` — every closure is `if let Ok(...)
{ return ...; }` chains with no error surfaced on a failed attempt, only
on running out of sources). **`Vfs` deliberately does NOT mirror this
fall-through-on-any-failure behavior** — see "Vfs deviations" below.

### Per-family rules

| Family | Confinement | Cap | Priority chain | Citation |
|---|---|---|---|---|
| Layer image (file scene) | Relative path; absolute/`..`/prefix components rejected; `root.join(reference).canonicalize()` must `starts_with(root)`; regular file only. No componentwise symlink check — a symlink that resolves back INSIDE root is tolerated (untested either way; analysis, not a proven behavior — no existing test builds that case). | `MAX_TEXTURE_SOURCE_BYTES` = 64 MiB (`textures.rs:91`) | scene dir only (this function IS the scene-dir lane) | `main.rs:1337-1371` (`resolve_layer_image`), test `main.rs:4858` |
| Layer image (pkg scene, model/particle-file lookups) | pkg entry table first (see "pkg entry matching" below), else `resolve_layer_image` against the pkg's own parent dir, else against the assets root — SAME function, SAME rules as the row above, just given two different roots in turn. **[SR-2d correction — this row conflated two DIFFERENT closures at SR-2a's own citation, an error in the original table, not a later regression]**: this pkg→pkg-dir→assets-root THREE-source chain is what `load_model_textures`/`load_particle_file_definitions`'s pkg-lane closures do (`main.rs`, current: the `if let Ok(index) = kwe_core::image_entry(...) {...} if let Some(dir) = &pkg_dir {...} assets_dir.and_then(...)` chain). The PLAIN image-layer family (`load_layer_textures`/`load_particle_textures`, pkg lane) does NOT: it is `kwe_core::image_entry` + `read_entry_bounded` with a bare `?`, pkg table ONLY, no directory fallback at all — confirmed by direct reading of the current source (SR-2d's own `legacy_trace_texture_pkg_lane`/`legacy_trace_model_asset_pkg_lane` functions, `kwe-scene-renderer/src/main.rs`, keep this distinction explicit for exactly this reason). SR-2d's migration to the shared `Vfs` therefore WIDENS the plain-image pkg-lane family's breadth to the full three-source chain — a THIRD divergence beyond the two SR-2a named below, evidence-gated the same way (SR-2d's own resolution-trace corpus test: 60/60 real items, zero observed difference). | Same 64 MiB, checked per source | pkg → pkg-parent-dir → assets root for model/particle-file; **pkg table ONLY** for the plain image-layer family (until SR-2d) | `main.rs:1246-1261` (pkg entries), `main.rs:1262-1304` (model layers, particle files) |
| Layer video | Relative path; absolute/`..`/prefix rejected (`video_candidate`, shared by both functions below); **production path** opens the leaf with `O_NOFOLLOW` (rejects a symlinked LEAF outright, whether or not it would resolve inside root), then canonicalizes + `starts_with(root)` (catches an escaping INTERMEDIATE symlinked dir — still tolerates one that stays inside root), then re-`fstat`s the canonical path and compares `(dev, ino)` against the already-open fd (TOCTOU: a file swapped between the open and the canonicalize is caught). A `#[cfg(test)]`-only mirror function skips the O_NOFOLLOW-open/dev-ino step and just canonicalizes + `starts_with` (documented in its own comment as weaker than production, kept only for tests/diagnostics). | `MAX_VIDEO_SOURCE_BYTES` = 160 MiB (`video.rs:70`), checked from metadata — the file is never read (libmpv opens the path) | scene dir only for file scenes; a pkg video is fully READ and extracted to a private worker-owned file (`extract_video`, a different operation — no "resolve to a pkg path" exists) | `video_candidate` `main.rs:3384-3410`; production `open_video_source` `main.rs:3447-3484`; test-only `resolve_layer_video` `main.rs:3416-3440`, test `main.rs:5084` |
| Shader source | The SAME pkg → pkg-parent-dir → assets-root chain as model textures, via `kwe_core::confined_read` for the two directory steps (see "confined_read" row) and `kwe_core::image_entry` for the pkg step — `resolve_shader_reference` itself does no confinement of its own; it is a pure reference-rewrite layer (`workshop/<id>/<file>` → try the `zcompat/scene/shaders/<id>/<file>` redirect first, else `shaders/<reference>`) sitting IN FRONT OF that shared lookup. | `MAX_SHADER_SOURCE_BYTES` = 256 KiB (`main.rs:1862`) for the raw read; `materialshader::MAX_SHADER_TEXT_BYTES` = 256 KiB (`materialshader.rs:17`) is a SEPARATE cap on the fully preprocessed text — the two are deliberately equal today (`main.rs:1860`'s own doc comment), not the same constant. | pkg → pkg-parent-dir → assets root | `resolve_shader_reference` `main.rs:1882-1900`; the actual lookup closure `main.rs:2859-2874` |
| Model / material (`scenemodel::resolve_model`/`resolve_material`) | Confinement is entirely the CALLER-supplied `AssetLookup` closure's job (`kwe-core` has no filesystem/package specifics of its own here) — in practice callers always compose it from `confined_read` (dir steps) + `kwe_core::asset_entry` (pkg step), i.e. the same chain as the row above. `resolve_model`/`resolve_material` themselves only bound the JSON parse. | `MAX_MODEL_JSON_BYTES` = 1 MiB (`scenemodel.rs:33`) for the model.json/material.json descriptor read itself; `MODEL_ASSET_READ_CAP` = 64 MiB (`scenemodel.rs:42`) is the generic per-file cap the CALLER's lookup closure applies to every step (JSON and `.tex` texture reads alike) — deliberately equal to `MAX_PKG_ENTRY_BYTES`/`MAX_TEXTURE_SOURCE_BYTES` per its own doc comment. | pkg → pkg-parent-dir → assets root (composed by the caller) | `scenemodel.rs:247-473` (`resolve_material`/`resolve_model`) |
| `confined_read` (the actual dir-lookup primitive `resolve_layer_image` does NOT use, but everything else routed through `scenemodel`/shaders DOES) | Relative path; absolute/`..`/prefix rejected; `root_canonical.join(reference).canonicalize()` must `starts_with(root_canonical)`; regular file only. Its own doc comment explicitly records that an earlier `symlink_metadata(&canonical).is_symlink()` check was REMOVED (S1 review NIT #7) because it "was always false and did nothing" — `canonicalize()` already fully resolves symlinks, so the containment defense is `starts_with` alone. Same tolerance as `resolve_layer_image`: an in-root-resolving symlink is not rejected. | Caller-supplied `cap` parameter (see callers above) | N/A (one lookup step; callers compose priority) | `scenemodel.rs:494-528`, test `scenemodel.rs:832` |
| `general.script` (file scene) | Relative, must end `.js` (a `.pkg` reference is explicitly rejected with a dedicated message), absolute/`..`/prefix rejected, `root.join(reference).canonicalize()` must `starts_with(root)`, regular file only. Same tolerance/no-componentwise-check as `resolve_layer_image`. | `MAX_SCRIPT_BYTES` = 2 MiB (`pkg.rs:106`) | scene dir only | `scene.rs:2117-2189` (`resolve_script`) |
| `general.script` (pkg scene) | Delegates entirely to `kwe_core::script_entry` → `resolve_pkg_entry` (see "pkg entry matching" below) plus its own `.js`-extension / no-`.pkg`-reference checks. | `MAX_SCRIPT_BYTES` (checked by the caller against the matched entry's declared size, same constant) | pkg table only | `scene.rs:2200-2206` (`resolve_pkg_script`); `pkg.rs:720-740` (`script_entry`) |
| `scene.json` itself (pkg) | Exactly one entry named `scene.json` (case-insensitive tail match, same core as every other pkg lookup). | `MAX_SCENE_JSON_BYTES` = 16 MiB (`pkg.rs:103`) | pkg table only (a file scene reads `scene.json` directly off disk, no confinement needed — it IS the root) | `pkg.rs:103`, `pkg.rs:897-902` |
| **pkg entry matching** (`resolve_pkg_entry`, the core EVERY `*_entry` function above shares) | Reject a reference starting with `/`, containing `\`, containing NUL, or with ANY `/`-split component equal to `..` (bare `.` is NOT rejected — untested/unlikely in the real corpus, but structurally allowed today). Entry paths were already validated at package OPEN (no `..`, no absolute, no backslash — `pkg.rs`'s own table parser), so this function's checks exist for the error message, not as the actual safety boundary. Matching is **case-insensitive** (`to_ascii_lowercase()` on both sides) and matches EITHER the literal entry path OR the entry's tail after any `/` — `path == needle \|\| path.ends_with("/{needle}")` — requiring EXACTLY one match (zero or 2+ is an error, i.e. "not found" for a caller doing `if let Ok(...)`). | N/A (index lookup only; the caller applies its own byte cap on the read) | N/A | `pkg.rs:684-717` |

### Where existing resolvers genuinely disagree (documented, not averaged — conductor decision (b))

1. **Leaf symlink strictness.** `resolve_layer_video`'s PRODUCTION path
   (`open_video_source`) rejects ANY symlinked leaf outright (`O_NOFOLLOW`
   open). `resolve_layer_image`/`confined_read`/`resolve_script` reject a
   leaf symlink only when it happens to resolve OUTSIDE root (an in-root
   symlink is tolerated by all three, per their own `canonicalize()` +
   `starts_with()`-only containment — never actually exercised by an
   existing test, since every existing symlink test only builds an
   ESCAPING symlink). `Vfs` adopts the video lane's strictness (leaf
   symlinks are always rejected) for every category, per rule (b).
2. **Intermediate-directory symlinks.** No existing resolver — not even
   `open_video_source` — rejects a symlinked INTERMEDIATE directory that
   itself resolves back inside root; all of them rely on the final
   `canonicalize()` + `starts_with(root)` catching only an ESCAPE. `Vfs`'s
   componentwise walk rejects any symlink anywhere in the path,
   intermediate or leaf, whether or not it would have stayed inside root —
   this has NO existing analog anywhere in the codebase; it is a pure
   strengthening, called out explicitly in `vfs.rs`'s own doc comments.
3. **Fall-through-on-any-failure vs. found-is-authoritative.** Documented
   above under "Source priority order" — every existing multi-source
   resolver silently tries the next source on ANY failure of the current
   one (not found, oversized, symlink-rejected — all the same "just try
   the next one" branch). `Vfs` treats a found-but-rejected/oversized
   match as the final answer for that reference; only a genuine "does not
   exist here" falls through. See "Vfs deviations" below for the
   reasoning — this is safe to change ONLY because SR-2a migrates no
   caller; SR-2c's differential test against the real corpus is where this
   difference either proves harmless (no corpus scene actually depends on
   the silent fallback) or gets its own conductor decision if it does.
4. **`.` component.** Nothing existing rejects a bare `.` path component
   (only `..` is checked everywhere). `Vfs` rejects it too, a small
   strengthening with no existing analog either.
5. **Extension policy is caller-level, not VFS-level.** `resolve_script`'s
   `.js`-only / no-`.pkg`-reference rules, and `video_extension_allowed`'s
   container allowlist, are APPLICATION policy layered on top of
   confinement, not part of it — `Vfs::resolve`/`resolve_path` do not
   enforce any extension policy; a migrating caller (SR-2c+) must still
   apply its own on the `logical_id`/reference before or after calling
   `Vfs`, exactly as it does today.

### `Vfs` deviations from every existing resolver (by design, SR-2a)

- **Found-is-authoritative** (point 3 above): a matched-but-rejected
  source never falls through to a different source.
- **Componentwise symlink walk** (point 2 above): every path component,
  not just the leaf, is checked.
- **Leaf `O_NOFOLLOW`** for every category (point 1 above), not just
  video.
- **Bare `.` component rejected** (point 4 above).
- **`AssetCategory::Video` is refused by `resolve()`** (bytes) entirely —
  it only ever answers through `resolve_path()`, matching how video
  content is used everywhere today (a path handed to libmpv, never bytes
  read into memory by the confinement layer itself).
- **`video_probe` cap has no existing analog** — reuses the tightest
  existing value (`shader_text`'s 256 KiB) per the task's instruction,
  inert until a real bounded video-probe reader exists.

## SR-2a — the confined VFS

Conductor scope decisions (verbatim):

- **(a)** SR-2a adds the type + tests ONLY. Zero existing `resolve_*` call
  site changes. Caller migration happens one family per child (SR-2c+)
  with differential tests, so today's per-family behavior differences (see
  above) are preserved and surfaced during migration, never silently
  unified now.
- **(b)** The VFS's confinement semantics must EQUAL the strictest
  already-tested behavior in the codebase (`resolve_layer_image`'s
  "confines to the content root" rules, and `open_video_source`'s
  production leaf `O_NOFOLLOW`); where existing resolvers differ from each
  other, the difference is documented above, not averaged.

```text
Task:            Introduce a single confined asset VFS type in kwe-core —
                 pkg-entries -> scene-dir -> assets-root lookup, per-
                 category byte caps, symlink-safe confinement at least as
                 strict as the strictest existing resolver — WITHOUT
                 migrating any of the resolve_* call sites that already
                 exist. The type and its tests are the whole deliverable;
                 no renderer/preflight behavior changes in this slice.
Milestone/Slice: SR-2a
Goal:            Give SR-2c+'s per-family migrations one real, tested
                 target to migrate ONTO, built from an accurate reading of
                 what every existing resolver actually does today (this
                 doc's semantics table) rather than a fresh design that
                 might silently diverge from any of them.
Outcome:         crates/kwe-core/src/vfs.rs (new): AssetCategory (Texture,
                 ShaderText, Model, Particle, Script, Json, Video),
                 VfsSource (PkgEntry, SceneDir, AssetsRoot), VfsCaps (seven
                 per-category u64 fields, Default = today's actual caps,
                 gathered per the semantics table above -- reusing kwe-
                 core's own constants directly (pkg::MAX_PKG_ENTRY_BYTES,
                 scenemodel::MAX_MODEL_JSON_BYTES, particlefile::
                 MAX_PARTICLE_FILE_BYTES, pkg::MAX_SCRIPT_BYTES, pkg::
                 MAX_SCENE_JSON_BYTES) where one exists, a cited literal
                 where the value lives only in kwe-scene-renderer
                 (shader_text: 256 KiB) or has no existing analog at all
                 (video_probe: reuses shader_text's value per the task's
                 own "tightest existing analog" instruction), ResolvedAsset
                 {logical_id, source, bytes}, ResolvedPath {logical_id,
                 source, path}, VfsError (thiserror: BadReference(&'static
                 str), NotFound, Oversize{category, limit}, SymlinkRejected,
                 NotAddressable, Io(String)), and Vfs{pkg: Option<PkgReader>,
                 scene_root: PathBuf, assets_root: Option<PathBuf>, caps:
                 VfsCaps} with Vfs::new (canonicalizes scene_root, fallibly;
                 canonicalizes assets_root best-effort, mirroring main.rs's
                 own assets_dir.and_then(|d| d.canonicalize().ok())),
                 Vfs::resolve (bytes; refuses AssetCategory::Video), and
                 Vfs::resolve_path (path only, no read; a pkg match answers
                 NotAddressable). Normalization is a hand-rolled `/`-splitter
                 (deliberately NOT std::path::Path::components(), which
                 silently collapses "a//b" into the same two components as
                 "a/b" -- exactly the hostile case the task's test list
                 requires catching) rejecting: empty, >512 bytes, NUL,
                 backslash, a leading `/`, and any `.`/`..`/empty
                 component. pkg lookup calls kwe_core::pkg::resolve_pkg_entry
                 directly (already pub(crate), reused verbatim -- same
                 case-insensitive tail-match algorithm every existing pkg
                 lookup already uses, per the semantics table). Directory
                 lookup (confined_leaf) walks every component with
                 symlink_metadata, rejecting a symlink ANYWHERE (the
                 documented strengthening past every existing resolver),
                 then still canonicalizes + starts_with(root) as a
                 belt-and-suspenders check matching the pattern the rest of
                 the codebase already uses. Reads go through an O_NOFOLLOW
                 open + a take(cap+1)-bounded read (mirrors scan::
                 read_bytes_limited's overflow-safe pattern) so "exactly at
                 the cap" and "one byte over" stay distinguishable without
                 ever buffering past the cap.
                 crates/kwe-core/src/lib.rs: `mod vfs;` (alphabetically
                 between texvheader and webpreflight) + a `pub use vfs::{...}`
                 re-export of every public type.
                 docs/SR2.md (this file, new): the SR-2 child list, the
                 full "Current resolver semantics" differential table (the
                 step-1 study this contract was built from), the 5 places
                 existing resolvers disagree with each other (documented,
                 not averaged, per decision (b)), and this filled contract.
In scope:        crates/kwe-core/src/vfs.rs (new), crates/kwe-core/src/
                 lib.rs (module + re-export), docs/SR2.md (new).
Out of scope:    Migrating ANY existing resolve_* call site (decision (a) --
                 the whole point of this slice is that nothing anywhere
                 else changes). docs/Scene-Rendering-Plan.md (conductor-
                 maintained). THIRD_PARTY.yml (no new dependency, no
                 borrowed code -- vfs.rs is original, citing existing
                 in-repo behavior only, not upstream source).
Acceptance tests:        crates/kwe-core: 9 new tests in vfs.rs's own test
                         module -- priority_pkg_beats_scene_dir_beats_
                         assets_root (both priority claims: pkg wins over
                         both, scene dir wins over assets root when absent
                         from pkg, plus a third case proving the assets
                         root still answers when it is the only source);
                         pkg_lookup_is_case_insensitive_and_matches_by_
                         tail_like_resolve_pkg_entry (proves the pkg lane
                         actually mirrors resolve_pkg_entry's algorithm,
                         not an assumed simpler one); hostile_references_
                         are_bad_reference (../x, a/../x, /abs, a//b, empty,
                         513 bytes, NUL, backslash, plus two extra bare-`.`
                         cases the task's strengthening added: ./x, x/.);
                         scene_dir_rejects_a_symlinked_leaf_and_a_
                         symlinked_intermediate_directory and the same for
                         assets_root (both halves of the componentwise
                         strengthening, in both sources); cap_boundary_is_
                         inclusive_at_the_limit (limit-1/limit/limit+1 ->
                         ok/ok/Oversize, with the exact limit value in the
                         assertion); resolve_refuses_the_video_category_
                         pointing_callers_at_resolve_path;
                         resolve_path_serves_a_dir_video_and_refuses_a_
                         pkg_embedded_one (dir video ok + confinement
                         identical to resolve(), pkg video ->
                         NotAddressable, authoritative even when the same
                         name also exists in the scene dir);
                         unicode_references_resolve_byte_identically_and_
                         case_folding_never_happens (#[cfg(target_os =
                         "linux")], non-ASCII component round-trips,
                         "A.png" != "a.png" on the dir lanes). 169 kwe-core
                         tests total, up from 160.
                         Reused crate::pkg::testutil::PkgWriter (SR-0c's
                         fixture builder, pub(crate) -- directly visible
                         from vfs.rs's own test module since both live in
                         kwe-core, unlike kwe-scene-inspector's tests which
                         had to duplicate its byte layout across a crate
                         boundary) rather than re-deriving pkg bytes by
                         hand.
                         830 workspace tests total, up from 821.
                         cargo fmt/clippy/test --workspace green.
                         ./scripts/check.sh green end to end, including the
                         C++/QML build and qml-typecheck.
Failure/recovery tests:  Covered by Acceptance tests above -- every
                         confinement failure mode (hostile reference,
                         symlinked leaf, symlinked intermediate dir, over
                         cap, pkg-embedded-but-path-requested) is a typed,
                         bounded VfsError, never a panic, a silent
                         truncation, or an escape.
Upstream/provenance:    Original; every confinement rule is either a direct
                         mirror of an existing in-repo resolver (cited in
                         the semantics table above and in vfs.rs's own doc
                         comments) or an explicitly documented
                         strengthening past all of them -- no third-party
                         source consulted or adapted.
Commands run and results: cargo fmt --all -- --check -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 830 passed, 0 failed.
                         ./scripts/check.sh -- green end-to-end, including
                         the C++/QML build and qml-typecheck.
Open risks:              The "found is authoritative, never fall through
                         past a rejection" deviation (point 3 above) is
                         UNTESTED against the real corpus in this slice --
                         SR-2a adds no caller, so nothing today actually
                         exercises this difference. SR-2c's differential
                         test against real corpus fixtures is where this
                         gets proven harmless or, if some real scene
                         actually depended on the silent legacy fallback,
                         where that surfaces for a conductor decision.
                         video_probe's cap is a placeholder value with no
                         real reader behind it yet (resolve() refuses the
                         Video category outright) -- a future slice that
                         gives Video a real bytes-probing use needs to
                         reconsider whether 256 KiB is actually right for
                         that read, not just reuse it because it was the
                         tightest number lying around today.
                         Vfs::new's assets_root canonicalize-best-effort
                         behavior is unverified against a REAL missing WE
                         assets install (only unit-tested with a
                         deliberately omitted assets_root, never a
                         present-but-broken one) -- low risk, same
                         fallback shape main.rs already uses today.
STOP findings:           None. The existing resolvers' semantics do not
                         fundamentally contradict each other -- they
                         disagree in the five specific, narrow, listed ways
                         above (leaf symlink strictness, intermediate-
                         symlink tolerance, fallthrough-on-failure,
                         extension policy layering, and the sheer number of
                         near-duplicate confinement implementations), none
                         of which makes a single "strictest of all" VFS
                         contract impossible for any family's actual needs
                         -- every existing resolver's real safety
                         requirement (stay inside root, bounded reads,
                         regular files only) is a strict SUBSET of what
                         Vfs enforces.
Commit(s):               79992d0
```

## scene.json field table (the SR-2b differential baseline)

Every field `kwe-scene-renderer/src/scene.rs` / `kwe-core/src/sceneobjects.rs`
actually reads today, condensed from the SR-2b study, with JSON key(s),
Rust type, default, alias precedence, and a file:line citation. Where
`scene.rs` and `sceneobjects.rs` could disagree (classification), they do
not: `scene.rs` calls `sceneobjects::classify_scene_object` directly
rather than re-deriving the rule, so `ir.rs` reuses that same function too
— by construction, the IR's object-family split can never drift from
either file's own behavior.

### Top level

Only `"general"` and `"objects"` are ever read off the root object
(`scene.rs:580,587,690`); any third root key is untouched by the real
parser. `ir.rs::parse_scene_ir` mirrors this — everything else lands in
`SceneIr::unknown`.

### `general` (5 keys, `scene.rs:1926-2113`, `:603-612`)

| Key | Type | Default | Alias | Citation |
|---|---|---|---|---|
| `clearcolor` | `[f32;4]` array, or `"r g b"` string (alpha forced 1.0) | `[0,0,0,1]` | — | `scene.rs:1926-2004` |
| `resolution` | 2-int array | falls through to `orthogonalprojection` | wins over it | `scene.rs:2023-2063` |
| `orthogonalprojection` | `{"width":W,"height":H}` | `None` | loses to `resolution` — **when `resolution` is present, `orthogonalprojection`'s bytes are never even read** | `scene.rs:2072-2094` |
| `fps` | finite float | `None` | — | `scene.rs:2096-2113` |
| `script` | string | `None` | — | `scene.rs:603-612` |

No `"camera"` key exists in the parser (a plausible-sounding guess that
turned out wrong during the study — recorded here so it is not
re-guessed later).

### `parse_common_props` (every family, `scene.rs:853-1033`)

| Field | Key(s) / precedence | Type | Default | Citation |
|---|---|---|---|---|
| `name` | `name` | `String`, REQUIRED (renderer rejects if absent/non-string) | — | `scene.rs:858-872` |
| `id` | `id` | `Option<i64>` (no unwrap) | `None` | `scene.rs:873` |
| `origin` | `origin` | `[f32;2]` (2-or-3, z dropped) | `[0,0]` | `scene.rs:882-893` |
| `angles` | `angles` | `[f32;3]` (2-or-3) | `[0,0,0]` | `scene.rs:897-915` |
| `scale` | `scale` | `[f32;2]` | `[1,1]` | `scene.rs:917-928` |
| `alpha` | `alpha` | `f32`, renderer REJECTS out-of-`0..=1` | `1.0` | `scene.rs:930-950` |
| `visible` | `visible` | renderer: `bool`, REJECTS any other post-unwrap shape | `true` | `scene.rs:952-966` |
| `blend_mode` | `blendMode` wins, `colorBlendMode` loses (the corpus key — every real scene.json observed uses this spelling) | `u32` | `0` | `scene.rs:973-984` |
| `brightness` | `brightness` | `f32` | `1.0` | `scene.rs:993-1020` |

### Per-family (classification: `sceneobjects.rs:92-130`)

| Family | Discriminator | Fields | Citation |
|---|---|---|---|
| Model | `image` unwraps to a `.json`-suffixed string | `model_ref` (=`image`), `size`/`tint` (`parse_size_and_tint`) | `scene.rs:1091-1142` |
| Image / TexvImage | `image` unwraps to a string ending `.tex` (TexvImage) or anything else (Image) | `image`, `size`/`tint` | `scene.rs:1147-1190` |
| TexturelessImage | `image` key present, unwraps to a NON-string | `size`/`tint` only — no typed slot for the non-string `image` itself | `scene.rs:1147-1190`; `sceneobjects.rs:113-116` |
| `size`/`tint` (shared) | `size` (2-comp), `tint` wins / `color` loses (3-or-4 comp) | `[f32;2]`/`[f32;4]` | `[0,0]`/`[1,1,1,1]` | `scene.rs:1038-1076` |
| Video | `"video"` key present at ALL (no type check) | `source` (unwrapped `video`), `size`/`tint`, `loop` (tolerant bool, default `true`), `rate` (default `1.0`) | `scene.rs:1318-1416` |
| Text | `"text"` key present at all | `text`, `font`, `pointsize` (default `12.0*4.0=48.0`px), `horizontalalign`/`verticalalign` (falling back to `alignment`'s polarity word), `color` (**`color` only — no `tint` alias for text**), `has_size` (presence only, value never read) | `scene.rs:1200-1305`, `:1740-1790` |
| Particle | `particle` unwraps to an object naming a `texture`/`material` | spawnRate/life/direction/spread/sizeStart/sizeEnd/alphaStart/alphaEnd (scalars), gravity (1-2-3 comp), colorStart/colorEnd, maxCount, `texture` wins/`material` loses, `instanceoverride.{count,rate,size,lifetime,speed,alpha}`, `instanceoverride.colorn` wins/`.color` loses (mean of 3 components) | `scene.rs:1421-1708` |
| ParticleFile | `particle` present, any OTHER shape (string or object without a resolvable material) | `file_ref` (unwrapped string; a non-string shape here is possible and untyped) | `scene.rs:1471-1489`; `sceneobjects.rs:107-121` |
| Other (sound, lights, anything else) | none of the above | **zero family-specific fields anywhere in the codebase** — no `Sound`/`Light` split exists | `sceneobjects.rs:122-129` |

**Genuine STOP case (task's own instruction, not guessed around):**
`speed`/`speedMin`/`speedMax` (particle, `scene.rs:1598-1611`) —
`speedMin`'s default is `speed`'s own resolved value, `speedMax`'s default
is `speedMin`'s, with a final min/max swap. A field whose default depends
on a SIBLING field's resolved value cannot be represented as a static
typed default without baking rendering-time derivation into the IR
(decision (b) rules this out). Per instruction: left untyped, all three
keys land in the particle's own residue inside `ObjectIr::unknown`
(`unknown.get("particle")`), byte-preserved, for a later slice to decide.

### `effects[]` entries (`sceneeffect.rs:596-643`, `ObjectEffect` @ `:196-201`)

`id` (`i64`, default `0`), `name` (`String`, default `""`), `visible`
(`bool`, default `true`) — **none of these three go through the
`scene_property_value` unwrap**, unlike almost everything else in this
table. `file` (`String`, REQUIRED — the renderer skips the whole entry
when absent/non-string). `passes` (raw `Vec<Value>`, default `[]`,
structure unvalidated at this level). This function also does real file
I/O (resolving `file` against the pkg/dir/assets chain) — out of scope
for a pure scene.json IR; `ir.rs::EffectRefIr` stops at the 5 authored
fields above, `file: Option<String>` rather than required.

**Asymmetry inherited, not fixed, by the IR:** `scene.rs` only reads an
object's OWN `effects` array at parse time for Model layers
(`scene.rs:1107-1117`); Image/Text/Video layers unconditionally get
`effects_raw: Vec::new()` (`scene.rs:1187,1302,1413`) even though nothing
stops an author from writing `effects` on any of them. `ir.rs` populates
`ObjectIr::effects` uniformly for every kind whenever the JSON has an
`effects` key — capturing MORE than three of four kinds' worth of
today's renderer, which is correct per decision (b) (authored state, not
runtime behavior) but is flagged here since it is not a "mirror exactly"
case.

### Bounds actually enforced at scene LOAD (not the SR-0c inspector's own, separate, sampling-only 4096)

`MAX_LAYERS`=256 (`layers.rs:27`, checked post-loop, `scene.rs:816-824`,
REJECTS the scene), `MAX_TEXT_LAYERS`=16 (`text.rs:37`, skip-not-reject,
`scene.rs:802-806`), `MAX_PARTICLE_SYSTEMS`=64 (`particles.rs:53`,
skip-not-reject, `scene.rs:791-794`), `MAX_EFFECTS_PER_OBJECT`=32
(`sceneeffect.rs:63`, truncates via `.take()` both at Model parse time
and at resolve time). **The raw `objects` array itself has NO length cap
in the real load path** — `Other`-kind objects (sound, etc.) accumulate
into nothing bounded at all; only the whole-file 16 MiB
`MAX_SCENE_JSON_BYTES` cap indirectly limits this. `ir.rs::MAX_OBJECTS`
(4096, `IrError::ObjectsCap`) is therefore a NEW cap this slice
introduces for the IR specifically (never-truncate, always-refuse — see
`ir.rs`'s own doc comment for why), reusing the SR-0c inspector's
`max_objects_walked` NUMBER as a convenient, already-corpus-vetted value —
not evidence the renderer enforces one.

## SR-2b — typed scene IR with unknown-field bags

Conductor decisions (verbatim):

- **(a)** The IR lives in kwe-core (`crates/kwe-core/src/ir.rs`) so both
  the renderer and the inspector can consume it later.
- **(b)** IR captures AUTHORED state only (what scene.json says), never
  runtime state — plan §4.2's authored/runtime split starts here.
- **(c)** The IR's known-field coverage for this slice is exactly the
  object families the renderer parses today (the scene.rs/sceneobjects.rs
  raw-Value reads found in SR-0c/2a) — not the full WE vocabulary.
  Everything else lands in unknown bags, preserved byte-faithfully.
  Coverage grows family-by-family in later children.

```text
Task:            A typed scene.json IR (crates/kwe-core/src/ir.rs) whose
                 known fields mirror exactly what scene.rs/sceneobjects.rs
                 parse today (same keys, types, defaults, alias
                 precedence), with an explicit unknown-field bag at every
                 JSON object level so nothing an author wrote is ever
                 silently dropped -- type + parser + tests ONLY, no
                 renderer/inspector caller migrates in this slice.
Milestone/Slice: SR-2b
Goal:            Give SR-2c+'s differential migration a typed structure to
                 migrate ONTO that is provably faithful to today's actual
                 parser (this doc's field table), not a fresh design that
                 might quietly diverge from it -- and prove the "unknown
                 fields survive a load/report round trip" acceptance the
                 plan's typed-IR goal depends on.
Outcome:         crates/kwe-core/src/ir.rs (new, ~950 lines + a
                 crates/kwe-core/src/ir/tests.rs test module, 29 tests):
                 SceneIr{schema_version, general: GeneralIr, objects:
                 Vec<ObjectIr>, unknown: UnknownBag, duplicate_ids:
                 Vec<String>}; GeneralIr mirrors the 5 general-block keys
                 exactly (clearcolor/resolution+orthogonalprojection-alias/
                 fps/script); ObjectIr{stable_id: StableId, authored_id:
                 Option<i64>, name: Option<String>, common: CommonPropsIr,
                 kind: ObjectKindIr, effects: Vec<EffectRefIr>, unknown};
                 CommonPropsIr mirrors parse_common_props's 7 remaining
                 fields (origin/angles/scale/alpha/visible/blend_mode/
                 brightness) with the renderer's exact defaults;
                 VisibleIr{Bool, PropertyBound(Value), Absent} -- a
                 genuine tri-state the renderer's own flat bool collapses
                 away (SR-0c/SR-11 semantics), never rejecting the way the
                 renderer does on a non-bool post-unwrap value;
                 ObjectKindIr mirrors SceneObjectKind's 8 discriminators
                 exactly by REUSING kwe_core::sceneobjects::
                 classify_scene_object directly (not a re-derived rule --
                 cannot drift), one deliberate departure from the task's
                 literal enum sketch: no separate Sound/Light variants --
                 neither exists as a distinct parse path anywhere in the
                 codebase (both are SceneObjectKind::Other), so both fold
                 into ObjectKindIr::Unknown rather than typing structure
                 the renderer does not have (decision (c)). StableId{
                 Authored(i64), Index(usize)} assigned first-authored-wins,
                 later duplicates demoted to Index and recorded in
                 SceneIr::duplicate_ids as "id {n} reused at index {i}".
                 EffectRefIr{id, name, visible, file: Option<String>,
                 passes: Vec<Value>, unknown} -- every authored entry is
                 KEPT even when `file` is missing (module doc departure:
                 the renderer skips a fileless entry because resolving it
                 needs file I/O this pure parse does not have; the IR
                 still records that the entry was authored).
                 Two documented, deliberate departures from "mirror
                 exactly" (both required by decision (b), spelled out in
                 ir.rs's own module doc): (1) no range clamping/rejection
                 anywhere -- every numeric field holds the
                 coerced-but-unclamped authored value; a shape a typed
                 field genuinely cannot represent (wrong JSON type, not
                 just an out-of-range number) defaults AND the raw value
                 survives in the nearest UnknownBag under its original key
                 -- applied systematically: a key is marked "consumed"
                 (excluded from the unknown bag) ONLY when its value was
                 actually read into the field it represents, never merely
                 "attempted"; (2) speed/speedMin/speedMax left entirely
                 untyped (this slice's STOP case, per the task's own
                 instruction) -- see the field table above.
                 An alias pair (blendMode/colorBlendMode,
                 resolution/orthogonalprojection, tint/color,
                 texture/material, colorn/color) consumes only the WINNING
                 spelling; a present LOSING spelling lands in the nearest
                 unknown bag under its own key -- resolving an apparent
                 contradiction between the task's type-spec sentence
                 ("consumes both, neither lands in unknown" -- describing
                 the alias PAIR's intent) and its test-list instruction
                 ("the ignored spelling still must not be lost: it goes in
                 the unknown bag" -- the concrete per-instance rule
                 actually implemented, since the renderer's `.or_else()`
                 genuinely never reads the loser's bytes).
                 SceneIr::to_raw_value() (+ private per-substructure
                 helpers) reconstructs a semantically-equal Value: typed
                 defaults are always re-emitted explicitly (even when the
                 original omitted the key), which is lossless for SceneIr
                 EQUALITY on re-parse even though the intermediate JSON
                 differs from the original bytes; the Particle kind's
                 residue (its own unknown-bag "particle" entry) is MERGED
                 back into the freshly-built "particle" object rather than
                 overwriting it. One real bug caught by the round-trip
                 test itself: instanceoverride.colorn must re-serialize as
                 a 3-vector (the authored WE shape), not the reduced
                 scalar instance_colorn holds -- a bare number fails
                 as_vector's shape check on re-parse and silently resets
                 to the 1.0 default; fixed before this slice's acceptance
                 run, not left for a later one to find.
                 crates/kwe-core/src/lib.rs: `mod ir;` (alphabetically
                 between capabilities and keyvalues) + a `pub use ir::{...}`
                 re-export of every public type.
                 docs/SR2.md (this section): the field table above.
In scope:        crates/kwe-core/src/ir.rs (new), crates/kwe-core/src/
                 ir/tests.rs (new), crates/kwe-core/src/lib.rs (module +
                 re-export), docs/SR2.md.
Out of scope:    Migrating ANY existing scene-loading call site (decision
                 mirrors SR-2a's own (a)). Effect-FILE resolution (needs
                 I/O this pure scene.json parse does not have --
                 EffectRefIr stops at the 5 authored fields scene.json
                 itself carries). Range clamping/validation (module doc
                 departure (1) -- explicitly renderer policy, not IR
                 scope). speed/speedMin/speedMax typing (this slice's STOP
                 case). docs/Scene-Rendering-Plan.md (conductor-
                 maintained). THIRD_PARTY.yml (original code; no upstream
                 source consulted for this slice -- every citation above
                 is to THIS repository's own existing parser).
Acceptance tests:        crates/kwe-core: 29 new tests in ir/tests.rs --
                         general block (defaults, authored values, the
                         resolution/orthogonalprojection alias both
                         directions, unknown general/root keys); per-family
                         (image tint/color alias, blendMode/colorBlendMode
                         alias both directions, model/texv/textureless
                         classification with the textureless raw `image`
                         preserved, video source/loop-tolerance/rate,
                         text alignment exact-word/alignment-fallback/
                         default plus has_size raw-value preservation,
                         particle known-fields incl. texture/material and
                         colorn/color aliases with speed/speedMin/speedMax
                         landing in the particle residue, particle
                         defaults, particle-file, an unclassifiable
                         "Other" object's common props still parsing);
                         visible's 3-state (Bool/PropertyBound/Absent);
                         minimal-object defaults matching the renderer
                         exactly; effects[] known fields + unknown +
                         the fileless-entry-kept departure; StableId
                         assignment + duplicate recording; bounds
                         (exactly MAX_OBJECTS ok, MAX_OBJECTS+1 ->
                         ObjectsCap, a non-object entry -> typed error
                         naming its index, invalid JSON -> Parse, a
                         non-object root -> NotAnObject, a missing/
                         non-array "objects" treated as empty);
                         determinism (same bytes parsed twice are equal);
                         one comprehensive round-trip test exercising
                         every family, every alias, duplicate ids, and
                         nested unknown bags at once, asserting SceneIr
                         equality after parse -> to_raw_value -> parse.
                         859 workspace tests total, up from 830.
                         cargo fmt/clippy/test --workspace green.
                         ./scripts/check.sh green end to end, including the
                         C++/QML build and qml-typecheck.
Failure/recovery tests:  Covered by Acceptance tests above -- every
                         structural failure (objects-cap, non-object
                         entry, invalid JSON, non-object root) is a typed,
                         total IrError; no partial IR is ever returned on
                         any of them.
Upstream/provenance:    Original; every typed field/default/alias mirrors
                         an existing in-repo parser (cited in the field
                         table above and in ir.rs's own doc comments) or
                         is an explicitly documented, decision-(b)-required
                         departure from it -- no third-party source
                         consulted or adapted.
Commands run and results: cargo fmt --all -- --check -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 859 passed, 0 failed.
                         ./scripts/check.sh -- green end-to-end, including
                         the C++/QML build and qml-typecheck.
Open risks:              The "consumed only on success" rule (departure
                         (1)) was applied by hand across ~20 extraction
                         sites; SR-2c's differential test against real
                         corpus fixtures is the first time this gets
                         checked against actual authored content at scale
                         rather than this slice's hand-built fixtures.
                         has_size's real `size` value and a non-string
                         image/video/particle/text discriminator's raw
                         value are captured in the unknown bag rather than
                         a typed field -- correct for round-trip fidelity,
                         but a family migration (2c+) that wants the
                         ACTUAL size number for a text layer (today's
                         renderer never needs it) will need to read it out
                         of the unknown bag explicitly rather than a typed
                         field.
                         effects[] entries with no `file` are kept by the
                         IR but skipped by the renderer -- a later
                         migration must decide whether to filter these at
                         the adapter boundary or change the renderer to
                         accept them; this slice deliberately did not
                         decide that (decision (b) scope boundary).
STOP findings:           speed/speedMin/speedMax (particle) -- see the
                         field table's "Genuine STOP case" entry above.
                         Left untyped in the particle's unknown-bag
                         residue per the task's own instruction; no other
                         field required stopping (the study found exactly
                         one cross-field-dependent default in the whole
                         parser).
Commit(s):               e6aac50
```

## SR-2c — the old->new differential adapter for the scene.json family, and the production swap

Conductor decisions (verbatim):

- **(a)** Family = the scene.json top-level load in kwe-scene-renderer
  (`general` + `objects[]` into the renderer's scene structures).
  Model/material/effect FILE loading (separate files) stays legacy --
  later children.
- **(b)** The legacy parser is NOT deleted: it moves behind `#[cfg(test)]`
  (individual attributes on ~20 items, not a nested submodule -- zero
  changes needed at the 74 existing tests' own call sites) as the
  differential oracle. Deletion happens only after a full SR-2 epic soak.
- **(c)** Where the IR's typed fields can't express a legacy quirk (the
  particle speed/speedMin/speedMax cross-field defaults left untyped in
  SR-2b), the adapter reads the preserved raw values from the IR's
  unknown-bag residue and applies the legacy logic verbatim.

```text
Task:            The old->new differential adapter for the FIRST loader
                 family (scene.json -> renderer scene structures), then the
                 production swap -- byte-identical behavior, proven, not
                 assumed.
Milestone/Slice: SR-2c
Goal:            Prove kwe_core::SceneIr (SR-2b) is a faithful intermediate
                 representation by building the SAME SceneConfig/LayerSpec/
                 ParticleSpec structures the legacy parser builds, from the
                 IR instead of raw JSON, differentially tested against the
                 legacy parser kept as an oracle -- then flip the
                 production load path onto it.
Outcome:         crates/kwe-scene-renderer/src/scene_ir_adapter.rs (new,
                 ~1270 lines): pub fn parse_scene_json_via_ir(bytes) ->
                 Result<SceneConfig, SceneError> = kwe_core::parse_scene_ir
                 + scene_from_ir. scene_from_ir(&SceneIr) walks
                 ir.objects, matching ObjectKindIr's 8 variants onto the
                 exact same LayerSpec/ParticleSpec-building logic
                 scene::parse_objects's per-kind branches use today
                 (require_common for the REJECTING families, optional_common
                 for Model's skip-never-reject contract, size_and_tint,
                 build_text_layer, build_particle_system, plus a NEW
                 build_particle_system_from_raw for one legacy quirk found
                 mid-slice -- see "major bug" below). The core
                 reconstruction technique: kwe_core::UnknownBag only holds
                 a key when the typed reader could NOT represent it
                 (SR-2b's consumed-only-on-success design), so
                 shape_rejected(unknown, key) = unknown.get(key).is_some()
                 reliably answers "would legacy have rejected this" for
                 every field legacy itself rejects on shape -- with one
                 alias refinement (tint/color, blendMode/colorBlendMode,
                 etc.): only the ALIAS WINNER's presence in unknown is a
                 valid reject signal, since the loser's presence is the
                 NORMAL trace of the alias rule whenever the winner won
                 cleanly. within_layer_bounds() closes a class of missing
                 +/-1e6-magnitude/finite checks SR-2b's own as_vector
                 deliberately never enforces (module doc departure (1)):
                 origin/angles(pre-degrees-conversion)/scale/size/tint/
                 particle gravity/colorStart/colorEnd/text color all
                 re-apply it here, matching every parse_vector call site.
                 angles convert radians->degrees here (IR stores raw
                 authored radians, decision (b) from SR-2b: conversion is a
                 rendering interpretation, not authored state) -- the bound
                 check runs on the RAW pre-conversion value, matching
                 legacy's own check ordering.

                 The swap: SceneConfig::parse and ::parse_pkg
                 (crates/kwe-scene-renderer/src/scene.rs) now call
                 crate::scene_ir_adapter::parse_scene_json_via_ir instead of
                 the local parse_scene_json; everything around it (script
                 resolution, canonical_root, read_bounded, pkg extraction)
                 is untouched. parse_scene_json and its ~19 sibling
                 functions/structs (parse_objects, parse_common_props,
                 parse_size_and_tint, parse_model_layer, parse_image_layer,
                 parse_text_layer, parse_video_layer, parse_particle_system,
                 parse_particle_color, parse_text_align(_v), field,
                 parse_clear_color, parse_resolution,
                 parse_orthogonal_projection, parse_fps, ObjectCounts,
                 particle_spec_defaults) are individually #[cfg(test)]-gated
                 as the oracle, per decision (b); SceneConfig/LayerSpec/
                 MaterialSpec/VideoSpec/ParticleSpec/TextSpec/
                 MaterialTextureSource, plus kwe-core's sceneeffect.rs
                 chain (ObjectEffect/EffectSpec/EffectPass/
                 EffectMaterialPass/EffectCommandPass/EffectCommand/
                 FboSpec/EffectTextureSlot) and kwe-scene-renderer's
                 particles::{ComponentModel, Emitter, Initializer,
                 Operator} and textures::DecodedTexture, gained PartialEq
                 (mechanical derives; no non-comparable member found --
                 the STOP-condition check this task named did not trigger).

                 Differential suite (crates/kwe-scene-renderer/src/
                 scene.rs, mod tests): assert_ir_parity(label, bytes)
                 compares legacy's parse_scene_json against
                 scene_ir_adapter::parse_scene_json_via_ir -- Ok/Err
                 discriminant + SceneErrorKind parity on Err (message TEXT
                 deliberately not compared -- see below), full SceneConfig
                 Eq on Ok. 10 new #[test] functions covering every case the
                 task named: general block (17 sub-cases: clearcolor
                 array/string/range/arity, resolution+orthogonalprojection
                 interplay, fps range, malformed objects/entries, invalid
                 JSON, a non-array "objects" -- see "objects-not-an-array"
                 bug below); alias precedence (6: tint/color,
                 blendMode/colorBlendMode, particle texture/material,
                 instanceoverride colorn/color); property-bound visible (4);
                 duplicate ids (1); missing name (6, incl. Model's
                 skip-never-reject exception); effects[] with unknown keys
                 (1, 3 entries); particle speed/speedMin/speedMax cross-
                 field combinations (5: bare speed, all three present,
                 max-falls-back-to-min, reversed pair normalizes, malformed
                 speed rejects -- exceeds the task's "at least 4" ask);
                 instanceoverride.colorn shapes (4: vector string, vector
                 array, scalar-tolerant, wrong-length-tolerant); one
                 "golden" fixture exercising every family/alias/duplicate-
                 id/unknown-key case at once; plus an #[ignore]d opt-in
                 ir_parity_corpus test reading KWE_SCENE_IR_PARITY_DIR (a
                 no-op skip when unset), walking every scene.pkg/scene.json
                 under it (reusing kwe_core::PkgReader for the .pkg case)
                 and asserting parity per item with an actionable panic
                 (item basename + which side diverged) -- NOT run against a
                 real corpus in this slice; the task text itself says "I
                 run this against the real corpus after merge", so it is
                 left for the coordinator, compiled/discoverable-verified
                 only. All 10 new tests + all 74 pre-existing scene.rs
                 tests + all 358 kwe-scene-renderer tests pass; the full
                 corpus-backed scripts/smoke-scene-corpus.sh (part of
                 ./scripts/check.sh) and scripts/smoke-scene.sh (every
                 case: B2/S1..S5b/M3c..M3g, standalone llvmpipe lane
                 included) both pass unchanged, end to end, THROUGH the new
                 production swap -- the strongest evidence available short
                 of the coordinator's own real-corpus run.

                 error-string consumers: none. A research fork grepped the
                 whole repo for SceneError/SceneErrorKind -- every match is
                 inside kwe-scene-renderer's own scene.rs test module. The
                 daemon classifies a renderer scene rejection by its fixed
                 EXIT CODE (main.rs::reject_scene), never by scraping
                 stderr text; the unrelated "draws nothing in this build"
                 string apps/kwe-manager/applyclient.cpp and
                 scripts/smoke-scene.sh match on is built independently by
                 kwe_core::preflight.rs (a structurally separate
                 implementation, since kwe-core cannot depend on
                 kwe-scene-renderer) from SceneError entirely, untouched by
                 this swap. This let the adapter's error reconstruction
                 skip byte-identical message text and match only
                 SceneErrorKind + Ok/Err.

                 Three bugs found and fixed in kwe-core/src/ir.rs (SR-2b)
                 by this slice's differential process, not by inspection --
                 exactly the "proven, not assumed" bar the task set:

                 1. "objects" shape reject missing. parse_scene_ir treated
                 BOTH a missing "objects" key AND a present-but-non-array
                 one as empty; legacy's own parse_objects rejects a
                 present-but-non-array value (only a missing key defaults
                 to empty). Fixed: a new IrError::ObjectsNotAnArray variant,
                 returned when the key is present and not a Value::Array.

                 2. instanceoverride read from the wrong object.
                 parse_particle_ir read "instanceoverride" from the
                 particle DEFINITION's own map; legacy reads it from the
                 OUTER scene object (object.get("instanceoverride"), a
                 SIBLING of "particle", applied unconditionally before
                 branching on whether "particle" is a string, an object, or
                 neither). Because no real scene nests instanceoverride
                 inside particle, this meant the IR path silently dropped
                 EVERY authored instanceoverride (a real, documented WE
                 feature, not a synthetic edge case) -- caught by
                 ir_parity_alias_precedence's colorn-vs-color case and
                 ir_parity_instanceoverride_colorn_shapes, both of which
                 initially passed VACUOUSLY (the first-draft fixtures also
                 nested instanceoverride inside particle, so both legacy
                 and the buggy adapter agreed on the wrong default; moving
                 the fixtures to the correct sibling placement is what
                 exposed the bug -- documented so the vacuous-test failure
                 mode itself is on record). Fixed: a shared
                 parse_object_instance_override(object, consumed) helper,
                 called from BOTH the Particle and ParticleFile branches of
                 parse_kind_ir (legacy applies instanceoverride to a
                 file-referenced particle system too --
                 instanceoverride_applies_to_file_ref_systems_and_scale_is_captured
                 already covered this on the legacy side); ParticleFileIr
                 widened with the same 7 instance_* fields ParticleIr
                 carries. EffectRefIr-adjacent: EffectRefIr::to_raw_value
                 and the adapter's own effects_raw_for both stopped
                 reconstructing raw JSON from typed fields (which silently
                 materializes id/name/visible defaults an entry never
                 authored) once instanceoverride's to_raw_value needed
                 fixing anyway -- see bug 3.

                 3. effects[] raw pass-through was lossy. Legacy's
                 parse_model_layer clones the raw "effects" array entries
                 UNCHANGED (no defaulting at parse time -- that happens
                 later, in sceneeffect::resolve_object_effects); the
                 adapter's first draft reconstructed each entry from
                 EffectRefIr's typed id/name/visible fields, which
                 (correctly, per SR-2b) always hold a value, indistinguish-
                 able from an authored one once typed -- so an entry that
                 never authored "visible" got a synthetic "visible":true
                 inserted, breaking Eq against legacy's untouched clone.
                 Caught by ir_parity_effects_with_unknown_keys. Fixed by
                 widening EffectRefIr with a raw: Value field (the entry's
                 original JSON, set once at parse time) and having both
                 EffectRefIr::to_raw_value and the adapter's
                 effects_raw_for return raw.clone() directly instead of
                 reconstructing -- simpler and exact.

                 A fourth item, decision (c)'s named case plus one BEYOND
                 it: speed/speedMin/speedMax are read via residue_scalar
                 against the particle definition's own unknown-bag residue
                 (resolve_speed_pair), reimplementing legacy's scalar
                 closure + swap-if-reversed logic verbatim, per the task's
                 own instruction. A SECOND raw-residue read, not named by
                 decision (c) but the same shape of fix: an object-valued
                 "particle" field with no texture/material key classifies
                 as ObjectKindIr::ParticleFile (classify_scene_object's
                 rule), but legacy's parse_particle_system does NOT re-
                 check that classification -- it parses the raw "particle"
                 value directly, and an Object gets every flat field parsed
                 exactly like an inline Particle-kind definition. ir.rs's
                 ParticleFileIr only ever captures a string file_ref, so
                 this exact sub-case's WHOLE raw object survives
                 unconsumed in object.unknown.get("particle"). New
                 build_particle_system_from_raw(definition, common, index,
                 file_ir) reimplements legacy's object-branch parsing
                 directly against that raw JSON (residue_scalar for the 8
                 clamped scalars + speed trio, raw_vector for gravity,
                 raw_particle_color for colorStart/colorEnd, file_ir's
                 already-typed instance_* fields for instanceoverride).
                 Caught by running the FULL kwe-scene-renderer test suite
                 (not just scene.rs's own), where
                 js::tests::particle_systems_registered_before_script_load
                 failed (spawn_rate defaulted to 10.0 instead of the
                 authored 100) -- the exact case now also covered by
                 ir_parity_particle_object_classified_as_particle_file_still_parses_flat_fields.

                 Three narrow, DOCUMENTED divergences remain (scene_ir_
                 adapter.rs's own module doc, "Known, documented
                 divergences" section) -- not STOP conditions: each is
                 SR-2b's own deliberate numeric-string leniency (as_number
                 accepts Number-or-String uniformly) or as_vector's [2,3]-
                 length tolerance, applied where legacy is genuinely
                 stricter --
                 (1) alpha/blendMode/colorBlendMode/general.fps/
                 general.resolution's two dims/general.clearcolor's array-
                 form elements accept a numeric STRING through the IR where
                 legacy's own .as_f64()/.as_u64() is Number-only (a VALUE
                 difference for the two tolerant fields, blendMode and
                 clearcolor's alpha-hardcoded string form aside; a
                 false-accept for the three that reject: alpha,
                 general.fps, general.resolution's direct array form);
                 (2) size's exact-2-components/non-negative legacy
                 strictness -- the sign IS recoverable and checked, but a
                 3-component size types fine through the IR ([2,3] like
                 origin/scale) where legacy rejects; (3) video rate's
                 "inf"/"nan" string tolerance (legacy's str::parse accepts
                 them as IEEE specials before clamping; the IR's
                 is_finite() filter treats them as a shape mismatch
                 instead) -- never a reject either way, a synthetic-only
                 value difference. None affects any in-repo fixture or any
                 real WE-authored content (verified: every test above
                 passes, and no real editor emits a numeric-string field or
                 a 3-component pixel size); closing them needs per-field
                 strictness modes in SR-2b's as_number/as_vector, out of
                 this slice's scope (recorded as an open risk below, per
                 the task's "show the quirk" instruction rather than
                 silently sweeping it aside).
In scope:        crates/kwe-scene-renderer/src/scene_ir_adapter.rs (new),
                 scene.rs (swap + #[cfg(test)] gating + differential
                 suite), main.rs (mod), text.rs (#[cfg(test)] gating of
                 HorizontalAlign::parse/VerticalAlign::parse, only called
                 from the now-gated parse_text_align), textures.rs
                 (DecodedTexture: PartialEq), particles.rs (ComponentModel/
                 Emitter/Initializer/Operator: PartialEq). kwe-core/src/
                 ir.rs (the 3 bugs above: ObjectsNotAnArray,
                 parse_object_instance_override + widened ParticleFileIr,
                 EffectRefIr::raw) + ir/tests.rs (2 fixtures moved to the
                 correct instanceoverride placement + the objects-not-an-
                 array test split into its own assertion). docs/SR2.md
                 (this section).
Out of scope:    Model/material/effect FILE loading (decision (a) --
                 image->model.json->material.json->.tex, effects[]
                 resolved against real files: stays legacy, unchanged by
                 this slice). Deleting the legacy parser (decision (b) --
                 stays #[cfg(test)] until a full SR-2 epic soak). The 3
                 documented numeric-string/shape-tolerance divergences
                 above (would require reopening SR-2b's as_number/
                 as_vector). Running ir_parity_corpus against a real
                 Workshop corpus (task text reserves this for the
                 coordinator after merge).
Acceptance tests:        kwe-scene-renderer: 359 tests (up from 348),
                         incl. the 11 ir_parity_* differential tests (10
                         from the slice's own acceptance run + 1 minimized
                         corpus-parity regression added post-merge, see the
                         "Corpus-parity fix-forward" paragraph below) + all
                         74 pre-existing scene.rs tests unchanged.
                         kwe-core: 30 ir:: tests (up from 29 in SR-2b) --
                         objects_missing_is_treated_as_empty_but_a_present_
                         non_array_is_rejected (renamed/split from the
                         SR-2b test this bug fix invalidated) plus the
                         instanceoverride-placement fix threaded through
                         particle_object_reads_known_fields_and_leaves_
                         speed_fields_in_unknown and round_trip_through_
                         to_raw_value_reproduces_an_equal_scene_ir.
                         871 workspace tests total, up from 859.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 871 passed, 0 failed.
                         ./scripts/check.sh -- green end to end (its scene
                         lanes, incl. scripts/smoke-scene-corpus.sh, prove
                         the swap through the real build/qml-typecheck
                         pipeline).
                         scripts/smoke-scene.sh -- every case passes
                         unchanged (B2 a/b/d, S1-S5b, M3c-M3g, the
                         standalone llvmpipe lane), running THROUGH the new
                         production swap.
                         CORPUS-PARITY (post-merge, coordinator run, then
                         verified again here after the fix below):
                         KWE_SCENE_IR_PARITY_DIR=<60-item real Workshop
                         corpus> cargo test -p kwe-scene-renderer
                         ir_parity_corpus -- --ignored --nocapture ->
                         "ir_parity_corpus: 60/60 item(s) parity-passed".
                         The coordinator's own first run found 2/60
                         failures (see "Corpus-parity fix-forward" below);
                         both are fixed and reverified 60/60 green.
Failure/recovery tests:  ir_parity_general_block's malformed-JSON/non-
                         object-root/non-array-objects/non-object-entry
                         cases assert Err/Err parity with matching
                         SceneErrorKind (never message text); ir_parity_
                         missing_name covers legacy's two distinct
                         rejecting families vs. Model's skip-never-reject
                         exception in one suite.
Upstream/provenance:    Original; every reconstruction mirrors an existing
                         in-repo parser (scene.rs's own parse_* functions,
                         cited by name throughout scene_ir_adapter.rs's own
                         comments) -- no third-party source consulted.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 871 passed, 0 failed.
                         ./scripts/check.sh -- exit 0, green end to end.
                         scripts/smoke-scene.sh -- all cases passed.
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> cargo test
                         -p kwe-scene-renderer ir_parity_corpus --
                         --ignored --nocapture -- "ir_parity_corpus: 60/60
                         item(s) parity-passed".

                         **Corpus-parity fix-forward (second commit,
                         same branch, post-merge report):** the
                         coordinator's own real-corpus run (60 items,
                         /media/crushinator/steamapps/workshop/content/431960
                         -- a real path on the coordinator's machine, never
                         copied into this repo) found 2/60 failures, both
                         `ParticleSpec` value mismatches. The FIRST version
                         of `ir_parity_corpus` aborted (panicked) on the
                         first divergence, so only 1 of the 2 failing items
                         -- and none of the 58 passing ones -- were ever
                         reported in one run; fixed first (harness-only,
                         no behavior change): the test now runs through
                         EVERY item, collects every divergence, and prints
                         a final "P/N item(s) parity-passed" summary plus a
                         per-item "basename -> diagnosis" list, so one bad
                         item never hides the rest. `assert_ir_parity` and
                         `ir_parity_corpus` also gained a shared
                         `first_diff_field`/`bounded_debug` pair: on a
                         values-differ failure they now name the first
                         top-level `SceneConfig` field (or the first
                         differing `layers[i]`/`particles[i]` index) that
                         disagrees, with each side's `Debug` output bounded
                         to ~200 bytes, instead of a bare "VALUES differ"
                         with no location.
                         ROOT CAUSE (found by reading the two real items'
                         scene.json locally for diagnosis only -- their
                         content was never copied into this repo or any
                         commit): both failing items were a `ParticleFile`
                         -kind particle system (a string `"particle"`
                         file reference) whose `instanceoverride.alpha`
                         was authored above legacy's 1.0 clamp ceiling
                         (2.0 in both). `kwe_core::ir.rs`'s
                         `parse_instance_override` deliberately does NOT
                         clamp any of its 7 `instance_*` fields (SR-2b's
                         "no range clamping" IR design, module doc
                         departure (1)); `scene.rs`'s own
                         `parse_particle_system` clamps each one
                         (`particles::clamp_instance_factor`, max 1e6 for
                         count/rate/size/lifetime/speed, max 1.0 for alpha;
                         `colorn` via `.clamp(0.0, 1.0)`) before assigning
                         into `ParticleSpec`. The Particle-kind path
                         (`build_particle_system`) already applied this
                         clamp; the TWO ParticleFile-kind builders
                         (`particle_file_spec` and
                         `build_particle_system_from_raw`, both added mid-
                         slice to fix the wrong-object `instanceoverride`
                         read reported in the original SR-2c acceptance
                         run) read `ParticleFileIr`'s fields straight
                         through UNCLAMPED -- an oversight in that same
                         mid-slice fix, invisible to every in-repo fixture
                         because none of them authored an out-of-range
                         instanceoverride factor on a ParticleFile-kind
                         system. Fixed with a shared
                         `clamp_instance_overrides(&ParticleFileIr) ->
                         (f32,...,f32)` helper (scene_ir_adapter.rs) that
                         both builders now call, applying the exact same
                         per-field clamps `build_particle_system` already
                         used. MINIMIZED synthetic fixture added as
                         `ir_parity_instanceoverride_clamps_on_particle_file_systems`
                         (scene.rs, 4 cases): a string-file-ref particle
                         with `instanceoverride.alpha: 2.0` (the exact real
                         shape, reduced to one field), `.count` above
                         1e6, the object-without-texture ParticleFile sub-
                         case with the same out-of-range alpha, and
                         `.colorn` above 1.0 -- no real corpus content in
                         any of them. Re-verified against the real corpus:
                         60/60 parity-passed.
Open risks:              The 3 documented numeric-string/shape-tolerance
                         divergences (module doc, scene_ir_adapter.rs) --
                         narrow, verified harmless against every existing
                         fixture and real WE content, but real; closing
                         them needs per-field strictness modes added to
                         SR-2b's as_number/as_vector.
                         This slice's differential process (both the
                         original acceptance run and the corpus fix-
                         forward above) found 5 real bugs across SR-2b's
                         ir.rs and this slice's own adapter that hand-built
                         fixtures alone did not catch until either a wider
                         test-suite run or the real corpus exercised them
                         -- a reminder that SR-2b's remaining unswapped
                         families (model/material/effect file loading)
                         likely carry similar undiscovered gaps until their
                         own differential slice, INCLUDING a real-corpus
                         run before declaring victory (not just after, as
                         this slice originally did), exercises them the
                         same way.
STOP findings:           None that blocked the slice. Every candidate STOP
                         case resolved to either a fixable bug (the 3 from
                         the original acceptance run plus the corpus-fix-
                         forward bug above, all fixed within this slice, in
                         kwe-core/src/ir.rs or scene_ir_adapter.rs since
                         that is where each root cause actually lived) or a
                         documented, narrow, verified-harmless divergence
                         (the 3 above, listed under Open risks).
                         No non-comparable struct member was found needing
                         a hand-written partial Eq. No external SceneError
                         message-text consumer was found.
Commit(s):               933b430, 5b80f1d
```

## SR-2c2 — investigation: the 1629635521 frame divergence (STOP — no scene/parsing bug found)

**Priority interrupt investigation**, filed after a corpus FRAME sweep (as
opposed to `ir_parity_corpus`'s PARSE-level sweep) flagged a rendering
difference on one real corpus scene (workshop id `1629635521`, a
16-particle-system, particle-file-referenced scene) between the pre-SR-2c
binary (`e6aac50`) and the post-SR-2c binary (`5b80f1d`): reference
`avg 28.1,28.1,28.1|86` nonzero px vs. candidate `avg 35.3,35.3,35.3|211`
nonzero px, each individually reported "self-stable across runs" by the
reporting sweep. `ir_parity_corpus` already passed 60/60 for this exact
scene, so the divergence — if real — had to live outside the compared
`SceneConfig`/`LayerSpec`/`ParticleSpec` structs.

**Root cause: there is no scene-parsing or particle-registration bug. The
divergence is a false positive from the corpus sweep's WALL-CLOCK-based
comparison methodology, colliding with a PRE-EXISTING (M3f-era, long
before SR-2c) property of the render loop: particle simulation is stepped
by REAL elapsed time, not a deterministic virtual clock** —
`crates/kwe-scene-renderer/src/main.rs`'s `RenderLoop::run`, the line
`let dt = now.duration_since(last_step).as_secs_f64();` (now carrying an
explanatory comment, see Outcome below). For a scene whose particle
systems are still young/ramping up (spawn count still growing, which this
16-system scene's `particles_capped` diagnostics confirm is exactly its
state within the sweep's 3 s window), the EXACT wall-clock tick count
reached by a fixed deadline is sensitive to incidental per-process timing
(OS scheduling, cache warmth, disk I/O for the pkg read, GPU driver
selection) that has NOTHING to do with scene data or renderer logic —
including, critically, being sensitive to which TWO BINARIES happen to be
compared, since different binaries (even unrelated ones) can have
different code size/layout shifting their own per-iteration overhead by a
few milliseconds, which compounds over a still-growing particle system
into a materially different frame.

**Evidence chain (each step independently verifiable, none relies on
"staring"):**

1. `git diff e6aac50 5b80f1d -- crates/kwe-scene-renderer/src/main.rs
   crates/kwe-scene-renderer/src/particles.rs` shows ZERO logic changes —
   only mechanical `PartialEq` derive additions (for the differential test
   infrastructure) and the `mod scene_ir_adapter;` line. The particle
   simulation code (`particles.rs`) and its consumer (`main.rs`'s
   `load_particle_file_definitions`, `ScriptEngine::new`'s
   `ParticleSystemState::from_spec` construction) are BYTE-IDENTICAL
   between the two binaries.
2. A temporary env-gated instrumentation (`KWE_DEBUG_DUMP_PARTICLES`,
   dumping every `ParticleSpec` via `{:?}` — including the fully decoded
   texture RGBA bytes and the resolved `ComponentModel` — right after
   `load_particle_file_definitions` runs, i.e. everything that reaches
   `ScriptEngine::new`) produced BYTE-IDENTICAL 1.2 MB dumps between the
   real `e6aac50` and `5b80f1d` binaries against the real
   `1629635521/scene.pkg` — confirming empirically, not just by parity-test
   inference, that every input to particle simulation (spec fields,
   decoded pixels, component model, PRNG seed source) is identical. This
   instrumentation was temporary (added to throwaway git worktrees for
   this investigation, never committed).
3. A second temporary instrumentation forced the render loop's `dt` to a
   FIXED synthetic value and its step count to a FIXED target (removing
   wall-clock sensitivity from the comparison entirely) via env vars.
   Under two INDEPENDENT deterministic trajectories (8 steps @ dt=0.1s;
   25 steps @ dt=0.05s), the two real binaries produced stderr AND
   rendered-frame-file output that `cmp` reports BYTE-IDENTICAL (exit 0,
   no differing byte) in both cases — the strongest possible proof that,
   with the wall-clock component neutralized, the two binaries render
   this scene identically.
4. Reproducing the ORIGINAL (wall-clock, `KWE_SWEEP_TIMEOUT=3`) sweep
   methodology against the SAME two binaries FOUR consecutive times
   produced: run 1 = "REGRESSION reference=35.3...|211 candidate=28.1...|86"
   (the exact two numbers from the original report, but with which build
   produced which number REVERSED from the original report — proving the
   split is not a fixed property of "SR-2c" vs "not SR-2c" at all); runs
   2-4 = "IDENTICAL" (both landed on 35.3.../211) — the SAME binary pair,
   same script, same scene, same machine, no code change between runs.
   This is direct, reproduced proof that the sweep's wall-clock
   methodology itself is what's non-deterministic here, not the renderer's
   handling of this scene's data.

**Deliverables (per the task's explicit "STOP and report... I will
decide" provision — there is no bug to fix, so no behavior-changing
commit was made):**

- `crates/kwe-scene-renderer/src/main.rs`: a comment-only addition at the
  `dt` computation explaining this property in place, for the next reader
  who investigates a similar false alarm.
- `scripts/scene-corpus-byte-identity-sweep.sh`: a new header-comment
  CAVEAT section naming this exact failure mode (particle-heavy scenes,
  single-run false positives) with the reproduced evidence summarized, a
  recommendation to re-run / prefer a longer timeout / prefer
  `particles.rs`'s own fixed-dt determinism unit test for this class of
  scene, and an inline reminder appended to the script's own FAILED output
  line so a future maintainer sees the caveat AT THE POINT OF FAILURE, not
  only in a header comment they may not have read.
  `deterministic_across_independent_runs` (`crates/kwe-scene-renderer/src/particles.rs`)
  ALREADY proves, at the unit level, that a FIXED dt sequence produces
  bit-identical particles — this existing test IS the "missing comparison
  surface" for the class of bug the task asked to guard against (parse/
  simulation-logic divergence); it was not extended because it already
  covers exactly this contract and nothing about SR-2c changed it.
- docs/SR2.md (this section).
- NOT changed: any scene-parsing/adapter code (`ir.rs`, `scene.rs`,
  `scene_ir_adapter.rs`) — there is nothing to fix there; `ir_parity_corpus`
  re-run fresh on this branch still passes 60/60 (unchanged, since no
  parsing code was touched).

Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 913 passed, 0 failed
                         (unchanged from SR-3a's own count -- no test was
                         added or removed; the pre-existing
                         deterministic_across_independent_runs already
                         covers the relevant contract).
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> cargo test
                         -p kwe-scene-renderer ir_parity_corpus --
                         --ignored -- "60/60 item(s) parity-passed"
                         (re-confirmed fresh on this branch).
                         ./scripts/check.sh -- exit 0, green end-to-end
                         (one transient, unrelated flake on
                         kwe-daemon's workshop_cache::tests::
                         fills_placeholder_metadata_on_missing_items on a
                         run taken while this investigation's build/render
                         load was still active on the machine -- passed in
                         isolation immediately after, and passed on a
                         clean re-run of the full check.sh moments later;
                         not this investigation's own doing, noted here
                         for the record since it happened during this
                         slice's acceptance run).
                         scripts/scene-corpus-byte-identity-sweep.sh
                         against 1629635521 specifically, same
                         reference(e6aac50)/candidate(this branch) pair,
                         4 consecutive runs -- see evidence step 4 above
                         (1 REGRESSION, 3 IDENTICAL, no code change
                         between them).
STOP findings:           The entire slice IS the STOP finding: no scene-
                         parsing, particle-registration, or rendering
                         LOGIC bug was found anywhere in SR-2c's changes.
                         The apparent regression is fully explained by
                         pre-existing (M3f-era) wall-clock-driven particle
                         simulation timing combined with the sweep
                         script's fixed-wall-clock-deadline methodology --
                         a property that predates SR-2c and can affect a
                         comparison between ANY two renderer binaries, not
                         specifically SR-2c's. Making particle simulation
                         timing fully deterministic across builds (e.g. a
                         frame-count-driven virtual clock instead of
                         `Instant::now()` deltas) would close this gap
                         properly, but is a real architecture change well
                         beyond this investigation's scope -- left for the
                         conductor to decide whether/when to schedule it.
                         Evidence: see the four-step chain above, all
                         independently reproducible (temporary
                         instrumentation was NOT committed; the two
                         reproduction commands anyone can re-run are
                         `cargo test -p kwe-scene-renderer
                         deterministic_across_independent_runs` and
                         `scripts/scene-corpus-byte-identity-sweep.sh`
                         against the same scene/binaries, run several
                         times).
Commit(s):               9271cbf
```

## SR-2d — model/material/layer-image resolution through the confined VFS

Filed as "the first VFS CALLER migration" after SR-3c2 merged, trunk
`a7b3c4c`.

Conductor decisions (verbatim):

- **(a)** Scope: the lookup closures feeding `scenemodel::resolve_material`/
  `resolve_model` plus `resolve_layer_image` in `kwe-scene-renderer`
  construct their reads from `Vfs::resolve` instead of the ad-hoc pkg/dir/
  assets chain. Fonts, scripts, video, shader includes stay legacy (later
  children).
- **(b)** The VFS's two deliberate strengthenings (componentwise symlink
  rejection; found-is-authoritative, no silent fallthrough past a
  matched-but-failed source) SHIP with this migration — accepted risk,
  gated on evidence: the resolution-trace diff and render checks must show
  ZERO corpus-visible change. If the corpus exercises either difference,
  STOP and report the item + trace line.
- **(c)** Caps: the `VfsCaps` defaults for these categories were set in
  SR-2a from the legacy caps — the migration must not change any effective
  byte limit (verify against the SR-2a semantics table; mismatch → STOP).

```text
Task:            Migrate the model/material/layer-image resolution family
                 (three call paths: the model/particle-file lookup
                 closures, and resolve_layer_image's own call sites) from
                 the ad-hoc pkg/dir/assets chain to kwe_core::Vfs, with a
                 resolution-trace oracle proving zero corpus-visible
                 difference from the two deliberate VFS strengthenings
                 SR-2a already shipped (componentwise symlink rejection,
                 found-is-authoritative).
Milestone/Slice: SR-2d
Goal:            Prove the SR-2a VFS is production-ready for its first
                 real caller family, empirically, not by inspection alone
                 -- a resolution-trace oracle comparing the OLD chain and
                 the NEW Vfs over every reference a real corpus scene
                 touches, plus the existing render-level oracles
                 (smoke-scene.sh, ir_parity_corpus) as a second,
                 independent line of evidence.
Outcome:         crates/kwe-scene-renderer/src/main.rs: load_scene builds
                 ONE kwe_core::Vfs per lane per scene load -- file lane:
                 Vfs::new(None, &root, assets_dir, VfsCaps::default());
                 pkg lane: a SECOND, independently-opened PkgReader over
                 the SAME content path (PkgReader is not Clone; `reader`
                 above is still needed for the scene.json/script entries
                 the Vfs does not touch -- re-opening re-runs
                 PkgReader::open's own TOCTOU-safe validation a second
                 time against the identical file, one extra open+parse
                 per scene load, negligible next to the render loop this
                 precedes) wrapped with Vfs::new(Some(vfs_pkg), pkg_root,
                 assets_dir, VfsCaps::default()), pkg_root = the pkg's own
                 parent directory (falls back to Path::new(".") if
                 content.parent() is None, essentially unreachable since
                 content was already opened as a real file moments
                 earlier). All 8 of load_scene's pre-migration closures
                 (4 families x 2 lanes) replaced by 2 new production
                 functions: vfs_resolve_texture(vfs, reference) ->
                 Result<Vec<u8>, String> (load_layer_textures/
                 load_particle_textures, always AssetCategory::Texture)
                 and vfs_resolve_model_asset(vfs, reference, top_level)
                 -> Option<Vec<u8>> (load_model_textures/
                 load_particle_file_definitions, category inferred per
                 reference via infer_model_asset_category: ".tex" ->
                 Texture; "materials/"-prefixed ".json" -> Model
                 (AssetCategory::Model's own doc already covers BOTH
                 model.json and material.json); otherwise the caller's
                 own top_level, Model for the model family / Particle for
                 the particle-file family). vfs_error_to_image_detail
                 maps each VfsError variant onto a string preserving
                 resolve_layer_image's own event=renderer.scene.
                 layer_skip/particle_skip detail= vocabulary as closely
                 as the differently-shaped error type allows -- see
                 "Preserved error strings" below for the exact list.
                 resolve_layer_image itself is UNCHANGED, now #[cfg(test)]
                 (decision (a)'s own allowance) -- no longer called from
                 production, kept as the canonical legacy reference the
                 resolution-trace oracle diffs against (also still backs
                 its own pre-existing unit tests unchanged).
                 Resolution-trace oracle (new, all #[cfg(test)]): a
                 TraceOutcome enum (Found{source,bytes}/NotFound/
                 Error(class)) with a trace_line() method producing the
                 task's exact "family reference -> source|NOTFOUND|
                 error-class bytes=<n>" format; classify_vfs_result maps a
                 Vfs::resolve outcome onto it; vfs_trace_texture/
                 vfs_trace_model_asset are the VFS side; 4 legacy_trace_*
                 functions (texture x{file,pkg} lane, model_asset
                 x{file,pkg} lane) reproduce TODAY's exact pre-migration
                 chains as standalone, directly-callable functions (not
                 just inline closures) so the oracle can call each side
                 independently for the same reference and diff the
                 result. An #[ignore]d corpus test,
                 vfs_resolution_trace_parity_corpus
                 (KWE_VFS_TRACE_PARITY_DIR=<dir>, KWE_VFS_TRACE=<file>
                 optionally dumps every traced line prefixed legacy:/
                 vfs:), walks every scene.json/scene.pkg under the
                 corpus, and for each layer image / particle material /
                 model_ref / particle file_ref reference, drives BOTH
                 paths: the flat texture family is one direct call per
                 side; the model/particle-file family drives
                 kwe_core::resolve_model/resolve_particle_file with a
                 DUAL-TRACING lookup closure that traces both legacy and
                 vfs at EVERY reference the real walk touches (model.json
                 -> material.json -> .tex) and continues the walk on the
                 VFS's own answer (the same answer production actually
                 uses) -- so nested references get full coverage
                 automatically, not just the top-level one. assets_dir is
                 deliberately None for this test (see the test's own doc
                 comment: this corpus is self-contained Workshop content,
                 not a WE install tree; exercising a real assets root
                 would only prove more "both NotFound" agreements, not
                 stronger coverage of where references actually resolve).
                 Ran against the real local corpus
                 (KWE_VFS_TRACE_PARITY_DIR=/media/crushinator/steamapps/
                 workshop/content/431960): 60/60 item(s) parity-passed,
                 EVERY layer/particle/model/material reference across the
                 whole corpus, zero divergence -- see "Evidence" below.
                 docs/SR2.md's own "Current resolver semantics" table
                 gets one correction (not a new finding about the CODE,
                 a finding about the DOCUMENT): the "Layer image (pkg
                 scene, model/particle-file lookups)" row conflated two
                 different closures at SR-2a's own citation -- see
                 "Findings" below.
In scope:        crates/kwe-scene-renderer/src/main.rs (load_scene's Vfs
                 construction x2 lanes, 8 closures replaced by 2
                 production functions, infer_model_asset_category,
                 vfs_error_to_image_detail, the #[cfg(test)] resolution-
                 trace oracle: TraceOutcome + 4 legacy_trace_* + 2
                 vfs_trace_* functions, resolve_layer_image gated
                 #[cfg(test)], 16 new unit tests + 1 new #[ignore]d
                 corpus test), crates/kwe-scene-renderer/src/textures.rs
                 (MAX_TEXTURE_SOURCE_BYTES gains #[allow(dead_code)] --
                 its only consumer is now the cfg(test) legacy/trace
                 code), docs/SR2.md (this section, plus the one table-row
                 correction).
Out of scope:    Fonts, scripts, video, shader includes (decision (a)'s
                 own "stays legacy" list) -- confirmed NOT touched:
                 resolve_shader_reference/the compile_material_layers
                 shader_lookup closure (main.rs ~3216-3245) is a
                 SEPARATE, independently-scoped resolution path (verified
                 by reading it) untouched by this slice.
                 PkgMatchMode (the task's own item 1 ask) -- investigated
                 and NOT built; see "Findings" below for why.
                 SR-2z's module split (deferred to the end of the SR-2
                 epic per its own overview above).
Acceptance tests:        kwe-scene-renderer: 386 tests passed (up from
                         370), 2 ignored (up from 1 -- the new corpus
                         oracle) -- 16 new: infer_model_asset_category
                         (every reference shape); vfs_error_to_image_
                         detail (bounded, names the reference, every
                         VfsError variant); vfs_resolve_texture x5
                         (pkg-hit, scene-dir-hit, assets-hit, not-found,
                         oversize); vfs_resolve_model_asset x5 (same 5,
                         through the real model.json -> material.json ->
                         .tex walk, plus a dedicated oversize-json test
                         proving decision (c)'s "effective limit
                         unchanged" claim directly: an over-
                         MAX_MODEL_JSON_BYTES model.json still fails
                         resolution, just via the VFS's own earlier
                         refusal instead of a later bounded_json check);
                         trace_outcome_line_format_matches_the_task_spec
                         (the exact wire format); 2 legacy-vs-vfs parity
                         spot checks (texture scene-dir family, model-
                         asset full-chain family); ONE test that proves
                         (not just documents) the pkg-lane texture-family
                         breadth-widening finding directly --
                         migration_widens_the_pkg_lane_texture_family_
                         breadth_beyond_pkg_entries_only constructs a
                         reference present ONLY beside the pkg (not in
                         its table), shows legacy_trace_texture_pkg_lane
                         returns NotFound and vfs_trace_texture returns
                         Found{SceneDir,...} for the IDENTICAL input --
                         the widening is real, asserted, not assumed.
                         933->386 corpus-scale coverage: the #[ignore]d
                         vfs_resolution_trace_parity_corpus, run against
                         the real local corpus (60 items,
                         /media/crushinator/steamapps/workshop/content/
                         431960) -- "60/60 item(s) parity-passed",
                         EVERY layer image / particle material / model
                         chain / particle-file chain reference in every
                         corpus scene resolves identically between the
                         legacy chain and the migrated VFS, INCLUDING
                         through the widened pkg-lane texture-family
                         breadth (decision (b)'s evidence gate: zero
                         actual divergence observed against real content,
                         despite the theoretical breadth difference).
                         KWE_SCENE_IR_PARITY_DIR=<real corpus>
                         ir_parity_corpus -- 60/60 (unchanged, decision
                         (c)/task's own "should be untouched" -- no
                         scene-parsing code was touched this slice).
                         963 workspace tests total, up from 947.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 963 passed, 0 failed.
                         ./scripts/check.sh -- exit 0 (one retry needed:
                         the first run hit a single unrelated flaky
                         failure, kwe-cdp's transport::tests::
                         drop_closes_the_fds -- an fd-number equality
                         assertion racing against fd reuse from OTHER
                         tests in the same parallel test process, in a
                         crate this slice never touches; confirmed flaky
                         by re-running it alone immediately after, which
                         passed cleanly, then re-running the whole script
                         for a clean end-to-end record).
                         scripts/smoke-scene.sh -- exit 0, every case
                         passed unchanged, INCLUDING every model-texture/
                         layer-image/particle-file smoke case this
                         migration's own production code path now serves
                         (S1/S2/S3/S4/S4b/S5a/S5b/B2 and the M3c/M3f
                         layer-image/particle-texture cases) -- the
                         strongest available RENDER-level proof that
                         decision (b)'s "zero corpus-visible change"
                         claim holds in practice, on top of the
                         resolution-trace oracle's own byte-level proof.
Failure/recovery tests:  vfs_resolve_texture_not_found/oversize,
                         vfs_resolve_model_asset_not_found/oversize --
                         every failure mode load_layer_textures/
                         load_model_textures already treat as a skip-not-
                         reject event still degrades the same way through
                         the VFS (proven directly, not just by the
                         closures' own unchanged signatures).
Upstream/provenance:    Original; the migration pattern (build one Vfs
                         per scene load, replace closures, prove parity
                         with a dual-path trace oracle before trusting
                         the swap) is this slice's own design, following
                         SR-2a's own explicit staging plan ("SR-2c/2d+
                         establish/repeat the migration pattern... behind
                         a differential test").
Findings (STOP-heavy slice -- investigated and reported, not silently
resolved either way):
  1. **PkgMatchMode (task item 1's own ask) — NOT built, because the gap
     it targets does not exist.** `Vfs::resolve`'s pkg-entry branch
     already calls `kwe_core::pkg::resolve_pkg_entry` directly
     (`crates/kwe-core/src/vfs.rs`'s own `resolve`/`resolve_path`
     bodies) — the EXACT SAME function `image_entry`/`asset_entry`/
     `video_entry`/`script_entry` all delegate to (`pkg.rs:684-717`,
     `pkg.rs:754-779`). This was SR-2a's own explicit, ALREADY-SHIPPED
     design (this document's own "pkg entry matching" table row, and
     `vfs.rs`'s own pre-existing test
     `pkg_lookup_is_case_insensitive_and_matches_by_tail_like_
     resolve_pkg_entry`) — case-insensitive tail matching, not exact
     normalized matching, on BOTH sides already. There is no pkg-
     matching semantic gap to bridge for this family; a `PkgMatchMode`
     toggle would be dead scaffolding for a divergence that does not
     exist in the current codebase. Recorded here in case the task's
     premise traces back to an earlier design draft that later changed
     without this document being updated to match.
  2. **Pkg tail-matching ambiguity (task's own STOP trigger) — checked,
     not a STOP.** `resolve_pkg_entry`'s `entries: &[PkgEntry]` is
     iterated as a plain slice in FILE (package table) order — never a
     `HashMap` — and 2+ matches is an explicit `Err` ("matches N package
     entries; exactly one is required"), never an undefined pick.
     Deterministic on both the legacy and VFS sides (they share the one
     function, per finding 1).
  3. **A THIRD divergence beyond decision (b)'s named two, found while
     migrating: the plain-image-layer family's pkg-lane breadth widens.**
     `load_layer_textures`/`load_particle_textures`'s pkg-lane closure
     tries ONLY pkg entries today (no directory fallback at all) — a
     narrower chain than `load_model_textures`/
     `load_particle_file_definitions`'s pkg-entries -> pkg-dir ->
     assets-root chain. This document's own SR-2a table (the "Layer
     image (pkg scene, model/particle-file lookups)" row) INCORRECTLY
     described these as the SAME chain — a transcription error in the
     ORIGINAL table (confirmed by reading the actual current source,
     not a later regression this slice introduced) — corrected in that
     row above. Migrating the plain-image pkg-lane family to the ONE
     shared Vfs (decision (a)'s literal "one Vfs per scene load, used
     for the model/material closures PLUS resolve_layer_image")
     necessarily widens its breadth to match the full three-source
     chain. This is real (proven directly by
     `migration_widens_the_pkg_lane_texture_family_breadth_beyond_
     pkg_entries_only`) but NOT a corpus-visible change: the resolution-
     trace oracle's 60/60 real-corpus run shows zero actual divergence
     — no corpus item has a plain image reference that resolves
     differently once the wider breadth is available (the pkg's own
     directory and the WE assets root essentially never contain a
     same-named file the pkg table doesn't already have). Reported per
     decision (b)'s own evidence-gating spirit, extended to this
     additional finding rather than silently shipped or silently
     reverted.
  4. **Vfs::new's mandatory scene_root vs. the pkg lane's optional
     pkg_dir.** `Vfs::new` requires a canonicalizable `scene_root`
     unconditionally; the legacy pkg-lane chain instead treats
     `content.parent().and_then(|p| p.canonicalize().ok())` as
     `Option<PathBuf>`, gracefully SKIPPING the dir-fallback step (not
     failing the whole resolution) when it is `None`. Since `content`
     was already opened as a valid `PkgReader` moments earlier in the
     SAME function, its parent directory is essentially guaranteed to
     exist and canonicalize — this path is not reachable in practice
     (confirmed: the 60-item corpus run never hit it) — but it IS a
     real behavior difference in the theoretical case where the pkg's
     own directory vanishes between the two opens: legacy would degrade
     gracefully (pkg-entries-only resolution continues), the migrated
     code hard-rejects the scene (`reject_pkg`). Documented rather than
     silently accepted; not judged worth adding a parallel degraded-Vfs
     construction path for a race this narrow.
Open risks:              Finding 3 above (the pkg-lane texture-family
                         breadth widening) is evidence-gated on the LOCAL
                         60-item corpus, not the full Workshop catalog —
                         a scene outside this local corpus with a
                         same-named file in BOTH the pkg table and beside
                         it on disk (or in a configured WE assets root)
                         would now resolve to the assets-root/pkg-dir
                         copy in a case where it previously could not
                         have (pkg-lane images never checked those
                         sources before). Considered unlikely (Workshop
                         packaging convention keeps a scene's own assets
                         inside its own pkg) but not exhaustively proven
                         beyond this document's own evidence.
                         Finding 4 above (Vfs::new's mandatory scene_root)
                         is a real, if practically unreachable, behavior
                         difference — noted for whoever eventually
                         hardens `Vfs::new` to accept an optional
                         scene_root, if that is ever judged worth doing.
                         SR-2a's own already-recorded open items (the
                         `video_probe` cap's inert placeholder status,
                         the fall-through-vs-authoritative risk for the
                         STILL-unmigrated families: shader includes,
                         scripts, video, fonts) remain open, unaffected
                         by this slice.
STOP findings:           None of the task's own three named STOP
                         triggers fired: (b)'s corpus-visible change --
                         zero observed across 60/60 real items, including
                         for the newly-identified THIRD divergence
                         (finding 3); (c)'s cap mismatch -- verified
                         exact (AssetCategory::Texture = 64 MiB =
                         MAX_TEXTURE_SOURCE_BYTES = MAX_PKG_ENTRY_BYTES;
                         AssetCategory::Model = AssetCategory::Particle =
                         1 MiB = MAX_MODEL_JSON_BYTES =
                         MAX_PARTICLE_FILE_BYTES exactly, and the model/
                         material JSON reads' EFFECTIVE limit is
                         unchanged even though the VFS now enforces it
                         earlier than `bounded_json` used to -- proven by
                         `vfs_resolve_model_asset_oversize_json_refuses_
                         before_the_model_json_cap`); pkg tail-matching
                         ambiguity -- deterministic, never HashMap-
                         ordered (finding 2). Findings 1, 3, and 4 above
                         are reported per the task's own "STOP-heavy...
                         investigated and reported" framing even though
                         none independently blocks the slice, since all
                         three are genuine discoveries the coordinator
                         did not already know about from the task text
                         alone.
Commit(s):               0bb4cf3
```
