//! An LZ4 file format for a manifest, written against tarseer's public API
//! only. A format of your own registers the same way: implement [`Codec`] and
//! [`Format`] in your crate, and pass the type to `write_file` and
//! `ManifestFile::parse`.
//!
//! ```text
//! [part 0][part 1]…[part n-1][index][table]
//! ```
//!
//! Each part and the index is one LZ4 frame with a content checksum. The
//! table is an LZ4 skippable frame, which an LZ4 decoder passes over. It
//! holds the compressed and decompressed size of every frame as two `u32`s,
//! then the number of frames and the tag `TLZ4`.
//!
//! `lz4 -dc` over the file prints a JSON Lines document: one line per part,
//! then the index.

use std::io::{Read, Write};

use error_stack::{Report, ResultExt as _};
use lz4_flex::frame::{FrameDecoder, FrameEncoder, FrameInfo};
use tarseer::file::{Format, FrameSize, FrameSpan};
use tarseer::{Codec, Encoder, ReadError, WriteError};

// One of the LZ4 frame format's skippable magics, for the table of frames.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
// A skippable frame's magic number and length.
const SKIPPABLE_HEAD_LEN: usize = 8;
// The compressed size and the decompressed size.
const ENTRY_LEN: usize = 8;
// This format's own mark, the last four bytes of a file.
const TAG: [u8; 4] = *b"TLZ4";
// The number of frames, then the tag.
const FOOTER_LEN: usize = 8;

/// The LZ4 codec and file format.
#[derive(Debug, Clone, Copy, Default)]
pub struct Lz4;

impl Codec for Lz4 {
    fn encoder(&self) -> Result<Box<dyn Encoder>, Report<WriteError>> {
        Ok(Box::new(Self))
    }

    fn decode(&self, frame: &[u8]) -> Result<Vec<u8>, Report<ReadError>> {
        let mut decoder = FrameDecoder::new(frame);
        let mut raw = Vec::new();
        decoder.read_to_end(&mut raw).change_context(ReadError)?;
        Ok(raw)
    }
}

impl Encoder for Lz4 {
    fn encode(&mut self, raw: &[u8], frame: &mut Vec<u8>) -> Result<(), Report<WriteError>> {
        frame.clear();
        // The checksum is what lets `decode` refuse a damaged frame.
        let info = FrameInfo::new()
            .content_checksum(true)
            .content_size(Some(raw.len() as u64));
        let mut encoder = FrameEncoder::with_frame_info(info, frame);
        encoder.write_all(raw).change_context(WriteError)?;
        encoder.finish().change_context(WriteError)?;
        Ok(())
    }
}

impl Format for Lz4 {
    fn codec(&self) -> &dyn Codec {
        self
    }

    fn write_trailer(
        &self,
        frames: &[FrameSize],
        out: &mut dyn Write,
    ) -> Result<(), Report<WriteError>> {
        let too_large =
            |what: &str| Report::new(WriteError).attach(format!("{what} is past a u32"));
        let payload_len = frames.len() * ENTRY_LEN + FOOTER_LEN;
        let mut table = Vec::with_capacity(SKIPPABLE_HEAD_LEN + payload_len);
        table.extend_from_slice(&SKIPPABLE_MAGIC.to_le_bytes());
        let payload_len = u32::try_from(payload_len).map_err(|_| too_large("the table"))?;
        table.extend_from_slice(&payload_len.to_le_bytes());
        for frame in frames {
            for size in [frame.len, frame.raw_len] {
                let size = u32::try_from(size).map_err(|_| too_large("a frame"))?;
                table.extend_from_slice(&size.to_le_bytes());
            }
        }
        let count = u32::try_from(frames.len()).map_err(|_| too_large("the frame count"))?;
        table.extend_from_slice(&count.to_le_bytes());
        table.extend_from_slice(&TAG);
        out.write_all(&table).change_context(WriteError)
    }

    fn locate(&self, bytes: &[u8]) -> Result<Vec<FrameSpan>, Report<ReadError>> {
        let refuse = |what: &str| Report::new(ReadError).attach(what.to_owned());
        let le_u32 =
            |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));

        let footer_at = bytes
            .len()
            .checked_sub(FOOTER_LEN)
            .ok_or_else(|| refuse("shorter than an LZ4 manifest's table"))?;
        if bytes[footer_at + 4..] != TAG {
            return Err(refuse("no LZ4 manifest table at the end"));
        }
        let count = le_u32(footer_at) as usize;
        let table_at = count
            .checked_mul(ENTRY_LEN)
            .and_then(|entries| footer_at.checked_sub(entries + SKIPPABLE_HEAD_LEN))
            .ok_or_else(|| refuse("the table claims more frames than fit"))?;
        if le_u32(table_at) != SKIPPABLE_MAGIC
            || le_u32(table_at + 4) as usize != bytes.len() - table_at - SKIPPABLE_HEAD_LEN
        {
            return Err(refuse("the table is not a skippable frame"));
        }

        let mut frames = Vec::with_capacity(count);
        let mut start = 0usize;
        for number in 0..count {
            let entry = table_at + SKIPPABLE_HEAD_LEN + number * ENTRY_LEN;
            let len = le_u32(entry) as usize;
            frames.push(FrameSpan {
                start,
                len,
                raw_len: Some(u64::from(le_u32(entry + 4))),
            });
            start += len;
        }
        if start != table_at {
            return Err(refuse("the frames do not end where the table begins"));
        }
        Ok(frames)
    }
}
