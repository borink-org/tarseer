//! Frames and files without any compressor: the `Plain` codec, a sink that
//! fails, and a format written here in the test.

mod common;

use std::io::Write;
use std::path::Path;

use common::wide_fixture;
use error_stack::Report;
use tarseer::file::{Format, FrameSize, FrameSpan, ManifestFile, write_file};
use tarseer::{
    Codec, Frame, Plain, ReadError, Walk, WalkOptions, WriteError, decode_index, walk, write_frames,
};

fn options(budget: u64) -> WalkOptions<'static> {
    WalkOptions {
        budget,
        ..WalkOptions::default()
    }
}

fn lines(walked: &Walk) -> Vec<Vec<u8>> {
    walked
        .parts
        .iter()
        .map(|part| part.to_json().unwrap().into_bytes())
        .collect()
}

fn frames_of(root: &Path, budget: u64, codec: &dyn Codec) -> Vec<Frame> {
    let mut frames = Vec::new();
    write_frames(root, &options(budget), codec, 3, &mut |frame| {
        frames.push(frame);
        Ok(())
    })
    .unwrap();
    frames
}

#[test]
fn the_plain_codec_hands_out_the_json_lines_themselves() {
    let temp_dir = wide_fixture("manifest-plain");
    let frames = frames_of(temp_dir.path(), 300, &Plain);
    let walked = walk(temp_dir.path(), &options(300)).unwrap();

    let (index_frame, part_frames) = frames.split_last().unwrap();
    for (frame, line) in part_frames.iter().zip(lines(&walked)) {
        assert_eq!(frame.bytes[..frame.bytes.len() - 1], line);
        assert_eq!(frame.bytes.last(), Some(&b'\n'));
    }
    let index = decode_index(&Plain, &index_frame.bytes).unwrap();
    assert_eq!(index.entries(), walked.len() as u64);
}

#[test]
fn an_error_from_the_sink_stops_the_write_and_is_returned() {
    let temp_dir = wide_fixture("manifest-sink");
    let mut seen = 0;
    let report = write_frames(temp_dir.path(), &options(150), &Plain, 2, &mut |_frame| {
        seen += 1;
        if seen == 2 {
            return Err(Report::new(WriteError).attach("the sink is full"));
        }
        Ok(())
    })
    .expect_err("the sink refused the second frame");
    assert!(format!("{report:?}").contains("the sink is full"));
    assert_eq!(seen, 2);
}

// A format with everything the trait offers: a magic number first,
// frames that are the text itself, and a trailer of decimal frame lengths
// closed by the trailer's own length.
struct Toy;

impl Format for Toy {
    fn codec(&self) -> &dyn Codec {
        &Plain
    }

    fn write_header(&self, out: &mut dyn Write) -> Result<(), Report<WriteError>> {
        out.write_all(b"TOY1")
            .map_err(|error| Report::new(error).change_context(WriteError))
    }

    fn write_trailer(
        &self,
        frames: &[FrameSize],
        out: &mut dyn Write,
    ) -> Result<(), Report<WriteError>> {
        let lengths: Vec<String> = frames.iter().map(|frame| frame.len.to_string()).collect();
        let list = lengths.join(",");
        let trailer = format!("{list}#{:08}", list.len());
        out.write_all(trailer.as_bytes())
            .map_err(|error| Report::new(error).change_context(WriteError))
    }

    fn locate(&self, bytes: &[u8]) -> Result<Vec<FrameSpan>, Report<ReadError>> {
        let refuse = || Report::new(ReadError).attach("not a toy file");
        if !bytes.starts_with(b"TOY1") || bytes.len() < 13 {
            return Err(refuse());
        }
        let text = |range: std::ops::Range<usize>| std::str::from_utf8(&bytes[range]).ok();
        let list_len: usize = text(bytes.len() - 8..bytes.len())
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(refuse)?;
        let list_at = (bytes.len() - 9).checked_sub(list_len).ok_or_else(refuse)?;
        let mut start = 4;
        let mut frames = Vec::new();
        for length in text(list_at..bytes.len() - 9)
            .ok_or_else(refuse)?
            .split(',')
        {
            let len: usize = length.parse().map_err(|_| refuse())?;
            frames.push(FrameSpan {
                start,
                len,
                raw_len: None,
            });
            start += len;
        }
        Ok(frames)
    }
}

#[test]
fn another_format_with_another_codec_writes_and_reads_through_the_same_calls() {
    let temp_dir = wide_fixture("manifest-toy");
    let mut bytes = Vec::new();
    let written = write_file(temp_dir.path(), &options(300), &Toy, 3, &mut bytes).unwrap();
    assert_eq!(written.len, bytes.len() as u64);
    assert!(bytes.starts_with(b"TOY1{\"stem\":"));

    let walked = walk(temp_dir.path(), &options(300)).unwrap();
    let file = ManifestFile::parse(&Toy, &bytes).unwrap();
    assert_eq!(file.index, written.index);
    for (number, line) in lines(&walked).into_iter().enumerate() {
        assert_eq!(file.part_json(number).unwrap(), line, "part {number}");
    }

    // Not a file of this format.
    assert!(ManifestFile::parse(&Toy, b"TOY2 and then some more bytes").is_err());
}
