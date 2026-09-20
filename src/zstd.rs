//! The zstd format: a manifest as one seekable zstd file.
//!
//! [`Zstd`] is a [`Codec`] and a [`Format`]. Pass it to
//! [`write_file`](crate::file::write_file) and
//! [`ManifestFile::parse`](crate::file::ManifestFile::parse); the
//! [`file`](mod@crate::file) module has the procedure and an example.
//!
//! # Layout
//!
//! ```text
//! [part 0][part 1]…[part n-1][index][seek table]
//! ```
//!
//! Each part and the index is one ordinary zstd frame, as
//! [`write_frames`](crate::frames::write_frames) hands it out. `zstd -dc` over
//! the file therefore prints a JSON Lines document: one line per part, then
//! the index. Every frame carries a checksum of its content, and a frame whose
//! checksum does not match is refused. The file has no header.
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

use error_stack::{Report, ResultExt as _};

use crate::file::{Format, FrameSize, FrameSpan};
use crate::frames::{Codec, Encoder, ReadError, WriteError};

/// The zstd level of [`Zstd::default()`].
pub const DEFAULT_LEVEL: i32 = 9;

/// The match window of [`Zstd::default()`], as a power of two: 512 KiB.
///
/// zstd sizes a compression context from the window, so the window sets how
/// much memory each compression thread holds.
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

/// The zstd codec and file format: every frame is one zstd frame with a
/// content checksum, and the file ends in a seek table.
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

// The seek table of zstd's seekable format, without per-frame checksums.
fn seek_table(frames: &[FrameSize]) -> Result<Vec<u8>, Report<WriteError>> {
    let payload_len = frames.len() * SEEK_TABLE_ENTRY_LEN + SEEK_TABLE_FOOTER_LEN;
    let (Ok(payload_len_u32), Ok(count)) =
        (u32::try_from(payload_len), u32::try_from(frames.len()))
    else {
        return Err(WriteError)
            .attach_with(|| format!("{} frames are past a seek table", frames.len()));
    };
    let mut table = Vec::with_capacity(SKIPPABLE_HEAD_LEN + payload_len);
    table.extend_from_slice(&SEEK_TABLE_MAGIC.to_le_bytes());
    table.extend_from_slice(&payload_len_u32.to_le_bytes());
    for frame in frames {
        for size in [frame.len, frame.raw_len] {
            let size = u32::try_from(size)
                .change_context(WriteError)
                .attach_with(|| format!("a frame of {size} bytes is past a seek table entry"))?;
            table.extend_from_slice(&size.to_le_bytes());
        }
    }
    table.extend_from_slice(&count.to_le_bytes());
    table.push(0);
    table.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
    Ok(table)
}

impl Format for Zstd {
    fn codec(&self) -> &dyn Codec {
        self
    }

    fn write_trailer(
        &self,
        frames: &[FrameSize],
        out: &mut dyn Write,
    ) -> Result<(), Report<WriteError>> {
        let table = seek_table(frames)?;
        out.write_all(&table).change_context(WriteError)
    }

    fn locate(&self, bytes: &[u8]) -> Result<Vec<FrameSpan>, Report<ReadError>> {
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
            frames.push(FrameSpan {
                start,
                len,
                raw_len: Some(u64::from(le_u32(&entry[4..8]))),
            });
            start = match start.checked_add(len) {
                Some(end) if end <= table_at => end,
                _ => return corrupt(|| "the frames run past the seek table".to_owned()),
            };
        }
        if start != table_at {
            return corrupt(|| "the frames do not end where the seek table begins".to_owned());
        }
        Ok(frames)
    }
}

fn corrupt<T>(what: impl FnOnce() -> String) -> Result<T, Report<ReadError>> {
    Err(ReadError).attach_with(what)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}
