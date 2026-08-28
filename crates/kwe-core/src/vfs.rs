// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-2a: a confined asset virtual filesystem — one type unifying the
//! pkg-entries → scene-dir → assets-root lookup chain every scene-asset
//! family (`kwe-scene-renderer`'s image/shader/video resolvers,
//! `kwe-core::scenemodel`'s model/material resolver) already implements
//! separately today, each with its own small drift from the others.
//!
//! **This module adds the type only — no existing `resolve_*` call site is
//! migrated to it in this slice** (SR-2a conductor decision (a)). Caller
//! migration happens one asset family at a time in SR-2c+, each with its
//! own differential test against today's behavior, so any per-family
//! difference this module's semantics does NOT already match is surfaced
//! and decided deliberately during that migration rather than silently
//! unified now.
//!
//! `docs/SR2.md`'s "Current resolver semantics" table is the differential
//! baseline this module's contract was built from (SR-2a conductor decision
//! (b): confinement here equals the STRICTEST already-tested behavior in
//! the codebase — never looser than any existing resolver, and in two
//! places (the componentwise intermediate-directory symlink walk; treating
//! a found-but-rejected/oversize source as authoritative instead of
//! silently falling through to a different source) deliberately stricter
//! than every existing resolver, which is called out explicitly below and
//! in that doc rather than left implicit).

use std::{
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::pkg::{PkgErrorKind, PkgReader, resolve_pkg_entry};

/// A reference longer than this is refused before any lookup — scene.json
/// asset references are short hand-authored relative paths in the real
/// corpus (see `docs/SR2.md`); this is a safety bound, not a realistic
/// content limit.
const MAX_REFERENCE_BYTES: usize = 512;

/// The asset families the VFS confines and caps independently. Every
/// existing resolver this module's contract is built from is named in
/// `docs/SR2.md`'s semantics table alongside the category it maps to here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetCategory {
    /// Layer/particle/model textures (`.png`/`.jpg`/`.tex`).
    Texture,
    /// Raw (pre-preprocess) shader source text (`.vert`/`.frag`/`.h`).
    ShaderText,
    /// `model.json` / `material.json` descriptors.
    Model,
    /// Particle system definition files (`.pkfx`-equivalent JSON).
    Particle,
    /// A scene's `general.script` JavaScript entry.
    Script,
    /// The `scene.json` descriptor itself.
    Json,
    /// Video layer sources. `resolve` refuses this category outright —
    /// video is always resolved by path (`resolve_path`), never read into
    /// memory here; see that method's doc comment.
    Video,
}

/// Which of the VFS's three lookup sources actually answered a `resolve`/
/// `resolve_path` call. The pkg entry table always wins a tie by
/// construction (it is tried first and, once a reference matches an entry,
/// is authoritative for that reference — see the module doc's "found is
/// authoritative" rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsSource {
    PkgEntry,
    SceneDir,
    AssetsRoot,
}

/// Per-category read caps. `Default` is today's ACTUAL caps, gathered from
/// the existing resolvers `docs/SR2.md`'s semantics table cites by
/// file:line; a family with no cap of its own today reuses the tightest
/// existing analog (documented per-field below and in that table) rather
/// than inventing a new number.
#[derive(Debug, Clone, Copy)]
pub struct VfsCaps {
    pub texture: u64,
    pub shader_text: u64,
    pub model: u64,
    pub particle: u64,
    pub script: u64,
    pub json: u64,
    pub video_probe: u64,
}

impl Default for VfsCaps {
    fn default() -> Self {
        Self {
            // Mirrors kwe-scene-renderer::textures::MAX_TEXTURE_SOURCE_BYTES
            // (64 MiB) — kwe-core cannot depend on kwe-scene-renderer, so
            // this reuses kwe-core's own `pkg::MAX_PKG_ENTRY_BYTES`, which
            // already agrees with it (and with `scenemodel::
            // MODEL_ASSET_READ_CAP`) at the same 64 MiB value today.
            texture: crate::pkg::MAX_PKG_ENTRY_BYTES,
            // No kwe-core constant exists for this: `materialshader::
            // MAX_SHADER_TEXT_BYTES` and `kwe-scene-renderer::main::
            // MAX_SHADER_SOURCE_BYTES` (which the latter's own doc comment
            // says are deliberately equal) both live in kwe-scene-renderer.
            // Mirrored here as a literal; see docs/SR2.md.
            shader_text: 256 * 1024,
            model: crate::scenemodel::MAX_MODEL_JSON_BYTES,
            particle: crate::particlefile::MAX_PARTICLE_FILE_BYTES,
            script: crate::pkg::MAX_SCRIPT_BYTES,
            json: crate::pkg::MAX_SCENE_JSON_BYTES,
            // No video-probe concept exists anywhere in the codebase today
            // (every existing video resolver either checks metadata alone
            // or hands a path to libmpv; nothing reads bounded video BYTES
            // through a cap like this). Per the SR-2a task's instruction to
            // pick the tightest EXISTING cap rather than invent one: the
            // tightest value already established above is shader_text's
            // 256 KiB, reused here. `resolve` refuses the Video category
            // outright in this slice, so this field is inert until a
            // future slice gives it a real reader.
            video_probe: 256 * 1024,
        }
    }
}

/// The result of a successful `Vfs::resolve` — the asset's bytes, already
/// read and bounded to its category's cap.
#[derive(Debug, Clone)]
pub struct ResolvedAsset {
    /// The normalized reference (already validated: relative, forward
    /// slashes, no `.`/`..`/empty components) — identical to the input
    /// `reference` string on every success, since normalization only ever
    /// rejects, never rewrites.
    pub logical_id: String,
    pub source: VfsSource,
    pub bytes: Vec<u8>,
}

/// The result of a successful `Vfs::resolve_path`: a confined, on-disk
/// path — no bytes read. **TOCTOU caveat** (same one `resolve_layer_video`
/// already documents for its own test-only path-returning form): this path
/// is confined and cap-checked at the instant of the call, but returning a
/// bare `PathBuf` cannot close the gap between this validation and whatever
/// the caller does with the path next. A caller that later *opens* the
/// path (rather than handing it to an external process like libmpv, which
/// re-opens by path anyway) should still open with `O_NOFOLLOW` and
/// re-verify, the way `kwe-scene-renderer`'s production `open_video_source`
/// does — `resolve_path` validates once, it does not hold anything open.
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub logical_id: String,
    pub source: VfsSource,
    pub path: PathBuf,
}

/// `Vfs::resolve`/`resolve_path` failures. Style mirrors this crate's other
/// error enums (e.g. `KvError`): `thiserror`, one variant per distinct
/// caller-actionable outcome.
#[derive(Debug, Error)]
pub enum VfsError {
    /// The reference itself is malformed — never reaches any lookup.
    #[error("{0}")]
    BadReference(&'static str),
    /// The reference is well-formed but resolves in none of the configured
    /// sources.
    #[error("asset not found in any configured source")]
    NotFound,
    /// The reference resolved to a real asset in some source, but that
    /// asset is over the category's cap. Authoritative for that source —
    /// see the module doc's "found is authoritative" rule: this does NOT
    /// fall through to try a different source.
    #[error("asset exceeds the {limit} byte cap for {category:?}")]
    Oversize { category: AssetCategory, limit: u64 },
    /// A path component (intermediate directory or leaf) is a symlink.
    /// Authoritative, same rule as `Oversize` above.
    #[error("asset path contains a symlink component")]
    SymlinkRejected,
    /// The reference matched a pkg entry, but `resolve_path` was called —
    /// a pkg-embedded asset has no addressable on-disk path (only
    /// `SceneDir`/`AssetsRoot` sources are; see `resolve_path`'s doc
    /// comment).
    #[error("asset is embedded in the package and has no addressable path")]
    NotAddressable,
    /// A filesystem operation failed for a reason other than "missing"
    /// (permission, a race, or similar).
    #[error("I/O error: {0}")]
    Io(String),
}

/// The confined asset VFS for one scene: an optional open package, the
/// scene's own content directory, and an optional Wallpaper Engine assets
/// root, looked up in that order (SR-2a conductor decision (b) baseline:
/// `docs/SR2.md`'s semantics table).
#[derive(Debug)]
pub struct Vfs {
    pkg: Option<PkgReader>,
    scene_root: PathBuf,
    assets_root: Option<PathBuf>,
    caps: VfsCaps,
}

impl Vfs {
    /// `scene_root` must exist and canonicalize (every scene — file or pkg
    /// — has a real content directory on disk; a pkg's own parent
    /// directory is what `kwe-scene-renderer`'s S3/S4b lookup chains
    /// already pass as this root, per `docs/SR2.md`). `assets_root`, when
    /// given, canonicalizes best-effort: a configured-but-missing assets
    /// root degrades to "not configured" rather than failing construction
    /// — mirrors `kwe-scene-renderer/src/main.rs`'s own
    /// `assets_dir.and_then(|dir| dir.canonicalize().ok())` (a missing WE
    /// assets install must not prevent a scene that never needs it from
    /// resolving anything).
    pub fn new(
        pkg: Option<PkgReader>,
        scene_root: &Path,
        assets_root: Option<&Path>,
        caps: VfsCaps,
    ) -> Result<Self, VfsError> {
        let scene_root = scene_root
            .canonicalize()
            .map_err(|error| VfsError::Io(error.to_string()))?;
        let assets_root = assets_root.and_then(|root| root.canonicalize().ok());
        Ok(Self {
            pkg,
            scene_root,
            assets_root,
            caps,
        })
    }

    fn cap_for(&self, category: AssetCategory) -> u64 {
        match category {
            AssetCategory::Texture => self.caps.texture,
            AssetCategory::ShaderText => self.caps.shader_text,
            AssetCategory::Model => self.caps.model,
            AssetCategory::Particle => self.caps.particle,
            AssetCategory::Script => self.caps.script,
            AssetCategory::Json => self.caps.json,
            AssetCategory::Video => self.caps.video_probe,
        }
    }

    /// Resolve `reference` to bytes, trying the pkg entry table, then the
    /// scene directory, then the assets root, in that fixed order — the
    /// pkg table wins a tie by construction, and once ANY source has a
    /// real (found) answer for the reference, that source is authoritative
    /// (an oversize or symlinked hit there is a hard refusal, never a
    /// silent fallback to a different source that might happen to also
    /// have that name — see the module doc). Same inputs always pick the
    /// same source (determinism).
    ///
    /// Refuses `AssetCategory::Video` outright: video sources are handed to
    /// libmpv by path, never read into memory — use `resolve_path`.
    pub fn resolve(
        &self,
        reference: &str,
        category: AssetCategory,
    ) -> Result<ResolvedAsset, VfsError> {
        if category == AssetCategory::Video {
            return Err(VfsError::BadReference(
                "video assets are resolved via resolve_path, not resolve",
            ));
        }
        let components = normalize_reference(reference)?;
        let logical_id = reference.to_string();
        let cap = self.cap_for(category);

        if let Some(pkg) = &self.pkg
            && let Ok(index) = resolve_pkg_entry(&logical_id, pkg.entries(), "asset")
        {
            return match pkg.read_entry_bounded(index, cap) {
                Ok(bytes) => Ok(ResolvedAsset {
                    logical_id,
                    source: VfsSource::PkgEntry,
                    bytes,
                }),
                Err(error) if error.kind == PkgErrorKind::Bounds => Err(VfsError::Oversize {
                    category,
                    limit: cap,
                }),
                Err(error) => Err(VfsError::Io(error.to_string())),
            };
        }

        if let Some(outcome) = resolve_bytes_in_dir(
            &self.scene_root,
            VfsSource::SceneDir,
            &components,
            &logical_id,
            cap,
            category,
        ) {
            return outcome;
        }
        if let Some(assets_root) = &self.assets_root
            && let Some(outcome) = resolve_bytes_in_dir(
                assets_root,
                VfsSource::AssetsRoot,
                &components,
                &logical_id,
                cap,
                category,
            )
        {
            return outcome;
        }
        Err(VfsError::NotFound)
    }

    /// Resolve `reference` to a confined on-disk path — no bytes read, and
    /// only ever a `SceneDir`/`AssetsRoot` source: a reference that matches
    /// a pkg entry answers `NotAddressable` immediately (authoritative,
    /// never falls back to a same-named file on disk — a pkg-embedded
    /// asset genuinely has no path of its own; `kwe-scene-renderer`'s
    /// production pkg video lane extracts such an entry's BYTES into a
    /// private worker-owned file instead, a different operation this
    /// narrow path-only API does not perform). Same confinement and same
    /// "found is authoritative" rule as `resolve`; the category's cap is
    /// still checked from metadata even though nothing is read (mirrors
    /// `resolve_layer_video`/`open_video_source`, which check the source's
    /// size without reading it either). See `ResolvedPath`'s doc comment
    /// for this method's TOCTOU caveat.
    pub fn resolve_path(
        &self,
        reference: &str,
        category: AssetCategory,
    ) -> Result<ResolvedPath, VfsError> {
        let components = normalize_reference(reference)?;
        let logical_id = reference.to_string();
        let cap = self.cap_for(category);

        if let Some(pkg) = &self.pkg
            && resolve_pkg_entry(&logical_id, pkg.entries(), "asset").is_ok()
        {
            return Err(VfsError::NotAddressable);
        }

        if let Some(outcome) = resolve_path_in_dir(
            &self.scene_root,
            VfsSource::SceneDir,
            &components,
            &logical_id,
            cap,
            category,
        ) {
            return outcome;
        }
        if let Some(assets_root) = &self.assets_root
            && let Some(outcome) = resolve_path_in_dir(
                assets_root,
                VfsSource::AssetsRoot,
                &components,
                &logical_id,
                cap,
                category,
            )
        {
            return outcome;
        }
        Err(VfsError::NotFound)
    }
}

/// Normalize and validate `reference` into path components. Deliberately
/// its own hand-rolled splitter rather than `std::path::Path::components()`
/// — `Path`'s own iterator silently collapses a repeated separator
/// (`a//b` parses to exactly the same two components as `a/b`), which
/// would let a hostile `a//b` reference through unnoticed; splitting on
/// `/` ourselves keeps every empty segment (leading, trailing, doubled)
/// visible and rejectable. Stricter than every existing resolver in one
/// respect (`docs/SR2.md`): a bare `.` component is rejected here, where
/// today's resolvers (and `resolve_pkg_entry`) only ever check for `..`.
fn normalize_reference(reference: &str) -> Result<Vec<String>, VfsError> {
    if reference.is_empty() {
        return Err(VfsError::BadReference("reference must not be empty"));
    }
    if reference.len() > MAX_REFERENCE_BYTES {
        return Err(VfsError::BadReference(
            "reference exceeds the 512 byte length limit",
        ));
    }
    if reference.as_bytes().contains(&0) {
        return Err(VfsError::BadReference(
            "reference must not contain a NUL byte",
        ));
    }
    if reference.contains('\\') {
        return Err(VfsError::BadReference(
            "reference must use forward slashes only",
        ));
    }
    if reference.starts_with('/') {
        return Err(VfsError::BadReference("reference must not be absolute"));
    }
    let mut components = Vec::new();
    for part in reference.split('/') {
        if part.is_empty() {
            return Err(VfsError::BadReference(
                "reference must not contain an empty path component",
            ));
        }
        if part == "." || part == ".." {
            return Err(VfsError::BadReference(
                "reference must not contain a \".\" or \"..\" component",
            ));
        }
        components.push(part.to_string());
    }
    Ok(components)
}

/// Walk `components` from `root` (already canonicalized), checking EVERY
/// component — every intermediate directory AND the leaf — with
/// `symlink_metadata` and refusing the whole lookup the instant any of them
/// is a symlink. This is the deliberate strengthening past every existing
/// resolver (`docs/SR2.md`): `resolve_layer_image`/`confined_read` check
/// nothing componentwise (they rely solely on the final `canonicalize()` +
/// `starts_with(root)`, which only catches an ESCAPING symlink, tolerating
/// one that resolves back inside root); `resolve_layer_video`'s production
/// path (`open_video_source`) goes further with an `O_NOFOLLOW` open at the
/// LEAF only, still nothing for intermediate directories. The VFS rejects
/// ANY symlink, anywhere in the path, whether or not it would have stayed
/// inside root.
///
/// A trailing `canonicalize()` + `starts_with(root)` check runs anyway,
/// once the componentwise walk has already proven every component is a
/// real (non-symlink) file/directory — belt-and-suspenders matching the
/// containment pattern every other resolver in the codebase already uses,
/// not a load-bearing check on its own here.
fn confined_leaf(root: &Path, components: &[String]) -> Result<PathBuf, VfsError> {
    let mut current = root.to_path_buf();
    let last = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(VfsError::NotFound);
            }
            Err(error) => return Err(VfsError::Io(error.to_string())),
        };
        if metadata.file_type().is_symlink() {
            return Err(VfsError::SymlinkRejected);
        }
        if index == last {
            if !metadata.is_file() {
                return Err(VfsError::NotFound);
            }
        } else if !metadata.is_dir() {
            return Err(VfsError::NotFound);
        }
    }
    let canonical = current
        .canonicalize()
        .map_err(|error| VfsError::Io(error.to_string()))?;
    if !canonical.starts_with(root) {
        return Err(VfsError::SymlinkRejected);
    }
    Ok(canonical)
}

/// Read a confined leaf's bytes, bounded to `cap`: opens with `O_NOFOLLOW`
/// (defense-in-depth against a swap between `confined_leaf`'s check and
/// this open — the componentwise walk already refused a symlink at this
/// exact point, so this should never actually trip), reads at most
/// `cap + 1` bytes (mirrors `scan::read_bytes_limited`'s overflow-safe
/// pattern) so "exactly at the cap" and "over the cap" stay
/// distinguishable without ever buffering past the cap.
fn read_leaf_bytes(path: &Path, cap: u64, category: AssetCategory) -> Result<Vec<u8>, VfsError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| VfsError::Io(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| VfsError::Io(error.to_string()))?;
    if !metadata.is_file() {
        return Err(VfsError::NotFound);
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(cap) as usize);
    (&file)
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| VfsError::Io(error.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(VfsError::Oversize {
            category,
            limit: cap,
        });
    }
    Ok(bytes)
}

/// One directory source's attempt at `resolve`: `None` means "not present
/// here, try the next source"; `Some` is an authoritative outcome (success,
/// symlink rejection, or oversize) that must not fall through.
fn resolve_bytes_in_dir(
    root: &Path,
    source: VfsSource,
    components: &[String],
    logical_id: &str,
    cap: u64,
    category: AssetCategory,
) -> Option<Result<ResolvedAsset, VfsError>> {
    match confined_leaf(root, components) {
        Ok(leaf) => Some(
            read_leaf_bytes(&leaf, cap, category).map(|bytes| ResolvedAsset {
                logical_id: logical_id.to_string(),
                source,
                bytes,
            }),
        ),
        Err(VfsError::NotFound) => None,
        Err(other) => Some(Err(other)),
    }
}

/// `resolve_bytes_in_dir`'s `resolve_path` counterpart: same confinement,
/// a metadata-only cap check instead of a read.
fn resolve_path_in_dir(
    root: &Path,
    source: VfsSource,
    components: &[String],
    logical_id: &str,
    cap: u64,
    category: AssetCategory,
) -> Option<Result<ResolvedPath, VfsError>> {
    match confined_leaf(root, components) {
        Ok(leaf) => {
            let metadata = match fs::symlink_metadata(&leaf) {
                Ok(metadata) => metadata,
                Err(error) => return Some(Err(VfsError::Io(error.to_string()))),
            };
            if metadata.len() > cap {
                return Some(Err(VfsError::Oversize {
                    category,
                    limit: cap,
                }));
            }
            Some(Ok(ResolvedPath {
                logical_id: logical_id.to_string(),
                source,
                path: leaf,
            }))
        }
        Err(VfsError::NotFound) => None,
        Err(other) => Some(Err(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::testutil::PkgWriter;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kwe-vfs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Builds a real `PkgReader` over a synthetic archive, reusing
    /// kwe-core's own `pkg::testutil::PkgWriter` (SR-0c's fixture builder,
    /// `pub(crate)` — visible from this module since both live in
    /// kwe-core, unlike kwe-scene-inspector's own tests, which had to
    /// duplicate its byte layout because it is a different crate). Every
    /// call gets its own directory (a monotonic counter, not `tmpdir`'s
    /// fixed per-tag name): cargo runs this crate's tests concurrently in
    /// one process, and more than one test in this module builds a pkg, so
    /// a shared fixed path would race a `remove_dir_all` in one test
    /// against a `PkgReader::open` in another.
    fn build_pkg(entries: &[(&str, &[u8])]) -> PkgReader {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut writer = PkgWriter::new();
        for (path, payload) in entries {
            writer.add(path, payload);
        }
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("kwe-vfs-pkg-src-{}-{serial}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scene.pkg");
        writer.write(&path, "0001");
        PkgReader::open(&path).unwrap()
    }

    #[test]
    fn priority_pkg_beats_scene_dir_beats_assets_root() {
        let scene_dir = tmpdir("priority-scene");
        let assets_dir = tmpdir("priority-assets");
        fs::write(scene_dir.join("shared.png"), b"scene-bytes").unwrap();
        fs::write(assets_dir.join("shared.png"), b"assets-bytes").unwrap();
        fs::write(scene_dir.join("dironly.png"), b"scene-only-bytes").unwrap();
        fs::write(assets_dir.join("dironly.png"), b"assets-only-bytes").unwrap();
        let pkg = build_pkg(&[("shared.png", b"pkg-bytes")]);
        let vfs = Vfs::new(Some(pkg), &scene_dir, Some(&assets_dir), VfsCaps::default()).unwrap();

        // Present in all three: the pkg wins by construction.
        let hit = vfs.resolve("shared.png", AssetCategory::Texture).unwrap();
        assert_eq!(hit.bytes, b"pkg-bytes");
        assert_eq!(hit.source, VfsSource::PkgEntry);

        // Absent from the pkg: the scene directory wins over the assets
        // root.
        let hit = vfs.resolve("dironly.png", AssetCategory::Texture).unwrap();
        assert_eq!(hit.bytes, b"scene-only-bytes");
        assert_eq!(hit.source, VfsSource::SceneDir);

        // Present only in the assets root: the last resort still answers.
        fs::write(assets_dir.join("assetsonly.png"), b"assets-root-bytes").unwrap();
        let hit = vfs
            .resolve("assetsonly.png", AssetCategory::Texture)
            .unwrap();
        assert_eq!(hit.bytes, b"assets-root-bytes");
        assert_eq!(hit.source, VfsSource::AssetsRoot);
    }

    #[test]
    fn pkg_lookup_is_case_insensitive_and_matches_by_tail_like_resolve_pkg_entry() {
        // Mirrors resolve_pkg_entry's exact algorithm (docs/SR2.md):
        // case-insensitive, and a reference matches either the literal
        // entry path or the entry's tail after a `/`.
        let pkg = build_pkg(&[("Wallpaper/Materials/Deco.PNG", b"tail-match")]);
        let vfs = Vfs::new(
            Some(pkg),
            &tmpdir("pkg-tail-scene"),
            None,
            VfsCaps::default(),
        )
        .unwrap();
        let resolved = vfs
            .resolve("materials/deco.png", AssetCategory::Texture)
            .unwrap();
        assert_eq!(resolved.bytes, b"tail-match");
        assert_eq!(resolved.source, VfsSource::PkgEntry);
    }

    #[test]
    fn hostile_references_are_bad_reference() {
        let vfs = Vfs::new(None, &tmpdir("hostile-scene"), None, VfsCaps::default()).unwrap();
        let long = "a".repeat(MAX_REFERENCE_BYTES + 1);
        let with_nul = format!("a{}b", '\0');
        for reference in [
            "../x",
            "a/../x",
            "/abs",
            "a//b",
            "",
            long.as_str(),
            with_nul.as_str(),
            "a\\b",
            "./x",
            "x/.",
        ] {
            let result = vfs.resolve(reference, AssetCategory::Texture);
            assert!(
                matches!(result, Err(VfsError::BadReference(_))),
                "{reference:?} -> {result:?}"
            );
        }
    }

    #[test]
    fn scene_dir_rejects_a_symlinked_leaf_and_a_symlinked_intermediate_directory() {
        let scene_dir = tmpdir("symlink-scene");
        let outside = tmpdir("symlink-scene-outside");
        fs::create_dir_all(scene_dir.join("textures")).unwrap();
        fs::write(outside.join("secret.png"), b"secret").unwrap();
        fs::write(outside.join("inner.png"), b"inner").unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.png"),
            scene_dir.join("textures").join("leaf.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, scene_dir.join("linked_dir")).unwrap();

        let vfs = Vfs::new(None, &scene_dir, None, VfsCaps::default()).unwrap();
        assert!(matches!(
            vfs.resolve("textures/leaf.png", AssetCategory::Texture),
            Err(VfsError::SymlinkRejected)
        ));
        assert!(matches!(
            vfs.resolve("linked_dir/inner.png", AssetCategory::Texture),
            Err(VfsError::SymlinkRejected)
        ));
    }

    #[test]
    fn assets_root_rejects_a_symlinked_leaf_and_a_symlinked_intermediate_directory() {
        let scene_dir = tmpdir("symlink-assets-scene");
        let assets_dir = tmpdir("symlink-assets-root");
        let outside = tmpdir("symlink-assets-outside");
        fs::create_dir_all(assets_dir.join("textures")).unwrap();
        fs::write(outside.join("secret.png"), b"secret").unwrap();
        fs::write(outside.join("inner.png"), b"inner").unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.png"),
            assets_dir.join("textures").join("leaf.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, assets_dir.join("linked_dir")).unwrap();

        let vfs = Vfs::new(None, &scene_dir, Some(&assets_dir), VfsCaps::default()).unwrap();
        assert!(matches!(
            vfs.resolve("textures/leaf.png", AssetCategory::Texture),
            Err(VfsError::SymlinkRejected)
        ));
        assert!(matches!(
            vfs.resolve("linked_dir/inner.png", AssetCategory::Texture),
            Err(VfsError::SymlinkRejected)
        ));
    }

    #[test]
    fn cap_boundary_is_inclusive_at_the_limit() {
        let scene_dir = tmpdir("cap-scene");
        let cap = 16u64;
        let caps = VfsCaps {
            texture: cap,
            ..VfsCaps::default()
        };
        fs::write(scene_dir.join("under.bin"), vec![0u8; (cap - 1) as usize]).unwrap();
        fs::write(scene_dir.join("exact.bin"), vec![0u8; cap as usize]).unwrap();
        fs::write(scene_dir.join("over.bin"), vec![0u8; (cap + 1) as usize]).unwrap();
        let vfs = Vfs::new(None, &scene_dir, None, caps).unwrap();
        assert!(vfs.resolve("under.bin", AssetCategory::Texture).is_ok());
        assert!(vfs.resolve("exact.bin", AssetCategory::Texture).is_ok());
        assert!(matches!(
            vfs.resolve("over.bin", AssetCategory::Texture),
            Err(VfsError::Oversize { category: AssetCategory::Texture, limit }) if limit == cap
        ));
    }

    #[test]
    fn resolve_refuses_the_video_category_pointing_callers_at_resolve_path() {
        let vfs = Vfs::new(
            None,
            &tmpdir("video-bytes-refused"),
            None,
            VfsCaps::default(),
        )
        .unwrap();
        assert!(matches!(
            vfs.resolve("clip.mp4", AssetCategory::Video),
            Err(VfsError::BadReference(_))
        ));
    }

    #[test]
    fn resolve_path_serves_a_dir_video_and_refuses_a_pkg_embedded_one() {
        let scene_dir = tmpdir("video-scene");
        fs::write(scene_dir.join("clip.mp4"), b"not really a video").unwrap();
        let pkg = build_pkg(&[("clip.mp4", b"pkg video bytes")]);
        let vfs = Vfs::new(Some(pkg), &scene_dir, None, VfsCaps::default()).unwrap();

        // In the pkg: NotAddressable, authoritative -- never falls back to
        // the scene dir's own clip.mp4 even though the same logical id
        // exists there too.
        assert!(matches!(
            vfs.resolve_path("clip.mp4", AssetCategory::Video),
            Err(VfsError::NotAddressable)
        ));

        let vfs_dir_only = Vfs::new(None, &scene_dir, None, VfsCaps::default()).unwrap();
        let resolved = vfs_dir_only
            .resolve_path("clip.mp4", AssetCategory::Video)
            .unwrap();
        assert_eq!(resolved.source, VfsSource::SceneDir);
        assert_eq!(fs::read(&resolved.path).unwrap(), b"not really a video");

        // Same confinement as resolve(): hostile references and symlink
        // escapes are refused identically.
        assert!(matches!(
            vfs_dir_only.resolve_path("../outside/x.mp4", AssetCategory::Video),
            Err(VfsError::BadReference(_))
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn unicode_references_resolve_byte_identically_and_case_folding_never_happens() {
        let scene_dir = tmpdir("unicode-scene");
        fs::create_dir_all(scene_dir.join("текстуры")).unwrap();
        fs::write(scene_dir.join("текстуры").join("A.png"), b"upper").unwrap();
        let vfs = Vfs::new(None, &scene_dir, None, VfsCaps::default()).unwrap();
        let resolved = vfs
            .resolve("текстуры/A.png", AssetCategory::Texture)
            .unwrap();
        assert_eq!(resolved.bytes, b"upper");
        // No case folding for a scene-dir/assets-root lookup: "a.png" must
        // not match "A.png" on a case-sensitive filesystem (unlike the pkg
        // lane, which deliberately mirrors resolve_pkg_entry's
        // case-insensitivity — see the pkg tail-match test above).
        assert!(matches!(
            vfs.resolve("текстуры/a.png", AssetCategory::Texture),
            Err(VfsError::NotFound)
        ));
    }
}
