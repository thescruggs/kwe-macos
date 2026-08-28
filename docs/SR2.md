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
| Layer image (pkg scene, model/particle-file lookups) | pkg entry table first (see "pkg entry matching" below), else `resolve_layer_image` against the pkg's own parent dir, else against the assets root — SAME function, SAME rules as the row above, just given two different roots in turn. | Same 64 MiB, checked per source | pkg → pkg-parent-dir → assets root | `main.rs:1246-1261` (pkg entries), `main.rs:1262-1304` (model layers, particle files) |
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
Commit(s):               (fill in after commit)
```
