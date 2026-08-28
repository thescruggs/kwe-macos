// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-1a: the framed report-FD wire codec (`docs/REPORT_PROTOCOL_V1.md`) and
//! the `scene-inspection-v1` record schema/digest validator.
//!
//! This crate only defines and codecs bytes. The report FD itself (a pipe
//! the daemon creates, dup2'd into the child at a known fd and named via
//! `--report-fd <n>`) is SR-1b's wiring; duplicate/malformed/missing/late
//! frame POLICY is SR-1c's daemon-side decision. `FrameReader` surfaces
//! frames in the arrival order and nothing more — see
//! `docs/REPORT_PROTOCOL_V1.md`'s "codec vs. policy" split.
//!
//! ## Deviation from the task text
//!
//! The originating task described this crate's error types as
//! "thiserror-free, manual Display... mirror kwe-frame-protocol/
//! kwe-input-protocol style" — but both of those crates actually use
//! `thiserror` (`#[derive(Error)]`, `#[error("...")]` per variant,
//! `#[from]` for I/O), not manual `Display` impls. `thiserror` is already a
//! workspace dependency used by both (not a new external dependency), so
//! this crate uses it too: that IS the style being mirrored.

use std::io::{self, Read, Write};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Frame magic: `b"KWR1"`.
pub const MAGIC: [u8; 4] = *b"KWR1";
/// Fixed header size: magic(4) + kind(1) + flags(1) + reserved(2) +
/// payload_len(4).
pub const HEADER_BYTES: usize = 12;
/// Per-frame payload cap.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Per-stream frame count cap (reader-enforced).
pub const MAX_FRAMES_PER_STREAM: usize = 16;
/// Per-stream total payload byte cap (reader-enforced), summed across every
/// frame including skipped `Unknown` ones.
pub const MAX_TOTAL_PAYLOAD_BYTES: usize = 1024 * 1024;

/// `scene-inspection-v1`'s own `"schema"` field value.
pub const SCENE_INSPECTION_SCHEMA: &str = "scene-inspection-v1";
/// `scene-inspection-v1`'s own `"capabilities_schema"` field value —
/// names which frozen `docs/SCENE_CAPABILITIES.md` taxonomy version the
/// record's capability IDs (`scene.layer.image`, ...) are drawn from.
pub const SCENE_CAPABILITIES_SCHEMA: &str = "scene-capabilities-v1";

/// One frame's kind (header byte 4). `Unknown` carries the raw byte so a
/// reader built against this version never has to reject a stream just
/// because a later version added a kind it does not recognize yet
/// (additive evolution, docs/REPORT_PROTOCOL_V1.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Kind 1: a `scene-inspection-v1` JSON record.
    SceneInspectionV1,
    /// Kind 2: a `scene-render-report-v1` JSON record. RESERVED — its
    /// producer arrives with the render-report slices; this codec carries
    /// the kind and payload opaquely until then.
    SceneRenderReportV1,
    /// Any other kind byte.
    Unknown(u8),
}

impl FrameKind {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::SceneInspectionV1,
            2 => Self::SceneRenderReportV1,
            other => Self::Unknown(other),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::SceneInspectionV1 => 1,
            Self::SceneRenderReportV1 => 2,
            Self::Unknown(value) => value,
        }
    }
}

/// One decoded frame: its kind and payload bytes, exactly as read off the
/// stream. `FrameReader` never interprets the payload (that is
/// `validate_inspection` for kind 1, and SR-1c's daemon policy generally);
/// an `Unknown` frame's payload is still handed back here rather than
/// discarded, so a caller built against a newer wire version than this
/// codec's `FrameKind` table is never retroactively broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

/// Wire/stream-level codec errors. Mirrors the `#[derive(Error)]` /
/// `#[error("...")]` style of `kwe-frame-protocol`/`kwe-input-protocol`
/// (see the module deviation note above).
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame magic is invalid")]
    BadMagic,
    #[error("frame flags must be 0 in v1, got {0}")]
    BadFlags(u8),
    #[error("frame reserved bytes must be 0, got {0}")]
    BadReserved(u16),
    #[error("frame payload is {len} bytes; maximum is {MAX_PAYLOAD_BYTES}")]
    PayloadOversize { len: u32 },
    #[error("frame header truncated")]
    TruncatedHeader,
    #[error("frame payload truncated")]
    TruncatedPayload,
    #[error("stream exceeded {MAX_FRAMES_PER_STREAM} frames")]
    FrameCountExceeded,
    #[error("stream exceeded {MAX_TOTAL_PAYLOAD_BYTES} total payload bytes")]
    TotalBytesExceeded,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Writes one frame: a 12-byte header (magic, kind, flags=0, reserved=0,
/// `payload.len()` as a little-endian u32) then `payload`. Refuses a
/// payload over `MAX_PAYLOAD_BYTES` and an `Unknown` kind (this codec only
/// ever WRITES a kind it knows the shape of; an unrecognized kind can only
/// ever be something this reader receives from a newer writer, never
/// something it produces itself).
pub fn write_frame(writer: &mut impl Write, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    if matches!(kind, FrameKind::Unknown(_)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot write an Unknown frame kind",
        ));
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "payload is {} bytes; maximum is {MAX_PAYLOAD_BYTES}",
                payload.len()
            ),
        ));
    }
    let mut header = [0_u8; HEADER_BYTES];
    header[0..4].copy_from_slice(&MAGIC);
    header[4] = kind.as_u8();
    header[5] = 0; // flags
    header[6..8].copy_from_slice(&0_u16.to_le_bytes()); // reserved
    // The oversize check above already bounds payload.len() to
    // MAX_PAYLOAD_BYTES (65536), so this cast never truncates.
    header[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    Ok(())
}

/// Bounded frame reader over one report stream. Enforces the stream caps
/// (`MAX_FRAMES_PER_STREAM`, `MAX_TOTAL_PAYLOAD_BYTES`) across the whole
/// `FrameReader` lifetime — construct a fresh one per stream/generation.
pub struct FrameReader<R: Read> {
    reader: R,
    frames_read: usize,
    total_payload_bytes: usize,
}

impl<R: Read> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            frames_read: 0,
            total_payload_bytes: 0,
        }
    }

    /// Reads and returns the next frame, or `Ok(None)` on a clean EOF
    /// exactly at a frame boundary (the stream ended after a whole number
    /// of frames — the normal "the child closed its write end" case).
    /// Any other EOF (mid-header or mid-payload) is a typed truncation
    /// error, never a silent `None`.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let mut header = [0_u8; HEADER_BYTES];
        let mut filled = 0_usize;
        while filled < HEADER_BYTES {
            let read = self.reader.read(&mut header[filled..])?;
            if read == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(FrameError::TruncatedHeader);
            }
            filled += read;
        }

        if header[0..4] != MAGIC {
            return Err(FrameError::BadMagic);
        }
        let kind_byte = header[4];
        let flags = header[5];
        if flags != 0 {
            return Err(FrameError::BadFlags(flags));
        }
        let reserved = u16::from_le_bytes([header[6], header[7]]);
        if reserved != 0 {
            return Err(FrameError::BadReserved(reserved));
        }
        let payload_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        if payload_len as usize > MAX_PAYLOAD_BYTES {
            return Err(FrameError::PayloadOversize { len: payload_len });
        }

        // Every well-formed header counts against the stream caps, even an
        // Unknown-kind one that gets skipped semantically below — a
        // flood of tiny unknown frames must not be a way around the caps.
        // The byte cap is checked before the frame-count cap: with this
        // protocol's exact constants (16 frames x 64 KiB == 1 MiB exactly),
        // a stream cannot go OVER the byte cap without ALSO being at frame
        // 17 or later, so checking bytes first is what makes
        // TotalBytesExceeded reachable at all as a distinct outcome from
        // FrameCountExceeded — a small-payload stream past 16 frames still
        // hits FrameCountExceeded here, since its byte total stays low.
        self.total_payload_bytes += payload_len as usize;
        if self.total_payload_bytes > MAX_TOTAL_PAYLOAD_BYTES {
            return Err(FrameError::TotalBytesExceeded);
        }
        self.frames_read += 1;
        if self.frames_read > MAX_FRAMES_PER_STREAM {
            return Err(FrameError::FrameCountExceeded);
        }

        let mut payload = vec![0_u8; payload_len as usize];
        self.reader.read_exact(&mut payload).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                FrameError::TruncatedPayload
            } else {
                FrameError::Io(error)
            }
        })?;

        Ok(Some(Frame {
            kind: FrameKind::from_u8(kind_byte),
            payload,
        }))
    }
}

/// `validate_inspection` failures. A missing field and a present-but-wrong-
/// type field are always distinguished (`MissingField` vs `WrongType`).
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("payload is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("payload is not a JSON object")]
    NotAnObject,
    #[error("\"schema\" does not match {SCENE_INSPECTION_SCHEMA:?}")]
    WrongSchema,
    #[error("\"capabilities_schema\" does not match {SCENE_CAPABILITIES_SCHEMA:?}")]
    WrongCapabilitiesSchema,
    #[error("missing required field {0:?}")]
    MissingField(&'static str),
    #[error("field {0:?} has the wrong JSON type")]
    WrongType(&'static str),
    #[error("digest does not match the record's own recomputed content hash")]
    DigestMismatch,
}

/// Validates one `scene-inspection-v1` payload: schema tags, every
/// required top-level (and one level nested) field's presence and JSON
/// type, and the digest.
///
/// The digest rule is byte-for-byte the same canonicalization
/// `kwe-scene-inspector`'s `build_record` uses (crates/kwe-scene-inspector/
/// src/main.rs): set `"digest"` to `""`, serialize the whole record with
/// `serde_json::to_vec`, SHA-256 the bytes, hex-encode. This is
/// deterministic ONLY because `serde_json::Value`'s object map is a
/// `BTreeMap` here (keys always serialize in sorted order) — this crate,
/// and every crate in this workspace, MUST keep the `preserve_order`
/// serde_json feature off; enabling it anywhere in the dependency graph
/// would make key order (and therefore the digest) insertion-order
/// dependent instead of a pure function of the record's content.
///
/// This function does not itself enforce `MAX_PAYLOAD_BYTES` — it assumes
/// the caller already read `payload` through a capped path (`FrameReader`,
/// or SR-1c's daemon-side frame cap). `serde_json::from_slice` handles an
/// oversized buffer safely regardless (parses or errors, never panics), so
/// calling this directly with an uncapped buffer degrades gracefully
/// rather than being unsound.
pub fn validate_inspection(payload: &[u8]) -> Result<Value, ValidationError> {
    let value: Value = serde_json::from_slice(payload)?;
    let object = value.as_object().ok_or(ValidationError::NotAnObject)?;

    let schema = require_str(object, "schema", "schema")?;
    if schema != SCENE_INSPECTION_SCHEMA {
        return Err(ValidationError::WrongSchema);
    }
    let capabilities_schema = require_str(object, "capabilities_schema", "capabilities_schema")?;
    if capabilities_schema != SCENE_CAPABILITIES_SCHEMA {
        return Err(ValidationError::WrongCapabilitiesSchema);
    }

    let content = require_object(object, "content", "content")?;
    require_str(content, "hash", "content.hash")?;
    require_number(content, "source_bytes", "content.source_bytes")?;
    require_str(content, "kind", "content.kind")?;

    let inspector = require_object(object, "inspector", "inspector")?;
    require_str(inspector, "build", "inspector.build")?;
    require_number(inspector, "abi", "inspector.abi")?;

    require_str(object, "outcome", "outcome")?;
    require_str(object, "reason", "reason")?;
    require_array(object, "required", "required")?;
    require_array(object, "detected", "detected")?;

    let unknown = require_object(object, "unknown", "unknown")?;
    require_number(unknown, "keys", "unknown.keys")?;
    require_number(unknown, "types", "unknown.types")?;
    require_number(unknown, "objects", "unknown.objects")?;
    require_array(unknown, "samples", "unknown.samples")?;
    require_bool(unknown, "truncated", "unknown.truncated")?;

    let bounds = require_object(object, "bounds", "bounds")?;
    require_number(bounds, "wall_ms", "bounds.wall_ms")?;
    require_array(bounds, "limits_hit", "bounds.limits_hit")?;

    require_null_or_object(object, "backend", "backend")?;
    let digest = require_str(object, "digest", "digest")?.to_string();

    let mut for_digest = value.clone();
    for_digest["digest"] = Value::String(String::new());
    let serialized = serde_json::to_vec(&for_digest).unwrap_or_default();
    let recomputed = hex::encode(Sha256::digest(&serialized));
    if recomputed != digest {
        return Err(ValidationError::DigestMismatch);
    }

    Ok(value)
}

fn require_str<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<&'a str, ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

fn require_object<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<&'a Map<String, Value>, ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

fn require_array<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<&'a Vec<Value>, ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::Array(value)) => Ok(value),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

fn require_number(
    map: &Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<(), ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::Number(_)) => Ok(()),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

fn require_bool(
    map: &Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<(), ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

fn require_null_or_object(
    map: &Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<(), ValidationError> {
    match map.get(key) {
        None => Err(ValidationError::MissingField(path)),
        Some(Value::Null) | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err(ValidationError::WrongType(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    // -----------------------------------------------------------------
    // Round-trip
    // -----------------------------------------------------------------

    #[test]
    fn round_trips_one_to_three_frames_of_both_known_kinds() {
        for count in 1..=3 {
            let mut buffer = Vec::new();
            let payloads: Vec<Vec<u8>> = (0..count)
                .map(|index| format!("payload-{index}").into_bytes())
                .collect();
            let kinds = [FrameKind::SceneInspectionV1, FrameKind::SceneRenderReportV1];
            for (index, payload) in payloads.iter().enumerate() {
                let kind = kinds[index % kinds.len()];
                write_frame(&mut buffer, kind, payload).unwrap();
            }

            let mut reader = FrameReader::new(Cursor::new(buffer));
            for (index, payload) in payloads.iter().enumerate() {
                let frame = reader.next_frame().unwrap().expect("frame present");
                assert_eq!(frame.kind, kinds[index % kinds.len()]);
                assert_eq!(&frame.payload, payload);
            }
            assert!(
                reader.next_frame().unwrap().is_none(),
                "clean EOF at boundary"
            );
        }
    }

    // -----------------------------------------------------------------
    // Unknown kind
    // -----------------------------------------------------------------

    #[test]
    fn writer_refuses_unknown_kind() {
        let mut buffer = Vec::new();
        let error = write_frame(&mut buffer, FrameKind::Unknown(99), b"x").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn reader_yields_unknown_kind_and_continues() {
        let mut buffer = Vec::new();
        write_hand_crafted_frame(&mut buffer, 99, b"future-payload");
        write_frame(&mut buffer, FrameKind::SceneInspectionV1, b"{}").unwrap();

        let mut reader = FrameReader::new(Cursor::new(buffer));
        let first = reader.next_frame().unwrap().unwrap();
        assert_eq!(first.kind, FrameKind::Unknown(99));
        assert_eq!(first.payload, b"future-payload");

        let second = reader.next_frame().unwrap().unwrap();
        assert_eq!(second.kind, FrameKind::SceneInspectionV1);
        assert_eq!(second.payload, b"{}");

        assert!(reader.next_frame().unwrap().is_none());
    }

    fn write_hand_crafted_frame(buffer: &mut Vec<u8>, kind: u8, payload: &[u8]) {
        buffer.extend_from_slice(&MAGIC);
        buffer.push(kind);
        buffer.push(0); // flags
        buffer.extend_from_slice(&0_u16.to_le_bytes()); // reserved
        buffer.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buffer.extend_from_slice(payload);
    }

    // -----------------------------------------------------------------
    // Stream caps: limit-1 / limit / limit+1
    // -----------------------------------------------------------------

    #[test]
    fn frame_count_cap_is_enforced_at_the_boundary() {
        for (count, expect_ok) in [(15, true), (16, true), (17, false)] {
            let mut buffer = Vec::new();
            for _ in 0..count {
                write_frame(&mut buffer, FrameKind::SceneInspectionV1, b"x").unwrap();
            }
            let mut reader = FrameReader::new(Cursor::new(buffer));
            let mut read = 0;
            let mut result = Ok(());
            loop {
                match reader.next_frame() {
                    Ok(Some(_)) => read += 1,
                    Ok(None) => break,
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
            if expect_ok {
                assert!(result.is_ok(), "count={count}: {result:?}");
                assert_eq!(read, count, "count={count}");
            } else {
                assert!(
                    matches!(result, Err(FrameError::FrameCountExceeded)),
                    "count={count}: {result:?}"
                );
                assert_eq!(read, MAX_FRAMES_PER_STREAM, "count={count}");
            }
        }
    }

    #[test]
    fn total_payload_cap_is_enforced_at_the_boundary() {
        // One frame each near the 1 MiB total cap; MAX_PAYLOAD_BYTES (64
        // KiB) bounds any single frame, so several frames make up the
        // total in each case.
        let per_frame = MAX_PAYLOAD_BYTES;
        let frames_needed = MAX_TOTAL_PAYLOAD_BYTES / per_frame; // 16, == MAX_FRAMES_PER_STREAM
        assert_eq!(
            frames_needed, MAX_FRAMES_PER_STREAM,
            "test assumption: the byte cap and the frame-count cap bind at exactly the same frame count for this payload size"
        );

        // Exactly at the cap: MAX_FRAMES_PER_STREAM frames of exactly
        // MAX_PAYLOAD_BYTES each sum to exactly MAX_TOTAL_PAYLOAD_BYTES.
        let mut at_cap = Vec::new();
        for _ in 0..frames_needed {
            write_frame(
                &mut at_cap,
                FrameKind::SceneInspectionV1,
                &vec![0_u8; per_frame],
            )
            .unwrap();
        }
        let mut reader = FrameReader::new(Cursor::new(at_cap));
        let mut read = 0;
        while reader.next_frame().unwrap().is_some() {
            read += 1;
        }
        assert_eq!(read, frames_needed);

        // Just under: one byte less in the last frame's payload.
        let mut under_cap = Vec::new();
        for index in 0..frames_needed {
            let size = if index == frames_needed - 1 {
                per_frame - 1
            } else {
                per_frame
            };
            write_frame(
                &mut under_cap,
                FrameKind::SceneInspectionV1,
                &vec![0_u8; size],
            )
            .unwrap();
        }
        let mut reader = FrameReader::new(Cursor::new(under_cap));
        let mut read = 0;
        while reader.next_frame().unwrap().is_some() {
            read += 1;
        }
        assert_eq!(read, frames_needed);

        // Over: the exactly-at-cap stream (frames_needed x per_frame ==
        // MAX_TOTAL_PAYLOAD_BYTES) plus one more 1-byte frame pushes the
        // total to MAX_TOTAL_PAYLOAD_BYTES + 1 -- one byte over. This is
        // also the stream's (frames_needed + 1)th frame, which would
        // independently trip FrameCountExceeded once frames_needed ==
        // MAX_FRAMES_PER_STREAM (it does, for these constants: 1 MiB / 64
        // KiB == 16), so next_frame() checks the byte cap BEFORE the
        // frame-count cap specifically so this reports TotalBytesExceeded,
        // proving it is independently reachable rather than always shadowed
        // by the frame-count cap.
        let mut over_cap = Vec::new();
        for _ in 0..frames_needed {
            write_frame(
                &mut over_cap,
                FrameKind::SceneInspectionV1,
                &vec![0_u8; per_frame],
            )
            .unwrap();
        }
        write_frame(&mut over_cap, FrameKind::SceneInspectionV1, b"x").unwrap();
        let mut reader = FrameReader::new(Cursor::new(over_cap));
        let mut result = Ok(());
        loop {
            match reader.next_frame() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        assert!(
            matches!(result, Err(FrameError::TotalBytesExceeded)),
            "{result:?}"
        );
    }

    #[test]
    fn payload_len_cap_is_enforced_at_the_boundary() {
        // 65535 (ok): a real write_frame + read round trip.
        let mut ok_buffer = Vec::new();
        write_frame(
            &mut ok_buffer,
            FrameKind::SceneInspectionV1,
            &vec![0_u8; MAX_PAYLOAD_BYTES - 1],
        )
        .unwrap();
        let mut reader = FrameReader::new(Cursor::new(ok_buffer));
        assert!(reader.next_frame().unwrap().is_some());

        // 65536 (ok, exactly at the cap).
        let mut at_cap = Vec::new();
        write_frame(
            &mut at_cap,
            FrameKind::SceneInspectionV1,
            &vec![0_u8; MAX_PAYLOAD_BYTES],
        )
        .unwrap();
        let mut reader = FrameReader::new(Cursor::new(at_cap));
        assert!(reader.next_frame().unwrap().is_some());

        // 65537: write_frame itself refuses this, so hand-craft a header
        // claiming an oversize payload_len (the reader must reject it
        // before ever trying to read that many payload bytes).
        let mut over = Vec::new();
        over.extend_from_slice(&MAGIC);
        over.push(1);
        over.push(0);
        over.extend_from_slice(&0_u16.to_le_bytes());
        over.extend_from_slice(&((MAX_PAYLOAD_BYTES + 1) as u32).to_le_bytes());
        let mut reader = FrameReader::new(Cursor::new(over));
        assert!(matches!(
            reader.next_frame(),
            Err(FrameError::PayloadOversize {
                len
            }) if len as usize == MAX_PAYLOAD_BYTES + 1
        ));

        assert!(
            write_frame(
                &mut Vec::new(),
                FrameKind::SceneInspectionV1,
                &vec![0_u8; MAX_PAYLOAD_BYTES + 1]
            )
            .is_err(),
            "write_frame must itself refuse an oversize payload"
        );
    }

    // -----------------------------------------------------------------
    // Corruption
    // -----------------------------------------------------------------

    #[test]
    fn bad_magic_is_rejected() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"XXXX");
        buffer.push(1);
        buffer.push(0);
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&0_u32.to_le_bytes());
        let mut reader = FrameReader::new(Cursor::new(buffer));
        assert!(matches!(reader.next_frame(), Err(FrameError::BadMagic)));
    }

    #[test]
    fn nonzero_flags_is_rejected() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&MAGIC);
        buffer.push(1);
        buffer.push(1); // flags
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&0_u32.to_le_bytes());
        let mut reader = FrameReader::new(Cursor::new(buffer));
        assert!(matches!(reader.next_frame(), Err(FrameError::BadFlags(1))));
    }

    #[test]
    fn nonzero_reserved_is_rejected() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&MAGIC);
        buffer.push(1);
        buffer.push(0);
        buffer.extend_from_slice(&7_u16.to_le_bytes()); // reserved
        buffer.extend_from_slice(&0_u32.to_le_bytes());
        let mut reader = FrameReader::new(Cursor::new(buffer));
        assert!(matches!(
            reader.next_frame(),
            Err(FrameError::BadReserved(7))
        ));
    }

    #[test]
    fn truncated_header_is_rejected_at_every_prefix_length() {
        let mut full = Vec::new();
        full.extend_from_slice(&MAGIC);
        full.push(1);
        full.push(0);
        full.extend_from_slice(&0_u16.to_le_bytes());
        full.extend_from_slice(&4_u32.to_le_bytes());
        for prefix_len in 1..HEADER_BYTES {
            let mut reader = FrameReader::new(Cursor::new(full[..prefix_len].to_vec()));
            assert!(
                matches!(reader.next_frame(), Err(FrameError::TruncatedHeader)),
                "prefix_len={prefix_len}"
            );
        }
        // A zero-byte prefix is a clean EOF, not a truncation.
        let mut reader = FrameReader::new(Cursor::new(Vec::<u8>::new()));
        assert!(reader.next_frame().unwrap().is_none());
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, FrameKind::SceneInspectionV1, b"hello").unwrap();
        buffer.truncate(buffer.len() - 2); // drop the last 2 payload bytes
        let mut reader = FrameReader::new(Cursor::new(buffer));
        assert!(matches!(
            reader.next_frame(),
            Err(FrameError::TruncatedPayload)
        ));
    }

    // -----------------------------------------------------------------
    // Random bytes: never panics
    // -----------------------------------------------------------------

    /// A tiny deterministic xorshift32 PRNG — no fuzz dependency, fixed
    /// seeds so these fixtures are reproducible.
    fn xorshift32(seed: u32) -> impl FnMut() -> u32 {
        let mut state = seed.max(1);
        move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        for seed in [1_u32, 12345, 0xdead_beef, 42, 0x1234_5678] {
            let mut rng = xorshift32(seed);
            let bytes: Vec<u8> = (0..4096).map(|_| (rng() & 0xff) as u8).collect();
            let mut reader = FrameReader::new(Cursor::new(bytes));
            while let Ok(Some(_)) = reader.next_frame() {}
        }
    }

    // -----------------------------------------------------------------
    // validate_inspection
    // -----------------------------------------------------------------

    /// Builds a valid `scene-inspection-v1` record with a correct digest,
    /// using the exact canonicalization `kwe-scene-inspector::build_record`
    /// uses (set digest to "", serialize, SHA-256, hex).
    fn golden_record() -> Value {
        let mut record = json!({
            "schema": SCENE_INSPECTION_SCHEMA,
            "capabilities_schema": SCENE_CAPABILITIES_SCHEMA,
            "content": {"hash": "sha256:aaaa", "source_bytes": 10, "kind": "json-dir"},
            "inspector": {"build": "dev", "abi": 0},
            "outcome": "inventoried",
            "reason": "ok",
            "required": ["scene.layer.image"],
            "detected": [
                {"capability": "scene.layer.image", "count": 1, "objects": ["id:1"], "truncated": false}
            ],
            "unknown": {"keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": false},
            "bounds": {"wall_ms": 5, "peak_bytes": 0, "limits_hit": []},
            "backend": Value::Null,
            "digest": "",
        });
        let serialized = serde_json::to_vec(&record).unwrap();
        let digest = hex::encode(Sha256::digest(&serialized));
        record["digest"] = json!(digest);
        record
    }

    #[test]
    fn golden_record_validates() {
        let record = golden_record();
        let payload = serde_json::to_vec(&record).unwrap();
        let validated = validate_inspection(&payload).unwrap();
        assert_eq!(validated, record);
    }

    #[test]
    fn each_required_field_removed_is_missing_field() {
        let top_level = [
            "schema",
            "capabilities_schema",
            "content",
            "inspector",
            "outcome",
            "reason",
            "required",
            "detected",
            "unknown",
            "bounds",
            "backend",
            "digest",
        ];
        for field in top_level {
            let mut record = golden_record();
            record.as_object_mut().unwrap().remove(field);
            let payload = serde_json::to_vec(&record).unwrap();
            assert!(
                matches!(
                    validate_inspection(&payload),
                    Err(ValidationError::MissingField(reported)) if reported == field
                ),
                "field={field}: {:?}",
                validate_inspection(&payload)
            );
        }

        for (parent, field) in [
            ("content", "hash"),
            ("content", "source_bytes"),
            ("content", "kind"),
            ("inspector", "build"),
            ("inspector", "abi"),
            ("unknown", "keys"),
            ("unknown", "types"),
            ("unknown", "objects"),
            ("unknown", "samples"),
            ("unknown", "truncated"),
            ("bounds", "wall_ms"),
            ("bounds", "limits_hit"),
        ] {
            let mut record = golden_record();
            record
                .get_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            let payload = serde_json::to_vec(&record).unwrap();
            let expected_path = format!("{parent}.{field}");
            assert!(
                matches!(
                    validate_inspection(&payload),
                    Err(ValidationError::MissingField(reported)) if reported == expected_path
                ),
                "field={parent}.{field}: {:?}",
                validate_inspection(&payload)
            );
        }
    }

    #[test]
    fn wrong_types_are_reported() {
        let mut record = golden_record();
        record["content"]["source_bytes"] = json!("not a number");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::WrongType("content.source_bytes"))
        ));

        let mut record = golden_record();
        record["required"] = json!("not an array");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::WrongType("required"))
        ));

        let mut record = golden_record();
        record["backend"] = json!("not null or an object");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::WrongType("backend"))
        ));
    }

    #[test]
    fn backend_accepts_null_or_object() {
        let mut record = golden_record();
        record["backend"] = json!({"name": "vulkan"});
        // Recompute the digest: mutating a validated field changes content.
        record["digest"] = json!("");
        let serialized = serde_json::to_vec(&record).unwrap();
        record["digest"] = json!(hex::encode(Sha256::digest(&serialized)));
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(validate_inspection(&payload).is_ok());
    }

    #[test]
    fn wrong_schema_strings_are_rejected() {
        let mut record = golden_record();
        record["schema"] = json!("scene-feature-inventory-v0");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::WrongSchema)
        ));

        let mut record = golden_record();
        record["capabilities_schema"] = json!("scene-capabilities-v0");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::WrongCapabilitiesSchema)
        ));
    }

    #[test]
    fn flipped_digest_is_rejected() {
        let mut record = golden_record();
        record["digest"] =
            json!("0000000000000000000000000000000000000000000000000000000000000000");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_inspection(&payload),
            Err(ValidationError::DigestMismatch)
        ));
    }

    #[test]
    fn not_an_object_and_invalid_json_are_rejected_gracefully() {
        assert!(matches!(
            validate_inspection(b"[1,2,3]"),
            Err(ValidationError::NotAnObject)
        ));
        assert!(matches!(
            validate_inspection(b"{not json"),
            Err(ValidationError::Parse(_))
        ));
    }

    #[test]
    fn validate_inspection_handles_an_oversized_buffer_gracefully() {
        // Not the frame reader's job to bound this (it assumes the caller
        // already capped it via FrameReader), but it must not panic.
        let huge = vec![b'{'; MAX_PAYLOAD_BYTES * 4];
        assert!(validate_inspection(&huge).is_err());
    }
}
