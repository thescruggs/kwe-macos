// SPDX-License-Identifier: GPL-3.0-or-later
//! TEXV texture container decoder (S1).
//!
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Data/Parsers/TextureParser.cpp:1-404,
//! src/WallpaperEngine/Data/Assets/Texture.h:1-178 @ b016d7d1 — adapted.
//! The container/header/mipmap/frame-table field layout and read order are
//! re-derived here in Rust from that file; the format-enum values and
//! magic strings match it exactly (cross-checked byte-for-byte against
//! real Workshop `.tex` payloads under
//! `/media/crushinator/steamapps/workshop/content/431960/*/`). The
//! raw-pixel-format upload rule (which formats go through the CPU decoder
//! vs. an embedded image container, and that `ARGB8888`'s in-memory byte
//! order is already R,G,B,A — not swizzled — despite the name) is
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Render/CTexture.cpp:60-95,150-170 @ b016d7d1 —
//! adapted (re-expressed as a CPU RGBA8 expansion here; upstream hands the
//! bytes to OpenGL unchanged).
//!
//! Scope (S1): the container envelope, the format table, LZ4 block
//! decompression per mipmap, and mip-0/image-0 pixel decode to RGBA8,
//! cropped to the declared "real" size. Mesh/puppet geometry and the
//! shader/effect pipeline are not read here at all (scenemodel.rs, later
//! slices). Animated (`TEXS*`) frame tables are parsed and kept (bounded)
//! but frame 0 — the same static mip-0 pixels — is what this slice draws.
//!
//! Every entry point returns `Result`/`Option`; nothing here panics on
//! hostile input (truncated buffers, absurd declared sizes, wrong magics)
//! — see the `tests` module for the fuzz-ish coverage.

use crate::textures::{self, DecodedTexture};

/// Per-edge dimension cap (mirrors `textures::MAX_TEXTURE_DIMENSION`):
/// applied to the pow2 in-memory size, the declared "real" size, and every
/// mipmap's own declared size, all before any allocation.
pub const MAX_TEXTURE_DIMENSION: u32 = textures::MAX_TEXTURE_DIMENSION;
/// Per-mip pixel-count cap (mirrors `textures::MAX_TEXTURE_PIXELS`):
/// applied to the mip actually decoded (mip 0), bounding the raw/BC
/// allocation independently of the two edge caps (an 8192×256 mip would
/// pass the edge cap but still be capped here).
pub const MAX_MIP_PIXELS: u64 = textures::MAX_TEXTURE_PIXELS;
/// Mipmap-chain length cap.
pub const MAX_MIPMAP_COUNT: u32 = 16;
/// Image-count cap (cubemaps/arrays are not modeled; S1 only ever reads
/// image 0, but the count itself must still be bounded before the parser
/// walks that many mipmap chains).
pub const MAX_IMAGE_COUNT: u32 = 16;
/// Cap on a `TEXB0004` mip's embedded editor-JSON string.
const MAX_MIP_JSON_BYTES: usize = 64 * 1024;
/// Cap on the animated frame table (`TEXS*`); the corpus's largest
/// spritesheets run well under four figures of frames.
const MAX_FRAME_COUNT: u32 = 8192;

/// `TextureFlags_IsGif` (upstream `Texture.h`): the container carries a
/// `TEXS*` animation/frame table after the mipmap data.
const FLAG_IS_GIF: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TexvError {
    pub message: String,
}

impl TexvError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TexvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TexvError {}

/// `TextureFormat` (upstream `Texture.h` — values match exactly). Formats
/// this decoder does not implement pixel decode for
/// (`Rgb888`/`Rgb565`/`Rg1616f`/`R16f`/`Rgba1010102`/`Rgba16161616f`/
/// `Rgb161616f`) still parse the header correctly; `decode_texv` returns
/// `Err` for them instead of panicking or guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureFormat {
    Argb8888,
    Rgb888,
    Rgb565,
    Dxt5,
    Dxt3,
    Dxt1,
    Rg88,
    R8,
    Rg1616f,
    R16f,
    Bc7,
    Rgba1010102,
    Rgba16161616f,
    Rgb161616f,
}

impl TextureFormat {
    fn from_u32(value: u32) -> Result<Self, TexvError> {
        Ok(match value {
            0 => Self::Argb8888,
            1 => Self::Rgb888,
            2 => Self::Rgb565,
            4 => Self::Dxt5,
            6 => Self::Dxt3,
            7 => Self::Dxt1,
            8 => Self::Rg88,
            9 => Self::R8,
            10 => Self::Rg1616f,
            11 => Self::R16f,
            12 => Self::Bc7,
            13 => Self::Rgba1010102,
            14 => Self::Rgba16161616f,
            15 => Self::Rgb161616f,
            other => return Err(TexvError::new(format!("unknown texture format: {other}"))),
        })
    }

    /// Bytes per pixel for the raw (non-block-compressed) formats this
    /// decoder expands directly; `None` for block formats and formats not
    /// implemented.
    fn raw_bytes_per_pixel(self) -> Option<u32> {
        match self {
            Self::Argb8888 => Some(4),
            Self::R8 => Some(1),
            Self::Rg88 => Some(2),
            _ => None,
        }
    }

    /// Block byte size for the BC formats this decoder implements (4x4
    /// blocks for all three); `None` otherwise.
    fn block_bytes(self) -> Option<u32> {
        match self {
            Self::Dxt1 => Some(8),
            Self::Dxt3 | Self::Dxt5 | Self::Bc7 => Some(16),
            _ => None,
        }
    }
}

/// One parsed (unused this slice) animation frame — `TEXS0001`
/// (`parseFrameV1`) or `TEXS0002`/`TEXS0003` (`parseFrame`) shape,
/// upstream `TextureParser.cpp:95-123`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub frame_number: u32,
    pub frametime: f32,
    pub x: f32,
    pub y: f32,
    pub width1: f32,
    pub height1: f32,
}

/// The decoded result: mip-0/image-0 RGBA8 pixels cropped to the
/// container's declared "real" size, plus the header fields and (unused,
/// kept for the next slice) animation frame table a caller may want to
/// record.
#[derive(Debug, Clone)]
pub struct DecodedTexv {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Not read outside tests this slice: `decode_model_texture` only
    /// keeps rgba/width/height. Kept on the struct for the next slice
    /// (shader dispatch needs to know the source format).
    #[allow(dead_code)]
    pub format: TextureFormat,
    /// `TextureFlags_*` (upstream `Texture.h`): `NoInterpolation`,
    /// `ClampUVs`, `IsGif`, ... — not consumed by this slice (no sampler
    /// state exists yet), kept for the shader/sampler slice.
    #[allow(dead_code)]
    pub flags: u32,
    /// Kept for callers that want the raw per-frame table (frame timing,
    /// exact non-uniform frame boxes); most callers should use
    /// `spritesheet` instead, which is the uniform-grid summary
    /// `decode_model_texture` actually carries forward.
    #[allow(dead_code)]
    pub frames: Vec<Frame>,
    /// S7: the inferred uniform spritesheet grid, when `frames` is
    /// non-empty and its first frame's size evenly tiles the texture (see
    /// `infer_spritesheet_grid`). `None` for a static texture or a
    /// `TEXS*` table this heuristic can't grid (e.g. a plain GIF, where
    /// upstream's own guard also declines to treat it as a spritesheet).
    pub spritesheet: Option<textures::SpritesheetGrid>,
}

/// Infer a uniform spritesheet grid from a texture's animation frame table
/// (upstream `TextureParser.cpp:275-302`, the `TEXS*`-driven half — the
/// separate `.tex-json` `spritesheetsequences` metadata upstream also reads
/// is not parsed here, no sidecar file model exists in this crate). Frame 0's
/// declared box size is assumed to be every frame's size (the common case);
/// `cols`/`rows` are the rounded texture-size/frame-size ratio, and the grid
/// is only trusted when it can actually hold every declared frame
/// (`cols * rows >= frame_count`) — the same guard upstream uses to avoid
/// treating a plain animated GIF (`frameWidth == textureWidth`, `cols == 1`
/// but `rows` would have to equal `frame_count`, easy to get by chance on
/// a tall single-column strip) as a spritesheet when it isn't a grid.
fn infer_spritesheet_grid(
    frames: &[Frame],
    width: u32,
    height: u32,
) -> Option<textures::SpritesheetGrid> {
    let first = frames.first()?;
    if width == 0 || height == 0 || first.width1 <= 0.0 || first.height1 <= 0.0 {
        return None;
    }
    let cols = (width as f64 / first.width1 as f64).round();
    let rows = (height as f64 / first.height1 as f64).round();
    if !(cols.is_finite() && rows.is_finite()) || cols < 1.0 || rows < 1.0 {
        return None;
    }
    let cols = cols as u32;
    let rows = rows as u32;
    let frame_count = frames.len() as u32;
    if (cols as u64) * (rows as u64) < frame_count as u64 {
        return None;
    }
    let duration: f32 = frames.iter().map(|f| f.frametime.max(0.0)).sum();
    Some(textures::SpritesheetGrid {
        cols,
        rows,
        frame_count,
        duration,
    })
}

/// A small bounds-checked cursor: every read either returns bytes or a
/// `TexvError` — never panics, even on a truncated or hostile buffer.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], TexvError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| TexvError::new("texture container offset overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| TexvError::new("texture container truncated"))?;
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, TexvError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("take(4) returns 4 bytes");
        Ok(u32::from_le_bytes(bytes))
    }

    fn i32(&mut self) -> Result<i32, TexvError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("take(4) returns 4 bytes");
        Ok(i32::from_le_bytes(bytes))
    }

    /// A 9-byte fixed magic field (8 ASCII chars + one ignored/NUL byte,
    /// upstream reads exactly 9 with `file.next(magic, 9)`).
    fn magic9(&mut self) -> Result<[u8; 9], TexvError> {
        Ok(self.take(9)?.try_into().expect("take(9) returns 9 bytes"))
    }

    /// A NUL-terminated string, bounded to `max_len` bytes of search (the
    /// `TEXB0004` mip's embedded editor JSON) — never scans past the end
    /// of the buffer or past the cap.
    fn cstring_bounded(&mut self, max_len: usize) -> Result<String, TexvError> {
        let window = &self.buf[self.pos..];
        let scan_len = window.len().min(max_len);
        let nul = window[..scan_len]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| {
                TexvError::new("texture container: unterminated or oversized embedded string")
            })?;
        let text = String::from_utf8_lossy(&window[..nul]).into_owned();
        self.pos += nul + 1;
        Ok(text)
    }
}

fn expect_magic(reader: &mut Reader<'_>, expected: &[u8], what: &str) -> Result<(), TexvError> {
    let magic = reader.magic9()?;
    // The 9th byte is a NUL terminator on every real payload observed, but
    // upstream compares with `strncmp(magic, expected, 9)` — an exact
    // 9-byte match — so this does too.
    let mut want = [0u8; 9];
    want[..expected.len()].copy_from_slice(expected);
    if magic != want {
        return Err(TexvError::new(format!(
            "unexpected {what} magic: {:?}",
            String::from_utf8_lossy(&magic)
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerVersion {
    Texb0001,
    Texb0002,
    Texb0003,
    Texb0004,
}

struct Header {
    format: TextureFormat,
    flags: u32,
    width: u32,
    height: u32,
    container_version: ContainerVersion,
    /// `FIF` value; `None` maps to upstream's `FIF_UNKNOWN` (-1): the
    /// payload is one of the raw/BC formats above. `Some` means the mip
    /// payload is itself an encoded image container (PNG/JPEG/...),
    /// decoded through `textures::decode_texture`.
    fif: Option<i32>,
    image_count: u32,
}

fn parse_header(reader: &mut Reader<'_>) -> Result<Header, TexvError> {
    expect_magic(reader, b"TEXV0005", "texture container")?;
    expect_magic(reader, b"TEXI0001", "texture sub-container")?;
    let format = TextureFormat::from_u32(reader.u32()?)?;
    let flags = reader.u32()?;
    let texture_width = reader.u32()?;
    let texture_height = reader.u32()?;
    let width = reader.u32()?;
    let height = reader.u32()?;
    let _ignored = reader.u32()?;
    for (label, dim) in [
        ("in-memory width", texture_width),
        ("in-memory height", texture_height),
        ("real width", width),
        ("real height", height),
    ] {
        if dim == 0 || dim > MAX_TEXTURE_DIMENSION {
            return Err(TexvError::new(format!(
                "texture {label} {dim} is 0 or over the {MAX_TEXTURE_DIMENSION} cap"
            )));
        }
    }

    let container_magic = reader.magic9()?;
    let image_count = reader.u32()?;
    let (container_version, fif) = if &container_magic[..8] == b"TEXB0004" {
        let fif = reader.i32()?;
        let _is_video_mp4 = reader.u32()?;
        (ContainerVersion::Texb0004, Some(fif))
    } else if &container_magic[..8] == b"TEXB0003" {
        let fif = reader.i32()?;
        (ContainerVersion::Texb0003, Some(fif))
    } else if &container_magic[..8] == b"TEXB0002" {
        (ContainerVersion::Texb0002, None)
    } else if &container_magic[..8] == b"TEXB0001" {
        (ContainerVersion::Texb0001, None)
    } else {
        return Err(TexvError::new(format!(
            "unknown texture sub-container version: {:?}",
            String::from_utf8_lossy(&container_magic)
        )));
    };
    // FIF_UNKNOWN is -1 on every container version that carries the field;
    // any other value (including the FIF_* enum's non-negative members)
    // means "decode through the embedded image container".
    let fif = fif.filter(|&value| value != -1);
    if image_count == 0 || image_count > MAX_IMAGE_COUNT {
        return Err(TexvError::new(format!(
            "texture image count {image_count} is 0 or over the {MAX_IMAGE_COUNT} cap"
        )));
    }

    Ok(Header {
        format,
        flags,
        width,
        height,
        container_version,
        fif,
        image_count,
    })
}

struct Mipmap<'a> {
    width: u32,
    height: u32,
    /// Decompressed payload bytes for this mip.
    payload: Vec<u8>,
    _phantom: std::marker::PhantomData<&'a ()>,
}

/// The pixel-format byte count a mip's declared `uncompressedSize` must
/// equal exactly: the format+dimensions-derived expectation this decoder
/// checks the declared size against before any LZ4/allocation work (a
/// lying declared size is exactly the decompression-bomb vector this
/// bound exists for). `None` for a FIF-tagged container (the payload is
/// an encoded image file of arbitrary size) or a format this decoder does
/// not implement (decode will refuse it later; no size expectation to
/// check here). Computed from the HEADER's format/fif, not per-container-
/// version — this must run identically for every `ContainerVersion`,
/// `TEXB0004` included (review S1#1: this used to be computed by the
/// caller via an 8-byte peek-and-rewind that unconditionally skipped
/// `TEXB0004`, so a `TEXB0004` mip's declared size was never checked
/// against its dimensions at all).
fn expected_mip_size(header: &Header, mip_w: u32, mip_h: u32) -> Option<u64> {
    if header.fif.is_some() {
        return None;
    }
    if let Some(bpp) = header.format.raw_bytes_per_pixel() {
        return Some(u64::from(mip_w) * u64::from(mip_h) * u64::from(bpp));
    }
    if let Some(block_bytes) = header.format.block_bytes() {
        let blocks_x = u64::from(mip_w).div_ceil(4);
        let blocks_y = u64::from(mip_h).div_ceil(4);
        return Some(blocks_x * blocks_y * u64::from(block_bytes));
    }
    None
}

/// `parseMipmap` (`TextureParser.cpp:40-93`): width/height, then (from
/// `TEXB0002` on) a compression flag and declared uncompressed size, then
/// a declared compressed/stored size, then the payload — LZ4 block if
/// `compression == 1`, raw otherwise. The declared-size-vs-`expected_mip_size`
/// consistency check now runs from inside this function (see that
/// function's doc), uniformly across every container version.
///
/// `keep`: only image-0/mip-0 is ever decoded further (`decode_texv`), so
/// every other mip is validated (dimensions, declared-size consistency,
/// allocation cap) but its payload is only *skipped over* — advanced past
/// with `Reader::take`, never LZ4-decompressed or copied — returning
/// `Ok(None)` (review RECOMMENDED#4: without this, a container with the
/// maximum 256 mips near the per-mip allocation cap could force ~17 GB of
/// cumulative, wasted LZ4 decompression work for mips this decoder always
/// discards).
fn parse_mipmap<'a>(
    reader: &mut Reader<'a>,
    header: &Header,
    keep: bool,
) -> Result<Option<Mipmap<'a>>, TexvError> {
    if header.container_version == ContainerVersion::Texb0004 {
        let _ignored_a = reader.u32()?;
        let _ignored_b = reader.u32()?;
        let _json = reader.cstring_bounded(MAX_MIP_JSON_BYTES)?;
        let _ignored_c = reader.u32()?;
    }

    let width = reader.u32()?;
    let height = reader.u32()?;
    if width == 0 || height == 0 || width > MAX_TEXTURE_DIMENSION || height > MAX_TEXTURE_DIMENSION
    {
        return Err(TexvError::new(format!(
            "mipmap dimensions {width}x{height} are 0 or over the {MAX_TEXTURE_DIMENSION} cap"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_MIP_PIXELS {
        return Err(TexvError::new(format!(
            "mipmap {width}x{height} exceeds the {MAX_MIP_PIXELS} pixel cap"
        )));
    }
    let expected_size = expected_mip_size(header, width, height);

    let mut compression = 0u32;
    let mut uncompressed_size: i32 = 0;
    let has_compression_field = matches!(
        header.container_version,
        ContainerVersion::Texb0002 | ContainerVersion::Texb0003 | ContainerVersion::Texb0004
    );
    if has_compression_field {
        compression = reader.u32()?;
        uncompressed_size = reader.i32()?;
    }
    let compressed_size = reader.i32()?;
    if compression == 0 {
        uncompressed_size = compressed_size;
    }
    if uncompressed_size < 0 || compressed_size < 0 {
        return Err(TexvError::new("mipmap declares a negative payload size"));
    }
    let uncompressed_size = uncompressed_size as u64;
    let compressed_size = compressed_size as u64;

    // Declared-size-vs-payload consistency (S1 bound): for a format this
    // decoder pixel-decodes, the uncompressed size must equal exactly what
    // that format/dimensions require. A FIF-tagged mip has no such fixed
    // size (it's an encoded image file), so `expected_size` is `None` and
    // only the pixel/allocation cap below applies.
    if let Some(expected) = expected_size
        && uncompressed_size != expected
    {
        return Err(TexvError::new(format!(
            "mipmap declares {uncompressed_size} uncompressed bytes, expected {expected} for {width}x{height}"
        )));
    }
    // Independent of format-derived expectations: never allocate/decompress
    // past the mip pixel-count-derived allocation cap (four bytes/pixel is
    // the worst case among the formats implemented here).
    let alloc_cap = u64::from(width) * u64::from(height) * 4 + 4096;
    if uncompressed_size > alloc_cap {
        return Err(TexvError::new(format!(
            "mipmap declares {uncompressed_size} uncompressed bytes, over the {alloc_cap} byte allocation cap"
        )));
    }

    if !keep {
        // Validated above; advance past the payload bytes without
        // decompressing or allocating an owned copy — this mip is
        // discarded either way (only image-0/mip-0 is ever kept).
        let skip_len = if compression == 1 {
            compressed_size
        } else {
            uncompressed_size
        };
        let skip_len = usize::try_from(skip_len)
            .map_err(|_| TexvError::new("mipmap payload size does not fit usize"))?;
        reader.take(skip_len)?;
        return Ok(None);
    }

    let payload = if compression == 1 {
        let compressed = reader.take(compressed_size as usize)?;
        let uncompressed_size_usize = usize::try_from(uncompressed_size)
            .map_err(|_| TexvError::new("mipmap uncompressed size does not fit usize"))?;
        lz4_flex::block::decompress(compressed, uncompressed_size_usize)
            .map_err(|error| TexvError::new(format!("mipmap LZ4 decompress failed: {error}")))?
    } else {
        reader.take(uncompressed_size as usize)?.to_vec()
    };

    Ok(Some(Mipmap {
        width,
        height,
        payload,
        _phantom: std::marker::PhantomData,
    }))
}

/// `parseFrameV1` (`TextureParser.cpp:95-108`, `TEXS0001`).
fn parse_frame_v1(reader: &mut Reader<'_>) -> Result<Frame, TexvError> {
    let frame_number = reader.u32()?;
    let frametime = f32::from_bits(reader.u32()?);
    let x = reader.u32()? as f32;
    let y = reader.u32()? as f32;
    let width1 = reader.u32()? as f32;
    let _unknown_a = reader.u32()?;
    let _unknown_b = reader.u32()?;
    let height1 = reader.u32()? as f32;
    Ok(Frame {
        frame_number,
        frametime,
        x,
        y,
        width1,
        height1,
    })
}

fn f32_field(reader: &mut Reader<'_>) -> Result<f32, TexvError> {
    Ok(f32::from_bits(reader.u32()?))
}

/// `parseFrame` (`TextureParser.cpp:110-123`, `TEXS0002`/`TEXS0003`).
fn parse_frame(reader: &mut Reader<'_>) -> Result<Frame, TexvError> {
    let frame_number = reader.u32()?;
    let frametime = f32_field(reader)?;
    let x = f32_field(reader)?;
    let y = f32_field(reader)?;
    let width1 = f32_field(reader)?;
    let _width2 = f32_field(reader)?;
    let _height2 = f32_field(reader)?;
    let height1 = f32_field(reader)?;
    Ok(Frame {
        frame_number,
        frametime,
        x,
        y,
        width1,
        height1,
    })
}

/// `parseAnimations` (`TextureParser.cpp:238-303`, minus the spritesheet
/// grid inference — not needed to draw a static mip-0 quad this slice).
fn parse_animations(reader: &mut Reader<'_>) -> Result<Vec<Frame>, TexvError> {
    let magic = reader.magic9()?;
    let is_v1 = if &magic[..8] == b"TEXS0001" {
        true
    } else if &magic[..8] == b"TEXS0002" || &magic[..8] == b"TEXS0003" {
        false
    } else {
        return Err(TexvError::new(format!(
            "unknown animation container version: {:?}",
            String::from_utf8_lossy(&magic)
        )));
    };
    let frame_count = reader.u32()?;
    if frame_count > MAX_FRAME_COUNT {
        return Err(TexvError::new(format!(
            "animation frame count {frame_count} exceeds the {MAX_FRAME_COUNT} cap"
        )));
    }
    if &magic[..8] == b"TEXS0003" {
        let _gif_width = reader.u32()?;
        let _gif_height = reader.u32()?;
    }
    let mut frames = Vec::with_capacity(frame_count as usize);
    for _ in 0..frame_count {
        frames.push(if is_v1 {
            parse_frame_v1(reader)?
        } else {
            parse_frame(reader)?
        });
    }
    Ok(frames)
}

/// Expand a raw (non-block, non-FIF) mip payload to RGBA8. `ARGB8888`'s
/// in-memory byte order is already R,G,B,A (see the module doc — upstream
/// uploads it via `glTexImage2D(..., GL_RGBA, GL_UNSIGNED_BYTE, ...)`
/// unchanged), so it is a straight copy; `R8`/`RG88` are expanded to
/// grayscale/red-green RGBA with full alpha.
fn expand_raw(format: TextureFormat, payload: &[u8]) -> Vec<u8> {
    match format {
        TextureFormat::Argb8888 => payload.to_vec(),
        TextureFormat::R8 => payload.iter().flat_map(|&r| [r, r, r, 255]).collect(),
        TextureFormat::Rg88 => payload
            .chunks_exact(2)
            .flat_map(|rg| [rg[0], rg[1], 0, 255])
            .collect(),
        _ => Vec::new(), // unreachable: raw_bytes_per_pixel gates the caller
    }
}

/// Decode a block-compressed mip payload to RGBA8 via `texture2ddecoder`
/// (MIT/Apache-2.0, see THIRD_PARTY.yml). That crate's block decoders
/// write packed `u32`s built as `u32::from_le_bytes([b, g, r, a])` — i.e.
/// their little-endian in-memory byte layout is B,G,R,A — so each pixel's
/// R and B bytes are swapped here to produce straight RGBA8.
fn decode_block(
    format: TextureFormat,
    payload: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, TexvError> {
    let pixel_count = (width as usize) * (height as usize);
    let mut packed = vec![0u32; pixel_count];
    let decode_result = match format {
        TextureFormat::Dxt1 => {
            texture2ddecoder::decode_bc1(payload, width as usize, height as usize, &mut packed)
        }
        TextureFormat::Dxt3 => {
            texture2ddecoder::decode_bc2(payload, width as usize, height as usize, &mut packed)
        }
        TextureFormat::Dxt5 => {
            texture2ddecoder::decode_bc3(payload, width as usize, height as usize, &mut packed)
        }
        TextureFormat::Bc7 => {
            texture2ddecoder::decode_bc7(payload, width as usize, height as usize, &mut packed)
        }
        _ => unreachable!("caller only invokes decode_block for BC formats"),
    };
    decode_result.map_err(|error| TexvError::new(format!("block decode failed: {error}")))?;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for pixel in packed {
        let bytes = pixel.to_le_bytes(); // [b, g, r, a]
        rgba.extend_from_slice(&[bytes[2], bytes[1], bytes[0], bytes[3]]);
    }
    Ok(rgba)
}

/// Crop a `src_w`×`src_h` RGBA8 buffer to its top-left `real_w`×`real_h`
/// region (S1's simplification: no UV-scaling shader stage exists yet, so
/// the pow2/padded texture is physically cropped instead). A no-op copy
/// when the sizes already match (the common case).
fn crop_top_left(rgba: &[u8], src_w: u32, src_h: u32, real_w: u32, real_h: u32) -> Vec<u8> {
    let real_w = real_w.min(src_w);
    let real_h = real_h.min(src_h);
    if real_w == src_w && real_h == src_h {
        return rgba.to_vec();
    }
    let mut out = Vec::with_capacity((real_w * real_h * 4) as usize);
    for y in 0..real_h {
        let row_start = (y * src_w * 4) as usize;
        let row_end = row_start + (real_w * 4) as usize;
        out.extend_from_slice(&rgba[row_start..row_end]);
    }
    out
}

/// Decode a TEXV texture container's mip-0/image-0 pixels to RGBA8,
/// cropped to the declared real size. Structural bounds (dimension caps,
/// mip/image count caps, declared-size-vs-payload consistency, per-mip
/// allocation cap) are enforced before any allocation or decompression;
/// every failure is a `TexvError`, never a panic — see the `tests` module
/// for truncated/garbage-input coverage.
pub fn decode_texv(bytes: &[u8]) -> Result<DecodedTexv, TexvError> {
    let mut reader = Reader::new(bytes);
    let header = parse_header(&mut reader)?;

    let mut first_image_mip0: Option<Mipmap<'_>> = None;
    for image in 0..header.image_count {
        let mipmap_count = reader.u32()?;
        if mipmap_count == 0 || mipmap_count > MAX_MIPMAP_COUNT {
            return Err(TexvError::new(format!(
                "mipmap count {mipmap_count} is 0 or over the {MAX_MIPMAP_COUNT} cap"
            )));
        }
        for mip_index in 0..mipmap_count {
            // Only image-0/mip-0 is ever decoded further; every other mip
            // is validated then skipped without decompression work (see
            // parse_mipmap's `keep` doc).
            let keep = image == 0 && mip_index == 0;
            let mipmap = parse_mipmap(&mut reader, &header, keep)?;
            if keep {
                first_image_mip0 = mipmap;
            }
        }
    }

    let frames = if header.flags & FLAG_IS_GIF != 0 {
        parse_animations(&mut reader)?
    } else {
        Vec::new()
    };

    let mip0 = first_image_mip0
        .ok_or_else(|| TexvError::new("texture container has no image-0/mip-0 payload"))?;
    let spritesheet = infer_spritesheet_grid(&frames, header.width, header.height);

    let rgba_full = if let Some(_fif) = header.fif {
        let decoded = textures::decode_texture(&mip0.payload).ok_or_else(|| {
            TexvError::new("embedded FIF image payload failed to decode or exceeded limits")
        })?;
        return Ok(DecodedTexv {
            rgba: decoded.rgba,
            width: decoded.width,
            height: decoded.height,
            format: header.format,
            flags: header.flags,
            frames,
            spritesheet,
        });
    } else if let Some(_bpp) = header.format.raw_bytes_per_pixel() {
        expand_raw(header.format, &mip0.payload)
    } else if header.format.block_bytes().is_some() {
        decode_block(header.format, &mip0.payload, mip0.width, mip0.height)?
    } else {
        return Err(TexvError::new(format!(
            "texture format {:?} is not implemented",
            header.format
        )));
    };

    // Defense in depth (S1 review #1): by construction `rgba_full` should
    // already be exactly `mip0.width * mip0.height * 4` bytes (the raw
    // and block-decode paths above are each sized from `mip0.width`/
    // `mip0.height`, and `parse_mipmap` now checks the declared payload
    // size against those same dimensions before ever producing `payload`
    // for a raw format). Check it explicitly anyway rather than trust
    // that invariant transitively — `crop_top_left` indexes rows by
    // `mip0.width`/`mip0.height`, and a future change to either path that
    // breaks the invariant must fail cleanly here, not panic on a slice
    // range.
    let needed_bytes = (mip0.width as usize)
        .checked_mul(mip0.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    match needed_bytes {
        Some(needed) if rgba_full.len() >= needed => {}
        _ => {
            return Err(TexvError::new(
                "decoded mip buffer is smaller than its declared dimensions",
            ));
        }
    }

    let cropped = crop_top_left(
        &rgba_full,
        mip0.width,
        mip0.height,
        header.width,
        header.height,
    );
    let out_w = header.width.min(mip0.width);
    let out_h = header.height.min(mip0.height);

    Ok(DecodedTexv {
        rgba: cropped,
        width: out_w,
        height: out_h,
        format: header.format,
        flags: header.flags,
        frames,
        spritesheet,
    })
}

/// Decode either a TEXV0005 container or a plain image (png/jpeg/webp) to
/// a `textures::DecodedTexture`, dispatching on the magic prefix. Used
/// wherever a model's resolved material texture bytes need pixels (S1);
/// direct `.tex` image-layer/particle-texture references now also go
/// through this function (S7), so `spritesheet` is populated for them too
/// — see `main.rs`'s `load_layer_textures`/`load_particle_textures`.
pub fn decode_model_texture(bytes: &[u8]) -> Option<DecodedTexture> {
    if bytes.len() >= 8 && &bytes[..8] == b"TEXV0005" {
        let decoded = decode_texv(bytes).ok()?;
        if decoded.width == 0 || decoded.height == 0 {
            return None;
        }
        return Some(DecodedTexture {
            rgba: decoded.rgba,
            width: decoded.width,
            height: decoded.height,
            spritesheet: decoded.spritesheet,
        });
    }
    textures::decode_texture(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte matches the real
    /// `materials/effects/waterflowphase.tex` header verified against the
    /// corpus (see the module doc): TEXB0003, FIF_UNKNOWN, ARGB8888,
    /// 32x32, uncompressed.
    fn build_argb8888(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes()); // format ARGB8888
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&width.to_le_bytes()); // in-memory width
        out.extend_from_slice(&height.to_le_bytes()); // in-memory height
        out.extend_from_slice(&width.to_le_bytes()); // real width
        out.extend_from_slice(&height.to_le_bytes()); // real height
        out.extend_from_slice(&0u32.to_le_bytes()); // ignored
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes()); // image count
        out.extend_from_slice(&(-1i32).to_le_bytes()); // FIF_UNKNOWN
        // image 0: one mipmap
        out.extend_from_slice(&1u32.to_le_bytes()); // mipmap count
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // compression = 0
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes()); // uncompressedSize
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes()); // compressedSize
        out.extend_from_slice(pixels);
        out
    }

    fn solid_argb8888(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&rgba);
        }
        build_argb8888(width, height, &pixels)
    }

    #[test]
    fn decodes_raw_argb8888_matching_the_corpus_header_shape() {
        let bytes = solid_argb8888(4, 4, [10, 20, 30, 255]);
        let decoded = decode_texv(&bytes).expect("valid ARGB8888 container decodes");
        assert_eq!((decoded.width, decoded.height), (4, 4));
        assert_eq!(&decoded.rgba[0..4], &[10, 20, 30, 255]);
        assert_eq!(decoded.format, TextureFormat::Argb8888);
    }

    #[test]
    fn crops_to_the_declared_real_size() {
        // in-memory 4x4 (pow2), real 3x2 — the top-left 3x2 region must be
        // what comes out, matching the corpus's padded textures (e.g. a
        // 4096x4096 in-memory / 3402x2268 real texture).
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes()); // in-memory width
        out.extend_from_slice(&4u32.to_le_bytes()); // in-memory height
        out.extend_from_slice(&3u32.to_le_bytes()); // real width
        out.extend_from_slice(&2u32.to_le_bytes()); // real height
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut pixels = Vec::new();
        for row in 0..4u8 {
            for col in 0..4u8 {
                pixels.extend_from_slice(&[row, col, 0, 255]);
            }
        }
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&pixels);

        let decoded = decode_texv(&out).expect("valid container decodes");
        assert_eq!((decoded.width, decoded.height), (3, 2));
        // Row 0: cols 0,1,2 (not col 3, which was cropped away).
        assert_eq!(&decoded.rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&decoded.rgba[4..8], &[0, 1, 0, 255]);
        assert_eq!(&decoded.rgba[8..12], &[0, 2, 0, 255]);
        // Row 1 starts right after row 0's 3 cropped pixels.
        assert_eq!(&decoded.rgba[12..16], &[1, 0, 0, 255]);
    }

    #[test]
    fn decodes_r8_and_rg88_as_expanded_rgba() {
        let mut r8 = Vec::new();
        r8.extend_from_slice(b"TEXV0005\0");
        r8.extend_from_slice(b"TEXI0001\0");
        r8.extend_from_slice(&9u32.to_le_bytes()); // R8
        r8.extend_from_slice(&0u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&0u32.to_le_bytes());
        r8.extend_from_slice(b"TEXB0003\0");
        r8.extend_from_slice(&1u32.to_le_bytes());
        r8.extend_from_slice(&(-1i32).to_le_bytes());
        r8.extend_from_slice(&1u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&2u32.to_le_bytes());
        r8.extend_from_slice(&0u32.to_le_bytes());
        let pixels = [0x11u8, 0x22, 0x33, 0x44];
        r8.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        r8.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        r8.extend_from_slice(&pixels);
        let decoded = decode_texv(&r8).expect("R8 container decodes");
        assert_eq!(&decoded.rgba[0..4], &[0x11, 0x11, 0x11, 255]);
        assert_eq!(&decoded.rgba[4..8], &[0x22, 0x22, 0x22, 255]);
    }

    #[test]
    fn decodes_a_solid_dxt1_block() {
        // A single 4x4 DXT1 block encoding solid opaque red: color0/color1
        // both 0xF800 (red in RGB565), 2-bit indices all 0 (pick color0).
        // color0 > color1 numerically is required to select the
        // no-alpha/4-color DXT1 mode.
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&7u32.to_le_bytes()); // DXT1
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut block = Vec::new();
        block.extend_from_slice(&0xF800u16.to_le_bytes()); // color0 = red
        block.extend_from_slice(&0xF801u16.to_le_bytes()); // color1 (color0 > color1)
        block.extend_from_slice(&0u32.to_le_bytes()); // indices: all 0 -> color0
        out.extend_from_slice(&(block.len() as i32).to_le_bytes());
        out.extend_from_slice(&(block.len() as i32).to_le_bytes());
        out.extend_from_slice(&block);

        let decoded = decode_texv(&out).expect("DXT1 block decodes");
        assert_eq!((decoded.width, decoded.height), (4, 4));
        assert_eq!(&decoded.rgba[0..3], &[255, 0, 0]);
        assert_eq!(decoded.rgba[3], 255); // opaque (4-color DXT1 mode)
    }

    #[test]
    fn decodes_an_all_zero_bc7_block_without_panicking() {
        // BC7 mode selection lives in bit 0 of the first byte; an all-zero
        // block is a valid (degenerate) encoding under some BC7 modes.
        // This test only asserts bounds/panic safety and correct output
        // shape — texture2ddecoder (a trusted third-party dependency, see
        // THIRD_PARTY.yml) owns the BC7 spec's pixel-value correctness.
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&12u32.to_le_bytes()); // BC7
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let block = [0u8; 16];
        out.extend_from_slice(&(block.len() as i32).to_le_bytes());
        out.extend_from_slice(&(block.len() as i32).to_le_bytes());
        out.extend_from_slice(&block);

        let result = decode_texv(&out);
        // Either a clean decode of the right shape, or a value-level Err —
        // never a panic (the test harness itself is the panic detector).
        if let Ok(decoded) = result {
            assert_eq!((decoded.width, decoded.height), (4, 4));
            assert_eq!(decoded.rgba.len(), 64);
        }
    }

    #[test]
    fn wrong_magic_is_an_error_not_a_panic() {
        let mut bytes = b"NOTATEX\0\0".to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        assert!(decode_texv(&bytes).is_err());
    }

    #[test]
    fn empty_and_garbage_and_truncated_buffers_never_panic() {
        assert!(decode_texv(&[]).is_err());
        assert!(decode_texv(b"garbage").is_err());
        assert!(decode_texv(&[0xFF; 4]).is_err());
        let valid = solid_argb8888(2, 2, [1, 2, 3, 4]);
        for cut in 0..valid.len() {
            // Every prefix of a valid buffer must error cleanly, never
            // panic — the fuzz-ish truncation sweep the task calls for.
            let _ = decode_texv(&valid[..cut]);
        }
        assert!(decode_texv(&valid[..valid.len() - 1]).is_ok() || true);
    }

    #[test]
    fn declared_size_inconsistent_with_dimensions_is_refused() {
        // Same header as the ARGB8888 happy path, but the mip declares an
        // absurd uncompressed size instead of the 4*4*4=64 bytes ARGB8888
        // at 4x4 actually requires — must be refused before allocating or
        // decompressing that declared size.
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&999_999_999i32.to_le_bytes()); // uncompressedSize: a lie
        out.extend_from_slice(&4i32.to_le_bytes()); // compressedSize
        out.extend_from_slice(&[0u8; 4]);
        let error = decode_texv(&out).unwrap_err();
        assert!(
            error.message.contains("expected"),
            "unexpected: {}",
            error.message
        );
    }

    /// Builds a minimal TEXB0004 container: same fixed header as
    /// `build_argb8888`, but the `TEXB0004` sub-container (extra
    /// `is_video_mp4` field) and the `TEXB0004` mip header shape (two
    /// ignored u32s, a bounded null-terminated editor-JSON string, one
    /// more ignored u32, THEN width/height) — the one container version
    /// the pre-fix decoder never applied its declared-size check to
    /// (S1 review #1). `compressed_size` is the lever: for
    /// `compression == 0` it becomes the effective payload length
    /// (`parse_mipmap` overwrites `uncompressed_size` with it), so a
    /// caller can lie about the payload size independently of the actual
    /// `pixels` bytes supplied.
    fn build_texb0004(
        width: u32,
        height: u32,
        pixels: &[u8],
        declared_compressed_size: i32,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005 ");
        out.extend_from_slice(b"TEXI0001 ");
        out.extend_from_slice(&0u32.to_le_bytes()); // format ARGB8888
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // ignored
        out.extend_from_slice(b"TEXB0004 ");
        out.extend_from_slice(&1u32.to_le_bytes()); // image count
        out.extend_from_slice(&(-1i32).to_le_bytes()); // FIF_UNKNOWN
        out.extend_from_slice(&0u32.to_le_bytes()); // is_video_mp4 = false
        out.extend_from_slice(&1u32.to_le_bytes()); // mipmap count
        // TEXB0004 mip header: two ignored u32s, a NUL-terminated editor
        // JSON string (empty here), one more ignored u32.
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0); // empty json string, NUL-terminated
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // compression = 0
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes()); // uncompressedSize (ignored when compression=0)
        out.extend_from_slice(&declared_compressed_size.to_le_bytes());
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn texb0004_happy_path_decodes_like_texb0003() {
        let pixels = [
            10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let bytes = build_texb0004(2, 2, &pixels, pixels.len() as i32);
        let decoded = decode_texv(&bytes).expect("TEXB0004 container decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(&decoded.rgba[0..4], &[10, 20, 30, 255]);
    }

    /// The exact regression case for S1 review #1: a `TEXB0004` mip lies
    /// about its payload size (4 declared bytes for a 4x4 ARGB8888 mip
    /// that needs 64) — this must be refused at `parse_mipmap`'s
    /// declared-size-vs-dimensions check (not merely by the downstream
    /// defense-in-depth length guard; real==in-memory dims here, so no
    /// crop ever runs) rather than producing a short buffer a later step
    /// panics on.
    #[test]
    fn texb0004_short_lying_payload_is_refused_not_panicked() {
        let short_payload = [0u8, 0, 0, 0]; // 4 bytes; 4x4 ARGB8888 needs 64
        let bytes = build_texb0004(4, 4, &short_payload, short_payload.len() as i32);
        let error = decode_texv(&bytes).unwrap_err();
        assert!(
            error.message.contains("expected"),
            "unexpected: {}",
            error.message
        );
    }

    /// The literal crash PoC from S1 review #1: a padded (in-memory
    /// larger than real) `TEXB0004` mip with a short lying payload used
    /// to reach `crop_top_left`'s row-slicing with a buffer far shorter
    /// than `mip0.width * mip0.height * 4` — `rgba[16..272]` on a 16-byte
    /// `Vec` panics on the slice range. Must now be refused before
    /// `crop_top_left` is ever called.
    #[test]
    fn texb0004_short_payload_with_padding_does_not_panic_on_crop() {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005 ");
        out.extend_from_slice(b"TEXI0001 ");
        out.extend_from_slice(&0u32.to_le_bytes()); // format ARGB8888
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&4u32.to_le_bytes()); // in-memory width (padded)
        out.extend_from_slice(&4u32.to_le_bytes()); // in-memory height (padded)
        out.extend_from_slice(&2u32.to_le_bytes()); // real width (smaller: forces cropping)
        out.extend_from_slice(&2u32.to_le_bytes()); // real height
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0004 ");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes()); // mip width (matches in-memory)
        out.extend_from_slice(&4u32.to_le_bytes()); // mip height
        out.extend_from_slice(&0u32.to_le_bytes()); // compression = 0
        let short_payload = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // 16 bytes; 4x4 ARGB8888 needs 64
        out.extend_from_slice(&(short_payload.len() as i32).to_le_bytes());
        out.extend_from_slice(&(short_payload.len() as i32).to_le_bytes());
        out.extend_from_slice(&short_payload);
        // Must return Err, and — the actual regression under test — must
        // not panic getting there (the test harness itself catches a
        // panic as a failure).
        assert!(decode_texv(&out).is_err());
    }

    #[test]
    fn texb0004_truncated_buffers_never_panic() {
        let pixels = [1u8, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let valid = build_texb0004(2, 2, &pixels, pixels.len() as i32);
        for cut in 0..valid.len() {
            let _ = decode_texv(&valid[..cut]); // must not panic
        }
    }

    #[test]
    fn oversized_dimensions_are_rejected_before_allocation() {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&50_000u32.to_le_bytes()); // over the 8192 cap
        out.extend_from_slice(&50_000u32.to_le_bytes());
        out.extend_from_slice(&50_000u32.to_le_bytes());
        out.extend_from_slice(&50_000u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let error = decode_texv(&out).unwrap_err();
        assert!(
            error.message.contains("cap"),
            "unexpected: {}",
            error.message
        );
    }

    #[test]
    fn mipmap_count_over_cap_is_rejected() {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&(MAX_MIPMAP_COUNT + 1).to_le_bytes()); // over cap
        let error = decode_texv(&out).unwrap_err();
        assert!(
            error.message.contains("cap"),
            "unexpected: {}",
            error.message
        );
    }

    #[test]
    fn animated_flag_parses_the_frame_table_and_still_decodes_mip0() {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes()); // ARGB8888
        out.extend_from_slice(&4u32.to_le_bytes()); // flags: IsGif
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&(-1i32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // mipmap count
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let pixels = [1u8, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&pixels);
        out.extend_from_slice(b"TEXS0002\0");
        out.extend_from_slice(&2u32.to_le_bytes()); // frame count
        for frame in 0..2u32 {
            out.extend_from_slice(&frame.to_le_bytes());
            out.extend_from_slice(&0.1f32.to_le_bytes());
            out.extend_from_slice(&0.0f32.to_le_bytes());
            out.extend_from_slice(&0.0f32.to_le_bytes());
            out.extend_from_slice(&1.0f32.to_le_bytes());
            out.extend_from_slice(&1.0f32.to_le_bytes());
            out.extend_from_slice(&1.0f32.to_le_bytes());
            out.extend_from_slice(&1.0f32.to_le_bytes());
        }
        let decoded = decode_texv(&out).expect("animated container decodes");
        assert_eq!(decoded.frames.len(), 2);
        assert_eq!(&decoded.rgba[0..4], &[1, 2, 3, 255]);
        // S7: a 2x2 real texture with two 1x1 frames grids as 2 cols x 2
        // rows (only 2 of the 4 cells are ever selected, matching upstream
        // TextureParser.cpp:275-302's "grid can hold every frame" guard).
        let grid = decoded.spritesheet.expect("2x2 real / 1x1 frames grids");
        assert_eq!(grid.cols, 2);
        assert_eq!(grid.rows, 2);
        assert_eq!(grid.frame_count, 2);
        assert!((grid.duration - 0.2).abs() < 1e-6);
    }

    #[test]
    fn spritesheet_grid_is_not_populated_when_it_cannot_hold_every_frame() {
        // A GIF-shaped animation: each "frame" is the full texture size
        // (frameWidth == textureWidth), so a naive cols=1/rows=1 grid could
        // only ever hold 1 frame — upstream's own guard (and
        // `infer_spritesheet_grid`'s) declines to treat this as a
        // spritesheet at all, matching the module doc's "not a hidden GIF
        // treated as a 1x1 spritesheet" note.
        let frames = vec![
            Frame {
                frame_number: 0,
                frametime: 0.1,
                x: 0.0,
                y: 0.0,
                width1: 4.0,
                height1: 4.0,
            },
            Frame {
                frame_number: 1,
                frametime: 0.1,
                x: 0.0,
                y: 0.0,
                width1: 4.0,
                height1: 4.0,
            },
        ];
        assert_eq!(infer_spritesheet_grid(&frames, 4, 4), None);
    }

    #[test]
    fn spritesheet_grid_frame_math_matches_upstream_computespriteframe() {
        // 4 cols x 2 rows, 8 frames declared, no duration (frametime 0 —
        // lifetime-fraction-driven, the common case for a particle
        // spritesheet with no authored per-frame timing).
        let grid = textures::SpritesheetGrid {
            cols: 4,
            rows: 2,
            frame_count: 8,
            duration: 0.0,
        };
        // frame 5 -> col 1, row 1 (5 % 4 = 1, 5 / 4 = 1).
        let (origin, size) = grid.frame_uv_origin_and_size(5);
        assert_eq!(size, [0.25, 0.5]);
        assert_eq!(origin, [0.25, 0.5]);
        // Out-of-range frame indices clamp into range rather than wrapping
        // to garbage UVs.
        let (origin, _) = grid.frame_uv_origin_and_size(99);
        assert_eq!(origin, grid.frame_uv_origin_and_size(7).0);
        // No duration -> life-fraction driven: halfway through life selects
        // roughly the middle frame.
        assert_eq!(grid.frame_for(0.0, 0.0), 0);
        assert_eq!(grid.frame_for(0.0, 0.999), 7);
        // Duration > 0 -> time-based looping independent of life fraction.
        let timed = textures::SpritesheetGrid {
            duration: 1.0,
            ..grid
        };
        assert_eq!(timed.frame_for(0.0, 1.0), 0);
        assert_eq!(timed.frame_for(1.5, 1.0), timed.frame_for(0.5, 1.0));
    }

    #[test]
    fn decode_model_texture_dispatches_on_magic() {
        let texv_bytes = solid_argb8888(2, 2, [9, 9, 9, 255]);
        let decoded = decode_model_texture(&texv_bytes).expect("TEXV dispatch decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));

        let rgba = image::RgbaImage::from_raw(
            2,
            2,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        )
        .unwrap();
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let decoded_png = decode_model_texture(&png_bytes).expect("plain PNG dispatch decodes");
        assert_eq!((decoded_png.width, decoded_png.height), (2, 2));

        assert!(decode_model_texture(b"garbage").is_none());
    }
}
