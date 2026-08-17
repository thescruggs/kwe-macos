// SPDX-License-Identifier: Apache-2.0
//! Versioned, bounded shared-frame fallback transport.
//!
//! The wire format and seqlock are original to this project. The architectural
//! choice of an external producer plus a thin consumer is informed by the
//! projects listed as idea-level references in `THIRD_PARTY.yml`; no upstream
//! protocol or implementation code was copied.

#[cfg(target_endian = "big")]
compile_error!("KWE frame protocol v1 currently requires a little-endian target");
#[cfg(not(target_has_atomic = "64"))]
compile_error!("KWE frame protocol v1 requires lock-free 64-bit atomics");

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, AtomicU64, Ordering, fence},
};

use memmap2::{Mmap, MmapMut, MmapOptions};
use thiserror::Error;

pub const MAGIC: [u8; 8] = *b"KWEFRM1\0";
pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 64;
pub const SLOT_COUNT: u32 = 2;
pub const BYTES_PER_PIXEL: u32 = 4;
pub const PIXEL_FORMAT_BGRA8888_PREMULTIPLIED: u32 = 1;
pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_MAPPING_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_SNAPSHOT_ATTEMPTS: usize = 8;

const OFFSET_VERSION: usize = 8;
const OFFSET_HEADER_BYTES: usize = 12;
const OFFSET_FILE_BYTES: usize = 16;
const OFFSET_WIDTH: usize = 24;
const OFFSET_HEIGHT: usize = 28;
const OFFSET_STRIDE: usize = 32;
const OFFSET_PIXEL_FORMAT: usize = 36;
const OFFSET_SLOT_COUNT: usize = 40;
const OFFSET_GENERATION: usize = 48;
const OFFSET_ACTIVE_SLOT: usize = 56;
const OFFSET_PRODUCER_STATE: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ProducerState {
    Starting = 1,
    Running = 2,
    Stopping = 3,
    Failed = 4,
}

impl ProducerState {
    pub fn from_raw(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Starting),
            2 => Some(Self::Running),
            3 => Some(Self::Stopping),
            4 => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpec {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub slot_bytes: u64,
    pub file_bytes: u64,
}

impl FrameSpec {
    pub fn new(width: u32, height: u32) -> Result<Self, ProtocolError> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(ProtocolError::InvalidDimensions { width, height });
        }
        let stride = width
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or(ProtocolError::SizeOverflow)?;
        let slot_bytes = u64::from(stride)
            .checked_mul(u64::from(height))
            .ok_or(ProtocolError::SizeOverflow)?;
        let file_bytes = (HEADER_BYTES as u64)
            .checked_add(
                slot_bytes
                    .checked_mul(u64::from(SLOT_COUNT))
                    .ok_or(ProtocolError::SizeOverflow)?,
            )
            .ok_or(ProtocolError::SizeOverflow)?;
        if file_bytes > MAX_MAPPING_BYTES {
            return Err(ProtocolError::MappingTooLarge(file_bytes));
        }
        Ok(Self {
            width,
            height,
            stride,
            slot_bytes,
            file_bytes,
        })
    }

    pub fn pixel_bytes(self) -> usize {
        self.slot_bytes as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    pub spec: FrameSpec,
    pub sequence: u64,
    pub producer_state: ProducerState,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("frame dimensions {width}x{height} are outside 1..={MAX_DIMENSION}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("frame size arithmetic overflowed")]
    SizeOverflow,
    #[error("frame mapping would be {0} bytes; maximum is {MAX_MAPPING_BYTES}")]
    MappingTooLarge(u64),
    #[error("frame file is smaller than the {HEADER_BYTES}-byte header")]
    TruncatedHeader,
    #[error("frame magic is invalid")]
    InvalidMagic,
    #[error("unsupported frame protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid frame header: {0}")]
    InvalidHeader(&'static str),
    #[error("producer is updating the frame; retry later")]
    Busy,
    #[error("pixel buffer has {actual} bytes; expected {expected}")]
    PixelLength { actual: usize, expected: usize },
    #[error("unsafe frame path: {0}")]
    UnsafePath(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub struct SharedFrameWriter {
    _file: File,
    mapping: MmapMut,
    spec: FrameSpec,
}

impl SharedFrameWriter {
    pub fn create(path: &Path, spec: FrameSpec) -> Result<Self, ProtocolError> {
        validate_parent(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        file.set_len(spec.file_bytes)?;
        // SAFETY: the new regular file has the validated size, remains open for
        // the mapping lifetime, and no returned slice can outlive the mapping.
        let mut mapping = unsafe {
            MmapOptions::new()
                .len(spec.file_bytes as usize)
                .map_mut(&file)?
        };
        initialize_header(&mut mapping, spec);
        mapping.flush_range(0, HEADER_BYTES)?;
        Ok(Self {
            _file: file,
            mapping,
            spec,
        })
    }

    pub fn spec(&self) -> FrameSpec {
        self.spec
    }

    pub fn publish(&mut self, pixels: &[u8]) -> Result<u64, ProtocolError> {
        if pixels.len() != self.spec.pixel_bytes() {
            return Err(ProtocolError::PixelLength {
                actual: pixels.len(),
                expected: self.spec.pixel_bytes(),
            });
        }
        let starting = generation_atomic(&self.mapping).load(Ordering::Acquire);
        let odd = if starting.is_multiple_of(2) {
            starting.wrapping_add(1)
        } else {
            starting.wrapping_add(2)
        };
        generation_atomic(&self.mapping).store(odd, Ordering::Release);
        fence(Ordering::SeqCst);

        let current = active_slot_atomic(&self.mapping).load(Ordering::Relaxed);
        let next = (current + 1) % SLOT_COUNT;
        let offset = slot_offset(self.spec, next)?;
        self.mapping[offset..offset + pixels.len()].copy_from_slice(pixels);
        active_slot_atomic(&self.mapping).store(next, Ordering::Relaxed);
        state_atomic(&self.mapping).store(ProducerState::Running as u32, Ordering::Relaxed);
        fence(Ordering::Release);
        let even = odd.wrapping_add(1);
        generation_atomic(&self.mapping).store(even, Ordering::Release);
        Ok(even / 2)
    }

    pub fn set_state(&self, state: ProducerState) {
        state_atomic(&self.mapping).store(state as u32, Ordering::Release);
    }

    /// Deliberately invalidates the magic for fault-injection tests.
    pub fn corrupt_magic_for_test(&mut self) {
        self.mapping[0..MAGIC.len()].fill(0);
        fence(Ordering::SeqCst);
    }
}

pub struct SharedFrameReader {
    _file: File,
    mapping: Mmap,
    spec: FrameSpec,
    path: PathBuf,
}

impl SharedFrameReader {
    pub fn open(path: &Path) -> Result<Self, ProtocolError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(ProtocolError::UnsafePath(
                "frame mapping must be a regular, non-symlink file",
            ));
        }
        if metadata.len() < HEADER_BYTES as u64 {
            return Err(ProtocolError::TruncatedHeader);
        }
        if metadata.len() > MAX_MAPPING_BYTES {
            return Err(ProtocolError::MappingTooLarge(metadata.len()));
        }
        // SAFETY: file type and size are validated, the file remains open, and
        // the immutable mapping is exposed only through bounded snapshots.
        let mapping = unsafe { MmapOptions::new().len(metadata.len() as usize).map(&file)? };
        let spec = validate_header(&mapping)?;
        Ok(Self {
            _file: file,
            mapping,
            spec,
            path: path.to_path_buf(),
        })
    }

    pub fn spec(&self) -> FrameSpec {
        self.spec
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> Result<FrameSnapshot, ProtocolError> {
        for _ in 0..MAX_SNAPSHOT_ATTEMPTS {
            let before = generation_atomic(&self.mapping).load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let spec = validate_header(&self.mapping)?;
            let slot = active_slot_atomic(&self.mapping).load(Ordering::Relaxed);
            let state_raw = state_atomic(&self.mapping).load(Ordering::Relaxed);
            let state = ProducerState::from_raw(state_raw)
                .ok_or(ProtocolError::InvalidHeader("unknown producer state"))?;
            let offset = slot_offset(spec, slot)?;
            let pixels = self.mapping[offset..offset + spec.pixel_bytes()].to_vec();
            fence(Ordering::Acquire);
            let after = generation_atomic(&self.mapping).load(Ordering::Acquire);
            if before == after && after.is_multiple_of(2) {
                return Ok(FrameSnapshot {
                    spec,
                    sequence: after / 2,
                    producer_state: state,
                    pixels,
                });
            }
        }
        Err(ProtocolError::Busy)
    }
}

fn validate_parent(path: &Path) -> Result<(), ProtocolError> {
    let parent = path
        .parent()
        .filter(|candidate| !candidate.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ProtocolError::UnsafePath(
            "frame parent must be a real directory",
        ));
    }
    Ok(())
}

fn initialize_header(mapping: &mut [u8], spec: FrameSpec) {
    mapping.fill(0);
    mapping[0..8].copy_from_slice(&MAGIC);
    write_u32(mapping, OFFSET_VERSION, VERSION);
    write_u32(mapping, OFFSET_HEADER_BYTES, HEADER_BYTES as u32);
    write_u64(mapping, OFFSET_FILE_BYTES, spec.file_bytes);
    write_u32(mapping, OFFSET_WIDTH, spec.width);
    write_u32(mapping, OFFSET_HEIGHT, spec.height);
    write_u32(mapping, OFFSET_STRIDE, spec.stride);
    write_u32(
        mapping,
        OFFSET_PIXEL_FORMAT,
        PIXEL_FORMAT_BGRA8888_PREMULTIPLIED,
    );
    write_u32(mapping, OFFSET_SLOT_COUNT, SLOT_COUNT);
    write_u64(mapping, OFFSET_GENERATION, 0);
    write_u32(mapping, OFFSET_ACTIVE_SLOT, 0);
    write_u32(
        mapping,
        OFFSET_PRODUCER_STATE,
        ProducerState::Starting as u32,
    );
}

fn validate_header(mapping: &[u8]) -> Result<FrameSpec, ProtocolError> {
    if mapping.len() < HEADER_BYTES {
        return Err(ProtocolError::TruncatedHeader);
    }
    if mapping[0..8] != MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let version = read_u32(mapping, OFFSET_VERSION);
    if version != VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    if read_u32(mapping, OFFSET_HEADER_BYTES) != HEADER_BYTES as u32 {
        return Err(ProtocolError::InvalidHeader("header size"));
    }
    if read_u32(mapping, OFFSET_PIXEL_FORMAT) != PIXEL_FORMAT_BGRA8888_PREMULTIPLIED {
        return Err(ProtocolError::InvalidHeader("pixel format"));
    }
    if read_u32(mapping, OFFSET_SLOT_COUNT) != SLOT_COUNT {
        return Err(ProtocolError::InvalidHeader("slot count"));
    }
    let spec = FrameSpec::new(
        read_u32(mapping, OFFSET_WIDTH),
        read_u32(mapping, OFFSET_HEIGHT),
    )?;
    if read_u32(mapping, OFFSET_STRIDE) != spec.stride {
        return Err(ProtocolError::InvalidHeader("stride"));
    }
    if read_u64(mapping, OFFSET_FILE_BYTES) != spec.file_bytes
        || mapping.len() as u64 != spec.file_bytes
    {
        return Err(ProtocolError::InvalidHeader("file size"));
    }
    Ok(spec)
}

fn slot_offset(spec: FrameSpec, slot: u32) -> Result<usize, ProtocolError> {
    if slot >= SLOT_COUNT {
        return Err(ProtocolError::InvalidHeader("active slot"));
    }
    let offset = (HEADER_BYTES as u64)
        .checked_add(
            spec.slot_bytes
                .checked_mul(u64::from(slot))
                .ok_or(ProtocolError::SizeOverflow)?,
        )
        .ok_or(ProtocolError::SizeOverflow)?;
    usize::try_from(offset).map_err(|_| ProtocolError::SizeOverflow)
}

fn generation_atomic(mapping: &[u8]) -> &AtomicU64 {
    debug_assert_eq!(
        (mapping.as_ptr() as usize + OFFSET_GENERATION) % align_of::<AtomicU64>(),
        0
    );
    // SAFETY: mmap bases are page-aligned, the offset is 8-byte aligned, and
    // the protocol reserves this field exclusively for atomic u64 access.
    unsafe { &*(mapping.as_ptr().add(OFFSET_GENERATION).cast::<AtomicU64>()) }
}

fn active_slot_atomic(mapping: &[u8]) -> &AtomicU32 {
    // SAFETY: page-aligned base plus a 4-byte-aligned reserved atomic field.
    unsafe { &*(mapping.as_ptr().add(OFFSET_ACTIVE_SLOT).cast::<AtomicU32>()) }
}

fn state_atomic(mapping: &[u8]) -> &AtomicU32 {
    // SAFETY: page-aligned base plus a 4-byte-aligned reserved atomic field.
    unsafe {
        &*(mapping
            .as_ptr()
            .add(OFFSET_PRODUCER_STATE)
            .cast::<AtomicU32>())
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed-size protocol field"),
    )
}
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed-size protocol field"),
    )
}
fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn unique_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("kwe-frame-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn computes_bounded_layout() {
        let spec = FrameSpec::new(1920, 1080).unwrap();
        assert_eq!(spec.stride, 7680);
        assert_eq!(spec.slot_bytes, 8_294_400);
        assert_eq!(spec.file_bytes, HEADER_BYTES as u64 + 16_588_800);
        assert!(matches!(
            FrameSpec::new(0, 1),
            Err(ProtocolError::InvalidDimensions { .. })
        ));
        assert!(matches!(
            FrameSpec::new(MAX_DIMENSION + 1, 1),
            Err(ProtocolError::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn publishes_and_reads_a_stable_frame() {
        let path = unique_path("roundtrip");
        let spec = FrameSpec::new(8, 4).unwrap();
        let mut writer = SharedFrameWriter::create(&path, spec).unwrap();
        let pixels = vec![0x5a; spec.pixel_bytes()];
        assert_eq!(writer.publish(&pixels).unwrap(), 1);
        let reader = SharedFrameReader::open(&path).unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert_eq!(snapshot.sequence, 1);
        assert_eq!(snapshot.producer_state, ProducerState::Running);
        assert_eq!(snapshot.pixels, pixels);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_corruption_without_losing_process_control() {
        let path = unique_path("corrupt");
        let spec = FrameSpec::new(2, 2).unwrap();
        let mut writer = SharedFrameWriter::create(&path, spec).unwrap();
        writer.publish(&vec![0; spec.pixel_bytes()]).unwrap();
        let reader = SharedFrameReader::open(&path).unwrap();
        writer.corrupt_magic_for_test();
        assert!(matches!(
            reader.snapshot(),
            Err(ProtocolError::InvalidMagic)
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn create_refuses_to_replace_existing_file() {
        let path = unique_path("existing");
        fs::write(&path, b"keep me").unwrap();
        let error = SharedFrameWriter::create(&path, FrameSpec::new(2, 2).unwrap())
            .err()
            .unwrap();
        assert!(
            matches!(error, ProtocolError::Io(ref source) if source.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_snapshots_are_never_torn() {
        let path = unique_path("concurrent");
        let spec = FrameSpec::new(64, 32).unwrap();
        let mut writer = SharedFrameWriter::create(&path, spec).unwrap();
        let reader = SharedFrameReader::open(&path).unwrap();
        let producer = thread::spawn(move || {
            for value in 1_u8..=100 {
                writer.publish(&vec![value; spec.pixel_bytes()]).unwrap();
            }
            writer.set_state(ProducerState::Stopping);
        });
        loop {
            match reader.snapshot() {
                Ok(snapshot) if snapshot.sequence > 0 => {
                    let first = snapshot.pixels[0];
                    assert!(snapshot.pixels.iter().all(|byte| *byte == first));
                    if snapshot.sequence >= 100 {
                        break;
                    }
                }
                Ok(_) | Err(ProtocolError::Busy) => std::hint::spin_loop(),
                Err(error) => panic!("unexpected snapshot failure: {error}"),
            }
        }
        producer.join().unwrap();
        fs::remove_file(path).unwrap();
    }
}
