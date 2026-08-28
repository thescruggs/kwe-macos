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
//! SR-3a: the SAME 12-byte `KWR1` frame codec is reused for the killable
//! shader-compile helper's stdin/stdout channels (kinds 16-18, a separate
//! namespace from the report-FD kinds below — see
//! `docs/SHADER_HELPER_PROTOCOL_V1.md`). The two channel FAMILIES are
//! otherwise unrelated: report-FD is a one-way, daemon-owned side channel a
//! worker writes once; the shader helper's stdin/stdout are a two-way
//! request/response exchange with its own stream caps (`StreamCaps`,
//! `SHADER_REQUEST_CAPS`/`SHADER_RESPONSE_CAPS` below) — only the wire
//! FORMAT (header shape, per-frame byte cap) and this crate's codec are
//! shared, never the channel semantics.
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

/// SR-3a: `shader-compile-request-v1`'s own `"schema"` field value (kind
/// 16 payload) — see `docs/SHADER_HELPER_PROTOCOL_V1.md`.
pub const SHADER_COMPILE_REQUEST_SCHEMA: &str = "shader-compile-request-v1";
/// SR-3a: `shader-compile-response-v1`'s own `"schema"` field value (kind
/// 17 payload).
pub const SHADER_COMPILE_RESPONSE_SCHEMA: &str = "shader-compile-response-v1";

/// SR-3a decision (b): the shader helper's REQUEST channel (daemon stdin
/// of the helper process) caps — one kind-16 frame plus slack, never
/// anywhere near the report channel's own 16-frame/1-MiB default.
pub const SHADER_REQUEST_MAX_FRAMES: usize = 4;
pub const SHADER_REQUEST_MAX_TOTAL_PAYLOAD_BYTES: usize = 1024 * 1024;
/// SR-3a decision (b): the shader helper's RESPONSE channel (helper
/// stdout) caps — one kind-17 frame plus up to 128 kind-18 SPIR-V chunks
/// (a future compiling helper's shape, reserved now) plus slack.
pub const SHADER_RESPONSE_MAX_FRAMES: usize = 132;
pub const SHADER_RESPONSE_MAX_TOTAL_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// SR-3a decision (b): `FrameReader::with_caps`' bundle of the two
/// stream-level caps (frame count, total payload bytes) — the per-frame
/// [`MAX_PAYLOAD_BYTES`] cap stays universal, unconfigurable, and is
/// checked unconditionally regardless of which `StreamCaps` a reader uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamCaps {
    pub max_frames: usize,
    pub max_total_payload_bytes: usize,
}

impl StreamCaps {
    /// The report-FD channel's own long-standing caps — byte-identical to
    /// what `FrameReader::new` always enforced before SR-3a made the caps
    /// configurable.
    pub const REPORT: StreamCaps = StreamCaps {
        max_frames: MAX_FRAMES_PER_STREAM,
        max_total_payload_bytes: MAX_TOTAL_PAYLOAD_BYTES,
    };
    /// SR-3a: the shader helper's request-channel caps.
    pub const SHADER_REQUEST: StreamCaps = StreamCaps {
        max_frames: SHADER_REQUEST_MAX_FRAMES,
        max_total_payload_bytes: SHADER_REQUEST_MAX_TOTAL_PAYLOAD_BYTES,
    };
    /// SR-3a: the shader helper's response-channel caps.
    pub const SHADER_RESPONSE: StreamCaps = StreamCaps {
        max_frames: SHADER_RESPONSE_MAX_FRAMES,
        max_total_payload_bytes: SHADER_RESPONSE_MAX_TOTAL_PAYLOAD_BYTES,
    };
}

/// SR-3a: a shader-compile request's `"includes"` map may name at most this
/// many files.
pub const MAX_SHADER_INCLUDES: usize = 32;
/// SR-3a: any single include's byte length cap.
pub const MAX_SHADER_INCLUDE_BYTES: usize = 64 * 1024;
/// SR-3a: a shader-compile request's `"combos"` map's entry-count cap.
pub const MAX_SHADER_COMBOS: usize = 128;
/// SR-3a: a shader-compile request's `"defines"` map's entry-count cap.
pub const MAX_SHADER_DEFINES: usize = 128;
/// SR-3c: a shader-compile request's optional `"options"` object's own
/// string fields (`target_env`/`target_env_version`/`optimization_level`)
/// — bounded well above any real value (`kwe-core::shader_compile_spec`'s
/// own constants are a handful of bytes each) purely so a hostile/crafted
/// request cannot make a diagnostic line unbounded.
pub const MAX_SHADER_OPTION_STRING_BYTES: usize = 128;

/// SR-3c: a `shader-compile-response-v1` "ok" response's declared
/// `"spirv_chunks"` — one less than [`SHADER_RESPONSE_MAX_FRAMES`] (the
/// response channel's own [`StreamCaps::SHADER_RESPONSE`] frame budget)
/// since that budget also has to cover the kind-17 header frame itself.
/// Refusing an over-claim HERE, at header-validation time, means a
/// dishonest/buggy helper that declares e.g. 200 chunks is rejected before
/// a single kind-18 frame is even read, rather than only being caught once
/// [`FrameReader`] itself hits the same cap partway through actually
/// reading that many frames.
pub const MAX_SPIRV_CHUNKS: usize = SHADER_RESPONSE_MAX_FRAMES - 1;
/// SR-3c: a `shader-compile-response-v1` "ok" response's declared
/// `"spirv_total_bytes"` — the same total-payload budget
/// [`StreamCaps::SHADER_RESPONSE`] enforces stream-wide, so an over-claim
/// is refused up front rather than discovered mid-reassembly.
pub const MAX_SPIRV_TOTAL_BYTES: usize = SHADER_RESPONSE_MAX_TOTAL_PAYLOAD_BYTES;
/// SR-3c: a `shader-compile-response-v1` "compile-error" response's
/// `"log"` field — bounded so a pathological/hostile GLSL compile error
/// (shaderc's own diagnostic text is not otherwise length-limited) cannot
/// make the response channel's own byte budget the limiting factor, and so
/// the eventual caller-side diagnostic line stays bounded without a
/// separate truncation step.
pub const MAX_SHADER_COMPILE_ERROR_LOG_BYTES: usize = 4 * 1024;

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
    /// Kind 16: a `shader-compile-request-v1` JSON record (SR-3a) — the
    /// daemon/caller -> shader helper direction, on the helper's stdin.
    /// See `docs/SHADER_HELPER_PROTOCOL_V1.md`.
    ShaderCompileRequestV1,
    /// Kind 17: a `shader-compile-response-v1` JSON record (SR-3a) — the
    /// helper -> caller direction, on the helper's stdout.
    ShaderCompileResponseV1,
    /// Kind 18: one raw SPIR-V binary chunk (SR-3a), repeatable — RESERVED,
    /// no producer yet in this skeleton (SR-3c's compiling helper is the
    /// first). Unlike kinds 1/2/16/17 this payload is NOT JSON.
    SpirvChunkV1,
    /// Any other kind byte.
    Unknown(u8),
}

impl FrameKind {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::SceneInspectionV1,
            2 => Self::SceneRenderReportV1,
            16 => Self::ShaderCompileRequestV1,
            17 => Self::ShaderCompileResponseV1,
            18 => Self::SpirvChunkV1,
            other => Self::Unknown(other),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::SceneInspectionV1 => 1,
            Self::SceneRenderReportV1 => 2,
            Self::ShaderCompileRequestV1 => 16,
            Self::ShaderCompileResponseV1 => 17,
            Self::SpirvChunkV1 => 18,
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
    /// SR-3a: carries the ACTUAL configured cap (`StreamCaps::max_frames`)
    /// now that it is no longer always [`MAX_FRAMES_PER_STREAM`].
    #[error("stream exceeded {max} frames")]
    FrameCountExceeded { max: usize },
    /// SR-3a: carries the ACTUAL configured cap
    /// (`StreamCaps::max_total_payload_bytes`) now that it is no longer
    /// always [`MAX_TOTAL_PAYLOAD_BYTES`].
    #[error("stream exceeded {max} total payload bytes")]
    TotalBytesExceeded { max: usize },
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

/// Bounded frame reader over one stream. Enforces its `StreamCaps` (frame
/// count, total payload bytes) across the whole `FrameReader` lifetime —
/// construct a fresh one per stream/generation. The per-frame
/// [`MAX_PAYLOAD_BYTES`] cap is separate and always enforced, regardless of
/// `caps`.
pub struct FrameReader<R: Read> {
    reader: R,
    frames_read: usize,
    total_payload_bytes: usize,
    caps: StreamCaps,
}

impl<R: Read> FrameReader<R> {
    /// The report-FD channel's reader: [`StreamCaps::REPORT`] — the same
    /// 16-frame/1-MiB caps this constructor always enforced before SR-3a
    /// made caps configurable (byte-identical behavior, existing callers
    /// unaffected).
    pub fn new(reader: R) -> Self {
        Self::with_caps(reader, StreamCaps::REPORT)
    }

    /// SR-3a: a reader over stream-level caps OTHER than the report
    /// channel's own defaults (e.g. [`StreamCaps::SHADER_REQUEST`]/
    /// [`StreamCaps::SHADER_RESPONSE`]).
    pub fn with_caps(reader: R, caps: StreamCaps) -> Self {
        Self {
            reader,
            frames_read: 0,
            total_payload_bytes: 0,
            caps,
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
        // The byte cap is checked before the frame-count cap: with the
        // report channel's own caps (16 frames x 64 KiB == 1 MiB exactly),
        // a stream cannot go OVER the byte cap without ALSO being at frame
        // 17 or later, so checking bytes first is what makes
        // TotalBytesExceeded reachable at all as a distinct outcome from
        // FrameCountExceeded — a small-payload stream past 16 frames still
        // hits FrameCountExceeded here, since its byte total stays low.
        // SR-3a: a caller with a DIFFERENT `StreamCaps` (the shader
        // helper's channels) may not have this exact coincidence, but the
        // check order itself — bytes, then count — stays the same either
        // way; each cap is independently reachable by construction (a
        // small-payload stream past the frame-count cap always hits
        // FrameCountExceeded, a stream at the frame cap but under the byte
        // cap never hits TotalBytesExceeded).
        self.total_payload_bytes += payload_len as usize;
        if self.total_payload_bytes > self.caps.max_total_payload_bytes {
            return Err(FrameError::TotalBytesExceeded {
                max: self.caps.max_total_payload_bytes,
            });
        }
        self.frames_read += 1;
        if self.frames_read > self.caps.max_frames {
            return Err(FrameError::FrameCountExceeded {
                max: self.caps.max_frames,
            });
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

// ---------------------------------------------------------------------------
// SR-3a: shader-compile-request-v1 / shader-compile-response-v1
// ---------------------------------------------------------------------------

/// `validate_shader_compile_request` failures — same "distinguish missing
/// from wrong-type" style as [`ValidationError`], plus the shader
/// request's own shape bounds (decision (b)/task §2).
#[derive(Debug, Error)]
pub enum ShaderRequestError {
    #[error("payload is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("payload is not a JSON object")]
    NotAnObject,
    #[error("\"schema\" does not match {SHADER_COMPILE_REQUEST_SCHEMA:?}")]
    WrongSchema,
    #[error("missing required field {0:?}")]
    MissingField(&'static str),
    #[error("field {0:?} has the wrong JSON type")]
    WrongType(&'static str),
    #[error("\"stage\" must be \"vertex\" or \"fragment\"")]
    InvalidStage,
    #[error("\"source\" is {len} bytes; maximum is {max}")]
    SourceOversize { len: usize, max: usize },
    #[error("\"includes\" has more than {MAX_SHADER_INCLUDES} entries")]
    TooManyIncludes,
    #[error("include {0:?} is not a string, or exceeds {MAX_SHADER_INCLUDE_BYTES} bytes")]
    InvalidInclude(String),
    #[error("\"combos\" has more than {MAX_SHADER_COMBOS} entries")]
    TooManyCombos,
    #[error("\"defines\" has more than {MAX_SHADER_DEFINES} entries")]
    TooManyDefines,
    #[error("field {0:?} exceeds {MAX_SHADER_OPTION_STRING_BYTES} bytes")]
    OptionOversize(&'static str),
}

/// Validates one `shader-compile-request-v1` payload (kind 16): schema tag,
/// every required field's presence/type, `stage`'s enum, `source`'s length
/// against `max_source_bytes` (a CALLER-supplied bound — the shader
/// helper's own `--max-source-bytes` flag, decision (b)/task §2; NOT a
/// fixed protocol constant, unlike the include/combo/define counts, which
/// are), and `includes`/`combos`/`defines`'s own fixed shape bounds
/// ([`MAX_SHADER_INCLUDES`]/[`MAX_SHADER_INCLUDE_BYTES`]/
/// [`MAX_SHADER_COMBOS`]/[`MAX_SHADER_DEFINES`]).
///
/// SR-3c: an OPTIONAL top-level `"options"` object — `{"target_env": ...,
/// "target_env_version": ..., "optimization_level": ...}`, each a string
/// no longer than [`MAX_SHADER_OPTION_STRING_BYTES`]. Additive to the
/// SR-3a schema (still named `shader-compile-request-v1`, per the task's
/// "version the schema additively, keep v1 name"): a payload with no
/// `"options"` key at all remains valid (absent means "use the compiling
/// side's own defaults" — `kwe-core::shader_compile_spec`'s constants on
/// both ends of this protocol today). When present, all three sub-fields
/// are required together (no partial object) and only their SHAPE is
/// checked here — this crate has no opinion on which target env/
/// optimization level a caller is allowed to ask for; that policy (today:
/// always `kwe-core`'s own fixed values) lives in the two crates that
/// actually call into `shaderc`.
///
/// Does not itself enforce [`MAX_PAYLOAD_BYTES`] — same caller-already-
/// capped-it assumption `validate_inspection` documents.
pub fn validate_shader_compile_request(
    payload: &[u8],
    max_source_bytes: usize,
) -> Result<Value, ShaderRequestError> {
    let value: Value = serde_json::from_slice(payload)?;
    let object = value.as_object().ok_or(ShaderRequestError::NotAnObject)?;

    let schema = shader_require_str(object, "schema", "schema")?;
    if schema != SHADER_COMPILE_REQUEST_SCHEMA {
        return Err(ShaderRequestError::WrongSchema);
    }
    let stage = shader_require_str(object, "stage", "stage")?;
    if stage != "vertex" && stage != "fragment" {
        return Err(ShaderRequestError::InvalidStage);
    }
    let source = shader_require_str(object, "source", "source")?;
    if source.len() > max_source_bytes {
        return Err(ShaderRequestError::SourceOversize {
            len: source.len(),
            max: max_source_bytes,
        });
    }

    let includes = shader_require_object(object, "includes", "includes")?;
    if includes.len() > MAX_SHADER_INCLUDES {
        return Err(ShaderRequestError::TooManyIncludes);
    }
    for (name, contents) in includes {
        match contents {
            Value::String(text) if text.len() <= MAX_SHADER_INCLUDE_BYTES => {}
            _ => return Err(ShaderRequestError::InvalidInclude(name.clone())),
        }
    }

    let combos = shader_require_object(object, "combos", "combos")?;
    if combos.len() > MAX_SHADER_COMBOS {
        return Err(ShaderRequestError::TooManyCombos);
    }
    let defines = shader_require_object(object, "defines", "defines")?;
    if defines.len() > MAX_SHADER_DEFINES {
        return Err(ShaderRequestError::TooManyDefines);
    }

    // SR-3c: optional "options" object -- absent entirely is valid (see
    // the doc comment above); when present, all three sub-fields are
    // required and shape-checked (string, bounded length) but their
    // VALUES are not otherwise interpreted by this crate.
    if let Some(options_value) = object.get("options") {
        let options = match options_value {
            Value::Object(map) => map,
            _ => return Err(ShaderRequestError::WrongType("options")),
        };
        for (field, path) in [
            ("target_env", "options.target_env"),
            ("target_env_version", "options.target_env_version"),
            ("optimization_level", "options.optimization_level"),
        ] {
            let text = shader_require_str(options, field, path)?;
            if text.len() > MAX_SHADER_OPTION_STRING_BYTES {
                return Err(ShaderRequestError::OptionOversize(path));
            }
        }
    }

    Ok(value)
}

/// `validate_shader_compile_response` failures.
#[derive(Debug, Error)]
pub enum ShaderResponseError {
    #[error("payload is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("payload is not a JSON object")]
    NotAnObject,
    #[error("\"schema\" does not match {SHADER_COMPILE_RESPONSE_SCHEMA:?}")]
    WrongSchema,
    #[error("missing required field {0:?}")]
    MissingField(&'static str),
    #[error("field {0:?} has the wrong JSON type")]
    WrongType(&'static str),
    #[error("\"spirv_chunks\" is {0}; maximum is {MAX_SPIRV_CHUNKS}")]
    TooManySpirvChunks(u64),
    #[error("\"spirv_total_bytes\" is {0}; maximum is {MAX_SPIRV_TOTAL_BYTES}")]
    SpirvTotalBytesTooLarge(u64),
    #[error("\"log\" is {len} bytes; maximum is {MAX_SHADER_COMPILE_ERROR_LOG_BYTES}")]
    LogOversize { len: usize },
}

/// Validates one `shader-compile-response-v1` payload (kind 17): schema
/// tag, `"status"` presence/type, then STATUS-DEPENDENT required fields —
/// the status enum itself is still not CLOSED here (an unrecognized status
/// falls back to the original SR-3a shape, `"reason"` required — the same
/// "don't retroactively break an older reader" principle `FrameKind::
/// Unknown` follows, now applied per-status rather than uniformly):
///
/// - `"unimplemented"` / `"protocol-error"` / any other/future status:
///   `"reason"` required (SR-3a's original shape, unchanged).
/// - `"ok"` (SR-3c, first compiling helper): `"spirv_chunks"` and
///   `"spirv_total_bytes"` required, both non-negative integers, bounded
///   by [`MAX_SPIRV_CHUNKS`]/[`MAX_SPIRV_TOTAL_BYTES`] — refusing an
///   over-claim HERE means a dishonest/buggy helper never gets as far as
///   the caller trying to read that many kind-18 frames.
/// - `"compile-error"` (SR-3c): `"log"` required, a string of at most
///   [`MAX_SHADER_COMPILE_ERROR_LOG_BYTES`] — a compile error is a
///   RESULT, not a protocol failure (SR-3c task text), so it gets its own
///   shape rather than being shoehorned into `"reason"`.
pub fn validate_shader_compile_response(payload: &[u8]) -> Result<Value, ShaderResponseError> {
    let value: Value = serde_json::from_slice(payload)?;
    let object = value.as_object().ok_or(ShaderResponseError::NotAnObject)?;

    let schema = match object.get("schema") {
        None => return Err(ShaderResponseError::MissingField("schema")),
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return Err(ShaderResponseError::WrongType("schema")),
    };
    if schema != SHADER_COMPILE_RESPONSE_SCHEMA {
        return Err(ShaderResponseError::WrongSchema);
    }
    let status = match object.get("status") {
        None => return Err(ShaderResponseError::MissingField("status")),
        Some(Value::String(status)) => status.as_str(),
        Some(_) => return Err(ShaderResponseError::WrongType("status")),
    };

    match status {
        "ok" => {
            let spirv_chunks = response_require_u64(object, "spirv_chunks")?;
            if spirv_chunks > MAX_SPIRV_CHUNKS as u64 {
                return Err(ShaderResponseError::TooManySpirvChunks(spirv_chunks));
            }
            let spirv_total_bytes = response_require_u64(object, "spirv_total_bytes")?;
            if spirv_total_bytes > MAX_SPIRV_TOTAL_BYTES as u64 {
                return Err(ShaderResponseError::SpirvTotalBytesTooLarge(
                    spirv_total_bytes,
                ));
            }
        }
        "compile-error" => match object.get("log") {
            None => return Err(ShaderResponseError::MissingField("log")),
            Some(Value::String(log)) => {
                if log.len() > MAX_SHADER_COMPILE_ERROR_LOG_BYTES {
                    return Err(ShaderResponseError::LogOversize { len: log.len() });
                }
            }
            Some(_) => return Err(ShaderResponseError::WrongType("log")),
        },
        _ => match object.get("reason") {
            None => return Err(ShaderResponseError::MissingField("reason")),
            Some(Value::String(_)) => {}
            Some(_) => return Err(ShaderResponseError::WrongType("reason")),
        },
    }

    Ok(value)
}

/// Like `shader_require_str`, for a required non-negative integer response
/// field (`"spirv_chunks"`/`"spirv_total_bytes"`) — a negative number or a
/// non-integer JSON number is `WrongType`, same as any other shape
/// mismatch here.
fn response_require_u64(
    map: &Map<String, Value>,
    path: &'static str,
) -> Result<u64, ShaderResponseError> {
    match map.get(path) {
        None => Err(ShaderResponseError::MissingField(path)),
        Some(Value::Number(number)) => number.as_u64().ok_or(ShaderResponseError::WrongType(path)),
        Some(_) => Err(ShaderResponseError::WrongType(path)),
    }
}

fn shader_require_str<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<&'a str, ShaderRequestError> {
    match map.get(key) {
        None => Err(ShaderRequestError::MissingField(path)),
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(ShaderRequestError::WrongType(path)),
    }
}

fn shader_require_object<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    path: &'static str,
) -> Result<&'a Map<String, Value>, ShaderRequestError> {
    match map.get(key) {
        None => Err(ShaderRequestError::MissingField(path)),
        Some(Value::Object(value)) => Ok(value),
        Some(_) => Err(ShaderRequestError::WrongType(path)),
    }
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
                    matches!(
                        result,
                        Err(FrameError::FrameCountExceeded {
                            max: MAX_FRAMES_PER_STREAM
                        })
                    ),
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
            matches!(
                result,
                Err(FrameError::TotalBytesExceeded {
                    max: MAX_TOTAL_PAYLOAD_BYTES
                })
            ),
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

    // -----------------------------------------------------------------
    // SR-3a: kinds 16/17/18 round-trip
    // -----------------------------------------------------------------

    #[test]
    fn shader_kinds_round_trip_and_map_to_their_wire_bytes() {
        for (kind, byte) in [
            (FrameKind::ShaderCompileRequestV1, 16_u8),
            (FrameKind::ShaderCompileResponseV1, 17),
            (FrameKind::SpirvChunkV1, 18),
        ] {
            assert_eq!(kind.as_u8(), byte);
            assert_eq!(FrameKind::from_u8(byte), kind);
        }

        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            FrameKind::ShaderCompileRequestV1,
            b"request-payload",
        )
        .unwrap();
        write_frame(
            &mut buffer,
            FrameKind::ShaderCompileResponseV1,
            b"response-payload",
        )
        .unwrap();
        write_frame(&mut buffer, FrameKind::SpirvChunkV1, b"\x00\x01spirv-bytes").unwrap();

        let mut reader = FrameReader::with_caps(Cursor::new(buffer), StreamCaps::SHADER_RESPONSE);
        let first = reader.next_frame().unwrap().unwrap();
        assert_eq!(first.kind, FrameKind::ShaderCompileRequestV1);
        assert_eq!(first.payload, b"request-payload");
        let second = reader.next_frame().unwrap().unwrap();
        assert_eq!(second.kind, FrameKind::ShaderCompileResponseV1);
        assert_eq!(second.payload, b"response-payload");
        let third = reader.next_frame().unwrap().unwrap();
        assert_eq!(third.kind, FrameKind::SpirvChunkV1);
        assert_eq!(third.payload, b"\x00\x01spirv-bytes");
        assert!(reader.next_frame().unwrap().is_none());
    }

    // -----------------------------------------------------------------
    // SR-3a: with_caps — custom caps at limit-1/limit/limit+1
    // -----------------------------------------------------------------

    #[test]
    fn with_caps_frame_count_is_enforced_at_the_configured_boundary() {
        let caps = StreamCaps {
            max_frames: 4,
            max_total_payload_bytes: 1024,
        };
        for (count, expect_ok) in [(3, true), (4, true), (5, false)] {
            let mut buffer = Vec::new();
            for _ in 0..count {
                write_frame(&mut buffer, FrameKind::ShaderCompileRequestV1, b"x").unwrap();
            }
            let mut reader = FrameReader::with_caps(Cursor::new(buffer), caps);
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
                    matches!(result, Err(FrameError::FrameCountExceeded { max: 4 })),
                    "count={count}: {result:?}"
                );
                assert_eq!(read, 4, "count={count}");
            }
        }
    }

    #[test]
    fn with_caps_total_bytes_is_enforced_at_the_configured_boundary() {
        // A cap small enough that the byte limit binds well before the
        // (generous) frame-count limit, so this exercises
        // TotalBytesExceeded specifically, independent of frame count —
        // the mirror of the report channel's own boundary test above, but
        // for a StreamCaps whose two limits do NOT coincide the way the
        // report channel's 16 x 64 KiB == 1 MiB happens to.
        let caps = StreamCaps {
            max_frames: 1000,
            max_total_payload_bytes: 300,
        };
        for (payload_len, expect_ok) in [(299, true), (300, true), (301, false)] {
            let mut buffer = Vec::new();
            write_frame(
                &mut buffer,
                FrameKind::ShaderCompileRequestV1,
                &vec![0_u8; payload_len],
            )
            .unwrap();
            let mut reader = FrameReader::with_caps(Cursor::new(buffer), caps);
            let result = reader.next_frame();
            if expect_ok {
                assert!(result.is_ok(), "len={payload_len}: {result:?}");
            } else {
                assert!(
                    matches!(result, Err(FrameError::TotalBytesExceeded { max: 300 })),
                    "len={payload_len}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn with_caps_never_relaxes_the_universal_per_frame_cap() {
        // StreamCaps only ever governs the STREAM-level totals; a single
        // frame over MAX_PAYLOAD_BYTES is refused regardless of how
        // generous the configured StreamCaps are.
        let generous = StreamCaps {
            max_frames: 1_000_000,
            max_total_payload_bytes: 1_000_000_000,
        };
        let mut over = Vec::new();
        over.extend_from_slice(&MAGIC);
        over.push(FrameKind::ShaderCompileRequestV1.as_u8());
        over.push(0);
        over.extend_from_slice(&0_u16.to_le_bytes());
        over.extend_from_slice(&((MAX_PAYLOAD_BYTES + 1) as u32).to_le_bytes());
        let mut reader = FrameReader::with_caps(Cursor::new(over), generous);
        assert!(matches!(
            reader.next_frame(),
            Err(FrameError::PayloadOversize { len }) if len as usize == MAX_PAYLOAD_BYTES + 1
        ));
    }

    #[test]
    fn new_and_with_caps_report_are_byte_identical() {
        // FrameReader::new must behave EXACTLY like with_caps(reader,
        // StreamCaps::REPORT) -- SR-3a's own additive-behavior promise for
        // every existing report-FD caller.
        let mut buffer = Vec::new();
        for _ in 0..(MAX_FRAMES_PER_STREAM + 1) {
            write_frame(&mut buffer, FrameKind::SceneInspectionV1, b"x").unwrap();
        }
        let mut via_new = FrameReader::new(Cursor::new(buffer.clone()));
        let mut via_with_caps = FrameReader::with_caps(Cursor::new(buffer), StreamCaps::REPORT);
        loop {
            let a = via_new.next_frame();
            let b = via_with_caps.next_frame();
            match (&a, &b) {
                (Ok(Some(fa)), Ok(Some(fb))) => assert_eq!(fa, fb),
                (Ok(None), Ok(None)) => break,
                (Err(_), Err(_)) => break,
                other => panic!("new()/with_caps(REPORT) diverged: {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------
    // SR-3a: validate_shader_compile_request
    // -----------------------------------------------------------------

    fn golden_shader_request() -> Value {
        json!({
            "schema": SHADER_COMPILE_REQUEST_SCHEMA,
            "stage": "fragment",
            "source": "void main() {}",
            "includes": {"common.glsl": "// shared"},
            "combos": {"USE_FOG": 1},
            "defines": {"MAX_LIGHTS": 4},
        })
    }

    #[test]
    fn golden_shader_request_validates() {
        let record = golden_shader_request();
        let payload = serde_json::to_vec(&record).unwrap();
        let validated = validate_shader_compile_request(&payload, 256 * 1024).unwrap();
        assert_eq!(validated, record);
    }

    #[test]
    fn shader_request_missing_fields_are_reported() {
        for field in ["schema", "stage", "source", "includes", "combos", "defines"] {
            let mut record = golden_shader_request();
            record.as_object_mut().unwrap().remove(field);
            let payload = serde_json::to_vec(&record).unwrap();
            assert!(
                matches!(
                    validate_shader_compile_request(&payload, 256 * 1024),
                    Err(ShaderRequestError::MissingField(reported)) if reported == field
                ),
                "field={field}: {:?}",
                validate_shader_compile_request(&payload, 256 * 1024)
            );
        }
    }

    #[test]
    fn shader_request_wrong_types_are_reported() {
        let mut record = golden_shader_request();
        record["source"] = json!(42);
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::WrongType("source"))
        ));

        let mut record = golden_shader_request();
        record["includes"] = json!("not an object");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::WrongType("includes"))
        ));
    }

    #[test]
    fn shader_request_wrong_schema_is_rejected() {
        let mut record = golden_shader_request();
        record["schema"] = json!("shader-compile-request-v0");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::WrongSchema)
        ));
    }

    #[test]
    fn shader_request_stage_must_be_vertex_or_fragment() {
        for stage in ["vertex", "fragment"] {
            let mut record = golden_shader_request();
            record["stage"] = json!(stage);
            let payload = serde_json::to_vec(&record).unwrap();
            assert!(validate_shader_compile_request(&payload, 256 * 1024).is_ok());
        }
        let mut record = golden_shader_request();
        record["stage"] = json!("geometry");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::InvalidStage)
        ));
    }

    #[test]
    fn shader_request_source_length_is_bounded_by_the_caller_supplied_max() {
        for (len, max, expect_ok) in [(9, 10, true), (10, 10, true), (11, 10, false)] {
            let mut record = golden_shader_request();
            record["source"] = json!("x".repeat(len));
            let payload = serde_json::to_vec(&record).unwrap();
            let result = validate_shader_compile_request(&payload, max);
            if expect_ok {
                assert!(result.is_ok(), "len={len} max={max}: {result:?}");
            } else {
                assert!(
                    matches!(
                        result,
                        Err(ShaderRequestError::SourceOversize { len: reported_len, max: reported_max })
                            if reported_len == len && reported_max == max
                    ),
                    "len={len} max={max}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn shader_request_includes_are_bounded_at_the_count_and_per_entry_boundary() {
        // Exactly MAX_SHADER_INCLUDES is fine; one more rejects.
        for (count, expect_ok) in [
            (MAX_SHADER_INCLUDES - 1, true),
            (MAX_SHADER_INCLUDES, true),
            (MAX_SHADER_INCLUDES + 1, false),
        ] {
            let mut record = golden_shader_request();
            let includes: Map<String, Value> = (0..count)
                .map(|index| (format!("f{index}.glsl"), json!("x")))
                .collect();
            record["includes"] = Value::Object(includes);
            let payload = serde_json::to_vec(&record).unwrap();
            let result = validate_shader_compile_request(&payload, 256 * 1024);
            assert_eq!(result.is_ok(), expect_ok, "count={count}: {result:?}");
            if !expect_ok {
                assert!(matches!(result, Err(ShaderRequestError::TooManyIncludes)));
            }
        }

        // Per-entry byte cap: at the boundary, then one over.
        let mut record = golden_shader_request();
        record["includes"] = json!({"big.glsl": "x".repeat(MAX_SHADER_INCLUDE_BYTES)});
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(validate_shader_compile_request(&payload, 256 * 1024).is_ok());

        let mut record = golden_shader_request();
        record["includes"] = json!({"big.glsl": "x".repeat(MAX_SHADER_INCLUDE_BYTES + 1)});
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::InvalidInclude(name)) if name == "big.glsl"
        ));

        // A non-string include value is invalid regardless of size.
        let mut record = golden_shader_request();
        record["includes"] = json!({"bad.glsl": 42});
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::InvalidInclude(name)) if name == "bad.glsl"
        ));
    }

    #[test]
    fn shader_request_combos_and_defines_are_bounded_at_the_count() {
        let mut record = golden_shader_request();
        let combos: Map<String, Value> = (0..MAX_SHADER_COMBOS + 1)
            .map(|index| (format!("C{index}"), json!(1)))
            .collect();
        record["combos"] = Value::Object(combos);
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::TooManyCombos)
        ));

        let mut record = golden_shader_request();
        let defines: Map<String, Value> = (0..MAX_SHADER_DEFINES + 1)
            .map(|index| (format!("D{index}"), json!(1)))
            .collect();
        record["defines"] = Value::Object(defines);
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::TooManyDefines)
        ));
    }

    #[test]
    fn shader_request_not_an_object_and_invalid_json_are_rejected_gracefully() {
        assert!(matches!(
            validate_shader_compile_request(b"[1,2,3]", 256 * 1024),
            Err(ShaderRequestError::NotAnObject)
        ));
        assert!(matches!(
            validate_shader_compile_request(b"{not json", 256 * 1024),
            Err(ShaderRequestError::Parse(_))
        ));
    }

    // -----------------------------------------------------------------
    // SR-3c: shader-compile-request-v1's optional "options" object
    // -----------------------------------------------------------------

    #[test]
    fn shader_request_options_absent_entirely_is_still_valid() {
        // golden_shader_request() has no "options" key at all -- SR-3c
        // additive: absent means "use the compiling side's own defaults".
        let record = golden_shader_request();
        assert!(!record.as_object().unwrap().contains_key("options"));
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(validate_shader_compile_request(&payload, 256 * 1024).is_ok());
    }

    #[test]
    fn shader_request_options_present_and_complete_is_valid() {
        let mut record = golden_shader_request();
        record["options"] = json!({
            "target_env": "vulkan",
            "target_env_version": "1.2",
            "optimization_level": "zero",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        let validated = validate_shader_compile_request(&payload, 256 * 1024).unwrap();
        assert_eq!(validated, record);
    }

    #[test]
    fn shader_request_options_wrong_type_is_rejected() {
        let mut record = golden_shader_request();
        record["options"] = json!("not an object");
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::WrongType("options"))
        ));
    }

    #[test]
    fn shader_request_options_requires_all_three_sub_fields_together() {
        for missing in ["target_env", "target_env_version", "optimization_level"] {
            let mut record = golden_shader_request();
            let mut options = serde_json::Map::new();
            for field in ["target_env", "target_env_version", "optimization_level"] {
                if field != missing {
                    options.insert(field.to_string(), json!("x"));
                }
            }
            record["options"] = Value::Object(options);
            let payload = serde_json::to_vec(&record).unwrap();
            let expected_path = format!("options.{missing}");
            assert!(
                matches!(
                    validate_shader_compile_request(&payload, 256 * 1024),
                    Err(ShaderRequestError::MissingField(reported)) if reported == expected_path
                ),
                "missing={missing}: {:?}",
                validate_shader_compile_request(&payload, 256 * 1024)
            );
        }
    }

    #[test]
    fn shader_request_options_sub_fields_are_bounded() {
        let mut record = golden_shader_request();
        record["options"] = json!({
            "target_env": "x".repeat(MAX_SHADER_OPTION_STRING_BYTES + 1),
            "target_env_version": "1.2",
            "optimization_level": "zero",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_request(&payload, 256 * 1024),
            Err(ShaderRequestError::OptionOversize("options.target_env"))
        ));

        // Exactly at the bound is fine.
        let mut record = golden_shader_request();
        record["options"] = json!({
            "target_env": "x".repeat(MAX_SHADER_OPTION_STRING_BYTES),
            "target_env_version": "1.2",
            "optimization_level": "zero",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(validate_shader_compile_request(&payload, 256 * 1024).is_ok());
    }

    // -----------------------------------------------------------------
    // SR-3a: validate_shader_compile_response
    // -----------------------------------------------------------------

    #[test]
    fn golden_shader_response_validates() {
        let record = json!({
            "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
            "status": "unimplemented",
            "reason": "skeleton",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert_eq!(validate_shader_compile_response(&payload).unwrap(), record);
    }

    #[test]
    fn shader_response_missing_fields_are_reported() {
        for field in ["schema", "status", "reason"] {
            let mut record = json!({
                "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
                "status": "protocol-error",
                "reason": "wrong-kind",
            });
            record.as_object_mut().unwrap().remove(field);
            let payload = serde_json::to_vec(&record).unwrap();
            assert!(
                matches!(
                    validate_shader_compile_response(&payload),
                    Err(ShaderResponseError::MissingField(reported)) if reported == field
                ),
                "field={field}"
            );
        }
    }

    #[test]
    fn shader_response_wrong_schema_and_types_are_rejected() {
        let record = json!({
            "schema": "shader-compile-response-v0",
            "status": "unimplemented",
            "reason": "skeleton",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_response(&payload),
            Err(ShaderResponseError::WrongSchema)
        ));

        let record = json!({
            "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
            "status": 1,
            "reason": "skeleton",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_response(&payload),
            Err(ShaderResponseError::WrongType("status"))
        ));
    }

    // -----------------------------------------------------------------
    // SR-3c: "ok" / "compile-error" response shapes
    // -----------------------------------------------------------------

    #[test]
    fn shader_response_ok_status_requires_spirv_chunks_and_total_bytes() {
        let record = json!({
            "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
            "status": "ok",
            "spirv_chunks": 3,
            "spirv_total_bytes": 12_345,
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert_eq!(validate_shader_compile_response(&payload).unwrap(), record);

        for missing in ["spirv_chunks", "spirv_total_bytes"] {
            let mut record = record.clone();
            record.as_object_mut().unwrap().remove(missing);
            let payload = serde_json::to_vec(&record).unwrap();
            assert!(
                matches!(
                    validate_shader_compile_response(&payload),
                    Err(ShaderResponseError::MissingField(reported)) if reported == missing
                ),
                "missing={missing}: {:?}",
                validate_shader_compile_response(&payload)
            );
        }

        // An "ok" response has no "reason" field at all -- unlike
        // unimplemented/protocol-error, it is not required here.
        assert!(!record.as_object().unwrap().contains_key("reason"));
    }

    #[test]
    fn shader_response_ok_spirv_chunks_is_bounded_by_the_response_channel_cap() {
        for (chunks, expect_ok) in [
            (MAX_SPIRV_CHUNKS as u64, true),
            (MAX_SPIRV_CHUNKS as u64 + 1, false),
        ] {
            let record = json!({
                "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
                "status": "ok",
                "spirv_chunks": chunks,
                "spirv_total_bytes": 4,
            });
            let payload = serde_json::to_vec(&record).unwrap();
            let result = validate_shader_compile_response(&payload);
            assert_eq!(result.is_ok(), expect_ok, "chunks={chunks}: {result:?}");
            if !expect_ok {
                assert!(matches!(
                    result,
                    Err(ShaderResponseError::TooManySpirvChunks(reported)) if reported == chunks
                ));
            }
        }
    }

    #[test]
    fn shader_response_ok_spirv_total_bytes_is_bounded() {
        let record = json!({
            "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
            "status": "ok",
            "spirv_chunks": 1,
            "spirv_total_bytes": MAX_SPIRV_TOTAL_BYTES as u64 + 1,
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert!(matches!(
            validate_shader_compile_response(&payload),
            Err(ShaderResponseError::SpirvTotalBytesTooLarge(reported))
                if reported == MAX_SPIRV_TOTAL_BYTES as u64 + 1
        ));
    }

    #[test]
    fn shader_response_compile_error_status_requires_a_bounded_log() {
        let record = json!({
            "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
            "status": "compile-error",
            "log": "ERROR: 0:1: 'foo' : undeclared identifier",
        });
        let payload = serde_json::to_vec(&record).unwrap();
        assert_eq!(validate_shader_compile_response(&payload).unwrap(), record);

        let mut missing = record.clone();
        missing.as_object_mut().unwrap().remove("log");
        let payload = serde_json::to_vec(&missing).unwrap();
        assert!(matches!(
            validate_shader_compile_response(&payload),
            Err(ShaderResponseError::MissingField("log"))
        ));

        let mut oversize = record.clone();
        oversize["log"] = json!("x".repeat(MAX_SHADER_COMPILE_ERROR_LOG_BYTES + 1));
        let payload = serde_json::to_vec(&oversize).unwrap();
        assert!(matches!(
            validate_shader_compile_response(&payload),
            Err(ShaderResponseError::LogOversize { .. })
        ));

        // A "compile-error" response has no "reason" field either.
        assert!(!record.as_object().unwrap().contains_key("reason"));
    }
}
