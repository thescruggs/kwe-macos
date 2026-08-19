// SPDX-License-Identifier: Apache-2.0
// Original scene.pkg archive reader (M3b slice).
//
// Wallpaper Engine scene wallpapers are distributed as a `scene.pkg` archive,
// sometimes next to a `scene.json`, sometimes with `scene.json` as an entry
// inside the pkg. This module reads that archive with the same defensive
// posture as the rest of kwe-core: bounded reads, a TOCTOU-safe open (the
// established scene.rs/read_bytes_limited pattern: lstat, O_NOFOLLOW open,
// fstat re-check on the fd, post-parse size re-check), structural validation
// of the whole table before any payload is touched, and a per-entry cap
// enforced during decompression.
//
// # Verified layout (corpus + public format documentation)
//
// The layout was verified three independent ways: byte-level inspection of
// ~60 real Workshop scene packages (20 distinct PKGV versions), the public
// QuickBMS extractor script (0.1a), and the BSD-3-licensed RePKG
// implementation (behavior reference only — ADR 0001; no code copied). All
// three agree:
//
// ```text
// u32 LE  magic-string length in bytes (8 on the corpus)
// bytes   magic string: b"PKGV" + 4 ASCII digits, e.g. "PKGV0001"
// u32 LE  entry count
//   per entry:
//     u32 LE  path length in bytes
//     bytes   UTF-8 path, e.g. "scene.json"
//     u32 LE  payload offset, relative to the start of the data section
//     u32 LE  payload size in bytes
// data section: raw concatenated payloads
// ```
//
// The QuickBMS script notes "PKGV0001, PKGV0006 and so on are all the same
// format", and the corpus confirms the layout is version-independent: every
// observed version (0001, 0002, 0004, 0005, 0007, 0009, 0011–0024) parses
// with this reader. Any "PKGV" + 4 ASCII digits is therefore accepted; the
// structured UnsupportedVersion error is reserved for PKGV-prefixed magic
// that is not that shape (wrong length, non-digit version).
//
// # Compression ambiguity (honest finding)
//
// The M3b brief stated "LZ4-compressed payloads in the commonly documented
// format". The evidence disproves that premise: none of the 3128 entries in
// the 60-package corpus is compressed (every payload is raw JSON, TEXV0005
// texture data, or raw pixel data — verified with an independent pure-Python
// LZ4 block decoder), and none of the three reference implementations
// decompresses anything. To satisfy both the brief and reality, payloads are
// treated as raw by default and additionally recognized as LZ4 *frames*
// (magic `04 22 4D 18`) at the payload start, decompressed with an output
// cap enforced during decompression — a declared size is never trusted. See
// docs/SCENE_FORMAT_V1.md (M3b section) for the full discussion.
//
// # Path-traversal policy (documented decision)
//
// M3b ships `read_entry` only, never an extract-to-disk API, so traversal
// cannot write outside the package on this slice. The entry table is still
// validated at open: empty paths, NUL bytes, backslashes, absolute paths,
// and `..` components are rejected outright (PathTraversal), because a
// future extractor must not inherit a hostile table. Callers that resolve
// entry paths (e.g. the renderer's script extraction) must additionally
// confine resolution to a directory they own.
//
// # Bounds
//
// | Bound | Value |
// |---|---|
// | package size | 512 MiB (MAX_PKG_BYTES, matches the scene preflight) |
// | entry count | 65 536 (MAX_PKG_ENTRIES) |
// | entry path | 512 bytes (MAX_PKG_PATH_BYTES) |
// | entry payload | 64 MiB (MAX_PKG_ENTRY_BYTES, at read time) |
// | total payload | 512 MiB (checked while parsing the table) |
// | magic length | 32 bytes (hard cap on the length prefix) |

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::preflight::ScenePreflight;

/// Cap on the whole package (mirrors the scene preflight).
pub const MAX_PKG_BYTES: u64 = 512 * 1024 * 1024;
/// Cap on the entry count (the table itself stays well under this too).
pub const MAX_PKG_ENTRIES: u64 = 65_536;
/// Cap on one entry path, in bytes.
pub const MAX_PKG_PATH_BYTES: usize = 512;
/// Per-entry payload cap, enforced at read time (before and during
/// decompression).
pub const MAX_PKG_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Hard cap on the magic-string length prefix.
const MAX_PKG_MAGIC_BYTES: usize = 32;
/// LZ4 frame magic: `\x04\x22\x4D\x18`.
const LZ4_FRAME_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];

/// One validated entry of the package table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgEntry {
    /// Entry path exactly as stored (UTF-8, validated: no `..`, no absolute
    /// path, no backslashes, no NULs, non-empty).
    pub path: String,
    /// Payload offset, relative to the start of the data section.
    pub offset: u64,
    /// Payload size in bytes.
    pub size: u64,
    /// True when the payload starts with the LZ4 frame magic and is
    /// decompressed on read. The corpus stores raw payloads; the frame
    /// path is defensive.
    pub compressed: bool,
}

/// Which part of a package read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgErrorKind {
    /// File system errors (missing file, permission, I/O).
    Read,
    /// Structural corruption: bad magic, truncated data, non-UTF-8 paths,
    /// entry ranges past the end of the package, overflows.
    Format,
    /// PKGV-prefixed magic that is not "PKGV" + exactly 4 ASCII digits.
    UnsupportedVersion,
    /// A path that could escape the package root on extraction (see the
    /// policy note above).
    PathTraversal,
    /// Structurally fine but over a hard limit (package size, entry count,
    /// path length, entry size, total payload).
    Bounds,
}

#[derive(Debug)]
pub struct PkgError {
    pub kind: PkgErrorKind,
    pub message: String,
}

impl PkgError {
    fn new(kind: PkgErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for PkgErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PkgErrorKind::Read => "read error",
            PkgErrorKind::Format => "format error",
            PkgErrorKind::UnsupportedVersion => "unsupported version",
            PkgErrorKind::PathTraversal => "path traversal",
            PkgErrorKind::Bounds => "bounds error",
        })
    }
}

impl fmt::Display for PkgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl StdError for PkgError {}

/// The package table, pinned to the opened file it was parsed from. Reads
/// always go through the fd, never the path, so a swapped file cannot change
/// what the table was validated against.
#[derive(Debug)]
pub struct PkgReader {
    file: fs::File,
    /// Byte offset where the data section starts (right after the table).
    data_start: u64,
    entries: Vec<PkgEntry>,
}

impl PkgReader {
    /// Open and fully validate `path` as a scene.pkg archive.
    ///
    /// The open is TOCTOU-safe the way scene.rs reads are: lstat (reject
    /// symlinks and non-regular files, bound the size), open with O_NOFOLLOW,
    /// fstat re-check on the fd (a path swapped between lstat and open is
    /// caught here), parse the whole table from the fd, then re-check the
    /// fd's size after parsing. No payload bytes are touched at open; the
    /// table is validated structurally (ranges inside the data section,
    /// bounds, paths).
    pub fn open(path: &Path) -> Result<PkgReader, PkgError> {
        let meta = fs::symlink_metadata(path).map_err(|e| {
            PkgError::new(
                PkgErrorKind::Read,
                format!("cannot stat pkg at {}: {e}", path.display()),
            )
        })?;
        if meta.file_type().is_symlink() {
            return Err(PkgError::new(
                PkgErrorKind::Read,
                "pkg entry must not be a symlink",
            ));
        }
        if !meta.file_type().is_file() {
            return Err(PkgError::new(
                PkgErrorKind::Read,
                "pkg entry must be a regular file",
            ));
        }
        if meta.len() > MAX_PKG_BYTES {
            return Err(PkgError::new(
                PkgErrorKind::Bounds,
                format!("pkg exceeds {MAX_PKG_BYTES} byte limit"),
            ));
        }

        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                PkgError::new(
                    PkgErrorKind::Read,
                    format!("cannot open pkg at {}: {e}", path.display()),
                )
            })?;

        // Re-check what was actually opened: the fd is the identity that all
        // later reads and the table validation are pinned to.
        let fd_meta = file
            .metadata()
            .map_err(|e| PkgError::new(PkgErrorKind::Read, format!("cannot fstat pkg: {e}")))?;
        if !fd_meta.file_type().is_file() {
            return Err(PkgError::new(
                PkgErrorKind::Read,
                "pkg entry must be a regular file",
            ));
        }
        let file_len = fd_meta.len();
        if file_len > MAX_PKG_BYTES {
            return Err(PkgError::new(
                PkgErrorKind::Bounds,
                format!("pkg exceeds {MAX_PKG_BYTES} byte limit"),
            ));
        }

        let (entries, data_start) = parse_table(&mut file, file_len, path)?;

        // Re-check the pinned fd's size after parsing: a file that shrank or
        // grew mid-parse invalidates the table we just validated.
        let after = file
            .metadata()
            .map_err(|e| PkgError::new(PkgErrorKind::Read, format!("cannot fstat pkg: {e}")))?;
        if after.len() != file_len {
            return Err(PkgError::new(
                PkgErrorKind::Read,
                "pkg changed size while being read",
            ));
        }

        Ok(PkgReader {
            file,
            data_start,
            entries,
        })
    }

    /// The validated entry table.
    pub fn entries(&self) -> &[PkgEntry] {
        &self.entries
    }

    /// Read one entry's payload, decompressing when the entry is flagged,
    /// bounded to `MAX_PKG_ENTRY_BYTES`.
    pub fn read_entry(&self, idx: usize) -> Result<Vec<u8>, PkgError> {
        self.read_entry_bounded(idx, MAX_PKG_ENTRY_BYTES)
    }

    /// Read one entry's payload with an explicit output cap.
    ///
    /// For raw entries the cap is checked against the declared size before
    /// any allocation. For LZ4 entries the declared size bounds only the
    /// compressed input; the decompressed output is capped *during*
    /// decompression (the frame decoder is wrapped in `take(cap + 1)`), so a
    /// lying content size or a bomb cannot allocate past the cap.
    pub fn read_entry_bounded(&self, idx: usize, cap: u64) -> Result<Vec<u8>, PkgError> {
        let entry = self.entries.get(idx).ok_or_else(|| {
            PkgError::new(
                PkgErrorKind::Bounds,
                format!(
                    "entry index {idx} out of range ({} entries)",
                    self.entries.len()
                ),
            )
        })?;
        if entry.size > cap {
            return Err(PkgError::new(
                PkgErrorKind::Bounds,
                format!(
                    "entry \"{}\" is {} bytes, over the {cap} byte read cap",
                    entry.path, entry.size
                ),
            ));
        }
        let start = self.data_start + entry.offset;
        (&self.file).seek(SeekFrom::Start(start)).map_err(|e| {
            PkgError::new(
                PkgErrorKind::Read,
                format!("cannot seek to entry \"{}\": {e}", entry.path),
            )
        })?;
        let mut raw = Vec::with_capacity(entry.size as usize);
        (&self.file)
            .take(entry.size)
            .read_to_end(&mut raw)
            .map_err(|e| {
                PkgError::new(
                    PkgErrorKind::Read,
                    format!("cannot read entry \"{}\": {e}", entry.path),
                )
            })?;
        if raw.len() as u64 != entry.size {
            return Err(PkgError::new(
                PkgErrorKind::Format,
                format!(
                    "entry \"{}\" truncated: declared {} bytes, read {}",
                    entry.path,
                    entry.size,
                    raw.len()
                ),
            ));
        }
        if entry.compressed {
            return decompress_lz4_bounded(&raw, cap, &format!("entry \"{}\"", entry.path));
        }
        Ok(raw)
    }
}

/// Parse and structurally validate the whole table from `file`, which must
/// be freshly opened at position 0. Returns the entries and the byte offset
/// of the data section (the table's end).
fn parse_table(
    file: &mut fs::File,
    file_len: u64,
    path: &Path,
) -> Result<(Vec<PkgEntry>, u64), PkgError> {
    let mut magic_len_buf = [0u8; 4];
    read_exact(file, &mut magic_len_buf, "magic length")?;
    let magic_len = u32::from_le_bytes(magic_len_buf) as usize;
    if magic_len > MAX_PKG_MAGIC_BYTES {
        return Err(PkgError::new(
            PkgErrorKind::Format,
            format!(
                "pkg magic string length {magic_len} is implausible (max {MAX_PKG_MAGIC_BYTES})"
            ),
        ));
    }
    let mut magic = vec![0u8; magic_len];
    read_exact(file, &mut magic, "magic string")?;
    if !magic.starts_with(b"PKGV") {
        return Err(PkgError::new(
            PkgErrorKind::Format,
            format!("bad pkg magic {:02x?} (expected PKGV####)", magic),
        ));
    }
    if magic.len() != 8 || !magic[4..].iter().all(u8::is_ascii_digit) {
        return Err(PkgError::new(
            PkgErrorKind::UnsupportedVersion,
            format!(
                "unsupported pkg version in magic \"{}\" (expected PKGV + 4 digits)",
                String::from_utf8_lossy(&magic)
            ),
        ));
    }

    let mut count_buf = [0u8; 4];
    read_exact(file, &mut count_buf, "entry count")?;
    let count = u64::from(u32::from_le_bytes(count_buf));
    if count > MAX_PKG_ENTRIES {
        return Err(PkgError::new(
            PkgErrorKind::Bounds,
            format!("pkg entry count {count} exceeds the {MAX_PKG_ENTRIES} entry limit"),
        ));
    }

    let mut entries = Vec::with_capacity(count as usize);
    let mut total_bytes: u64 = 0;
    for _ in 0..count {
        let mut path_len_buf = [0u8; 4];
        read_exact(file, &mut path_len_buf, "entry path length")?;
        let path_len = u32::from_le_bytes(path_len_buf) as usize;
        if path_len > MAX_PKG_PATH_BYTES {
            return Err(PkgError::new(
                PkgErrorKind::Bounds,
                format!("entry path is {path_len} bytes, over the {MAX_PKG_PATH_BYTES} byte limit"),
            ));
        }
        let mut path_bytes = vec![0u8; path_len];
        read_exact(file, &mut path_bytes, "entry path")?;
        let entry_path = String::from_utf8(path_bytes).map_err(|e| {
            PkgError::new(
                PkgErrorKind::Format,
                format!("entry path is not valid UTF-8: {e}"),
            )
        })?;
        validate_path(&entry_path)?;

        let mut offset_buf = [0u8; 4];
        read_exact(file, &mut offset_buf, "entry offset")?;
        let mut size_buf = [0u8; 4];
        read_exact(file, &mut size_buf, "entry size")?;
        let offset = u64::from(u32::from_le_bytes(offset_buf));
        let size = u64::from(u32::from_le_bytes(size_buf));
        total_bytes = total_bytes.checked_add(size).ok_or_else(|| {
            PkgError::new(
                PkgErrorKind::Bounds,
                "entry sizes overflow the total payload bound",
            )
        })?;
        if total_bytes > MAX_PKG_BYTES {
            return Err(PkgError::new(
                PkgErrorKind::Bounds,
                format!(
                    "pkg payloads total {total_bytes} bytes, over the {MAX_PKG_BYTES} byte limit"
                ),
            ));
        }
        entries.push(PkgEntry {
            path: entry_path,
            offset,
            size,
            compressed: false,
        });
    }

    let data_start = file
        .stream_position()
        .map_err(|e| PkgError::new(PkgErrorKind::Read, format!("cannot read pkg position: {e}")))?;
    let data_len = file_len.checked_sub(data_start).ok_or_else(|| {
        PkgError::new(
            PkgErrorKind::Read,
            format!(
                "pkg at {} shrank while its table was being read",
                path.display()
            ),
        )
    })?;
    for entry in &entries {
        let end = entry.offset.checked_add(entry.size).ok_or_else(|| {
            PkgError::new(
                PkgErrorKind::Format,
                format!("entry \"{}\" offset/size overflow", entry.path),
            )
        })?;
        if end > data_len {
            return Err(PkgError::new(
                PkgErrorKind::Format,
                format!(
                    "entry \"{}\" ({}-{}) lies past the end of the pkg data section ({} bytes)",
                    entry.path, entry.offset, end, data_len
                ),
            ));
        }
    }

    // Flag LZ4-frame payloads by their first four bytes. A payload smaller
    // than the magic cannot be a frame. The peek is bounded: 4 bytes per
    // entry, never more than the entry size.
    for entry in &mut entries {
        if entry.size >= 4 {
            let mut peek = [0u8; 4];
            file.seek(SeekFrom::Start(data_start + entry.offset))
                .map_err(|e| {
                    PkgError::new(
                        PkgErrorKind::Read,
                        format!("cannot seek to entry \"{}\": {e}", entry.path),
                    )
                })?;
            read_exact(file, &mut peek, "entry payload prefix")?;
            entry.compressed = peek == LZ4_FRAME_MAGIC;
        }
    }

    Ok((entries, data_start))
}

/// Reject paths that could escape the package root on a future extraction.
fn validate_path(entry_path: &str) -> Result<(), PkgError> {
    if entry_path.is_empty() {
        return Err(PkgError::new(
            PkgErrorKind::PathTraversal,
            "pkg entry path must not be empty",
        ));
    }
    if entry_path.contains('\0') {
        return Err(PkgError::new(
            PkgErrorKind::PathTraversal,
            "pkg entry path contains a NUL byte",
        ));
    }
    if entry_path.contains('\\') {
        return Err(PkgError::new(
            PkgErrorKind::PathTraversal,
            format!("pkg entry path \"{entry_path}\" uses backslashes"),
        ));
    }
    if entry_path.starts_with('/') {
        return Err(PkgError::new(
            PkgErrorKind::PathTraversal,
            format!("pkg entry path \"{entry_path}\" is absolute"),
        ));
    }
    if entry_path.split('/').any(|component| component == "..") {
        return Err(PkgError::new(
            PkgErrorKind::PathTraversal,
            format!("pkg entry path \"{entry_path}\" escapes the package root"),
        ));
    }
    Ok(())
}

/// Read `buf` bytes from the fd, mapping EOF onto a format error so the
/// caller can distinguish "truncated package" from a real I/O failure.
fn read_exact(file: &mut fs::File, buf: &mut [u8], what: &str) -> Result<(), PkgError> {
    file.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            PkgError::new(
                PkgErrorKind::Format,
                format!("pkg file truncated while reading {what}"),
            )
        } else {
            PkgError::new(PkgErrorKind::Read, format!("cannot read pkg {what}: {e}"))
        }
    })
}

/// Decompress an LZ4 frame into at most `cap` bytes. The decoder is wrapped
/// in `take(cap + 1)` so the cap is enforced during decompression; the frame
/// header's declared content size is never trusted.
fn decompress_lz4_bounded(raw: &[u8], cap: u64, what: &str) -> Result<Vec<u8>, PkgError> {
    let mut out = Vec::new();
    let decoder = lz4_flex::frame::FrameDecoder::new(raw);
    decoder.take(cap + 1).read_to_end(&mut out).map_err(|e| {
        PkgError::new(
            PkgErrorKind::Format,
            format!("{what} is not a valid LZ4 frame: {e}"),
        )
    })?;
    if out.len() as u64 > cap {
        return Err(PkgError::new(
            PkgErrorKind::Bounds,
            format!(
                "{what} decompresses to {} bytes, over the {cap} byte cap",
                out.len()
            ),
        ));
    }
    Ok(out)
}

/// Structural preflight for a scene.pkg: the same outer checks as
/// `preflight_scene` (regular non-symlink file, 512 MiB cap) followed by a
/// full table validation. No payload is read or decompressed.
pub fn preflight_pkg(path: &Path) -> ScenePreflight {
    let mut report = ScenePreflight {
        path: path.to_path_buf(),
        safe: false,
        format: "scene-package".into(),
        size_bytes: 0,
        reasons: Vec::new(),
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) => {
            report.reasons.push(format!("cannot stat scene: {error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() {
        report
            .reasons
            .push("scene entry must not be a symlink".into());
        return report;
    }
    if !metadata.is_file() {
        report
            .reasons
            .push("scene entry must be a regular file".into());
        return report;
    }
    report.size_bytes = metadata.len();
    if report.size_bytes > MAX_PKG_BYTES {
        report
            .reasons
            .push(format!("scene exceeds {MAX_PKG_BYTES} byte limit"));
        return report;
    }
    if let Err(error) = PkgReader::open(path) {
        report
            .reasons
            .push(format!("scene package is invalid: {error}"));
        return report;
    }
    report.safe = true;
    report
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::io::Write;

    /// Synthetic pkg fixture builder for tests (original code; mirrors the
    /// corpus layout — raw payloads by default, LZ4 frames on request).
    pub(crate) struct PkgWriter {
        entries: Vec<(String, Vec<u8>)>,
        compressed: Vec<bool>,
    }

    impl PkgWriter {
        pub(crate) fn new() -> Self {
            Self {
                entries: Vec::new(),
                compressed: Vec::new(),
            }
        }

        pub(crate) fn add(&mut self, path: &str, payload: &[u8]) {
            self.entries.push((path.to_owned(), payload.to_vec()));
            self.compressed.push(false);
        }

        /// Store an LZ4 *frame* in the archive (encoded with lz4_flex), so
        /// the reader's defensive frame path is exercised.
        pub(crate) fn add_lz4(&mut self, path: &str, payload: &[u8]) {
            self.entries.push((path.to_owned(), payload.to_vec()));
            self.compressed.push(true);
        }

        /// Serialize with the corpus layout. `version` must be four ASCII
        /// digits.
        pub(crate) fn build(&self, version: &str) -> Vec<u8> {
            assert!(
                version.len() == 4 && version.bytes().all(|b| b.is_ascii_digit()),
                "version must be four ASCII digits"
            );
            let mut out = Vec::new();
            out.extend_from_slice(&8_u32.to_le_bytes());
            out.extend_from_slice(b"PKGV");
            out.extend_from_slice(version.as_bytes());
            out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
            let mut offset: u32 = 0;
            let mut payloads = Vec::with_capacity(self.entries.len());
            for (i, (path, payload)) in self.entries.iter().enumerate() {
                out.extend_from_slice(&(path.len() as u32).to_le_bytes());
                out.extend_from_slice(path.as_bytes());
                let stored: Vec<u8> = if self.compressed[i] {
                    frame_encode(payload)
                } else {
                    payload.clone()
                };
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
                offset = offset
                    .checked_add(stored.len() as u32)
                    .expect("test fixture payloads must fit in u32");
                payloads.push(stored);
            }
            for payload in payloads {
                out.extend_from_slice(&payload);
            }
            out
        }

        pub(crate) fn write(&self, path: &std::path::Path, version: &str) {
            std::fs::write(path, self.build(version)).unwrap();
        }
    }

    fn frame_encode(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = lz4_flex::frame::FrameEncoder::new(&mut out);
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::PkgWriter;
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwe-pkg-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_bytes(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn round_trip_raw_and_compressed_entries() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        let scene_json = br#"{"general":{"clearcolor":[0.1,0.2,0.3,1.0]}}"#;
        let script = b"function init(){Engine.clearcolor=[1,0,0,1];}\n";
        writer.add("scene.json", scene_json);
        writer.add_lz4("script.js", script);
        let pkg = write_bytes(&dir, "scene.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        let entries = reader.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "scene.json");
        assert!(!entries[0].compressed);
        assert_eq!(entries[1].path, "script.js");
        assert!(entries[1].compressed);
        assert_eq!(reader.read_entry(0).unwrap(), scene_json);
        assert_eq!(reader.read_entry(1).unwrap(), script);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn all_corpus_style_versions_accepted() {
        let dir = tmpdir();
        for version in [
            "0001", "0002", "0004", "0005", "0007", "0009", "0011", "0012", "0024",
        ] {
            let mut writer = PkgWriter::new();
            writer.add("scene.json", b"{}");
            let pkg = write_bytes(&dir, &format!("{version}.pkg"), &writer.build(version));
            let reader = PkgReader::open(&pkg).unwrap();
            assert_eq!(reader.entries().len(), 1, "version {version}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_header_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let full = writer.build("0001");
        for cut in [1, 5, 11, 13, 15] {
            let pkg = write_bytes(&dir, &format!("cut{cut}.pkg"), &full[..cut]);
            let error = PkgReader::open(&pkg).unwrap_err();
            assert_eq!(error.kind, PkgErrorKind::Format, "cut at {cut}");
            assert!(
                error.message.contains("truncated"),
                "cut at {cut}: {}",
                error.message
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn truncated_table_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        writer.add("script.js", b"// x");
        let full = writer.build("0001");
        // Cut mid-table: after the first entry's path, before its sizes.
        let cut = 12 + 4 + 4 + "scene.json".len() + 2;
        let pkg = write_bytes(&dir, "cut.pkg", &full[..cut]);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(error.message.contains("truncated"), "{}", error.message);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn payload_extending_past_eof_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let mut bytes = writer.build("0001");
        // Drop the last payload byte: the entry now lies past the data end.
        bytes.truncate(bytes.len() - 1);
        let pkg = write_bytes(&dir, "short.pkg", &bytes);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(
            error
                .message
                .contains("past the end of the pkg data section"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn offset_field_past_eof_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let mut bytes = writer.build("0001");
        // Entry 0's offset field: 4 (magic len) + 8 (magic) + 4 (count)
        // + 4 (path len) + 10 (path) + 4 = byte 34.
        bytes[30..34].copy_from_slice(&u32::MAX.to_le_bytes());
        let pkg = write_bytes(&dir, "far.pkg", &bytes);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(
            error
                .message
                .contains("past the end of the pkg data section"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_entry_rejected_at_read() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        let declared = MAX_PKG_ENTRY_BYTES + 1;
        // 10-byte path: header 12 + count 4 + path_len 4 + path 10
        // + offset 4 + size 4 = byte 34, with the size field at 34..38.
        writer.add("scene.json", &[]);
        let mut bytes = writer.build("0001");
        // Patch the declared size and make the file long enough for the
        // range check by extending it sparsely (nothing is actually read
        // that far: read_entry_bounded rejects before allocating).
        bytes[34..38].copy_from_slice(&(declared as u32).to_le_bytes());
        let pkg = write_bytes(&dir, "big.pkg", &bytes);
        let file = fs::OpenOptions::new().write(true).open(&pkg).unwrap();
        file.set_len(bytes.len() as u64 + declared).unwrap();
        drop(file);
        let reader = PkgReader::open(&pkg).unwrap();
        let error = reader.read_entry(0).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(
            error.message.contains("over the 67108864 byte read cap"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn entry_count_over_limit_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let mut bytes = writer.build("0001");
        bytes[12..16].copy_from_slice(&(MAX_PKG_ENTRIES as u32 + 1).to_le_bytes());
        let pkg = write_bytes(&dir, "many.pkg", &bytes);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(
            error
                .message
                .contains("entry count 65537 exceeds the 65536 entry limit"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn maximum_entry_count_accepted() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        for _ in 0..MAX_PKG_ENTRIES {
            writer.add("e", &[]);
        }
        let pkg = write_bytes(&dir, "max.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        assert_eq!(reader.entries().len(), MAX_PKG_ENTRIES as usize);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_magic_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let mut bytes = writer.build("0001");
        bytes[4..12].copy_from_slice(b"XXXX0001");
        let pkg = write_bytes(&dir, "bad.pkg", &bytes);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(error.message.contains("bad pkg magic"), "{}", error.message);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn implausible_magic_length_rejected() {
        let dir = tmpdir();
        let mut bytes = vec![0_u8; 8];
        bytes[0..4].copy_from_slice(&33_u32.to_le_bytes());
        let pkg = write_bytes(&dir, "huge-magic.pkg", &bytes);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(
            error
                .message
                .contains("magic string length 33 is implausible"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_version_magic_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let cases: [(&str, &[u8]); 3] = [
            ("alpha", b"PKGVABCD"),
            ("long", b"PKGV12345"),
            ("mixed", b"PKGV123A"),
        ];
        for (name, magic) in cases {
            let mut bytes = writer.build("0001");
            bytes[0..4].copy_from_slice(&(magic.len() as u32).to_le_bytes());
            bytes[4..4 + magic.len()].copy_from_slice(magic);
            let pkg = write_bytes(&dir, &format!("{name}.pkg"), &bytes);
            let error = PkgReader::open(&pkg).unwrap_err();
            assert_eq!(error.kind, PkgErrorKind::UnsupportedVersion, "{name}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn traversal_paths_rejected() {
        let dir = tmpdir();
        let hostile = [
            "../evil",
            "a/../../b",
            "/etc/passwd",
            "back\\slash",
            "nul\0byte",
            "",
        ];
        for path in hostile {
            let mut writer = PkgWriter::new();
            writer.add(path, b"x");
            let pkg = write_bytes(&dir, "traversal.pkg", &writer.build("0001"));
            let error = PkgReader::open(&pkg).unwrap_err();
            assert_eq!(error.kind, PkgErrorKind::PathTraversal, "{path:?}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn valid_subdirectory_path_accepted() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scenes/main/scene.json", b"{}");
        let pkg = write_bytes(&dir, "sub.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        assert_eq!(reader.entries()[0].path, "scenes/main/scene.json");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_length_boundary_enforced() {
        let dir = tmpdir();
        let mut ok_writer = PkgWriter::new();
        ok_writer.add(&"a".repeat(MAX_PKG_PATH_BYTES), b"x");
        let pkg = write_bytes(&dir, "ok.pkg", &ok_writer.build("0001"));
        assert!(PkgReader::open(&pkg).is_ok());
        let mut big_writer = PkgWriter::new();
        big_writer.add(&"a".repeat(MAX_PKG_PATH_BYTES + 1), b"x");
        let pkg = write_bytes(&dir, "big.pkg", &big_writer.build("0001"));
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(
            error.message.contains("over the 512 byte limit"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn decompression_bomb_bounded_at_cap() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        // 100 MiB of zeros compresses to a few KB as an LZ4 frame; the
        // reader must refuse to materialize it (cap is 64 MiB).
        writer.add_lz4("bomb.bin", &vec![0_u8; 100 * 1024 * 1024]);
        let pkg = write_bytes(&dir, "bomb.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        assert!(reader.entries()[0].compressed);
        let error = reader.read_entry(0).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        // The frame decoder is capped at cap+1 bytes mid-stream, so the
        // bomb stops at the cap boundary instead of materializing 100 MiB.
        assert!(
            error.message.contains("over the 67108864 byte cap"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lying_frame_magic_rejected_as_format() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        // Starts with the LZ4 frame magic but is not a valid frame.
        writer.add(
            "fake.bin",
            &[0x04, 0x22, 0x4D, 0x18, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00],
        );
        let pkg = write_bytes(&dir, "fake.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        assert!(reader.entries()[0].compressed);
        let error = reader.read_entry(0).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Format);
        assert!(
            error.message.contains("not a valid LZ4 frame"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_package_valid_but_entryless() {
        let dir = tmpdir();
        let writer = PkgWriter::new();
        let pkg = write_bytes(&dir, "empty.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        assert!(reader.entries().is_empty());
        let report = preflight_pkg(&pkg);
        assert!(report.safe);
        assert_eq!(report.format, "scene-package");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_entry_index_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let pkg = write_bytes(&dir, "one.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        let error = reader.read_entry(7).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(error.message.contains("out of range"), "{}", error.message);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_read_cap_enforced_for_raw_entries() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", &vec![b'x'; 1024]);
        let pkg = write_bytes(&dir, "cap.pkg", &writer.build("0001"));
        let reader = PkgReader::open(&pkg).unwrap();
        let error = reader.read_entry_bounded(0, 512).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(
            error.message.contains("over the 512 byte read cap"),
            "{}",
            error.message
        );
        assert_eq!(reader.read_entry_bounded(0, 1024).unwrap().len(), 1024);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn total_payload_over_512_mib_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        // Two entries with 10-byte paths. Entry layout per entry:
        // path_len 4 + path 10 + offset 4 + size 4 = 22 bytes; header
        // + count = 16. Entry a: offset field 30..34, size 34..38.
        // Entry b: offset 52..56, size 56..60.
        writer.add("aaaaaaaaaa", &[]);
        writer.add("bbbbbbbbbb", &[]);
        let mut bytes = writer.build("0001");
        // Overlapping entries both point at the same 300 MiB payload region:
        // the file stays well under the 512 MiB size cap while the *sum* of
        // declared payloads (600 MiB) trips the total-payload check while
        // the table is parsed.
        let each = 300 * 1024 * 1024_u32;
        for (offset_at, size_at) in [(30, 34), (52, 56)] {
            bytes[offset_at..offset_at + 4].copy_from_slice(&0_u32.to_le_bytes());
            bytes[size_at..size_at + 4].copy_from_slice(&each.to_le_bytes());
        }
        // Sparse-extend so both per-entry range checks would pass.
        let pkg = write_bytes(&dir, "total.pkg", &bytes);
        let file = fs::OpenOptions::new().write(true).open(&pkg).unwrap();
        file.set_len(bytes.len() as u64 + u64::from(each)).unwrap();
        drop(file);
        let error = PkgReader::open(&pkg).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Bounds);
        assert!(
            error
                .message
                .contains("pkg payloads total 629145600 bytes, over the 536870912 byte limit"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn symlinked_package_rejected() {
        let dir = tmpdir();
        let mut writer = PkgWriter::new();
        writer.add("scene.json", b"{}");
        let real = write_bytes(&dir, "real.pkg", &writer.build("0001"));
        let link = dir.join("link.pkg");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = PkgReader::open(&link).unwrap_err();
        assert_eq!(error.kind, PkgErrorKind::Read);
        assert!(
            error.message.contains("must not be a symlink"),
            "{}",
            error.message
        );
        let _ = fs::remove_dir_all(dir);
    }
}
