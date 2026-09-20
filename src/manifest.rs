//! The manifest: a walk written to a file as compressed parts, an index of
//! the parts, and a footer.
//!
//! # How a write works
//!
//! 1. Fill a [`WalkOptions`] and a [`WriteOptions`].
//!    [`WriteOptions::default()`] compresses at [`DEFAULT_LEVEL`] with a
//!    [`DEFAULT_WINDOW_LOG`] window on [`DEFAULT_THREADS`] threads.
//! 2. Call [`write_manifest`] with the root, both option sets and a writer.
//!    It walks the root, compresses each part as the walk seals it, and writes
//!    the parts in walk order, then the index, then the footer.
//! 3. Keep the returned [`Written`] if you want the [`Index`] without
//!    reading the file back.
//!
//! # How a read works
//!
//! 1. Load the file, or the tail of it that holds the manifest, into memory.
//! 2. Call [`Manifest::parse`] on the bytes. It reads the footer and the
//!    index and checks that the parts fill the space before the index. It
//!    decompresses no part.
//! 3. Call [`Manifest::part_json`] with a part number to decompress that one
//!    part. Search [`PartEntry::first`] across [`Index::parts`] to find the
//!    parts a directory spans: a directory is one contiguous run of parts.
//!
//! # Examples
//!
//! ```
//! use std::path::Path;
//! use tarseer::{Manifest, WalkOptions, WriteOptions, write_manifest};
//!
//! let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
//! let mut bytes = Vec::new();
//! let written = write_manifest(
//!     &root,
//!     &WalkOptions::default(),
//!     &WriteOptions::default(),
//!     &mut bytes,
//! )
//! .unwrap();
//!
//! let manifest = Manifest::parse(&bytes).unwrap();
//! assert_eq!(manifest.index, written.index);
//! let json = manifest.part_json(0).unwrap();
//! assert!(json.starts_with(b"{\"stem\":[]"));
//! ```
//!
//!
//! # Layout
//!
//! ```text
//! [part 0][part 1]…[part n-1][index][footer]
//! ```
//!
//! Each of these is a zstd *skippable frame*: a frame that a zstd decoder
//! passes over without output. A part frame and the index frame each hold an
//! ordinary compressed zstd frame inside, which only a reader of this layout
//! reaches. A plain `zstd -d` over a manifest therefore outputs nothing, and
//! a manifest appended to an ordinary zstd stream leaves that stream decoding
//! to the same bytes.
//!
//! - A part frame holds the tag `TSPT` and one ordinary zstd frame of the
//!   part's JSON. Each part decompresses on its own.
//! - The index frame holds the tag `TSIX`, [`FORMAT_VERSION`], and one zstd
//!   frame of JSON. That JSON holds one column per field of [`PartEntry`],
//!   with one value per part in walk order, and the [`Skips`].
//! - The footer is [`FOOTER_LEN`] bytes and comes last. It holds the
//!   manifest's length, the index frame's length, [`FORMAT_VERSION`] and
//!   [`MAGIC`].
//!
//! Every offset counts from the manifest's first byte, so bytes placed
//! before the manifest do not change it. Every zstd frame carries a checksum
//! of its content, and a frame whose checksum does not match is refused.
//!
//! The compression level and the window are not recorded in the manifest.
//! Two writes with different settings differ in bytes and read back the same.
//!
//! # Memory
//!
//! Each compression thread keeps one zstd context, one JSON buffer and one
//! output buffer, and reuses them for every part. The context is the largest
//! of the three, and its size follows the level and the window, not the
//! part: see [`DEFAULT_WINDOW_LOG`]. The walk waits when every thread is
//! busy and the queue of sealed parts, one slot per thread, is full.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use error_stack::{Report, ResultExt as _};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::json::Column;
use crate::part::TreePart;
use crate::walk::{Skips, WalkError, WalkOptions, walk_parts};

/// The last eight bytes of every manifest: `TARSEER` and `0x1a`, the byte
/// at which a DOS text viewer stops.
pub const MAGIC: [u8; 8] = *b"TARSEER\x1a";

/// The version of the layout this build writes and reads. [`Manifest::parse`]
/// refuses any other.
pub const FORMAT_VERSION: u16 = 1;

const PART_FRAME_MAGIC: u32 = 0x184D_2A55;
const FOOTER_FRAME_MAGIC: u32 = 0x184D_2A56;
const INDEX_FRAME_MAGIC: u32 = 0x184D_2A57;
const PART_TAG: [u8; 4] = *b"TSPT";
const INDEX_TAG: [u8; 4] = *b"TSIX";

const SKIPPABLE_HEAD_LEN: usize = 8;
const PART_PREFIX_LEN: usize = SKIPPABLE_HEAD_LEN + 4;
const INDEX_PREFIX_LEN: usize = SKIPPABLE_HEAD_LEN + 4 + 2;
// Manifest length, index frame length, format version, magic.
const FOOTER_PAYLOAD_LEN: u32 = 8 + 8 + 2 + 8;

/// The length in bytes of the footer frame, the last thing in every manifest.
pub const FOOTER_LEN: usize = SKIPPABLE_HEAD_LEN + FOOTER_PAYLOAD_LEN as usize;

/// The zstd level of [`WriteOptions::default()`].
pub const DEFAULT_LEVEL: i32 = 9;

/// The number of compression threads of [`WriteOptions::default()`].
pub const DEFAULT_THREADS: usize = 2;

/// The match window of [`WriteOptions::default()`], as a power of two: 512 KiB.
///
/// zstd sizes a compression context from the window, so the window sets how
/// much memory each compression thread holds. At level 9 a context takes
/// 10.5 MiB when zstd chooses the window from the input, 5.5 MiB at a window
/// of 19, and 1.7 MiB at 17. Measured on a walk of `/nix/store` on
/// 2026-09-19, a window of 19 made the manifest 0.19% larger and 17 made it
/// 2.8% larger than zstd's own choice.
pub const DEFAULT_WINDOW_LOG: u32 = 19;

/// The most bytes [`Manifest`] will decompress for one part or for the index:
/// 1 GiB. A frame that declares more is refused, since the declared size comes
/// from the file and the file may be damaged or hostile.
pub const MAX_RAW: u64 = 1 << 30;

/// The manifest could not be written.
///
/// If the walk failed, its [`WalkError`] is inside the report:
/// `report.contains::<WalkError>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError;

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not write the manifest")
    }
}

impl std::error::Error for WriteError {}

/// The bytes are not a manifest this build can read. The report says what
/// does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadError;

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a readable tarseer manifest")
    }
}

impl std::error::Error for ReadError {}

#[derive(Debug)]
struct ZstdFailure(&'static str);

impl fmt::Display for ZstdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "zstd: {}", self.0)
    }
}

impl std::error::Error for ZstdFailure {}

/// One row of the index: where a part is in the manifest and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEntry {
    /// The offset of the part's skippable frame from the manifest's first
    /// byte.
    pub offset: u64,
    /// The length in bytes of the part's skippable frame.
    pub frame_len: u64,
    /// The length in bytes of the part's JSON, before compression.
    pub raw_len: u64,
    /// The number of directory rows in the part.
    pub directories: u64,
    /// The number of file rows in the part.
    pub files: u64,
    /// The number of symlink rows in the part.
    pub symlinks: u64,
    /// The path of the part's first row in walk order. Empty if the part
    /// holds no rows.
    pub first: String,
}

/// The index of a manifest: every part, in walk order, and what the walk
/// skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    /// One entry per part, in walk order.
    pub parts: Vec<PartEntry>,
    /// What the walk skipped.
    pub skips: Skips,
}

impl Index {
    /// Returns the number of rows over every part.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.parts
            .iter()
            .map(|part| part.directories + part.files + part.symlinks)
            .sum()
    }

    /// Writes the index as the JSON document that the index frame holds.
    ///
    /// # Errors
    /// [`WriteError`] if a column cannot be serialized. The types of the
    /// columns rule that out.
    pub fn to_json(&self) -> Result<String, Report<WriteError>> {
        serde_json::to_string(&IndexJson(self)).change_context(WriteError)
    }

    fn from_json(raw: &[u8]) -> Result<Self, Report<ReadError>> {
        let document: serde_json::Value = serde_json::from_slice(raw).change_context(ReadError)?;
        if document["format_version"].as_u64() != Some(u64::from(FORMAT_VERSION)) {
            return corrupt(|| "the index names another format version".to_owned());
        }
        let parts = &document["parts"];
        let column = |name: &str| -> Result<Vec<u64>, Report<ReadError>> {
            let Some(values) = parts[name].as_array() else {
                return corrupt(|| format!("index column {name} is missing"));
            };
            values
                .iter()
                .map(|value| match value.as_u64() {
                    Some(number) => Ok(number),
                    None => corrupt(|| format!("index column {name} holds a non-integer")),
                })
                .collect()
        };
        let offset = column("offset")?;
        let frame_len = column("frame_len")?;
        let raw_len = column("raw_len")?;
        let directories = column("directories")?;
        let files = column("files")?;
        let symlinks = column("symlinks")?;
        let Some(first) = parts["first"].as_array() else {
            return corrupt(|| "index column first is missing".to_owned());
        };
        let count = offset.len();
        if [
            frame_len.len(),
            raw_len.len(),
            directories.len(),
            files.len(),
            symlinks.len(),
            first.len(),
        ]
        .iter()
        .any(|&len| len != count)
        {
            return corrupt(|| "index columns disagree on the number of parts".to_owned());
        }
        let mut entries = Vec::with_capacity(count);
        for row in 0..count {
            let Some(first) = first[row].as_str() else {
                return corrupt(|| "index column first holds a non-string".to_owned());
            };
            entries.push(PartEntry {
                offset: offset[row],
                frame_len: frame_len[row],
                raw_len: raw_len[row],
                directories: directories[row],
                files: files[row],
                symlinks: symlinks[row],
                first: first.to_owned(),
            });
        }

        let skip = |name: &str| -> Result<u32, Report<ReadError>> {
            match document["skips"][name].as_u64().map(u32::try_from) {
                Some(Ok(count)) => Ok(count),
                _ => corrupt(|| format!("index skip count {name} is missing")),
            }
        };
        let skips = Skips {
            special: skip("special")?,
            non_utf8: skip("non_utf8")?,
            unreadable: skip("unreadable")?,
            failed: skip("failed")?,
        };
        Ok(Self {
            parts: entries,
            skips,
        })
    }
}

struct IndexJson<'a>(&'a Index);

impl Serialize for IndexJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let index = self.0;
        let mut out = serializer.serialize_struct("index", 3)?;
        out.serialize_field("format_version", &FORMAT_VERSION)?;
        out.serialize_field("parts", &Parts(&index.parts))?;
        out.serialize_field("skips", &SkipsJson(index.skips))?;
        out.end()
    }
}

struct Parts<'a>(&'a [PartEntry]);

impl Serialize for Parts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let rows = self.0;
        let mut out = serializer.serialize_struct("parts", 7)?;
        out.serialize_field("offset", &Column(|| rows.iter().map(|row| row.offset)))?;
        out.serialize_field(
            "frame_len",
            &Column(|| rows.iter().map(|row| row.frame_len)),
        )?;
        out.serialize_field("raw_len", &Column(|| rows.iter().map(|row| row.raw_len)))?;
        out.serialize_field(
            "directories",
            &Column(|| rows.iter().map(|row| row.directories)),
        )?;
        out.serialize_field("files", &Column(|| rows.iter().map(|row| row.files)))?;
        out.serialize_field("symlinks", &Column(|| rows.iter().map(|row| row.symlinks)))?;
        out.serialize_field("first", &Column(|| rows.iter().map(|row| &row.first)))?;
        out.end()
    }
}

struct SkipsJson(Skips);

impl Serialize for SkipsJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("skips", 4)?;
        out.serialize_field("special", &self.0.special)?;
        out.serialize_field("non_utf8", &self.0.non_utf8)?;
        out.serialize_field("unreadable", &self.0.unreadable)?;
        out.serialize_field("failed", &self.0.failed)?;
        out.end()
    }
}

/// The settings of one write.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// The zstd level for every part and for the index.
    pub level: i32,
    /// The match window for every part and for the index, as a power of two.
    /// `0` lets zstd choose the window from each input. The window sets the
    /// memory each compression thread holds: see [`DEFAULT_WINDOW_LOG`].
    pub window_log: u32,
    /// The number of compression threads. `0` is treated as 1. The bytes
    /// written do not depend on this.
    pub threads: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            level: DEFAULT_LEVEL,
            window_log: DEFAULT_WINDOW_LOG,
            threads: DEFAULT_THREADS,
        }
    }
}

/// What [`write_manifest`] wrote.
#[derive(Debug, Clone)]
pub struct Written {
    /// The index, as the index frame holds it.
    pub index: Index,
    /// The length in bytes of every part's JSON added together, before
    /// compression.
    pub raw_len: u64,
    /// The length in bytes of the whole manifest, footer included.
    pub len: u64,
}

// A part, compressed and framed, waiting for its turn.
struct Framed {
    sequence: usize,
    frame: Vec<u8>,
    raw_len: u64,
    directories: u64,
    files: u64,
    symlinks: u64,
    first: String,
}

/// Walks `root` and writes its manifest to `out`.
///
/// The walk runs on the calling thread. Each part is compressed on one of
/// `options.threads` threads as soon as the walk seals it. `out` receives
/// the parts in walk order, then the index, then the footer. After an error,
/// `out` holds whatever was written before it; nothing is taken back.
///
/// # Errors
/// [`WriteError`] if the walk failed (its [`WalkError`] is inside the
/// report), if a part or the index could not be compressed or framed, or if
/// `out` returned an error.
///
/// # Panics
/// If a compression thread panics. Nothing in one is expected to.
pub fn write_manifest(
    root: &Path,
    walk_options: &WalkOptions<'_>,
    options: &WriteOptions,
    out: &mut (dyn Write + Send),
) -> Result<Written, Report<WriteError>> {
    let threads = options.threads.max(1);
    let level = options.level;
    let window_log = options.window_log;
    let writer_out = &mut *out;

    let (in_order, walked) = thread::scope(|scope| {
        // Bounded, so a walk that outruns compression waits instead of
        // queueing parts without limit.
        let (job_sender, job_receiver) = mpsc::sync_channel::<(usize, TreePart)>(threads);
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let (done_sender, done_receiver) = mpsc::channel();
        for _ in 0..threads {
            let job_receiver = Arc::clone(&job_receiver);
            let done_sender = done_sender.clone();
            scope.spawn(move || {
                let mut compressor = match Compressor::new(level, window_log) {
                    Ok(compressor) => compressor,
                    Err(report) => {
                        let _ = done_sender.send(Err(report));
                        return;
                    }
                };
                loop {
                    let job = job_receiver
                        .lock()
                        .expect("no worker panics holding the job queue")
                        .recv();
                    let Ok((sequence, part)) = job else { break };
                    if done_sender.send(compressor.frame(sequence, &part)).is_err() {
                        break;
                    }
                }
            });
        }
        // Only the workers hold these now: when they have all stopped, the
        // walk's next send fails instead of waiting forever.
        drop(job_receiver);
        drop(done_sender);
        let writer = scope.spawn(move || write_in_order(done_receiver, writer_out));

        let mut sequence = 0;
        let walked = walk_parts(root, walk_options, &mut |part| {
            if job_sender.send((sequence, part)).is_err() {
                // The writer stopped; its own error is the one returned.
                return Err(Report::new(WalkError));
            }
            sequence += 1;
            Ok(())
        });
        drop(job_sender);
        let in_order = writer.join().expect("the manifest writer does not panic");
        (in_order, walked)
    });
    let (parts, parts_len, raw_len) = in_order?;
    let skips = walked.change_context(WriteError)?;

    let index = Index { parts, skips };
    let json = index.to_json()?;
    let blob = zstd(json.as_bytes(), level, window_log)?;
    let index_frame = skippable(
        INDEX_FRAME_MAGIC,
        &[&INDEX_TAG, &FORMAT_VERSION.to_le_bytes(), &blob],
    )?;
    out.write_all(&index_frame).change_context(WriteError)?;

    let index_len = index_frame.len() as u64;
    let len = parts_len + index_len + FOOTER_LEN as u64;
    let mut footer = Vec::with_capacity(FOOTER_LEN);
    footer.extend_from_slice(&FOOTER_FRAME_MAGIC.to_le_bytes());
    footer.extend_from_slice(&FOOTER_PAYLOAD_LEN.to_le_bytes());
    footer.extend_from_slice(&len.to_le_bytes());
    footer.extend_from_slice(&index_len.to_le_bytes());
    footer.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    footer.extend_from_slice(&MAGIC);
    out.write_all(&footer).change_context(WriteError)?;
    out.flush().change_context(WriteError)?;

    Ok(Written {
        index,
        raw_len,
        len,
    })
}

// One worker's reusable state. Only the framed result, a fraction of the
// JSON's size, is allocated per part. Allocating and freeing the context and
// the buffers per part left each thread's glibc arena holding its own copy of
// them, about 13 MB a thread (measured 2026-09-14 on a walk of /nix/store).
struct Compressor {
    context: zstd_safe::CCtx<'static>,
    json: Vec<u8>,
    compressed: Vec<u8>,
}

impl Compressor {
    fn new(level: i32, window_log: u32) -> Result<Self, Report<WriteError>> {
        let mut context = zstd_safe::CCtx::create();
        set_parameters(&mut context, level, window_log)?;
        Ok(Self {
            context,
            json: Vec::new(),
            compressed: Vec::new(),
        })
    }

    fn frame(&mut self, sequence: usize, part: &TreePart) -> Result<Framed, Report<WriteError>> {
        part.write_json(&mut self.json).change_context(WriteError)?;
        self.compressed.clear();
        self.compressed
            .reserve(zstd_safe::compress_bound(self.json.len()));
        // Resets the session, keeps the parameters and the allocated tables.
        self.context
            .compress2(&mut self.compressed, &self.json)
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(WriteError)?;
        Ok(Framed {
            sequence,
            frame: skippable(PART_FRAME_MAGIC, &[&PART_TAG, &self.compressed])?,
            raw_len: self.json.len() as u64,
            directories: part.directories.len() as u64,
            files: part.files.len() as u64,
            symlinks: part.symlinks.len() as u64,
            first: part.first_path().unwrap_or_default(),
        })
    }
}

// Write framed parts as they arrive, holding back any that come early.
// Returns the index rows, the bytes written and the JSON bytes they hold.
fn write_in_order(
    done: Receiver<Result<Framed, Report<WriteError>>>,
    out: &mut (dyn Write + Send),
) -> Result<(Vec<PartEntry>, u64, u64), Report<WriteError>> {
    let mut early = BTreeMap::new();
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut raw_len = 0;
    for framed in done {
        let framed = framed?;
        early.insert(framed.sequence, framed);
        while let Some(framed) = early.remove(&entries.len()) {
            out.write_all(&framed.frame).change_context(WriteError)?;
            let frame_len = framed.frame.len() as u64;
            entries.push(PartEntry {
                offset,
                frame_len,
                raw_len: framed.raw_len,
                directories: framed.directories,
                files: framed.files,
                symlinks: framed.symlinks,
                first: framed.first,
            });
            offset += frame_len;
            raw_len += framed.raw_len;
        }
    }
    if let Some(&sequence) = early.keys().next() {
        return Err(WriteError).attach_with(|| {
            format!(
                "part {} never arrived, but part {sequence} did",
                entries.len()
            )
        });
    }
    Ok((entries, offset, raw_len))
}

fn skippable(magic: u32, pieces: &[&[u8]]) -> Result<Vec<u8>, Report<WriteError>> {
    let len: usize = pieces.iter().map(|piece| piece.len()).sum();
    let payload_len = u32::try_from(len)
        .change_context(WriteError)
        .attach_with(|| format!("{len} bytes is past a skippable frame's 4 GiB"))?;
    let mut frame = Vec::with_capacity(SKIPPABLE_HEAD_LEN + len);
    frame.extend_from_slice(&magic.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    for piece in pieces {
        frame.extend_from_slice(piece);
    }
    Ok(frame)
}

// Sets the level, the window and the checksum flag on `context`. A window of
// 0 leaves the parameter unset, and zstd then chooses the window from the
// input.
fn set_parameters(
    context: &mut zstd_safe::CCtx<'_>,
    level: i32,
    window_log: u32,
) -> Result<(), Report<WriteError>> {
    let mut set = |parameter| {
        context
            .set_parameter(parameter)
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(WriteError)
            .map(|_| ())
    };
    set(zstd_safe::CParameter::CompressionLevel(level))?;
    if window_log != 0 {
        set(zstd_safe::CParameter::WindowLog(window_log))?;
    }
    // A checksum of the decoded content, four bytes a frame. Without it a
    // flipped byte inside a frame can still decode, to JSON that parses with
    // different values. With it the decoder refuses the frame.
    set(zstd_safe::CParameter::ChecksumFlag(true))?;
    Ok(())
}

fn zstd(input: &[u8], level: i32, window_log: u32) -> Result<Vec<u8>, Report<WriteError>> {
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(input.len()));
    let mut context = zstd_safe::CCtx::create();
    set_parameters(&mut context, level, window_log)?;
    context
        .compress2(&mut out, input)
        .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
        .change_context(WriteError)?;
    Ok(out)
}

/// A manifest over bytes held in memory, with its index decoded.
#[derive(Debug, Clone)]
pub struct Manifest<'a> {
    bytes: &'a [u8],
    // Where the manifest starts within `bytes`.
    start: usize,
    /// The index, decoded by [`Manifest::parse`].
    pub index: Index,
}

impl<'a> Manifest<'a> {
    /// Opens the manifest at the end of `bytes`. Any bytes may come before
    /// it.
    ///
    /// This checks the footer and the index frame, decodes the index, and
    /// checks that the parts fill the space before the index exactly. It
    /// decompresses no part.
    ///
    /// # Errors
    /// [`ReadError`] if `bytes` does not end in a manifest of
    /// [`FORMAT_VERSION`], if the index frame is damaged or its checksum does
    /// not match, or if the parts and the footer disagree about the layout.
    /// The report says which.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Report<ReadError>> {
        let Some(footer_at) = bytes.len().checked_sub(FOOTER_LEN) else {
            return corrupt(|| format!("{} bytes is shorter than a footer", bytes.len()));
        };
        let footer = &bytes[footer_at..];
        if le_u32(&footer[0..4]) != FOOTER_FRAME_MAGIC
            || le_u32(&footer[4..8]) != FOOTER_PAYLOAD_LEN
            || footer[26..34] != MAGIC
        {
            return corrupt(|| "no tarseer footer at the end".to_owned());
        }
        let version = le_u16(&footer[24..26]);
        if version != FORMAT_VERSION {
            return corrupt(|| {
                format!("format version {version}, and this build reads {FORMAT_VERSION}")
            });
        }
        let len = le_u64(&footer[8..16]);
        let Some(len) = usize::try_from(len)
            .ok()
            .filter(|&len| (FOOTER_LEN..=bytes.len()).contains(&len))
        else {
            return corrupt(|| format!("the footer claims {len} bytes of manifest"));
        };
        let index_len = le_u64(&footer[16..24]);
        let Some(index_len) = usize::try_from(index_len)
            .ok()
            .filter(|&index_len| index_len >= INDEX_PREFIX_LEN && index_len <= len - FOOTER_LEN)
        else {
            return corrupt(|| format!("the footer claims a {index_len}-byte index"));
        };

        let start = bytes.len() - len;
        let index_at = footer_at - index_len;
        let frame = &bytes[index_at..footer_at];
        check_frame(frame, INDEX_FRAME_MAGIC, INDEX_TAG)?;
        if le_u16(&frame[12..14]) != FORMAT_VERSION {
            return corrupt(|| "the index frame names another format version".to_owned());
        }
        let raw = unzstd(&frame[INDEX_PREFIX_LEN..], None)?;
        let index = Index::from_json(&raw).attach_with(|| "in the index".to_owned())?;

        let mut offset: u64 = 0;
        for (number, part) in index.parts.iter().enumerate() {
            if part.offset != offset {
                return corrupt(|| format!("part {number} is not where the one before it ends"));
            }
            let Some(end) = offset.checked_add(part.frame_len) else {
                return corrupt(|| format!("part {number} claims an impossible length"));
            };
            offset = end;
        }
        if offset != (index_at - start) as u64 {
            return corrupt(|| "the parts do not end where the index begins".to_owned());
        }
        Ok(Self {
            bytes,
            start,
            index,
        })
    }

    /// Decompresses part `number` and returns its JSON.
    ///
    /// # Errors
    /// [`ReadError`] if there is no part `number`, if the part's frame is
    /// not tagged as a part, if the frame is damaged or its checksum does
    /// not match, or if the frame does not decompress to the length the
    /// index gives for it.
    pub fn part_json(&self, number: usize) -> Result<Vec<u8>, Report<ReadError>> {
        let Some(entry) = self.index.parts.get(number) else {
            return corrupt(|| format!("there is no part {number}"));
        };
        // Both fit: `parse` checked the parts tile bytes that are in memory.
        let at = self.start + usize::try_from(entry.offset).change_context(ReadError)?;
        let len = usize::try_from(entry.frame_len).change_context(ReadError)?;
        let frame = &self.bytes[at..at + len];
        check_frame(frame, PART_FRAME_MAGIC, PART_TAG).attach_with(|| format!("part {number}"))?;
        unzstd(&frame[PART_PREFIX_LEN..], Some(entry.raw_len))
            .attach_with(|| format!("part {number}"))
    }
}

fn corrupt<T>(what: impl FnOnce() -> String) -> Result<T, Report<ReadError>> {
    Err(ReadError).attach_with(what)
}

fn check_frame(frame: &[u8], magic: u32, tag: [u8; 4]) -> Result<(), Report<ReadError>> {
    if frame.len() < SKIPPABLE_HEAD_LEN + tag.len()
        || le_u32(&frame[0..4]) != magic
        || le_u32(&frame[4..8]) as usize != frame.len() - SKIPPABLE_HEAD_LEN
        || frame[8..12] != tag
    {
        return corrupt(|| {
            format!(
                "expected a {}-tagged frame here",
                String::from_utf8_lossy(&tag)
            )
        });
    }
    Ok(())
}

// Decompress exactly one zstd frame, holding it to the size it declares, to
// `expected` when the caller knows it, and to `MAX_RAW` always.
fn unzstd(blob: &[u8], expected: Option<u64>) -> Result<Vec<u8>, Report<ReadError>> {
    let compressed = zstd_safe::find_frame_compressed_size(blob)
        .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
        .change_context(ReadError)?;
    if compressed != blob.len() {
        return corrupt(|| "bytes follow the zstd frame inside its skippable frame".to_owned());
    }
    let Ok(Some(declared)) = zstd_safe::get_frame_content_size(blob) else {
        return corrupt(|| "the zstd frame does not declare its size".to_owned());
    };
    if expected.is_some_and(|expected| expected != declared) || declared > MAX_RAW {
        return corrupt(|| format!("the zstd frame declares {declared} bytes"));
    }
    let declared = usize::try_from(declared).change_context(ReadError)?;
    let mut out = Vec::with_capacity(declared);
    let produced = zstd_safe::decompress(&mut out, blob)
        .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
        .change_context(ReadError)?;
    if produced != declared {
        return corrupt(|| format!("the zstd frame produced {produced} of {declared} bytes"));
    }
    Ok(out)
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("two bytes"))
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}
