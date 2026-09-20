//! A manifest as one file, in a [`Format`] of your choice.
//!
//! # How a write works
//!
//! 1. Choose a [`Format`]. The `zstd` feature, on by default, has the one
//!    this crate ships.
//! 2. Call [`write_file`] with the root, a [`WalkOptions`], the format, a
//!    thread count and a writer.
//!
//! # How a read works
//!
//! 1. Load the file into memory and call [`ManifestFile::parse`] with the
//!    format that wrote it. It finds the frames and decodes the index, and
//!    decodes no part.
//! 2. Call [`ManifestFile::part_json`] with a part number to decode that one
//!    part.
//!
//! # Examples
//!
//! ```
//! # #[cfg(feature = "zstd")]
//! # {
//! use std::path::Path;
//! use tarseer::file::{ManifestFile, write_file};
//! use tarseer::{WalkOptions, zstd::Zstd};
//!
//! let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
//! let format = Zstd::default();
//! let mut bytes = Vec::new();
//! let written = write_file(&root, &WalkOptions::default(), &format, 2, &mut bytes).unwrap();
//!
//! let file = ManifestFile::parse(&format, &bytes).unwrap();
//! assert_eq!(file.index, written.index);
//! let json = file.part_json(0).unwrap();
//! assert!(json.starts_with(b"{\"stem\":[]"));
//! # }
//! ```
//!
//! # Writing another format
//!
//! A file is a header, then the frames of [`write_frames`] in order, then a
//! trailer. Implement [`Codec`] for how a frame is encoded and [`Format`] for
//! the header, the trailer and how a reader finds the frames again. Nothing
//! else in this crate depends on which format a file has.

use std::io::Write;
use std::path::Path;

use error_stack::{Report, ResultExt as _};

use crate::frames::{Codec, Frame, ReadError, WriteError, decode_index, decode_line, write_frames};
use crate::manifest::Index;
use crate::walk::WalkOptions;

/// The sizes of one frame that [`write_file`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSize {
    /// The length in bytes of the encoded frame.
    pub len: u64,
    /// The length in bytes of the text the frame decodes to.
    pub raw_len: u64,
}

/// Where one frame lies in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpan {
    /// The offset of the frame's first byte from the start of the file.
    pub start: usize,
    /// The length in bytes of the encoded frame.
    pub len: usize,
    /// The length in bytes of the text the frame decodes to, if the format
    /// records it. [`ManifestFile::part_json`] then checks it.
    pub raw_len: Option<u64>,
}

/// A file format for a manifest: a codec, and the bytes around the frames.
pub trait Format: Send + Sync {
    /// Returns the codec that encodes and decodes this format's frames.
    fn codec(&self) -> &dyn Codec;

    /// Writes what comes before the first frame. The default writes nothing.
    ///
    /// # Errors
    /// [`WriteError`] if `out` returned an error.
    fn write_header(&self, out: &mut dyn Write) -> Result<(), Report<WriteError>> {
        let _ = out;
        Ok(())
    }

    /// Writes what comes after the last frame. `frames` has the sizes of
    /// every frame written, in order; the last one is the index.
    ///
    /// # Errors
    /// [`WriteError`] if the sizes cannot be recorded in this format, or if
    /// `out` returned an error.
    fn write_trailer(
        &self,
        frames: &[FrameSize],
        out: &mut dyn Write,
    ) -> Result<(), Report<WriteError>>;

    /// Finds every frame of the file `bytes`, in order. The last one is the
    /// index.
    ///
    /// # Errors
    /// [`ReadError`] if `bytes` is not a file of this format, or if what it
    /// says about its frames does not fit its length.
    fn locate(&self, bytes: &[u8]) -> Result<Vec<FrameSpan>, Report<ReadError>>;
}

/// What [`write_file`] wrote.
#[derive(Debug, Clone)]
pub struct Written {
    /// The index, as the last frame holds it.
    pub index: Index,
    /// The length in bytes of the JSON Lines document the frames decode to.
    pub raw_len: u64,
    /// The length in bytes of the whole file.
    pub len: u64,
}

// Counts the bytes that pass through to `out`.
struct Counting<'a> {
    out: &'a mut (dyn Write + Send),
    len: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(bytes)?;
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// Walks `root` and writes its manifest to `out` as one file of `format`.
///
/// `out` receives the format's header, the frames of [`write_frames`] in
/// order, and the format's trailer. After an error, `out` holds whatever was
/// written before it.
///
/// # Errors
/// [`WriteError`] as [`write_frames`], as the format's header and trailer, or
/// if `out` returned an error.
///
/// # Panics
/// As [`write_frames`].
pub fn write_file(
    root: &Path,
    walk_options: &WalkOptions<'_>,
    format: &dyn Format,
    threads: usize,
    out: &mut (dyn Write + Send),
) -> Result<Written, Report<WriteError>> {
    let mut out = Counting { out, len: 0 };
    format.write_header(&mut out)?;
    let mut sizes = Vec::new();
    let index = {
        let frame_out = &mut out;
        write_frames(
            root,
            walk_options,
            format.codec(),
            threads,
            &mut |frame: Frame| {
                sizes.push(FrameSize {
                    len: frame.bytes.len() as u64,
                    raw_len: frame.raw_len,
                });
                frame_out.write_all(&frame.bytes).change_context(WriteError)
            },
        )?
    };
    format.write_trailer(&sizes, &mut out)?;
    out.flush().change_context(WriteError)?;
    Ok(Written {
        index,
        raw_len: sizes.iter().map(|size| size.raw_len).sum(),
        len: out.len,
    })
}

/// A manifest file held in memory, with its index decoded.
pub struct ManifestFile<'a> {
    format: &'a dyn Format,
    bytes: &'a [u8],
    // Every frame of the file. The last one is the index.
    frames: Vec<FrameSpan>,
    /// The index, decoded by [`ManifestFile::parse`].
    pub index: Index,
}

impl<'a> ManifestFile<'a> {
    /// Opens the manifest that `bytes` holds, as a file of `format`.
    ///
    /// This finds the frames and decodes the index. It decodes no part.
    ///
    /// # Errors
    /// [`ReadError`] as [`Format::locate`], if a frame lies outside `bytes`,
    /// if the index frame is damaged, or if the index lists a different
    /// number of parts than the file holds. The report says which.
    pub fn parse(format: &'a dyn Format, bytes: &'a [u8]) -> Result<Self, Report<ReadError>> {
        let frames = format.locate(bytes)?;
        if let Some(number) = frames.iter().position(|span| {
            span.start
                .checked_add(span.len)
                .is_none_or(|end| end > bytes.len())
        }) {
            return corrupt(|| format!("frame {number} lies outside the file"));
        }
        let Some((index_at, parts)) = frames.split_last() else {
            return corrupt(|| "the file holds no index frame".to_owned());
        };
        let index = decode_index(
            format.codec(),
            &bytes[index_at.start..index_at.start + index_at.len],
        )
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
            format,
            bytes,
            frames,
            index,
        })
    }

    /// Returns the bytes of frame `number`: a part, or the index when
    /// `number` is the number of parts. `None` past that.
    #[must_use]
    pub fn frame(&self, number: usize) -> Option<&'a [u8]> {
        let span = self.frames.get(number)?;
        Some(&self.bytes[span.start..span.start + span.len])
    }

    /// Decodes part `number`, counted in walk order as the index does, and
    /// returns its JSON.
    ///
    /// # Errors
    /// [`ReadError`] if there is no part `number`, if the frame is damaged,
    /// or if it does not decode to the length the format recorded for it.
    pub fn part_json(&self, number: usize) -> Result<Vec<u8>, Report<ReadError>> {
        if number >= self.index.parts.len() {
            return corrupt(|| format!("there is no part {number}"));
        }
        let held = usize::try_from(self.index.parts[number].frame)
            .ok()
            .and_then(|at| self.frames.get(at));
        let Some(&span) = held else {
            return corrupt(|| format!("part {number} is in a frame the file does not hold"));
        };
        let frame = &self.bytes[span.start..span.start + span.len];
        let line =
            decode_line(self.format.codec(), frame).attach_with(|| format!("part {number}"))?;
        let decoded = line.len() as u64 + 1;
        if span.raw_len.is_some_and(|recorded| recorded != decoded) {
            return corrupt(|| {
                format!("part {number} decodes to {decoded} bytes, not the recorded length")
            });
        }
        Ok(line)
    }
}

fn corrupt<T>(what: impl FnOnce() -> String) -> Result<T, Report<ReadError>> {
    Err(ReadError).attach_with(what)
}

impl std::fmt::Debug for ManifestFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManifestFile")
            .field("len", &self.bytes.len())
            .field("frames", &self.frames)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}
