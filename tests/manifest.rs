//! A manifest as frames and as one zstd file: that it reads back as the parts
//! the walk gave, that each frame stands alone, and that damage is refused.

mod common;

use std::path::Path;

use common::{TempDir, wide_fixture};
use error_stack::Report;
use tarseer::zstd::{SeekableFile, Written, Zstd, write_file};
use tarseer::{
    Frame, FrameKind, Plain, ReadError, Walk, WalkOptions, WriteError, decode_index, decode_line,
    walk, write_frames,
};

fn options(budget: u64) -> WalkOptions<'static> {
    WalkOptions {
        budget,
        ..WalkOptions::default()
    }
}

fn write(root: &Path, budget: u64, threads: usize) -> (Vec<u8>, Written) {
    let mut out = Vec::new();
    let written = write_file(root, &options(budget), &Zstd::default(), threads, &mut out).unwrap();
    (out, written)
}

fn lines(walked: &Walk) -> Vec<Vec<u8>> {
    walked
        .parts
        .iter()
        .map(|part| part.to_json().unwrap().into_bytes())
        .collect()
}

#[test]
fn every_part_reads_back_as_the_json_the_walk_gave() {
    let temp_dir = wide_fixture("manifest-roundtrip");
    let budget = 300;
    let (bytes, written) = write(temp_dir.path(), budget, 4);
    let walked = walk(temp_dir.path(), &options(budget)).unwrap();
    assert!(walked.parts.len() > 3, "the fixture is cut at this budget");

    let file = SeekableFile::parse(&bytes).unwrap();
    assert_eq!(file.index, written.index);
    assert_eq!(written.len, bytes.len() as u64);
    assert_eq!(file.index.parts.len(), walked.parts.len());
    assert_eq!(file.index.entries(), walked.len() as u64);

    for (number, part) in walked.parts.iter().enumerate() {
        let entry = &file.index.parts[number];
        assert_eq!(
            file.part_json(number).unwrap(),
            part.to_json().unwrap().into_bytes(),
            "part {number}"
        );
        assert_eq!(entry.first, part.entries()[0].0);
        assert_eq!(
            (entry.directories, entry.files, entry.symlinks),
            (
                part.directories.len() as u64,
                part.files.len() as u64,
                part.symlinks.len() as u64
            )
        );
    }
    assert!(
        file.part_json(walked.parts.len()).is_err(),
        "the index is no part"
    );
}

#[test]
fn a_plain_zstd_decoder_prints_the_parts_and_then_the_index_as_json_lines() {
    let temp_dir = wide_fixture("manifest-jsonl");
    let (bytes, written) = write(temp_dir.path(), 300, 2);
    let walked = walk(temp_dir.path(), &options(300)).unwrap();

    let mut want = Vec::new();
    for line in lines(&walked) {
        want.extend_from_slice(&line);
        want.push(b'\n');
    }
    want.extend_from_slice(written.index.to_json().unwrap().as_bytes());
    want.push(b'\n');
    assert_eq!(written.raw_len, want.len() as u64);

    // One call over the whole file: every frame in turn, the seek table
    // passed over.
    let mut decoded = Vec::with_capacity(want.len());
    zstd_safe::decompress(&mut decoded, &bytes).unwrap();
    assert_eq!(decoded, want);
}

fn frames_of(root: &Path, budget: u64, codec: &dyn tarseer::Codec) -> Vec<Frame> {
    let mut frames = Vec::new();
    write_frames(root, &options(budget), codec, 3, &mut |frame| {
        frames.push(frame);
        Ok(())
    })
    .unwrap();
    frames
}

#[test]
fn each_frame_decodes_on_its_own_and_the_file_is_the_frames_then_a_seek_table() {
    let temp_dir = wide_fixture("manifest-frames");
    let codec = Zstd::default();
    let frames = frames_of(temp_dir.path(), 300, &codec);
    let walked = walk(temp_dir.path(), &options(300)).unwrap();
    let want = lines(&walked);

    let (index_frame, part_frames) = frames.split_last().unwrap();
    assert_eq!(part_frames.len(), want.len());
    for (number, frame) in part_frames.iter().enumerate() {
        assert_eq!(frame.kind, FrameKind::Part(number));
        let line = decode_line(&codec, &frame.bytes).unwrap();
        assert_eq!(line, want[number], "part {number}");
        assert_eq!(frame.raw_len, line.len() as u64 + 1);
    }
    assert_eq!(index_frame.kind, FrameKind::Index);
    let index = decode_index(&codec, &index_frame.bytes).unwrap();
    assert_eq!(index.parts.len(), want.len());

    let (file, _) = write(temp_dir.path(), 300, 1);
    let joined: Vec<u8> = frames
        .iter()
        .flat_map(|frame| frame.bytes.clone())
        .collect();
    assert_eq!(&file[..joined.len()], joined);
    let table = &file[joined.len()..];
    assert_eq!(table.len(), 8 + frames.len() * 8 + 9);
    assert_eq!(table[table.len() - 4..], 0x8F92_EAB1_u32.to_le_bytes());

    let parsed = SeekableFile::parse(&file).unwrap();
    for (number, frame) in frames.iter().enumerate() {
        assert_eq!(parsed.frame(number), Some(&frame.bytes[..]));
    }
    assert_eq!(parsed.frame(frames.len()), None);
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
fn the_bytes_do_not_depend_on_the_thread_count() {
    let temp_dir = wide_fixture("manifest-threads");
    let (one, _) = write(temp_dir.path(), 150, 1);
    for threads in [0, 2, 3, 16] {
        let (many, _) = write(temp_dir.path(), 150, threads);
        assert!(one == many, "{threads} threads wrote different bytes");
    }
}

#[test]
fn the_match_window_changes_the_bytes_but_not_what_they_say() {
    let temp_dir = wide_fixture("manifest-window");
    let walked = walk(temp_dir.path(), &options(300)).unwrap();

    // 0 is zstd's own choice, 10 the smallest window it accepts.
    for window_log in [0, 10, 17, 19, 27] {
        let codec = Zstd {
            window_log,
            ..Zstd::default()
        };
        let mut bytes = Vec::new();
        write_file(temp_dir.path(), &options(300), &codec, 2, &mut bytes).unwrap();

        let file = SeekableFile::parse(&bytes).unwrap();
        assert_eq!(file.index.parts.len(), walked.parts.len());
        for (number, line) in lines(&walked).into_iter().enumerate() {
            assert_eq!(
                file.part_json(number).unwrap(),
                line,
                "window {window_log}, part {number}"
            );
        }
    }
}

#[test]
fn a_window_zstd_does_not_accept_fails_the_write() {
    let temp_dir = wide_fixture("manifest-badwindow");
    let codec = Zstd {
        window_log: 99,
        ..Zstd::default()
    };
    let report = write_file(temp_dir.path(), &options(300), &codec, 2, &mut Vec::new())
        .expect_err("no such window");
    assert_eq!(report.current_context(), &WriteError);
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

#[test]
fn an_empty_tree_has_an_index_and_no_parts() {
    let temp_dir = TempDir::new("manifest-empty");
    let (bytes, written) = write(temp_dir.path(), 4 << 20, 2);
    let file = SeekableFile::parse(&bytes).unwrap();
    assert!(file.index.parts.is_empty());
    assert_eq!(file.index, written.index);
}

fn refused(bytes: &[u8]) -> String {
    let report = SeekableFile::parse(bytes).expect_err("damaged bytes are refused");
    assert_eq!(report.current_context(), &ReadError);
    format!("{report:?}")
}

#[test]
fn damage_anywhere_is_refused_rather_than_misread() {
    let temp_dir = wide_fixture("manifest-damage");
    let (bytes, written) = write(temp_dir.path(), 300, 2);
    let frames = written.index.parts.len() + 1;
    let table_at = bytes.len() - (8 + frames * 8 + 9);

    // Too short to hold the end of a seek table at all.
    refused(&bytes[..8]);

    // The magic number at the very end.
    let mut magic = bytes.clone();
    *magic.last_mut().unwrap() ^= 0xff;
    assert!(refused(&magic).contains("no seek table"));

    // A truncated tail no longer ends in a seek table.
    refused(&bytes[..bytes.len() - 1]);

    // A lie about the number of frames.
    let mut count = bytes.clone();
    let at = bytes.len() - 9;
    count[at..at + 4].copy_from_slice(&(u32::try_from(frames).unwrap() + 1).to_le_bytes());
    refused(&count);

    // A lie about the first frame's compressed size.
    let mut size = bytes.clone();
    size[table_at + 8] ^= 0x01;
    refused(&size);

    // Bytes before the first frame.
    let mut prefixed = b"junk".to_vec();
    prefixed.extend_from_slice(&bytes);
    refused(&prefixed);

    // A byte inside the index's compressed JSON.
    let index_at = SeekableFile::parse(&bytes)
        .unwrap()
        .frame(frames - 1)
        .unwrap()
        .len();
    let mut index = bytes.clone();
    index[table_at - index_at / 2] ^= 0x55;
    refused(&index);
}

#[test]
fn a_flipped_byte_inside_a_part_is_refused_rather_than_decoded() {
    let temp_dir = wide_fixture("manifest-flip");
    let (bytes, _) = write(temp_dir.path(), 300, 2);
    let file = SeekableFile::parse(&bytes).unwrap();
    let start: usize = file.frame(0).unwrap().len();
    let len = file.frame(1).unwrap().len();

    // Inside the compressed payload, past the frame's own header. Nothing but
    // the frame checksum notices this: the damage decodes, to JSON that may
    // well parse.
    for at in [start + 12, start + len / 2] {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x55;
        let file = SeekableFile::parse(&damaged).unwrap();
        file.part_json(0).unwrap();
        let report = file.part_json(1).expect_err("a damaged part is refused");
        assert_eq!(report.current_context(), &ReadError);
    }
}
