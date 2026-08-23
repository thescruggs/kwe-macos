// SPDX-License-Identifier: GPL-3.0-or-later
//! Cheap, decode-free structural check of a TEXV0005 texture container's
//! fixed header (S1 adversarial-review fix: preflight/worker agreement on
//! model-texture drawability).
//!
//! `kwe-core` (preflight) cannot depend on `kwe-scene-renderer` (the
//! worker, which owns the real TEXV decoder, `texv.rs` — the dependency
//! already runs the other direction). Rather than let that boundary mean
//! preflight has NO opinion on whether a resolved `.tex` texture can
//! actually be decoded, this module re-parses the same fixed-size header
//! fields `texv::parse_header` reads (magic, format enum, container
//! version, dimensions, image count) — no mip chain, no LZ4, no BC decode
//! — and rejects the container shapes that would definitely fail to
//! decode: a corrupt/truncated header, a format this build's decoder does
//! not implement (when not FIF-tagged), or dimensions/image counts
//! outside the same bounds `texv.rs` enforces. This intentionally mirrors
//! `crates/kwe-scene-renderer/src/texv.rs`'s `parse_header` field layout;
//! keep the two in sync when the container format changes.
//!
//! What this does NOT catch: a corrupt LZ4 stream, a wrong per-mip
//! declared size deeper in the mip chain, or a truncated payload after
//! the header — those still surface only as a worker-side degraded layer
//! (`event=renderer.scene.model_texture_skip`), same as before this fix.
//! The header-level check closes the common, cheap-to-detect failure
//! modes (unsupported format, corrupt/absurd header) without duplicating
//! the decoder.

/// True when `bytes` carries the TEXV0005 magic — the point at which
/// `check_header` and (downstream, in the worker) `texv::decode_texv`
/// both apply; non-TEXV bytes (a plain image container) are not this
/// module's concern.
pub fn is_texv(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[..8] == b"TEXV0005"
}

/// Per-edge dimension cap; mirrors `kwe-scene-renderer`'s
/// `texv::MAX_TEXTURE_DIMENSION`.
const MAX_TEXTURE_DIMENSION: u32 = 8192;
/// Image-count cap; mirrors `texv::MAX_IMAGE_COUNT`.
const MAX_IMAGE_COUNT: u32 = 16;
/// Single-texture RGBA8 byte budget; mirrors `kwe-scene-renderer`'s
/// `textures::MAX_TOTAL_TEXTURE_BYTES` (256 MiB) — a single resolved
/// model texture cannot alone exceed the whole-scene texture budget the
/// worker's own `texture_budget_allows` enforces cumulatively.
pub const MAX_SINGLE_TEXTURE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4-byte slice")))
        .ok_or_else(|| "texture header truncated".to_string())
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32, String> {
    u32_at(bytes, offset).map(|value| value as i32)
}

fn magic9_at(bytes: &[u8], offset: usize) -> Result<&[u8], String> {
    bytes
        .get(offset..offset + 9)
        .ok_or_else(|| "texture header truncated".to_string())
}

/// `Ok(real_rgba8_bytes)` — the RGBA8 byte count the worker's decoded
/// texture would need (real width * real height * 4) — when the header
/// is well-formed and (for a non-FIF-tagged container) names a format
/// `texv.rs` implements pixel decode for; `Err(reason)` otherwise. Caller
/// must check `is_texv` first — this does not itself re-verify the
/// primary magic beyond reading it at its fixed offset.
pub fn check_header(bytes: &[u8]) -> Result<u64, String> {
    if magic9_at(bytes, 0)? != b"TEXV0005\0" {
        return Err("unexpected texture container magic".into());
    }
    if magic9_at(bytes, 9)? != b"TEXI0001\0" {
        return Err("unexpected texture sub-container magic".into());
    }
    let format = u32_at(bytes, 18)?;
    let _flags = u32_at(bytes, 22)?;
    let texture_width = u32_at(bytes, 26)?;
    let texture_height = u32_at(bytes, 30)?;
    let width = u32_at(bytes, 34)?;
    let height = u32_at(bytes, 38)?;
    // offset 42..46: ignored u32.
    for (label, dim) in [
        ("in-memory width", texture_width),
        ("in-memory height", texture_height),
        ("real width", width),
        ("real height", height),
    ] {
        if dim == 0 || dim > MAX_TEXTURE_DIMENSION {
            return Err(format!(
                "texture {label} {dim} is 0 or over the {MAX_TEXTURE_DIMENSION} cap"
            ));
        }
    }

    let container_magic = magic9_at(bytes, 46)?;
    let image_count = u32_at(bytes, 55)?;
    let fif = if container_magic == b"TEXB0004\0" || container_magic == b"TEXB0003\0" {
        Some(i32_at(bytes, 59)?) // TEXB0004's is_video_mp4 (offset 63) is unused here
    } else if container_magic == b"TEXB0002\0" || container_magic == b"TEXB0001\0" {
        None
    } else {
        return Err("unknown texture sub-container version".into());
    };
    let fif_active = fif.is_some_and(|value| value != -1);

    if image_count == 0 || image_count > MAX_IMAGE_COUNT {
        return Err(format!(
            "texture image count {image_count} is 0 or over the {MAX_IMAGE_COUNT} cap"
        ));
    }

    if !fif_active {
        // Formats `texv::TextureFormat::raw_bytes_per_pixel`/`block_bytes`
        // implement: ARGB8888(0), R8(9), RG88(8), DXT1(7), DXT3(6),
        // DXT5(4), BC7(12). Keep this list in sync with that match.
        const IMPLEMENTED: [u32; 7] = [0, 9, 8, 7, 6, 4, 12];
        if !IMPLEMENTED.contains(&format) {
            return Err(format!(
                "texture format {format} is not implemented by this build's decoder"
            ));
        }
    }

    Ok(u64::from(width) * u64::from(height) * 4)
}

/// Test-only builder for a structurally valid minimal TEXV0005 header
/// (`TEXB0003`, `FIF_UNKNOWN`, one image) — shared across this crate's
/// test modules (`scenemodel.rs`, `pkg.rs`, `preflight.rs`) so their model
/// resolution fixtures carry a header this check actually accepts,
/// instead of the pre-fix placeholder `b"TEXV0005fake"` bytes that this
/// very check now (correctly) rejects.
#[cfg(test)]
pub(crate) fn valid_minimal_texv(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"TEXV0005\0");
    out.extend_from_slice(b"TEXI0001\0");
    out.extend_from_slice(&0u32.to_le_bytes()); // format ARGB8888
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // ignored
    out.extend_from_slice(b"TEXB0003\0");
    out.extend_from_slice(&1u32.to_le_bytes()); // image count
    out.extend_from_slice(&(-1i32).to_le_bytes()); // FIF_UNKNOWN
    // A real decoder would keep reading the mip chain here; this header
    // check never does, so nothing further is required for `check_header`
    // to accept this buffer.
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_structurally_valid_header() {
        let bytes = valid_minimal_texv(4, 4);
        assert!(is_texv(&bytes));
        assert_eq!(check_header(&bytes), Ok(4 * 4 * 4));
    }

    #[test]
    fn rejects_an_unimplemented_format() {
        let mut bytes = valid_minimal_texv(4, 4);
        bytes[18..22].copy_from_slice(&2u32.to_le_bytes()); // RGB565, not implemented
        assert!(check_header(&bytes).is_err());
    }

    #[test]
    fn accepts_any_format_when_fif_tagged() {
        // FIF-tagged payloads decode through the image crate, not the
        // raw/BC path, so the format field's value doesn't gate them here.
        let mut bytes = valid_minimal_texv(4, 4);
        bytes[18..22].copy_from_slice(&2u32.to_le_bytes()); // RGB565 (irrelevant when FIF-tagged)
        let fif_offset = 46 + 9 + 4; // container magic (9), image_count (4), then fif
        bytes[fif_offset..fif_offset + 4].copy_from_slice(&13i32.to_le_bytes()); // FIF_PNG
        assert!(check_header(&bytes).is_ok());
    }

    #[test]
    fn rejects_oversized_dimensions() {
        let mut bytes = valid_minimal_texv(4, 4);
        bytes[34..38].copy_from_slice(&50_000u32.to_le_bytes()); // real width over the cap
        assert!(check_header(&bytes).is_err());
    }

    #[test]
    fn truncated_and_garbage_buffers_are_errors_not_panics() {
        assert!(check_header(&[]).is_err());
        assert!(check_header(b"garbage").is_err());
        let valid = valid_minimal_texv(4, 4);
        for cut in 0..valid.len() {
            let _ = check_header(&valid[..cut]); // must not panic
        }
        assert!(!is_texv(b"not a texv"));
        assert!(is_texv(b"TEXV0005whatever-else"));
    }

    #[test]
    fn wrong_sub_container_magic_is_rejected() {
        let mut bytes = valid_minimal_texv(4, 4);
        bytes[46..55].copy_from_slice(b"NOTAREAL\0");
        assert!(check_header(&bytes).is_err());
    }
}
