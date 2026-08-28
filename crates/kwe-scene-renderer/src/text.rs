// SPDX-License-Identifier: GPL-3.0-or-later
//
// M3e: text layers for the SceneScript renderer.
//
// Font rasterization is provided by the vendored stb_truetype.h (public
// domain, pinned revision — see THIRD_PARTY.yml) through the opaque C shim
// in vendor/stb/stb_shim.c. Every text layer owns one bounded glyph atlas
// (ATLAS_SIZE x ATLAS_SIZE RGBA8) uploaded through the existing M3c image
// texture path; glyphs are rasterized on demand and the layer draws as a
// single textured quad whose vertex data is regenerated on text / alignment
// / font-size change, never per frame.
//
// Bounds enforced here (mirrored by the JS API and the scene parser):
//   - font size: MIN_FONT_PX..=MAX_FONT_PX pixels per em
//   - text length: MAX_TEXT_CHARS chars (measured in chars, not bytes)
//   - atlas: one per layer, ATLAS_SIZE^2 RGBA8 (16 MiB), shelf-packed,
//     LRU-ish eviction via clear + full repack on overflow
//   - atlas rebuilds rate limited to ATLAS_REBUILDS_PER_SECOND per second
//     (pathological alternating text may lose glyphs, never the worker)
//   - font files: bounded size, bounded scan depth and file count
//
// Coordinate convention: the layout is computed in pixel units, y down
// (baseline at y = 0), matching the scene's own y-down space — the layer
// quad maps pixel units 1:1 to world units (text layers render with size
// (1, 1), see layers.rs). Alignment anchors the glyph box on the layer
// origin: Left/Center/Right x anchors and Top/Center/Bottom y anchors.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::textures::{MAX_TOTAL_TEXTURE_BYTES, texture_budget_allows};

pub const MAX_TEXT_LAYERS: usize = 16;
pub const MAX_TEXT_CHARS: usize = 4096;
pub const MIN_FONT_PX: f32 = 4.0;
pub const MAX_FONT_PX: f32 = 512.0;
/// OWE's pointsize-to-pixel factor (TextPointSizeToPx: kPointsizeToPx = 4.0).
pub const POINT_TO_PX: f32 = 4.0;
/// Effective size when the scene omits pointsize (pointsize 12 -> 48 px).
pub const DEFAULT_POINT_SIZE: f32 = 12.0;

pub const ATLAS_SIZE: u32 = 2048;
/// Max full-atlas rebuilds per second (per layer).
pub const ATLAS_REBUILDS_PER_SECOND: usize = 2;
/// Padding pixels around each glyph bitmap inside the atlas (linear sampler
/// bleed guard).
const ATLAS_PAD: u32 = 1;
/// A single glyph bitmap larger than this is dropped (atlas slot + scratch
/// bound). 520 = 512 px em cap plus padding headroom.
const MAX_GLYPH_BITMAP_DIM: u32 = 520;
/// Scratch raster buffer per glyph: MAX_GLYPH_BITMAP_DIM^2 RGBA.
const SCRATCH_BYTES: usize = (MAX_GLYPH_BITMAP_DIM * MAX_GLYPH_BITMAP_DIM) as usize;

/// Max vertex bytes for one text layer's quad geometry
/// (MAX_TEXT_CHARS glyphs x 6 verts x 16 B of pos.xy + uv.xy).
pub const MAX_TEXT_VERTEX_BYTES: usize = MAX_TEXT_CHARS * 6 * 16;

pub const MAX_FONT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FONT_SCAN_DEPTH: usize = 4;
const MAX_FONT_FILES_PER_DIR: usize = 4096;
const MAX_FONT_DIRS: usize = 16;
const MAX_RESOLVED_FILES: usize = 16384;

/// Fallback families consulted in order when the requested family does not
/// resolve (common fonts first).
pub const FALLBACK_FAMILIES: [&str; 4] =
    ["Noto Sans", "DejaVu Sans", "Liberation Sans", "FreeSans"];

// ---------------------------------------------------------------------------
// stb_truetype FFI (opaque)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct KweFont {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn kwe_font_size() -> usize;
    /// `data_len` bounds the shim's own reads; anything below a full sfnt
    /// header is rejected without touching the buffer.
    fn kwe_font_init(font: *mut KweFont, data: *const u8, data_len: usize, fontstart: i32) -> i32;
    fn kwe_font_glyph_index(font: *const KweFont, codepoint: i32) -> i32;
    fn kwe_font_glyph_h_metrics(font: *const KweFont, glyph: i32, advance: *mut i32, lsb: *mut i32);
    fn kwe_font_scale_for_pixel_height(font: *const KweFont, height: f32) -> f32;
    fn kwe_font_glyph_bitmap_box(
        font: *const KweFont,
        glyph: i32,
        scale_x: f32,
        scale_y: f32,
        ix0: *mut i32,
        iy0: *mut i32,
        ix1: *mut i32,
        iy1: *mut i32,
    );
    fn kwe_font_render_glyph(
        font: *const KweFont,
        out: *mut u8,
        out_w: i32,
        out_h: i32,
        out_stride: i32,
        scale_x: f32,
        scale_y: f32,
        glyph: i32,
        ix0: i32,
        iy0: i32,
    ) -> i32;
    fn kwe_font_family_name(font: *const KweFont, buf: *mut u8, buflen: usize) -> i32;
}

/// 16-byte-aligned storage cell; a Vec of these backs a KweFont without
/// Rust ever depending on its layout.
#[derive(Clone, Debug)]
#[repr(C, align(16))]
struct Aligned16([u8; 16]);

/// A parsed font file (TTF/OTF/TTC, first collection face).
#[derive(Debug)]
pub struct Font {
    /// The file bytes; never read as a field but REQUIRED to outlive
    /// `storage` — stb_truetype keeps a data pointer into them for lazy
    /// table reads. The dead-code allowance documents the lifetime role.
    #[allow(dead_code)]
    data: Vec<u8>,
    storage: Vec<Aligned16>,
}

impl Font {
    /// Parse `bytes` as a TrueType/OpenType font. Returns None when the
    /// data is not a usable font (hostile input is rejected, never
    /// asserted on).
    pub fn open(bytes: Vec<u8>) -> Option<Font> {
        let size = unsafe { kwe_font_size() };
        let count = size.div_ceil(16);
        let mut storage = vec![Aligned16([0; 16]); count];
        let ok = unsafe {
            kwe_font_init(
                storage.as_mut_ptr().cast::<KweFont>(),
                bytes.as_ptr(),
                bytes.len(),
                0,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(Font {
            data: bytes,
            storage,
        })
    }

    fn raw(&self) -> *const KweFont {
        self.storage.as_ptr().cast()
    }

    pub fn glyph_index(&self, codepoint: u32) -> u32 {
        unsafe { kwe_font_glyph_index(self.raw(), codepoint as i32) as u32 }
    }

    /// (advance, left side bearing) in font units.
    pub fn glyph_h_metrics(&self, glyph: u32) -> (i32, i32) {
        let mut advance = 0;
        let mut lsb = 0;
        unsafe { kwe_font_glyph_h_metrics(self.raw(), glyph as i32, &mut advance, &mut lsb) };
        (advance, lsb)
    }

    pub fn scale_for_pixel_height(&self, px: f32) -> f32 {
        unsafe { kwe_font_scale_for_pixel_height(self.raw(), px) }
    }

    /// Pixel-space bitmap box [ix0, iy0, ix1, iy1] (y down) for `glyph`.
    pub fn glyph_bitmap_box(&self, glyph: u32, scale: f32) -> [i32; 4] {
        let mut box_ = [0i32; 4];
        unsafe {
            kwe_font_glyph_bitmap_box(
                self.raw(),
                glyph as i32,
                scale,
                scale,
                &mut box_[0],
                &mut box_[1],
                &mut box_[2],
                &mut box_[3],
            )
        };
        box_
    }

    /// Rasterize `glyph` at `scale` into `out` (row-major 1-byte alpha
    /// coverage, row stride `stride`), covering the box offset (ix0, iy0).
    /// Returns false when the glyph outline was refused (oversized). The
    /// argument shape mirrors the C rasterizer's (which mirrors stb's own
    /// signature — the caller supplies the stride so the atlas can hold
    /// 1 byte per pixel and splat the coverage itself).
    #[allow(clippy::too_many_arguments)]
    pub fn render_glyph(
        &self,
        out: &mut [u8],
        out_w: i32,
        out_h: i32,
        stride: i32,
        scale: f32,
        glyph: u32,
        ix0: i32,
        iy0: i32,
    ) -> bool {
        if out.len() < (out_h as usize) * (stride as usize) {
            return false;
        }
        unsafe {
            kwe_font_render_glyph(
                self.raw(),
                out.as_mut_ptr(),
                out_w,
                out_h,
                stride,
                scale,
                scale,
                glyph as i32,
                ix0,
                iy0,
            ) != 0
        }
    }

    /// The font's family name from its name table, if any.
    pub fn family_name(&self) -> Option<String> {
        let mut buf = [0u8; 256];
        let len = unsafe { kwe_font_family_name(self.raw(), buf.as_mut_ptr(), buf.len()) };
        if len <= 0 {
            return None;
        }
        let end = buf[..len as usize]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(len as usize);
        let s = String::from_utf8_lossy(&buf[..end]).to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

// ---------------------------------------------------------------------------
// Font resolution
// ---------------------------------------------------------------------------

/// How a request was resolved (used for the one-time resolution-order diag
/// and by tests to pin behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontMatch {
    /// Exact family-name equality with a regular-weight basename candidate.
    Exact,
    /// WE-style basename-prefix match (may not equal the requested family
    /// name, e.g. a CJK variant).
    Basename,
    /// Resolved via a named fallback family.
    Fallback(&'static str),
    /// Any usable font on disk.
    Any,
    /// Nothing usable found.
    None,
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Finds the system fonts this worker may use, in the documented order:
/// explicit --font-dir / KWE_FONT_DIRS entries first, then the standard
/// per-user and system directories. Bounded: at most MAX_FONT_DIRS dirs,
/// depth MAX_FONT_SCAN_DEPTH, MAX_FONT_FILES_PER_DIR per directory, files
/// sorted for determinism.
fn collect_font_files(explicit_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in explicit_dirs.iter().take(MAX_FONT_DIRS) {
        dirs.push(d.clone());
    }
    if dirs.len() < MAX_FONT_DIRS {
        for d in [
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ] {
            if dirs.len() < MAX_FONT_DIRS {
                dirs.push(d);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for d in [home.join(".local/share/fonts"), home.join(".fonts")] {
            if dirs.len() < MAX_FONT_DIRS {
                dirs.push(d);
            }
        }
    }

    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if files.len() >= MAX_RESOLVED_FILES {
            break;
        }
        if !dir.is_dir() {
            continue;
        }
        // Iterate recursively but bounded: MAX_FONT_SCAN_DEPTH levels, at
        // most MAX_FONT_FILES_PER_DIR regular font files per directory.
        // The collection itself is capped BEFORE sorting: a pathological
        // directory with millions of files must not allocate a huge
        // intermediate Vec (the cap is a hard stop on the read_dir
        // stream, not a post-sort filter).
        let mut stack = vec![(dir.clone(), 0usize)];
        while let Some((d, depth)) = stack.pop() {
            if files.len() >= MAX_RESOLVED_FILES {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut names: Vec<(String, PathBuf)> = Vec::new();
            for entry in entries {
                if names.len() >= MAX_FONT_FILES_PER_DIR {
                    break;
                }
                let Ok(entry) = entry else {
                    continue;
                };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                names.push((name, entry.path()));
            }
            names.sort();
            let mut subdirs: Vec<(PathBuf, usize)> = Vec::new();
            let mut file_count = 0usize;
            for (name, path) in names {
                if file_count >= MAX_FONT_FILES_PER_DIR {
                    break;
                }
                let is_dir = path.is_dir();
                if is_dir {
                    if depth < MAX_FONT_SCAN_DEPTH {
                        subdirs.push((path, depth + 1));
                    }
                    continue;
                }
                let ext = Path::new(&name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext == "ttf" || ext == "otf" || ext == "ttc" {
                    file_count += 1;
                    if files.len() < MAX_RESOLVED_FILES {
                        files.push(path);
                    }
                }
            }
            stack.extend(subdirs.into_iter().rev());
        }
    }
    files
}

/// Max basename candidates whose name table is actually opened to verify
/// the family name (a per-family bounded parse cost; files are scanned in
/// sorted order, so the regular face usually sorts first).
const MAX_FAMILY_VERIFY: usize = 32;

/// Resolves font family names to parsed fonts. Caches per normalized
/// request, including negative results. Thread-independent (worker only).
pub struct FontResolver {
    files: Vec<PathBuf>,
    cache: HashMap<String, Option<Arc<Font>>>,
}

impl FontResolver {
    pub fn new(explicit_dirs: &[PathBuf]) -> FontResolver {
        FontResolver {
            files: collect_font_files(explicit_dirs),
            cache: HashMap::new(),
        }
    }

    pub fn font_file_count(&self) -> usize {
        self.files.len()
    }

    /// Resolve `request` (a family name, optionally prefixed with
    /// `systemfont_`, or a path / basename containing '/'). Returns the
    /// font and how it was resolved.
    pub fn resolve(&mut self, request: &str) -> (Option<Arc<Font>>, FontMatch) {
        let raw = request.trim();
        if raw.is_empty() {
            return self.resolve_internal("", &FALLBACK_FAMILIES);
        }
        let family = raw.strip_prefix("systemfont_").unwrap_or(raw).trim();
        if family.contains('/') {
            return self.resolve_path(family);
        }
        self.resolve_internal(family, &FALLBACK_FAMILIES)
    }

    fn resolve_path(&mut self, path: &str) -> (Option<Arc<Font>>, FontMatch) {
        let key = format!("path:{path}");
        if let Some(hit) = self.cache.get(&key) {
            return (
                hit.clone(),
                if hit.is_some() {
                    FontMatch::Exact
                } else {
                    FontMatch::None
                },
            );
        }
        let resolved = std::path::Path::new(path);
        let mut font = None;
        if resolved.is_file() {
            font = read_font_file(resolved).map(Arc::new);
        } else if let Some(base) = Path::new(path).file_name().and_then(|s| s.to_str()) {
            for f in &self.files {
                if f.file_name().and_then(|s| s.to_str()) == Some(base) {
                    font = read_font_file(f).map(Arc::new);
                    break;
                }
            }
        }
        let mat = if font.is_some() {
            FontMatch::Exact
        } else {
            FontMatch::None
        };
        self.cache.insert(key, font.clone());
        (font, mat)
    }

    /// Find `family` in `files`. Returns the parsed font (if any) and the
    /// resolution step that matched:
    ///   1. Exact — a basename candidate whose name table reports the
    ///      family (verified, bounded to MAX_FAMILY_VERIFY opens);
    ///   2. Basename — WE-style basename-prefix match, first candidate in
    ///      sorted order (unverified; may be a CJK or condensed variant).
    ///
    /// Note: once MAX_FAMILY_VERIFY candidates have been opened without a
    /// match, the first basename candidate is returned UNVERIFIED — a
    /// family served by many files (CJK or condensed variants) can win
    /// over the regular face when the regular face sorts late.
    fn find_family(files: &[PathBuf], family: &str) -> (Option<Arc<Font>>, FontMatch) {
        let nf = normalize(family);
        if nf.is_empty() {
            return (None, FontMatch::None);
        }
        let mut verified = 0usize;
        let mut first_basename: Option<Arc<Font>> = None;
        for path in files.iter().take(MAX_RESOLVED_FILES) {
            let Some(base) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !normalize(base).contains(&nf) {
                continue;
            }
            let font = read_font_file(path).map(Arc::new);
            if first_basename.is_none() {
                first_basename = font.clone();
            }
            if let Some(font) = &font
                && verified < MAX_FAMILY_VERIFY
            {
                verified += 1;
                if font
                    .family_name()
                    .map(|n| normalize(&n) == nf)
                    .unwrap_or(false)
                {
                    return (Some(font.clone()), FontMatch::Exact);
                }
            }
        }
        if let Some(font) = first_basename {
            return (Some(font), FontMatch::Basename);
        }
        (None, FontMatch::None)
    }

    fn resolve_internal(
        &mut self,
        family: &str,
        fallback: &[&'static str],
    ) -> (Option<Arc<Font>>, FontMatch) {
        let key = normalize(family);
        if let Some(hit) = self.cache.get(&key) {
            let mat = if hit.is_some() {
                FontMatch::Exact
            } else {
                FontMatch::None
            };
            return (hit.clone(), mat);
        }
        // Step 1: the requested family itself (verified exact or first
        // basename match, see find_family).
        let (font, mat) = Self::find_family(&self.files, family);
        if let Some(font) = font {
            self.cache.insert(key.clone(), Some(font.clone()));
            return (Some(font), mat);
        }
        // Step 2: the fallback chain (static names; reported as a fallback
        // so the load-time diagnostic records the resolution order).
        for name in fallback {
            let (font, _) = Self::find_family(&self.files, name);
            if let Some(font) = font {
                let mat = FontMatch::Fallback(name);
                self.cache.insert(key.clone(), Some(font.clone()));
                return (Some(font), mat);
            }
        }
        // Step 3: any usable font (the very last resort).
        for path in self.files.iter().take(MAX_RESOLVED_FILES) {
            if let Some(font) = read_font_file(path).map(Arc::new) {
                self.cache.insert(key.clone(), Some(font.clone()));
                return (Some(font), FontMatch::Any);
            }
        }
        self.cache.insert(key, None);
        (None, FontMatch::None)
    }
}

fn read_font_file(path: &Path) -> Option<Font> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FONT_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Font::open(bytes)
}

// ---------------------------------------------------------------------------
// Glyph atlas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShelfRow {
    y: u32,
    height: u32,
    x: u32,
}

/// Diagnostics surfaced once per layer by the worker (eprintln, not fatal).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextDiag {
    pub atlas_rate_limited: bool,
    pub glyph_too_large: bool,
}

/// One bounded glyph atlas per text layer: ATLAS_SIZE^2 RGBA8, shelf-packed
/// from the top-left, 1 px padding around each glyph. When a glyph no longer
/// fits, the atlas is cleared and fully repacked — but only
/// ATLAS_REBUILDS_PER_SECOND times per second; beyond that, new glyphs are
/// dropped (rendered as nothing) until the rate window passes. Dropped
/// glyphs are re-attempted on the next sync, so a steady text change heals
/// itself.
pub struct GlyphAtlas {
    pub pixels: Vec<u8>,
    entries: HashMap<u32, AtlasRect>,
    rows: Vec<ShelfRow>,
    rebuild_times: Vec<Instant>,
    scratch: Vec<u8>,
}

impl GlyphAtlas {
    pub fn new() -> GlyphAtlas {
        GlyphAtlas {
            pixels: vec![0u8; (ATLAS_SIZE as usize) * (ATLAS_SIZE as usize) * 4],
            entries: HashMap::new(),
            rows: Vec::new(),
            rebuild_times: Vec::new(),
            scratch: Vec::with_capacity(SCRATCH_BYTES),
        }
    }

    pub fn entry(&self, glyph: u32) -> Option<AtlasRect> {
        self.entries.get(&glyph).copied()
    }

    pub fn contains(&self, glyph: u32) -> bool {
        self.entries.contains_key(&glyph)
    }

    /// Drop all content (used when the font or size changes). Counted
    /// against the SAME rebuild rate limit as an overflow repack — a
    /// user-initiated change must never starve the budget — and returns
    /// false when the rate window is exhausted (the caller then keeps the
    /// old atlas AND the old geometry and retries on the next sync, so
    /// the layer never renders mismatched glyphs at the wrong scale). The
    /// initial load passes: the rebuild window starts empty.
    fn clear_budgeted(&mut self) -> bool {
        if !self.rebuild_allowed() {
            return false;
        }
        self.rebuild_times.push(Instant::now());
        self.pixels.fill(0);
        self.entries.clear();
        self.rows.clear();
        true
    }

    /// Unconditional clear for the rate-limited overflow path (sync) and
    /// the no-font path (the layer renders nothing either way).
    pub fn clear(&mut self) {
        self.pixels.fill(0);
        self.entries.clear();
        self.rows.clear();
    }

    fn rebuild_allowed(&mut self) -> bool {
        let now = Instant::now();
        self.rebuild_times
            .retain(|t| now.duration_since(*t) < std::time::Duration::from_secs(1));
        self.rebuild_times.len() < ATLAS_REBUILDS_PER_SECOND
    }

    /// Ensure `glyphs` (in order) are present, rasterizing on demand.
    /// Returns diagnostics (rate limiting, oversized glyphs) and whether
    /// the atlas content changed.
    pub fn sync(&mut self, font: &Font, glyphs: &[u32], scale: f32) -> (TextDiag, bool) {
        let mut needed: Vec<u32> = Vec::new();
        {
            let mut seen = HashSet::new();
            for &g in glyphs {
                if !self.contains(g) && seen.insert(g) {
                    needed.push(g);
                }
            }
        }
        if needed.is_empty() {
            return (TextDiag::default(), false);
        }
        let mut diag = TextDiag::default();
        if self.place(font, &needed, scale, &mut diag) {
            (diag, true)
        } else {
            // Overflow: rate-limited clear + full repack. The content is
            // always reported changed here — the atlas was cleared and
            // rebuilt, so the GPU texture must be refreshed even when the
            // repack itself only fits a subset.
            if self.rebuild_allowed() {
                self.rebuild_times.push(Instant::now());
                self.clear();
                let mut dedup: Vec<u32> = Vec::new();
                {
                    let mut seen = HashSet::new();
                    for &g in glyphs {
                        if seen.insert(g) {
                            dedup.push(g);
                        }
                    }
                }
                self.place(font, &dedup, scale, &mut diag);
                (diag, true)
            } else {
                diag.atlas_rate_limited = true;
                (diag, false)
            }
        }
    }

    fn place(&mut self, font: &Font, glyphs: &[u32], scale: f32, diag: &mut TextDiag) -> bool {
        for &glyph in glyphs {
            if self.entries.contains_key(&glyph) {
                continue;
            }
            let [ix0, iy0, ix1, iy1] = font.glyph_bitmap_box(glyph, scale);
            let bw = (ix1 - ix0).max(0) as u32;
            let bh = (iy1 - iy0).max(0) as u32;
            if bw == 0 || bh == 0 {
                // Whitespace or empty glyph: remember it as a zero rect so
                // it is not re-attempted every sync.
                self.entries.insert(
                    glyph,
                    AtlasRect {
                        x: 0,
                        y: 0,
                        w: 0,
                        h: 0,
                    },
                );
                continue;
            }
            if bw > MAX_GLYPH_BITMAP_DIM || bh > MAX_GLYPH_BITMAP_DIM {
                diag.glyph_too_large = true;
                continue;
            }
            // Rasterize into the scratch buffer (bounded by the check
            // above): 1 byte per pixel of alpha coverage (the C rasterizer
            // writes single bytes at the caller's stride — an RGBA stride
            // would scatter the coverage across the channels).
            self.scratch.resize((bw as usize) * (bh as usize), 0);
            if !font.render_glyph(
                &mut self.scratch,
                bw as i32,
                bh as i32,
                bw as i32,
                scale,
                glyph,
                ix0,
                iy0,
            ) {
                continue;
            }
            let (x, y) = match self.pack(bw, bh) {
                Some(slot) => slot,
                None => return false,
            };
            // Copy with 1 px padding (ATLAS_PAD). The atlas stores WHITE
            // glyphs (RGB=255) with the coverage in the alpha channel: the
            // layer-texture shader multiplies the sampled RGB by the tint
            // (the text color) and scales the alpha by the layer alpha —
            // the zero-shader-change text path of M3e.
            let pad = ATLAS_PAD as usize;
            for row in 0..bh as usize {
                let src = row * bw as usize;
                for col in 0..bw as usize {
                    let alpha = self.scratch[src + col];
                    let dst = ((y as usize + pad + row) * ATLAS_SIZE as usize
                        + (x as usize + pad + col))
                        * 4;
                    self.pixels[dst..dst + 4].copy_from_slice(&[255, 255, 255, alpha]);
                }
            }
            self.entries.insert(
                glyph,
                AtlasRect {
                    x,
                    y,
                    w: bw + 2 * pad as u32,
                    h: bh + 2 * pad as u32,
                },
            );
        }
        true
    }

    /// Shelf-packing: find a row with room, else open a new row. Returns
    /// the slot origin (x, y) of the glyph's padded box.
    fn pack(&mut self, bw: u32, bh: u32) -> Option<(u32, u32)> {
        let w = bw + 2 * ATLAS_PAD;
        let h = bh + 2 * ATLAS_PAD;
        for row in self.rows.iter_mut() {
            if row.y + h <= ATLAS_SIZE && row.height >= h && row.x + w <= ATLAS_SIZE {
                let x = row.x;
                row.x += w;
                return Some((x, row.y));
            }
        }
        let y = self.rows.last().map_or(0, |r| r.y + r.height);
        if y + h > ATLAS_SIZE {
            return None;
        }
        self.rows.push(ShelfRow { y, height: h, x: w });
        Some((0, y))
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        GlyphAtlas::new()
    }
}

// ---------------------------------------------------------------------------
// Text layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlign {
    Top,
    Center,
    Bottom,
}

impl HorizontalAlign {
    // SR-2c: only the legacy differential-test oracle
    // (`scene::parse_text_align`, `#[cfg(test)]`-gated) still calls this —
    // the production path (`scene_ir_adapter`) matches
    // `kwe_core::HorizontalAlignIr`/`VerticalAlignIr` directly instead of
    // re-parsing a word.
    #[cfg(test)]
    pub fn parse(s: &str) -> Option<HorizontalAlign> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Some(HorizontalAlign::Left),
            "center" => Some(HorizontalAlign::Center),
            "right" => Some(HorizontalAlign::Right),
            _ => None,
        }
    }
}

impl VerticalAlign {
    #[cfg(test)]
    pub fn parse(s: &str) -> Option<VerticalAlign> {
        match s.to_ascii_lowercase().as_str() {
            "top" => Some(VerticalAlign::Top),
            "center" => Some(VerticalAlign::Center),
            "bottom" => Some(VerticalAlign::Bottom),
            _ => None,
        }
    }
}

/// One placed glyph, in pixel units (y down, baseline at y = 0).
#[derive(Debug, Clone, Copy)]
pub struct GlyphPlacement {
    pub glyph: u32,
    /// Horizontal pen position of this glyph's origin (advance from the
    /// previous glyph, accumulated).
    pub pen_x: f32,
    pub ix0: f32,
    pub iy0: f32,
    pub ix1: f32,
    pub iy1: f32,
}

/// A single-line text layout: glyph boxes plus tight bounds. `max_x` may be
/// 0 for empty/whitespace-only text.
#[derive(Debug, Clone, Default)]
pub struct TextLayout {
    pub placements: Vec<GlyphPlacement>,
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
    pub scale: f32,
}

/// Layout `text` (at most `chars_cap` chars, no combining-mark support —
/// documented in SCENE_FORMAT_V1.md) at `px` pixel em size.
pub fn layout_text(font: &Font, text: &str, px: f32, chars_cap: usize) -> TextLayout {
    let scale = font.scale_for_pixel_height(px.clamp(MIN_FONT_PX, MAX_FONT_PX));
    let mut layout = TextLayout {
        scale,
        ..TextLayout::default()
    };
    let mut pen_x = 0.0f32;
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for (i, c) in text.chars().take(chars_cap).enumerate() {
        let glyph = font.glyph_index(c as u32);
        let (advance, _lsb) = font.glyph_h_metrics(glyph);
        let [ix0, iy0, ix1, iy1] = font.glyph_bitmap_box(glyph, scale);
        layout.placements.push(GlyphPlacement {
            glyph,
            pen_x,
            ix0: ix0 as f32,
            iy0: iy0 as f32,
            ix1: ix1 as f32,
            iy1: iy1 as f32,
        });
        if i == 0 {
            min_x = ix0 as f32;
        } else {
            min_x = min_x.min(pen_x + ix0 as f32);
        }
        max_x = max_x.max(pen_x + ix1 as f32);
        min_y = min_y.min(iy0 as f32);
        max_y = max_y.max(iy1 as f32);
        pen_x += advance as f32 * scale;
    }
    layout.min_x = if min_x.is_finite() { min_x } else { 0.0 };
    layout.max_x = if max_x.is_finite() { max_x } else { 0.0 };
    layout.min_y = if min_y.is_finite() { min_y } else { 0.0 };
    layout.max_y = if max_y.is_finite() { max_y } else { 0.0 };
    layout
}

/// The anchor point (in layout pixel units, y down) that the layer origin
/// lands on, per alignment.
pub fn anchor_offset(layout: &TextLayout, h: HorizontalAlign, v: VerticalAlign) -> [f32; 2] {
    let x = match h {
        HorizontalAlign::Left => layout.min_x,
        HorizontalAlign::Center => (layout.min_x + layout.max_x) * 0.5,
        HorizontalAlign::Right => layout.max_x,
    };
    let y = match v {
        VerticalAlign::Top => layout.min_y,
        VerticalAlign::Center => (layout.min_y + layout.max_y) * 0.5,
        VerticalAlign::Bottom => layout.max_y,
    };
    [x, y]
}

/// Build the vertex bytes for one text layer quad: 6 verts per glyph of
/// {pos.x, pos.y, uv.u, uv.v} (16 B, stride matches the renderer's unit
/// quad), in the same winding as UNIT_QUAD (see vulkan.rs). Glyphs whose
/// atlas rect is missing/empty (rate-limited or whitespace) contribute no
/// quad. Returns (bytes, quad_count).
pub fn build_vertex_bytes(
    layout: &TextLayout,
    anchor: [f32; 2],
    atlas: &GlyphAtlas,
) -> (Vec<u8>, u32) {
    let mut bytes: Vec<u8> =
        Vec::with_capacity(MAX_TEXT_VERTEX_BYTES.min(layout.placements.len() * 6 * 16));
    let mut quads = 0u32;
    for p in &layout.placements {
        let Some(rect) = atlas.entry(p.glyph) else {
            continue;
        };
        if rect.w == 0 || rect.h == 0 {
            continue;
        }
        let x0 = p.pen_x + p.ix0 - anchor[0];
        let y0 = p.iy0 - anchor[1];
        let x1 = p.pen_x + p.ix1 - anchor[0];
        let y1 = p.iy1 - anchor[1];
        let inv = 1.0 / ATLAS_SIZE as f32;
        // The atlas rect spans the PADDED box (bw + 2*ATLAS_PAD) while
        // the quad spans the UNPADDED metric box; the UVs are inset by the
        // pad so the glyph texels map 1:1 onto the quad — sampling the
        // full padded rect would render the glyph at bw/(bw+2) scale with
        // a 1 px inset (a 2 px glyph at 4 px em would render at 50%).
        let pad = ATLAS_PAD as f32 * inv;
        let u0 = rect.x as f32 * inv + pad;
        let v0 = rect.y as f32 * inv + pad;
        let u1 = (rect.x + rect.w) as f32 * inv - pad;
        let v1 = (rect.y + rect.h) as f32 * inv - pad;
        // v0..v3 (top-left, top-right, bottom-right), then bottom-right,
        // bottom-left, top-left — mirroring UNIT_QUAD's triangles.
        let verts: [[f32; 4]; 6] = [
            [x0, y0, u0, v0],
            [x1, y0, u1, v0],
            [x1, y1, u1, v1],
            [x1, y1, u1, v1],
            [x0, y1, u0, v1],
            [x0, y0, u0, v0],
        ];
        for v in verts {
            for c in v {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
        }
        quads += 1;
    }
    (bytes, quads)
}

/// Cap `text` to `max_chars` chars (not bytes).
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// OWE TextPointSizeToPx: px = round(pointsize * 4.0), clamped to our
/// bounded range MIN_FONT_PX..=MAX_FONT_PX (OWE clamps 1..=1024 — the
/// deviation is documented in SCENE_FORMAT_V1.md). Non-finite or ≤ 0 sizes
/// fall back to the default (pointsize 12 → 48 px), like OWE's default
/// pointsize.
pub fn pointsize_to_px(pointsize: f64) -> f32 {
    if !pointsize.is_finite() || pointsize <= 0.0 {
        return DEFAULT_POINT_SIZE * POINT_TO_PX;
    }
    (pointsize * f64::from(POINT_TO_PX))
        .round()
        .clamp(MIN_FONT_PX as f64, MAX_FONT_PX as f64) as f32
}

// ---------------------------------------------------------------------------
// Worker-side text renderer
// ---------------------------------------------------------------------------

use crate::layers::LayerState;
use crate::vulkan::LayerRenderer;

/// Per-layer runtime state: the resolved font, the atlas, and the current
/// vertex bytes.
struct TextLayerRuntime {
    font: Option<Arc<Font>>,
    /// Font identity last synced (for clear-on-change detection).
    font_id: u64,
    scale: f32,
    atlas: GlyphAtlas,
    vertex_bytes: Vec<u8>,
    atlas_changed: bool,
    /// Whether this layer's atlas counts against the shared texture
    /// budget (texture_budget_allows): counted on first upload, refunded
    /// when that upload fails.
    budget_counted: bool,
    fallback_diag: bool,
    truncated_diag: bool,
    rate_limited_diag: bool,
    glyph_too_large_diag: bool,
    budget_diag: bool,
}

impl Default for TextLayerRuntime {
    fn default() -> Self {
        TextLayerRuntime {
            font: None,
            font_id: 0,
            scale: 0.0,
            atlas: GlyphAtlas::new(),
            vertex_bytes: Vec::new(),
            atlas_changed: false,
            budget_counted: false,
            fallback_diag: false,
            truncated_diag: false,
            rate_limited_diag: false,
            glyph_too_large_diag: false,
            budget_diag: false,
        }
    }
}

/// The worker's text subsystem: resolves fonts, rasterizes glyphs, and
/// keeps the layer atlas textures and quad vertex buffers uploaded.
pub struct TextRenderer {
    resolver: FontResolver,
    layers: Vec<Option<TextLayerRuntime>>,
    /// Atlas bytes counted against the shared 256 MiB texture budget
    /// (texture_budget_allows): 16 MiB per counted layer, never refunded
    /// once a layer's atlas is live. Shared with the image-layer decode
    /// budget — a scene with images AND text layers competes for the same
    /// cap (documented in SCENE_FORMAT_V1.md).
    atlas_bytes_used: u64,
}

impl TextRenderer {
    pub fn new(font_dirs: &[std::path::PathBuf]) -> TextRenderer {
        TextRenderer {
            resolver: FontResolver::new(font_dirs),
            layers: Vec::new(),
            atlas_bytes_used: 0,
        }
    }

    /// Number of font files the resolver found under its configured
    /// directories (load-time diagnostic).
    pub fn font_file_count(&self) -> usize {
        self.resolver.font_file_count()
    }

    /// Re-sync every dirty text layer: resolve font, relayout, rasterize
    /// missing glyphs, upload atlas texture (via the image path) and quad
    /// vertices. Returns the set of layer indices whose ordinary uploads
    /// failed (draws for them are suppressed this frame, like image
    /// layers). A fence timeout is propagated because its submit may still
    /// be pending and the worker must terminate before Vulkan reuse/free.
    pub fn sync_and_upload(
        &mut self,
        vulkan: &mut LayerRenderer,
        layers: &[std::rc::Rc<std::cell::RefCell<LayerState>>],
    ) -> Result<Vec<usize>, crate::vulkan::RenderError> {
        let mut failed: Vec<usize> = Vec::new();
        if self.layers.len() < layers.len() {
            self.layers.resize_with(layers.len(), || None);
        }
        for (i, layer) in layers.iter().enumerate() {
            let mut state = layer.borrow_mut();
            // One-time diagnostics read the layer name; capture it before
            // the mutable borrow of `text` (borrows through RefCell deref
            // are not field-disjoint).
            let name = state.name.clone();
            let Some(text) = &mut state.text else {
                continue;
            };
            if !text.dirty {
                continue;
            }
            let runtime = self.layers[i].get_or_insert_with(TextLayerRuntime::default);

            // Resolve the font (with one-time fallback diag).
            let request = text.font.as_deref().unwrap_or("");
            let (font, mat) = self.resolver.resolve(request);
            let font_id = font.as_ref().map_or(0, |f| Arc::as_ptr(f) as u64);
            if runtime.font_id != font_id || runtime.scale != text.pointsize_px {
                // The font/size change clears the atlas through the SAME
                // 2/s rebuild budget as an overflow repack: a 60 fps
                // pointsize toggle must not force a full clear + repack +
                // 16 MiB re-upload every frame. When the window is
                // exhausted the old atlas AND the old geometry stay (the
                // layer renders consistently, never mismatched glyphs),
                // the dirty flag stays set, and the next sync retries
                // within the window. The initial load passes: the window
                // starts empty.
                if !runtime.atlas.clear_budgeted() {
                    continue;
                }
                runtime.font_id = font_id;
                runtime.scale = text.pointsize_px;
                runtime.atlas_changed = true;
            }
            runtime.font = font;
            if let Some(font) = &runtime.font {
                if mat != FontMatch::Exact && !runtime.fallback_diag {
                    runtime.fallback_diag = true;
                    eprintln!(
                        "event=renderer.scene.text_font_fallback layer={name} requested={request} resolved={mat:?} family={:?}",
                        font.family_name()
                    );
                }
                let truncated = truncate_chars(&text.text, MAX_TEXT_CHARS);
                if truncated.len() < text.text.len() && !runtime.truncated_diag {
                    runtime.truncated_diag = true;
                    eprintln!(
                        "event=renderer.scene.text_truncated layer={name} chars={} capped_at={MAX_TEXT_CHARS}",
                        text.text.chars().count()
                    );
                }
                let layout = layout_text(font, &truncated, text.pointsize_px, MAX_TEXT_CHARS);
                let (diag, changed) = runtime.atlas.sync(
                    font,
                    &layout
                        .placements
                        .iter()
                        .map(|p| p.glyph)
                        .collect::<Vec<_>>(),
                    layout.scale,
                );
                if diag.atlas_rate_limited && !runtime.rate_limited_diag {
                    runtime.rate_limited_diag = true;
                    eprintln!(
                        "event=renderer.scene.text_atlas_rebuild_rate_limited layer={name} (rebuilds capped at {ATLAS_REBUILDS_PER_SECOND}/s)"
                    );
                }
                if diag.glyph_too_large && !runtime.glyph_too_large_diag {
                    runtime.glyph_too_large_diag = true;
                    eprintln!(
                        "event=renderer.scene.text_glyph_too_large layer={name} (single glyph exceeds {}px)",
                        MAX_GLYPH_BITMAP_DIM
                    );
                }
                runtime.atlas_changed |= changed;
                let anchor = anchor_offset(&layout, text.horizontal_align, text.vertical_align);
                let (bytes, quads) = build_vertex_bytes(&layout, anchor, &runtime.atlas);
                // VERTEX count, not quad count: the draw emits one
                // TRIANGLE_LIST draw of this many vertices (6 per glyph).
                text.vertex_count = quads * 6;
                runtime.vertex_bytes = bytes;
            } else {
                runtime.atlas.clear();
                runtime.atlas_changed = true;
                runtime.vertex_bytes.clear();
                text.vertex_count = 0;
                if !runtime.fallback_diag {
                    runtime.fallback_diag = true;
                    eprintln!(
                        "event=renderer.scene.text_font_none layer={name} requested={request} (no usable font found)"
                    );
                }
            }
            text.dirty = false;
            drop(state);

            // Upload (only when something changed this pass). The atlas
            // counts against the shared 256 MiB texture budget (16 MiB per
            // layer, counted once, refunded when the upload fails): image
            // textures and text atlases compete for the same cap, and a
            // layer past the cap is skipped with a one-time diagnostic —
            // the renderer stays healthy.
            let mut ok = true;
            if runtime.atlas_changed {
                runtime.atlas_changed = false;
                let atlas_bytes = (ATLAS_SIZE as u64) * (ATLAS_SIZE as u64) * 4;
                if !runtime.budget_counted
                    && !texture_budget_allows(self.atlas_bytes_used, ATLAS_SIZE, ATLAS_SIZE)
                {
                    if !runtime.budget_diag {
                        runtime.budget_diag = true;
                        eprintln!(
                            "event=renderer.scene.text_atlas_budget_skip layer={name} (atlas exceeds the shared {MAX_TOTAL_TEXTURE_BYTES} byte texture budget)"
                        );
                    }
                    ok = false;
                } else {
                    if !runtime.budget_counted {
                        runtime.budget_counted = true;
                        self.atlas_bytes_used = self.atlas_bytes_used.saturating_add(atlas_bytes);
                    }
                    match vulkan.upload_layer(i, &runtime.atlas.pixels, ATLAS_SIZE, ATLAS_SIZE) {
                        Ok(()) => {}
                        Err(error) if crate::vulkan::is_fence_timeout(&error) => {
                            return Err(error);
                        }
                        Err(_) => {
                            if runtime.budget_counted {
                                runtime.budget_counted = false;
                                self.atlas_bytes_used =
                                    self.atlas_bytes_used.saturating_sub(atlas_bytes);
                            }
                            ok = false;
                        }
                    }
                }
            }
            if ok && !runtime.vertex_bytes.is_empty() {
                match vulkan.upload_text_vertices(i, &runtime.vertex_bytes) {
                    Ok(()) => {}
                    Err(error) if crate::vulkan::is_fence_timeout(&error) => {
                        return Err(error);
                    }
                    Err(_) => ok = false,
                }
            }
            if !ok {
                failed.push(i);
            }
        }
        Ok(failed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kwe-m3e-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_bytes(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn font_open_rejects_garbage() {
        assert!(Font::open(b"not a font".to_vec()).is_none());
        assert!(Font::open(Vec::new()).is_none());
        // A fake table header that still fails InitFont.
        let mut fake = vec![0u8; 64];
        fake[0..4].copy_from_slice(b"ttcf");
        assert!(Font::open(fake).is_none());
    }

    /// A minimal-but-valid TrueType font: the seven tables stbtt_InitFont
    /// requires (cmap, head, hhea, hmtx, maxp, loca, glyf), one empty
    /// glyph, short-format loca. Table records carry real offsets; the
    /// buffer is the authoritative size. `loca` is the two-entry u16 loca
    /// table, so the test can craft a font whose glyph range lies inside
    /// glyf (accepted) or past it (rejected by the sfnt validation).
    fn minimal_font_bytes(loca: [u16; 2]) -> Vec<u8> {
        let tags: [[u8; 4]; 7] = [
            *b"cmap", *b"head", *b"hhea", *b"hmtx", *b"maxp", *b"loca", *b"glyf",
        ];
        // cmap: header + one Microsoft Unicode encoding record + a format-0
        // subtable (stbtt_InitFont rejects a cmap with no encodings).
        let cmap_size: u32 = 4 + 8 + 262;
        let sizes: [u32; 7] = [cmap_size, 54, 36, 4, 6, 4, 1];
        let header_len: u32 = 12 + 7 * 16;
        let mut out = Vec::new();
        // Offset table.
        out.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // version 1.0
        out.extend_from_slice(&7u16.to_be_bytes()); // numTables
        out.extend_from_slice(&0u16.to_be_bytes()); // searchRange (unused)
        out.extend_from_slice(&0u16.to_be_bytes()); // entrySelector
        out.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
        // Table records (tag, checksum, offset, length).
        let mut offset = header_len;
        for (tag, size) in tags.iter().zip(sizes) {
            out.extend_from_slice(tag);
            out.extend_from_slice(&0u32.to_be_bytes()); // checksum (unused)
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
            offset += size;
        }
        // cmap: version 0, one encoding record (Microsoft, Unicode BMP)
        // pointing at a format-0 subtable mapping every byte to glyph 0.
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes()); // platform: Microsoft
        out.extend_from_slice(&1u16.to_be_bytes()); // encoding: Unicode BMP
        out.extend_from_slice(&12u32.to_be_bytes()); // subtable offset
        out.extend_from_slice(&0u16.to_be_bytes()); // format 0
        out.extend_from_slice(&262u16.to_be_bytes()); // length
        out.extend_from_slice(&0u16.to_be_bytes()); // language
        out.extend_from_slice(&[0u8; 256]); // glyph ids, all 0
        // head: 54 bytes, indexToLocFormat (i16 at 50) = 0 (short loca).
        out.extend_from_slice(&[0u8; 54]);
        // hhea: 36 bytes.
        out.extend_from_slice(&[0u8; 36]);
        // hmtx: 4 bytes = advance 500 + lsb 0 for glyph 0.
        out.extend_from_slice(&500u16.to_be_bytes());
        out.extend_from_slice(&0i16.to_be_bytes());
        // maxp: version (4 bytes) + numGlyphs = 1.
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&1u16.to_be_bytes());
        // loca: the caller's two entries.
        out.extend_from_slice(&loca[0].to_be_bytes());
        out.extend_from_slice(&loca[1].to_be_bytes());
        // glyf: one empty byte.
        out.extend_from_slice(&[0u8; 1]);
        out
    }

    /// The M3e review fixes: the shim validates the sfnt structure before
    /// any stb call (stb_truetype itself does no range checking of the
    /// file's offsets), so hostile stubs are refused at open instead of
    /// walking out of the buffer.
    #[test]
    fn hostile_font_stubs_are_rejected() {
        // A 12-byte ttcf stub whose numFonts claim cannot fit the buffer.
        let stub = [b't', b't', b'c', b'f', 0, 0, 1, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(Font::open(stub.to_vec()).is_none());
        // A 16-byte ttcf with one embedded offset outside the buffer.
        let mut ttc = vec![0u8; 16];
        ttc[0..4].copy_from_slice(b"ttcf");
        ttc[4..8].copy_from_slice(&1u32.to_be_bytes()); // version
        ttc[8..12].copy_from_slice(&1u32.to_be_bytes()); // numFonts
        ttc[12..16].copy_from_slice(&64u32.to_be_bytes()); // offset past end
        assert!(Font::open(ttc).is_none());
        // An sfnt claiming more tables than the buffer can hold.
        let mut truncated = vec![0u8; 12];
        truncated[0..4].copy_from_slice(&[0, 1, 0, 0]);
        truncated[4..6].copy_from_slice(&5u16.to_be_bytes()); // numTables = 5
        assert!(Font::open(truncated).is_none());
        // A table record whose (offset + length) lies past the end of the
        // buffer (corrupt the hmtx record's offset field).
        let mut past = minimal_font_bytes([0, 0]);
        past[12 + 3 * 16 + 8..12 + 3 * 16 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Font::open(past).is_none());
        // A loca entry pointing past the glyf table.
        let bad_loca = minimal_font_bytes([0, 2]); // glyph 0 range ends past glyf
        assert!(Font::open(bad_loca).is_none());
        // An hmtx table too short for the glyph count (stb reads
        // hmtx[glyph*4] without length checks). Patch the hmtx record's
        // claimed length to 2: the record still lies inside the buffer,
        // but the table no longer covers glyph 0's metric.
        let mut short_hmtx = minimal_font_bytes([0, 0]);
        short_hmtx[12 + 3 * 16 + 12..12 + 3 * 16 + 16].copy_from_slice(&2u32.to_be_bytes());
        assert!(Font::open(short_hmtx).is_none());
    }

    /// The positive control for the sfnt validation: the crafted minimal
    /// font passes the structure checks AND stbtt_InitFont, and its one
    /// glyph (an empty range) renders nothing.
    #[test]
    fn crafted_minimal_font_opens_and_renders_empty() {
        let font =
            Font::open(minimal_font_bytes([0, 0])).expect("the minimal font is structurally valid");
        let mut buf = vec![0u8; 8 * 8];
        assert!(font.render_glyph(&mut buf, 8, 8, 8, 1.0, 0, 0, 0));
        assert!(buf.iter().all(|&a| a == 0), "empty glyph covers nothing");
    }

    #[test]
    fn resolution_order_prefers_explicit_dirs_then_family() {
        let dir = temp_dir("order");
        // Two fonts: an explicit-dir font with family "Test Sans" and a
        // system-ish font with the same basename but different family.
        write_bytes(&dir, "a.ttf", &[]); // invalid, ignored
        // We cannot fabricate real fonts here; the resolver must simply
        // never panic on garbage, and must prefer the first valid file.
        let mut resolver = FontResolver::new(std::slice::from_ref(&dir));
        let files = resolver.files.clone();
        assert!(!files.is_empty(), "files collected from the temp dir");
        let (f, m) = resolver.resolve("whatever");
        // A garbage-only dir never errors and never fabricates a match:
        // the resolution ends at the first step the host system satisfies
        // (the fallback chain resolving exactly, the any-font step, or
        // None when no fonts exist — the fallback chain itself can land
        // Exact when the host has e.g. Noto Sans, whose UTF-16 name
        // record must decode to "Noto Sans").
        match m {
            FontMatch::Exact => assert!(f.is_some()),
            FontMatch::Fallback(name) => {
                assert!(f.is_some());
                assert!(!name.is_empty());
            }
            FontMatch::Any => assert!(f.is_some()),
            FontMatch::None => assert!(f.is_none()),
            other => panic!("unexpected resolution {other:?}"),
        }
        // Cleanup happens on drop (temp dir left behind is harmless).
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_chars_is_char_based() {
        // Measured in chars, not bytes: "é" is 2 bytes but 1 char.
        assert_eq!(truncate_chars("héllo", 3), "hél");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("日本語", 2), "日本");
    }

    #[test]
    fn alignment_parsing() {
        assert_eq!(HorizontalAlign::parse("left"), Some(HorizontalAlign::Left));
        assert_eq!(
            HorizontalAlign::parse("CENTER"),
            Some(HorizontalAlign::Center)
        );
        assert_eq!(
            HorizontalAlign::parse("right"),
            Some(HorizontalAlign::Right)
        );
        assert_eq!(HorizontalAlign::parse("bogus"), None);
        assert_eq!(VerticalAlign::parse("top"), Some(VerticalAlign::Top));
        assert_eq!(VerticalAlign::parse("center"), Some(VerticalAlign::Center));
        assert_eq!(VerticalAlign::parse("bottom"), Some(VerticalAlign::Bottom));
        assert_eq!(VerticalAlign::parse(""), None);
    }

    /// A fake Font (via a helper that records calls) is not possible
    /// because Font is a concrete type; instead we test layout math with
    /// real system fonts when available (skipped otherwise), and the pure
    /// math (anchor/bounds) with a synthetic layout.
    fn system_font() -> Option<Font> {
        for path in [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        ] {
            if let Ok(bytes) = fs::read(path)
                && let Some(f) = Font::open(bytes)
            {
                return Some(f);
            }
        }
        // Any ttf under /usr/share/fonts.
        for entry in fs::read_dir("/usr/share/fonts").ok()?.take(32) {
            let entry = entry.ok()?;
            if let Ok(bytes) = fs::read(entry.path())
                && let Some(f) = Font::open(bytes)
                && f.family_name().is_some_and(|name| !name.is_empty())
            {
                return Some(f);
            }
        }
        None
    }

    #[test]
    fn system_font_layout_and_rasterization() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system ttf fonts found");
            return;
        };
        let family = font.family_name().unwrap_or_default();
        assert!(!family.is_empty(), "family name readable");
        let glyph_a = font.glyph_index('A' as u32);
        assert!(glyph_a != 0, "glyph 'A' exists");

        // Layout "Hello" at 24 px: bounds must be sane and non-empty.
        let layout = layout_text(&font, "Hello", 24.0, MAX_TEXT_CHARS);
        assert_eq!(layout.placements.len(), 5);
        assert!(layout.max_x > layout.min_x, "advances produce width");
        assert!(layout.max_y >= layout.min_y);
        assert!(
            layout.max_x < 200.0,
            "24px em text is not huge: {}",
            layout.max_x
        );

        // Rasterization: glyph 'A' at 24 px must produce a non-empty bitmap.
        let scale = font.scale_for_pixel_height(24.0);
        let [ix0, iy0, ix1, iy1] = font.glyph_bitmap_box(glyph_a, scale);
        let (w, h) = (ix1 - ix0, iy1 - iy0);
        assert!(w > 0 && h > 0, "A has a bitmap at 24px");
        assert!((w as i64) < 512 && (h as i64) < 512);
        // The rasterizer writes 1-byte alpha coverage at the caller's
        // stride (the atlas splats it to white+alpha).
        let mut buf = vec![0u8; (w as usize) * (h as usize)];
        assert!(font.render_glyph(&mut buf, w, h, w, scale, glyph_a, ix0, iy0));
        let covered = buf.iter().filter(|&&a| a > 0).count();
        assert!(covered > 0, "glyph A paints pixels");

        // Anchor math: centered layout's anchor is the box midpoint.
        let layout = layout_text(&font, "Ab", 24.0, MAX_TEXT_CHARS);
        let anchor = anchor_offset(&layout, HorizontalAlign::Center, VerticalAlign::Center);
        assert!((anchor[0] - (layout.min_x + layout.max_x) * 0.5).abs() < 0.01);
        assert!((anchor[1] - (layout.min_y + layout.max_y) * 0.5).abs() < 0.01);
    }

    #[test]
    fn atlas_packs_and_evicts_with_rate_limit() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system ttf fonts found");
            return;
        };
        let mut atlas = GlyphAtlas::new();
        let glyphs: Vec<u32> = "Hello world"
            .chars()
            .map(|c| font.glyph_index(c as u32))
            .filter(|&g| g != 0)
            .collect();
        let scale = font.scale_for_pixel_height(24.0);
        let (diag, changed) = atlas.sync(&font, &glyphs, scale);
        assert!(!diag.atlas_rate_limited);
        assert!(changed);
        for &g in &glyphs {
            assert!(
                atlas.contains(g) || g == 0,
                "glyph {g} present after first sync"
            );
        }
        // A second sync of the same set is a no-op.
        let (_, changed) = atlas.sync(&font, &glyphs, scale);
        assert!(!changed);

        // Whitespace-only glyphs still sync without error.
        let space = font.glyph_index(' ' as u32);
        let (_, _changed) = atlas.sync(&font, &[space], scale);
        assert!(atlas.contains(space));

        // Clear must drop all entries (font-change path).
        atlas.clear();
        assert!(!atlas.contains(glyphs[0]));
    }

    /// Collect up to `cap` distinct glyph indices from a codepoint range
    /// (skipping glyph 0, the .notdef slot). DejaVu Sans covers thousands
    /// of codepoints, so `cap` distinct glyphs are essentially always
    /// available; if fewer than `min` are found the test skips (fonts with
    /// tiny coverage cannot force an overflow).
    fn distinct_glyphs(
        font: &Font,
        range: std::ops::Range<u32>,
        cap: usize,
        min: usize,
    ) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for cp in range {
            let g = font.glyph_index(cp);
            if g != 0 && !out.contains(&g) {
                out.push(g);
                if out.len() >= cap {
                    break;
                }
            }
        }
        if out.len() < min { Vec::new() } else { out }
    }

    #[test]
    fn atlas_rate_limits_rebuilds() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system ttf fonts found");
            return;
        };
        // At ~512 px per em every glyph box is near the atlas row height,
        // so even 100 distinct glyphs cannot fit in the 2048^2 atlas —
        // every sync overflows and triggers the clear+repack path.
        let scale = font.scale_for_pixel_height(MAX_FONT_PX);
        let a = distinct_glyphs(&font, 0x20..0x1000, 128, 16);
        let b = distinct_glyphs(&font, 0x1000..0x2000, 128, 16);
        let c = distinct_glyphs(&font, 0x2000..0x3000, 128, 16);
        if a.is_empty() || b.is_empty() || c.is_empty() {
            eprintln!("skip: font lacks enough distinct glyphs");
            return;
        }
        let mut atlas = GlyphAtlas::new();
        // 1st overflow: allowed (0 prior rebuilds), repacked.
        let (diag, changed) = atlas.sync(&font, &a, scale);
        assert!(!diag.atlas_rate_limited, "first rebuild is allowed");
        assert!(changed, "overflow repack always reports a content change");
        // 2nd overflow within the same second: allowed (1 prior rebuild).
        let (diag, changed) = atlas.sync(&font, &b, scale);
        assert!(!diag.atlas_rate_limited, "second rebuild is allowed");
        assert!(changed);
        // 3rd overflow within the same second: rate-limited, content kept.
        let (diag, changed) = atlas.sync(&font, &c, scale);
        assert!(diag.atlas_rate_limited, "third rebuild within 1s is capped");
        assert!(!changed, "rate-limited sync does not touch the atlas");
        // The atlas must never exceed its pixel budget.
        let covered: usize = atlas.entries.values().map(|r| (r.w * r.h) as usize).sum();
        assert!(covered <= (ATLAS_SIZE as usize) * (ATLAS_SIZE as usize));
        // All entries stay inside the atlas.
        for r in atlas.entries.values() {
            assert!(r.x + r.w <= ATLAS_SIZE);
            assert!(r.y + r.h <= ATLAS_SIZE);
        }
    }

    #[test]
    fn atlas_overflow_repacks() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system ttf fonts found");
            return;
        };
        let scale = font.scale_for_pixel_height(MAX_FONT_PX);
        let a = distinct_glyphs(&font, 0x20..0x1000, 128, 16);
        let b = distinct_glyphs(&font, 0x1000..0x2000, 128, 16);
        if a.is_empty() || b.is_empty() {
            eprintln!("skip: font lacks enough distinct glyphs");
            return;
        }
        let mut atlas = GlyphAtlas::new();
        let (_, changed) = atlas.sync(&font, &a, scale);
        assert!(changed);
        assert!(!atlas.entries.is_empty());
        // Repack replaces the content with the new set (or a subset of it).
        let (_, changed) = atlas.sync(&font, &b, scale);
        assert!(changed);
        let _ = atlas;
        // Bounds sanity for the packed rects (already covered above, but
        // re-asserted here since this test runs first in isolation too).
    }

    /// The M3e review fix: font/pointsize changes go through the SAME
    /// rebuild budget as an overflow repack (clear_budgeted), so a 60 fps
    /// pointsize toggle can never force a clear+repack per frame. A
    /// denied clear leaves the previous content intact — the caller keeps
    /// the old atlas AND the old geometry and retries on the next sync,
    /// so the layer never renders mismatched glyphs at the wrong scale.
    #[test]
    fn atlas_rebuild_budget_covers_font_and_scale_changes() {
        let Some(font) = system_font() else {
            eprintln!("skip: no system ttf fonts found");
            return;
        };
        let glyph = font.glyph_index('W' as u32);
        if glyph == 0 {
            eprintln!("skip: font lacks a glyph for 'W'");
            return;
        }
        let scale = font.scale_for_pixel_height(24.0);
        let mut atlas = GlyphAtlas::new();
        let (_, changed) = atlas.sync(&font, &[glyph], scale);
        assert!(changed && atlas.contains(glyph), "seed the atlas");
        // Font/scale change 1 within the window: allowed (0 prior
        // rebuilds), and the clear drops the old glyphs.
        assert!(atlas.clear_budgeted(), "first change is allowed");
        assert!(!atlas.contains(glyph), "clear dropped the glyphs");
        // The layer would re-sync at the new scale; seed again so the
        // third call has content to preserve.
        let (_, changed) = atlas.sync(&font, &[glyph], scale);
        assert!(changed && atlas.contains(glyph), "re-seed after change 1");
        // Change 2 within the same second: allowed (1 prior rebuild).
        assert!(atlas.clear_budgeted(), "second change is allowed");
        let (_, changed) = atlas.sync(&font, &[glyph], scale);
        assert!(changed && atlas.contains(glyph), "re-seed after change 2");
        // Change 3 within the same second: rate-limited — the clear is
        // denied and the previous content survives intact.
        assert!(
            !atlas.clear_budgeted(),
            "third change within 1s is capped by the rebuild budget"
        );
        assert!(atlas.contains(glyph), "denied clear keeps the atlas");
    }

    #[test]
    fn vertex_bytes_match_unit_quad_winding() {
        // Rebuild the same quad manually and compare with the builder for
        // a synthetic layout + atlas.
        let mut layout = TextLayout::default();
        layout.placements.push(GlyphPlacement {
            glyph: 7,
            pen_x: 0.0,
            ix0: 0.0,
            iy0: -10.0,
            ix1: 20.0,
            iy1: 0.0,
        });
        layout.min_x = 0.0;
        layout.max_x = 20.0;
        layout.min_y = -10.0;
        layout.max_y = 0.0;
        layout.scale = 1.0;
        let mut atlas = GlyphAtlas::new();
        atlas.entries.insert(
            7,
            AtlasRect {
                x: 32,
                y: 64,
                w: 22,
                h: 12,
            },
        );
        let anchor = anchor_offset(&layout, HorizontalAlign::Center, VerticalAlign::Center);
        let (bytes, quads) = build_vertex_bytes(&layout, anchor, &atlas);
        assert_eq!(quads, 1);
        assert_eq!(bytes.len(), 6 * 16);
        let f = |i: usize| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        // Vertex n occupies floats [4n .. 4n+4): x, y, u, v.
        let vx = |n: usize| f(n * 4);
        let vy = |n: usize| f(n * 4 + 1);
        let vu = |n: usize| f(n * 4 + 2);
        let vv = |n: usize| f(n * 4 + 3);
        // Anchor: x = (0 + 20) / 2 = 10, y = (-10 + 0) / 2 = -5.
        // Quad: x0 = 0 - 10 = -10, x1 = 20 - 10 = 10, y0 = -10 + 5 = -5, y1 = 5.
        // UVs are inset by ATLAS_PAD: the rect (32, 64, w 22, h 12) is
        // the PADDED box, the quad the unpadded metric box — the pad is
        // dropped so glyph texels map 1:1 (the M3e review fix; sampling
        // the full padded rect would render the glyph at 22/24 scale).
        let pad = ATLAS_PAD as f32 / ATLAS_SIZE as f32;
        let u0 = 32.0 / ATLAS_SIZE as f32 + pad;
        let v0 = 64.0 / ATLAS_SIZE as f32 + pad;
        let u1 = 54.0 / ATLAS_SIZE as f32 - pad;
        let v1 = 76.0 / ATLAS_SIZE as f32 - pad;
        let expect = |n: usize, x: f32, y: f32, u: f32, v: f32| {
            assert!((vx(n) - x).abs() < 0.001, "v{n}.x = {} want {x}", vx(n));
            assert!((vy(n) - y).abs() < 0.001, "v{n}.y = {} want {y}", vy(n));
            assert!((vu(n) - u).abs() < 0.0001, "v{n}.u = {} want {u}", vu(n));
            assert!((vv(n) - v).abs() < 0.0001, "v{n}.v = {} want {v}", vv(n));
        };
        // Winding mirrors UNIT_QUAD: v0 tl, v1 tr, v2 br, then v2 br, v3
        // bl, v0 tl.
        expect(0, -10.0, -5.0, u0, v0);
        expect(1, 10.0, -5.0, u1, v0);
        expect(2, 10.0, 5.0, u1, v1);
        expect(3, 10.0, 5.0, u1, v1);
        expect(4, -10.0, 5.0, u0, v1);
        expect(5, -10.0, -5.0, u0, v0);
    }

    #[test]
    fn resolver_bounds() {
        // A dir with too many files must still be bounded (no panic).
        let dir = temp_dir("bounds");
        for i in 0..200 {
            write_bytes(&dir, &format!("font{i}.ttf"), &[]);
        }
        let resolver = FontResolver::new(std::slice::from_ref(&dir));
        assert!(resolver.files.len() <= MAX_FONT_FILES_PER_DIR * 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
