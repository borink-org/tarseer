//! The zstd codec, and a manifest as one zstd file.
//!
//! # How a write works
//!
//! 1. Fill a [`Zstd`]. [`Zstd::default()`] compresses at [`DEFAULT_LEVEL`]
//!    with a [`DEFAULT_WINDOW_LOG`] window.
//! 2. Call [`write_file`] with the root, a [`WalkOptions`], the codec, a
//!    thread count and a writer.
//!
//! # How a read works
//!
//! 1. Load the file into memory and call [`SeekableFile::parse`]. It reads
//!    the seek table and the index, and decompresses no part.
//! 2. Call [`SeekableFile::part_json`] with a part number to decompress that
//!    one part.
//!
//! # Examples
//!
//! ```
//! use std::path::Path;
//! use tarseer::WalkOptions;
//! use tarseer::zstd::{SeekableFile, Zstd, write_file};
//!
//! let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
//! let mut bytes = Vec::new();
//! let written = write_file(&root, &WalkOptions::default(), &Zstd::default(), 2, &mut bytes).unwrap();
//!
//! let file = SeekableFile::parse(&bytes).unwrap();
//! assert_eq!(file.index, written.index);
//! let json = file.part_json(0).unwrap();
//! assert!(json.starts_with(b"{\"stem\":[]"));
//! ```
//!
//! # Layout
//!
//! ```text
//! [part 0][part 1]…[part n-1][index][seek table]
//! ```
//!
//! Each part and the index is one ordinary zstd frame, as [`write_frames`]
//! hands it out. `zstd -dc` over the file therefore prints a JSON Lines
//! document: one line per part, then the index. Every frame carries a
//! checksum of its content, and a frame whose checksum does not match is
//! refused.
//!
//! The seek table is the one from zstd's seekable format. It is a skippable
//! frame, which a zstd decoder passes over without output. It lists the
//! compressed and decompressed size of every frame before it, and ends in the
//! number of frames and the magic number `0x8F92EAB1`.
//!
//! The level and the window are not recorded. Two writes with different
//! settings differ in bytes and read back the same.
//!
//! # Memory
//!
//! zstd sizes a compression context from the level and the window, not from
//! the input, and each worker thread keeps one: see [`DEFAULT_WINDOW_LOG`].

use std::fmt;
use std::io::Write;
use std::path::Path;

use error_stack::{Report, ResultExt as _};

use crate::frames::{
    Codec, Encoder, Frame, ReadError, WriteError, decode_index, decode_line, write_frames,
};
use crate::manifest::Index;
use crate::walk::WalkOptions;

/// The zstd level of [`Zstd::default()`].
pub const DEFAULT_LEVEL: i32 = 9;

/// The match window of [`Zstd::default()`], as a power of two: 512 KiB.
///
/// zstd sizes a compression context from the window, so the window sets how
/// much memory each compression thread holds. At level 9 a context takes
/// 10.5 MiB when zstd chooses the window from the input, 5.5 MiB at a window
/// of 19, and 1.7 MiB at 17. Measured on a walk of `/nix/store` on
/// 2026-09-19, a window of 19 made the output 0.19% larger and 17 made it
/// 2.8% larger than zstd's own choice.
pub const DEFAULT_WINDOW_LOG: u32 = 19;

/// The most bytes one frame may decompress to: 1 GiB, which is also the limit
/// of the seekable format. A frame that declares more is refused, since the
/// declared size comes from the file and the file may be damaged or hostile.
pub const MAX_RAW: u64 = 1 << 30;

const SKIPPABLE_HEAD_LEN: usize = 8;
const SEEK_TABLE_MAGIC: u32 = 0x184D_2A5E;
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
const SEEK_TABLE_FOOTER_LEN: usize = 9;
// Compressed size and decompressed size. A third field, a checksum, is
// present when the descriptor's top bit is set; this crate does not write it.
const SEEK_TABLE_ENTRY_LEN: usize = 8;

#[derive(Debug)]
struct ZstdFailure(&'static str);

impl fmt::Display for ZstdFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "zstd: {}", self.0)
    }
}

impl std::error::Error for ZstdFailure {}

/// The zstd codec: every frame is one zstd frame with a content checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zstd {
    /// The compression level.
    pub level: i32,
    /// The match window, as a power of two. `0` lets zstd choose the window
    /// from each input. The window sets the memory each compression thread
    /// holds: see [`DEFAULT_WINDOW_LOG`].
    pub window_log: u32,
}

impl Default for Zstd {
    fn default() -> Self {
        Self {
            level: DEFAULT_LEVEL,
            window_log: DEFAULT_WINDOW_LOG,
        }
    }
}

struct ZstdEncoder(zstd_safe::CCtx<'static>);

impl Codec for Zstd {
    fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
        let mut context = zstd_safe::CCtx::create();
        let mut set = |parameter| {
            context
                .set_parameter(parameter)
                .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
                .change_context(WriteError)
                .map(|_| ())
        };
        set(zstd_safe::CParameter::CompressionLevel(self.level))?;
        // A window of 0 leaves the parameter unset, and zstd then chooses the
        // window from the input.
        if self.window_log != 0 {
            set(zstd_safe::CParameter::WindowLog(self.window_log))?;
        }
        // A checksum of the decoded content, four bytes a frame. Without it a
        // flipped byte inside a frame can still decode, to JSON that parses
        // with different values. With it the decoder refuses the frame.
        set(zstd_safe::CParameter::ChecksumFlag(true))?;
        Ok(Box::new(ZstdEncoder(context)))
    }

    fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
        let compressed = zstd_safe::find_frame_compressed_size(frame)
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(ReadError)?;
        if compressed != frame.len() {
            return corrupt(|| "bytes follow the zstd frame".to_owned());
        }
        let Ok(Some(declared)) = zstd_safe::get_frame_content_size(frame) else {
            return corrupt(|| "the zstd frame does not declare its size".to_owned());
        };
        if declared > MAX_RAW {
            return corrupt(|| format!("the zstd frame declares {declared} bytes"));
        }
        let declared = usize::try_from(declared).change_context(ReadError)?;
        let mut out = Vec::with_capacity(declared);
        let produced = zstd_safe::decompress(&mut out, frame)
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(ReadError)?;
        if produced != declared {
            return corrupt(|| format!("the zstd frame produced {produced} of {declared} bytes"));
        }
        Ok(out)
    }
}

impl Encoder for ZstdEncoder {
    fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
        frame.clear();
        frame.reserve(zstd_safe::compress_bound(raw.len()));
        // Resets the session, keeps the parameters and the allocated tables.
        self.0
            .compress2(frame, raw)
            .map_err(|code| ZstdFailure(zstd_safe::get_error_name(code)))
            .change_context(WriteError)?;
        Ok(())
    }
}

/// What [`write_file`] wrote.
#[derive(Debug, Clone)]
pub struct Written {
    /// The index, as the last frame holds it.
    pub index: Index,
    /// The length in bytes of the JSON Lines document the file decompresses
    /// to.
    pub raw_len: u64,
    /// The length in bytes of the whole file, seek table included.
    pub len: u64,
}

/// Walks `root` and writes its manifest to `out` as one seekable zstd file.
///
/// `out` receives the frames of [`write_frames`] in order, then the seek
/// table. After an error, `out` holds whatever was written before it.
///
/// # Errors
/// [`WriteError`] as [`write_frames`], if a frame passes the 4 GiB that a
/// seek table entry can record, or if `out` returned an error.
///
/// # Panics
/// As [`write_frames`].
pub fn write_file(
    root: &Path,
    walk_options: &WalkOptions<'_>,
    codec: &Zstd,
    threads: usize,
    out: &mut (dyn Write + Send),
) -> Result<Written, Report<WriteError>> {
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    let index = {
        let frame_out = &mut *out;
        write_frames(root, walk_options, codec, threads, &mut |frame: Frame| {
            let size = |len: u64| {
                u32::try_from(len)
                    .change_context(WriteError)
                    .attach_with(|| format!("a frame of {len} bytes is past a seek table entry"))
            };
            sizes.push((size(frame.bytes.len() as u64)?, size(frame.raw_len)?));
            frame_out.write_all(&frame.bytes).change_context(WriteError)
        })?
    };
    let table = seek_table(&sizes)?;
    out.write_all(&table).change_context(WriteError)?;
    out.flush().change_context(WriteError)?;

    let sum = |pick: fn(&(u32, u32)) -> u32| sizes.iter().map(|size| u64::from(pick(size))).sum();
    let compressed: u64 = sum(|size| size.0);
    Ok(Written {
        index,
        raw_len: sum(|size| size.1),
        len: compressed + table.len() as u64,
    })
}

// The seek table of zstd's seekable format, without per-frame checksums.
fn seek_table(sizes: &[(u32, u32)]) -> Result<Vec<u8>, Report<WriteError>> {
    let payload_len = sizes.len() * SEEK_TABLE_ENTRY_LEN + SEEK_TABLE_FOOTER_LEN;
    let (Ok(payload_len_u32), Ok(frames)) =
        (u32::try_from(payload_len), u32::try_from(sizes.len()))
    else {
        return Err(WriteError)
            .attach_with(|| format!("{} frames are past a seek table", sizes.len()));
    };
    let mut table = Vec::with_capacity(SKIPPABLE_HEAD_LEN + payload_len);
    table.extend_from_slice(&SEEK_TABLE_MAGIC.to_le_bytes());
    table.extend_from_slice(&payload_len_u32.to_le_bytes());
    for (compressed, raw) in sizes {
        table.extend_from_slice(&compressed.to_le_bytes());
        table.extend_from_slice(&raw.to_le_bytes());
    }
    table.extend_from_slice(&frames.to_le_bytes());
    table.push(0);
    table.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
    Ok(table)
}

// Where one frame lies in the file, and what it decompresses to.
#[derive(Debug, Clone, Copy)]
struct FrameAt {
    start: usize,
    len: usize,
    raw_len: u32,
}

/// A manifest file held in memory, with its index decoded.
#[derive(Debug, Clone)]
pub struct SeekableFile<'a> {
    bytes: &'a [u8],
    // Every frame before the seek table. The last one is the index.
    frames: Vec<FrameAt>,
    /// The index, decoded by [`SeekableFile::parse`].
    pub index: Index,
}

impl<'a> SeekableFile<'a> {
    /// Opens the manifest that `bytes` holds.
    ///
    /// This reads the seek table, checks that its frames fill the file
    /// exactly, and decodes the index. It decompresses no part.
    ///
    /// # Errors
    /// [`ReadError`] if `bytes` does not end in a seek table, if the table
    /// and the file disagree about the layout, if the index frame is damaged
    /// or its checksum does not match, or if the index lists a different
    /// number of parts than the file holds. The report says which.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Report<ReadError>> {
        let Some(footer_at) = bytes.len().checked_sub(SEEK_TABLE_FOOTER_LEN) else {
            return corrupt(|| format!("{} bytes is shorter than a seek table", bytes.len()));
        };
        let footer = &bytes[footer_at..];
        if le_u32(&footer[5..9]) != SEEKABLE_MAGIC {
            return corrupt(|| "no seek table at the end".to_owned());
        }
        let descriptor = footer[4];
        if descriptor & 0x7c != 0 {
            return corrupt(|| "the seek table sets reserved bits".to_owned());
        }
        let entry_len = SEEK_TABLE_ENTRY_LEN + if descriptor & 0x80 == 0 { 0 } else { 4 };
        let count = le_u32(&footer[0..4]) as usize;
        let Some(table_at) = count
            .checked_mul(entry_len)
            .and_then(|entries| entries.checked_add(SKIPPABLE_HEAD_LEN + SEEK_TABLE_FOOTER_LEN))
            .and_then(|table_len| bytes.len().checked_sub(table_len))
        else {
            return corrupt(|| format!("the seek table claims {count} frames"));
        };
        let table = &bytes[table_at..];
        if le_u32(&table[0..4]) != SEEK_TABLE_MAGIC
            || le_u32(&table[4..8]) as usize != table.len() - SKIPPABLE_HEAD_LEN
        {
            return corrupt(|| "the seek table is not a skippable frame".to_owned());
        }

        let mut frames = Vec::with_capacity(count);
        let mut start = 0usize;
        for entry in table[SKIPPABLE_HEAD_LEN..footer_at - table_at].chunks_exact(entry_len) {
            let len = le_u32(&entry[0..4]) as usize;
            frames.push(FrameAt {
                start,
                len,
                raw_len: le_u32(&entry[4..8]),
            });
            start = match start.checked_add(len) {
                Some(end) if end <= table_at => end,
                _ => return corrupt(|| "the frames run past the seek table".to_owned()),
            };
        }
        if start != table_at {
            return corrupt(|| "the frames do not end where the seek table begins".to_owned());
        }
        let Some((index_at, parts)) = frames.split_last() else {
            return corrupt(|| "the file holds no index frame".to_owned());
        };

        let index = decode_index(&Zstd::default(), &bytes[index_at.start..table_at])
            .attach_with(|| "in the index".to_owned())?;
        if index.parts.len() != parts.len() {
            return corrupt(|| {
                format!(
                    "the index lists {} parts and the file holds {}",
                    index.parts.len(),
                    parts.len()
                )
            });
        }
        Ok(Self {
            bytes,
            frames,
            index,
        })
    }

    /// Returns the bytes of frame `number`: a part, or the index when
    /// `number` is the number of parts. `None` past that.
    #[must_use]
    pub fn frame(&self, number: usize) -> Option<&'a [u8]> {
        let at = self.frames.get(number)?;
        Some(&self.bytes[at.start..at.start + at.len])
    }

    /// Decompresses part `number` and returns its JSON.
    ///
    /// # Errors
    /// [`ReadError`] if there is no part `number`, if the frame is damaged or
    /// its checksum does not match, or if it does not decompress to the
    /// length the seek table gives for it.
    pub fn part_json(&self, number: usize) -> Result<Vec<u8>, Report<ReadError>> {
        if number >= self.index.parts.len() {
            return corrupt(|| format!("there is no part {number}"));
        }
        let at = self.frames[number];
        let frame = &self.bytes[at.start..at.start + at.len];
        let line = decode_line(&Zstd::default(), frame).attach_with(|| format!("part {number}"))?;
        if line.len() as u64 + 1 != u64::from(at.raw_len) {
            return corrupt(|| {
                format!(
                    "part {number} holds {} bytes and the seek table says {}",
                    line.len() + 1,
                    at.raw_len
                )
            });
        }
        Ok(line)
    }
}

fn corrupt<T>(what: impl FnOnce() -> String) -> Result<T, Report<ReadError>> {
    Err(ReadError).attach_with(what)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}
