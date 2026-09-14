// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! The manifest on disk: every part as its own zstd frame, an index of the
//! parts, and a footer that locates both.
//!
//! ```text
//! [part 0][part 1]…[part n-1][index][footer]
//! ```
//!
//! - every piece is a zstd skippable frame, so a plain zstd decoder skips the
//!   whole manifest; once a payload sits in front, what it yields is exactly
//!   the payload
//! - a part frame: a tag, then one ordinary zstd frame of the part's JSON;
//!   parts decompress independently and in any order
//! - the index frame: a tag, the format version, then one zstd frame of
//!   columnar JSON with one row per part, in walk order
//! - the footer: fixed size and last, so a reader starts from the tail
//! - offsets count from the manifest's first byte, so the same bytes can follow
//!   a payload unchanged
//! - parts are compressed on worker threads while the walk goes on, and put
//!   back in order by the writer; a full job queue holds the walk back
//! - a worker keeps one zstd context, one JSON buffer and one output buffer for
//!   every part it compresses: allocating and freeing them per part left each
//!   thread's glibc arena holding its own copy, ~13 MB a thread
//! - one level for every part, not recorded anywhere a reader looks, so not a
//!   format property
//!
//! # Defaults, measured
//!
//! `/nix/store`: 1.7 million entries, 91 parts, 67 MB of JSON, a 2.7 s walk.
//!
//! | level | manifest | compress CPU | peak RSS (2 threads) |
//! |---|---|---|---|
//! | 6  | 10.95 MB | 1.2 s  | 41 MB  |
//! | 9  | 10.73 MB | 1.6 s  | 59 MB  |
//! | 15 | 10.59 MB | 3.0 s  | 137 MB |
//! | 19 | 9.88 MB  | 12.1 s | 149 MB |
//!
//! - level 9: past it, size barely moves while CPU and workspace memory climb
//! - not by size, as a single whole-tree manifest would be: a third of the
//!   parts crossed a 1 MiB threshold, and a 19-below/3-above rule came out
//!   larger than a flat 9 for twice the CPU
//! - 2 threads: the serial walk seals a part every ~30 ms, one thread
//!   compresses it in ~15 ms, and each extra thread keeps a zstd workspace;
//!   on 22 threads peak RSS was 314 MB for the same wall time

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
use crate::part::Part;
use crate::walk::{Skips, WalkError, WalkOptions, walk_parts};

/// The last eight bytes of every manifest: the name, then `0x1a`, the
/// end-of-file byte a text viewer stops at.
pub const MAGIC: [u8; 8] = *b"TARSEER\x1a";

/// Bumped by any change to the bytes; a mismatch is refused, not guessed at.
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

/// Bytes of the footer frame, the last thing in every manifest.
pub const FOOTER_LEN: usize = SKIPPABLE_HEAD_LEN + FOOTER_PAYLOAD_LEN as usize;

/// zstd level for every part and the index.
pub const DEFAULT_LEVEL: i32 = 9;

/// Compression threads unless told otherwise.
pub const DEFAULT_THREADS: usize = 2;

/// Most JSON a reader will decompress for one part or the index. The sizes
/// come from the file, and the file may be hostile.
pub const MAX_RAW: u64 = 1 << 30;

/// The manifest could not be written.
///
/// - a failed walk is inside it: `report.contains::<WalkError>()`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError;

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not write the manifest")
    }
}

impl std::error::Error for WriteError {}

/// The bytes are not a manifest this build can read; what is wrong is
/// attached.
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

/// One row of the index: where a part is and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEntry {
    /// Start of the part's skippable frame, from the manifest's first byte.
    pub offset: u64,
    /// Bytes of the whole skippable frame.
    pub frame_len: u64,
    /// Bytes of its JSON.
    pub raw_len: u64,
    pub dirs: u64,
    pub files: u64,
    pub links: u64,
    /// Path of its first row in walk order. Searching this column finds the
    /// parts a folder spans, since a folder is one contiguous run.
    pub first: String,
}

/// Every part, in walk order, and what the walk skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    pub parts: Vec<PartEntry>,
    pub skips: Skips,
}

impl Index {
    /// Rows across every part.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.parts
            .iter()
            .map(|part| part.dirs + part.files + part.links)
            .sum()
    }

    /// The index's JSON.
    ///
    /// # Errors
    /// [`WriteError`] if it cannot be serialized, which its types rule out.
    pub fn to_json(&self) -> Result<String, Report<WriteError>> {
        serde_json::to_string(&IndexJson(self)).change_context(WriteError)
    }

    fn from_json(raw: &[u8]) -> Result<Self, Report<ReadError>> {
        let doc: serde_json::Value = serde_json::from_slice(raw).change_context(ReadError)?;
        if doc["format_version"].as_u64() != Some(u64::from(FORMAT_VERSION)) {
            return corrupt(|| "the index names another format version".to_owned());
        }
        let parts = &doc["parts"];
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
        let dirs = column("dirs")?;
        let files = column("files")?;
        let links = column("links")?;
        let Some(first) = parts["first"].as_array() else {
            return corrupt(|| "index column first is missing".to_owned());
        };
        let count = offset.len();
        if [
            frame_len.len(),
            raw_len.len(),
            dirs.len(),
            files.len(),
            links.len(),
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
                dirs: dirs[row],
                files: files[row],
                links: links[row],
                first: first.to_owned(),
            });
        }

        let skip = |name: &str| -> Result<u32, Report<ReadError>> {
            match doc["skips"][name].as_u64().map(u32::try_from) {
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
        out.serialize_field("dirs", &Column(|| rows.iter().map(|row| row.dirs)))?;
        out.serialize_field("files", &Column(|| rows.iter().map(|row| row.files)))?;
        out.serialize_field("links", &Column(|| rows.iter().map(|row| row.links)))?;
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

/// How to write.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// zstd level for every part and the index.
    pub level: i32,
    /// Compression threads. The bytes written do not depend on it.
    pub threads: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            level: DEFAULT_LEVEL,
            threads: DEFAULT_THREADS,
        }
    }
}

/// What a write produced.
#[derive(Debug, Clone)]
pub struct Written {
    pub index: Index,
    /// Bytes of JSON across every part, before compression.
    pub raw_len: u64,
    /// Bytes of the whole manifest, footer included.
    pub len: u64,
}

// A part, compressed and framed, waiting for its turn.
struct Framed {
    seq: usize,
    frame: Vec<u8>,
    raw_len: u64,
    dirs: u64,
    files: u64,
    links: u64,
    first: String,
}

/// Walk `root` and write its manifest to `out`.
///
/// - parts are compressed on `options.threads` threads as the walk seals them
/// - `out` sees the parts in walk order, then the index, then the footer
/// - on failure `out` holds a prefix; nothing is taken back
///
/// # Errors
/// [`WriteError`]: the walk failed (inside it), a part or the index could not
/// be compressed or framed, or `out` refused a write.
///
/// # Panics
/// If a compression thread panics, which nothing in it is expected to do.
pub fn write_manifest(
    root: &Path,
    walk_options: &WalkOptions<'_>,
    options: &WriteOptions,
    out: &mut (dyn Write + Send),
) -> Result<Written, Report<WriteError>> {
    let threads = options.threads.max(1);
    let level = options.level;
    let writer_out = &mut *out;

    let (in_order, walked) = thread::scope(|scope| {
        // Bounded, so a walk that outruns compression waits instead of
        // queueing parts without limit.
        let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Part)>(threads);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (done_tx, done_rx) = mpsc::channel();
        for _ in 0..threads {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            scope.spawn(move || {
                let mut compressor = match Compressor::new(level) {
                    Ok(compressor) => compressor,
                    Err(report) => {
                        let _ = done_tx.send(Err(report));
                        return;
                    }
                };
                loop {
                    let job = job_rx
                        .lock()
                        .expect("no worker panics holding the job queue")
                        .recv();
                    let Ok((seq, part)) = job else { break };
                    if done_tx.send(compressor.frame(seq, &part)).is_err() {
                        break;
                    }
                }
            });
        }
        // Only the workers hold these now: when they have all stopped, the
        // walk's next send fails instead of waiting forever.
        drop(job_rx);
        drop(done_tx);
        let writer = scope.spawn(move || write_in_order(done_rx, writer_out));

        let mut seq = 0;
        let walked = walk_parts(root, walk_options, &mut |part| {
            if job_tx.send((seq, part)).is_err() {
                // The writer stopped; its own error is the one returned.
                return Err(Report::new(WalkError));
            }
            seq += 1;
            Ok(())
        });
        drop(job_tx);
        let in_order = writer.join().expect("the manifest writer does not panic");
        (in_order, walked)
    });
    let (parts, parts_len, raw_len) = in_order?;
    let skips = walked.change_context(WriteError)?;

    let index = Index { parts, skips };
    let json = index.to_json()?;
    let blob = zstd(json.as_bytes(), level)?;
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
// JSON's size, is allocated per part.
struct Compressor {
    context: zstd_safe::CCtx<'static>,
    json: Vec<u8>,
    compressed: Vec<u8>,
}

impl Compressor {
    fn new(level: i32) -> Result<Self, Report<WriteError>> {
        let mut context = zstd_safe::CCtx::create();
        context
            .set_parameter(zstd_safe::CParameter::CompressionLevel(level))
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(WriteError)?;
        Ok(Self {
            context,
            json: Vec::new(),
            compressed: Vec::new(),
        })
    }

    fn frame(&mut self, seq: usize, part: &Part) -> Result<Framed, Report<WriteError>> {
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
            seq,
            frame: skippable(PART_FRAME_MAGIC, &[&PART_TAG, &self.compressed])?,
            raw_len: self.json.len() as u64,
            dirs: part.dirs.len() as u64,
            files: part.files.len() as u64,
            links: part.links.len() as u64,
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
        early.insert(framed.seq, framed);
        while let Some(framed) = early.remove(&entries.len()) {
            out.write_all(&framed.frame).change_context(WriteError)?;
            let frame_len = framed.frame.len() as u64;
            entries.push(PartEntry {
                offset,
                frame_len,
                raw_len: framed.raw_len,
                dirs: framed.dirs,
                files: framed.files,
                links: framed.links,
                first: framed.first,
            });
            offset += frame_len;
            raw_len += framed.raw_len;
        }
    }
    if let Some(&seq) = early.keys().next() {
        return Err(WriteError)
            .attach_with(|| format!("part {} never arrived, but part {seq} did", entries.len()));
    }
    Ok((entries, offset, raw_len))
}

fn skippable(magic: u32, pieces: &[&[u8]]) -> Result<Vec<u8>, Report<WriteError>> {
    let len: usize = pieces.iter().map(|piece| piece.len()).sum();
    let len_u32 = u32::try_from(len)
        .change_context(WriteError)
        .attach_with(|| format!("{len} bytes is past a skippable frame's 4 GiB"))?;
    let mut frame = Vec::with_capacity(SKIPPABLE_HEAD_LEN + len);
    frame.extend_from_slice(&magic.to_le_bytes());
    frame.extend_from_slice(&len_u32.to_le_bytes());
    for piece in pieces {
        frame.extend_from_slice(piece);
    }
    Ok(frame)
}

fn zstd(input: &[u8], level: i32) -> Result<Vec<u8>, Report<WriteError>> {
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(input.len()));
    zstd_safe::compress(&mut out, input, level)
        .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
        .change_context(WriteError)?;
    Ok(out)
}

/// A manifest over bytes held in memory.
#[derive(Debug, Clone)]
pub struct Manifest<'a> {
    bytes: &'a [u8],
    // Where the manifest starts within `bytes`.
    start: usize,
    pub index: Index,
}

impl<'a> Manifest<'a> {
    /// Open the manifest that ends `bytes`. Anything may come before it.
    ///
    /// - checks the footer, the index frame, and that the parts tile the
    ///   space before the index exactly
    /// - does not decompress any part
    ///
    /// # Errors
    /// [`ReadError`], saying what does not hold.
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

    /// The JSON of part `number`, decompressed.
    ///
    /// # Errors
    /// [`ReadError`]: no such part, or its frame is not what the index says.
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
