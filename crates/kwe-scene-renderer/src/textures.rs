// SPDX-License-Identifier: GPL-3.0-or-later
// Bounded image decoding for 2D layer textures (M3c).
//
// Mirrors the web renderer's decode limits (kwe-web-renderer
// decode_screencast): every texture is decoded to RGBA8 — the identity
// channel order for R8G8B8A8_UNORM, per the M3a readback lesson — with the
// dimension, allocation, and pixel-count caps enforced by the `image` crate
// Limits. A decode failure skips the layer with a bounded diagnostic; it is
// never a backend rejection.

use image::ImageReader;

/// One decoded layer texture, ready for a R8G8B8A8_UNORM upload. RGBA8,
/// straight alpha (the GPU's src-over blend combines it with the layer
/// alpha).
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedTexture {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The texture's animation grid (S7), when its container carried a
    /// `TEXS*` frame table AND that table's per-frame size evenly tiles the
    /// texture (see `texv::infer_spritesheet_grid`) — `None` for a plain
    /// (non-`.tex`) image or a `.tex` with no usable frame table. `decode_texture`
    /// (this module — plain png/jpeg/webp) never produces one; only
    /// `texv::decode_model_texture` does.
    pub spritesheet: Option<SpritesheetGrid>,
}

/// A texture's spritesheet animation grid: `cols`×`rows` equal-size frames
/// packed left-to-right, top-to-bottom (upstream `Texture::spritesheetCols/
/// Rows/Frames/Duration`, `Data/Assets/Texture.h`). `frame_count` is the
/// number of frames actually declared by the `TEXS*` table — it can be less
/// than `cols * rows` (a partially-filled grid; the remaining cells are
/// never selected). `duration` is the sum of every frame's `frametime`
/// (seconds for one full loop); 0.0 when the table declared no frametime
/// values, in which case a consumer should fall back to a lifetime-fraction
/// based frame pick instead of a time-based one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpritesheetGrid {
    pub cols: u32,
    pub rows: u32,
    pub frame_count: u32,
    pub duration: f32,
}

impl SpritesheetGrid {
    /// The 0..=1 UV origin (top-left) and per-frame UV size for `frame`
    /// (clamped into `0..frame_count`) — upstream `CParticle.cpp`'s
    /// `ComputeSpriteFrame`, re-derived here for CPU-side use: row-major,
    /// `col = frame % cols`, `row = frame / cols`, frame box
    /// `[col/cols, row/rows] .. [+1/cols, +1/rows]`.
    pub fn frame_uv_origin_and_size(&self, frame: u32) -> ([f32; 2], [f32; 2]) {
        let frame = frame.min(self.frame_count.saturating_sub(1));
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        let col = frame % cols;
        let row = frame / cols;
        let size = [1.0 / cols as f32, 1.0 / rows as f32];
        let origin = [col as f32 * size[0], row as f32 * size[1]];
        (origin, size)
    }

    /// The frame index to draw at simulation time `elapsed_secs`/lifetime
    /// fraction `life_fraction` (0..=1): time-based looping when `duration`
    /// is known (>0, independent of the particle/layer's own lifetime —
    /// upstream's default when the texture declares frame timings),
    /// otherwise a lifetime-fraction-based pick spanning the whole grid
    /// once per life (upstream's fallback when no duration is known).
    pub fn frame_for(&self, elapsed_secs: f32, life_fraction: f32) -> u32 {
        if self.frame_count == 0 {
            return 0;
        }
        let unit = if self.duration > 0.0 {
            (elapsed_secs.max(0.0) / self.duration).fract()
        } else {
            life_fraction.clamp(0.0, 1.0)
        };
        let raw = (unit * self.frame_count as f32).floor();
        (raw as u32).min(self.frame_count - 1)
    }
}

/// Maximum texture edge in pixels (mirrors the web worker's Limits).
pub const MAX_TEXTURE_DIMENSION: u32 = 8192;
/// Maximum decoded pixel count — 4096² (mirrors the web worker).
pub const MAX_TEXTURE_PIXELS: u64 = 16_777_216;
/// Maximum decoder allocation in bytes (mirrors the web worker).
pub const MAX_TEXTURE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum raw source bytes for one image file / package entry read.
pub const MAX_TEXTURE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Total decoded RGBA budget across all layers (256 MiB), so a scene full
/// of 8192×8192 textures cannot push the worker past a bounded footprint.
pub const MAX_TOTAL_TEXTURE_BYTES: u64 = 256 * 1024 * 1024;

/// Decode one bounded texture to RGBA8. `None` when the bytes are not a
/// supported image (png/jpeg/webp — the brief's formats; the corpus's
/// TEXV0005 textures are planned with M3h), exceed any cap, or decode to
/// zero pixels. The caps are enforced inside the decoder (Limits), so a
/// hostile image can neither allocate past `MAX_TEXTURE_ALLOC_BYTES` nor
/// decode past `MAX_TEXTURE_PIXELS`.
pub fn decode_texture(bytes: &[u8]) -> Option<DecodedTexture> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_TEXTURE_DIMENSION);
    limits.max_image_height = Some(MAX_TEXTURE_DIMENSION);
    limits.max_alloc = Some(MAX_TEXTURE_ALLOC_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_TEXTURE_PIXELS {
        return None;
    }
    Some(DecodedTexture {
        rgba: rgba.into_raw(),
        width,
        height,
        spritesheet: None,
    })
}

/// Does a w×h texture still fit the total decoded RGBA budget, given the
/// bytes already used? Overflow-safe: an absurd dimension cannot wrap the
/// arithmetic into a pass.
pub fn texture_budget_allows(used_bytes: u64, width: u32, height: u32) -> bool {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| used_bytes.checked_add(bytes))
        .is_some_and(|total| total <= MAX_TOTAL_TEXTURE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A valid 2×2 RGBA png, via the image crate's encoder (the same crate
    /// the decoder uses — the fixtures and the encoder agree by
    /// construction).
    fn tiny_png() -> Vec<u8> {
        let rgba = image::RgbaImage::from_raw(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        )
        .unwrap();
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// The minimal png header with an arbitrary IHDR: signature + IHDR
    /// chunk (width, height, bit depth 8, color type 6 RGBA). The CRC is
    /// computed over the chunk data so the decoder reads the dimensions.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        const CRC_TABLE: [u32; 256] = {
            let mut table = [0_u32; 256];
            let mut n = 0;
            while n < 256 {
                let mut c = n as u32;
                let mut k = 0;
                while k < 8 {
                    c = if c & 1 != 0 {
                        0xEDB88320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                    k += 1;
                }
                table[n] = c;
                n += 1;
            }
            table
        };
        fn crc32(data: &[u8]) -> u32 {
            let mut c = !0_u32;
            for &byte in data {
                c = CRC_TABLE[((c ^ u32::from(byte)) & 0xFF) as usize] ^ (c >> 8);
            }
            !c
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.push(8); // bit depth
        ihdr.push(6); // color type RGBA
        ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        let mut out = Vec::new();
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        out.extend_from_slice(&13_u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&ihdr);
        let mut crc_input = Vec::new();
        crc_input.extend_from_slice(b"IHDR");
        crc_input.extend_from_slice(&ihdr);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        out
    }

    #[test]
    fn decodes_png_to_rgba8() {
        let png = tiny_png();
        let texture = decode_texture(&png).expect("tiny png decodes");
        assert_eq!((texture.width, texture.height), (2, 2));
        // Identity channel order: R, G, B, A straight from the source.
        assert_eq!(&texture.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&texture.rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn garbage_and_empty_bytes_are_none() {
        assert!(decode_texture(b"").is_none());
        assert!(decode_texture(b"not an image at all").is_none());
        assert!(decode_texture(b"\x89PNG\x0d\x0a\x1a\x0a truncated").is_none());
    }

    #[test]
    fn oversized_dimensions_rejected() {
        // 9000 × 9000 exceeds both the dimension cap and the allocation
        // cap; the decoder must refuse from the header.
        let header = png_header(9000, 9000);
        assert!(decode_texture(&header).is_none());
    }

    #[test]
    fn dimension_cap_boundary_honored() {
        // 8192 × 8192 passes the dimension check but is 4× the pixel cap
        // and 4× the allocation cap — refused either way. The pixel cap
        // lives post-decode, but the alloc cap bounds the decode itself.
        let header = png_header(8192, 8192);
        assert!(decode_texture(&header).is_none());
    }

    #[test]
    fn total_texture_budget_is_exact() {
        assert!(texture_budget_allows(0, 10, 10));
        assert!(texture_budget_allows(MAX_TOTAL_TEXTURE_BYTES - 400, 10, 10));
        assert!(!texture_budget_allows(MAX_TOTAL_TEXTURE_BYTES, 1, 1));
        assert!(!texture_budget_allows(
            MAX_TOTAL_TEXTURE_BYTES - 399,
            10,
            10
        ));
        // An absurd dimension cannot wrap the arithmetic into a pass.
        assert!(!texture_budget_allows(0, u32::MAX, u32::MAX));
    }
}
