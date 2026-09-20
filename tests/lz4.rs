//! The LZ4 format of `examples/lz4`, which lives outside the library: that it
//! goes through the same calls as the built-in format.

mod common;

#[path = "../examples/lz4/format.rs"]
mod format;

use std::io::Read;

use common::wide_fixture;
use format::Lz4;
use tarseer::file::{ManifestFile, write_file};
use tarseer::{ReadError, WalkOptions, walk};

fn options() -> WalkOptions<'static> {
    WalkOptions {
        budget: 300,
        ..WalkOptions::default()
    }
}

#[test]
fn an_lz4_file_reads_back_as_the_json_the_walk_gave() {
    let temp_dir = wide_fixture("lz4-roundtrip");
    let mut bytes = Vec::new();
    let written = write_file(temp_dir.path(), &options(), &Lz4, 3, &mut bytes).unwrap();
    assert_eq!(written.len, bytes.len() as u64);

    let walked = walk(temp_dir.path(), &options()).unwrap();
    assert!(walked.parts.len() > 3, "the fixture is cut at this budget");
    let file = ManifestFile::parse(&Lz4, &bytes).unwrap();
    assert_eq!(file.index, written.index);
    for (number, part) in walked.parts.iter().enumerate() {
        assert_eq!(
            file.part_json(number).unwrap(),
            part.to_json().unwrap().into_bytes(),
            "part {number}"
        );
    }
}

#[test]
fn the_frames_are_back_to_back_lz4_frames_that_need_no_table_to_read() {
    let temp_dir = wide_fixture("lz4-jsonl");
    let mut bytes = Vec::new();
    let written = write_file(temp_dir.path(), &options(), &Lz4, 2, &mut bytes).unwrap();
    let frames = written.index.parts.len() + 1;

    // One LZ4 frame after another from the first byte, without looking at the
    // table: each frame says where it ends.
    let mut rest = &bytes[..];
    let mut lines = Vec::new();
    for _ in 0..frames {
        let mut frame = lz4_flex::frame::FrameDecoder::new(rest);
        frame.read_to_end(&mut lines).unwrap();
        rest = frame.into_inner();
    }
    assert_eq!(lines.len() as u64, written.raw_len);
    assert_eq!(lines.split(|&byte| byte == b'\n').count(), frames + 1);
    assert!(lines.ends_with(format!("{}\n", written.index.to_json().unwrap()).as_bytes()));
    // What is left is the table, in a frame an LZ4 decoder skips.
    assert_eq!(rest[..4], 0x184D_2A50_u32.to_le_bytes());
}

#[test]
fn a_flipped_byte_inside_an_lz4_part_is_refused() {
    let temp_dir = wide_fixture("lz4-flip");
    let mut bytes = Vec::new();
    write_file(temp_dir.path(), &options(), &Lz4, 2, &mut bytes).unwrap();
    let at = {
        let file = ManifestFile::parse(&Lz4, &bytes).unwrap();
        file.frame(0).unwrap().len() + file.frame(1).unwrap().len() / 2
    };
    bytes[at] ^= 0x55;
    let file = ManifestFile::parse(&Lz4, &bytes).unwrap();
    let report = file.part_json(1).expect_err("a damaged part is refused");
    assert_eq!(report.current_context(), &ReadError);
}

#[cfg(feature = "zstd")]
#[test]
fn the_lz4_and_zstd_formats_refuse_each_others_files() {
    use tarseer::zstd::Zstd;

    let temp_dir = wide_fixture("lz4-cross");
    let zstd = Zstd::default();
    let mut lz4_bytes = Vec::new();
    write_file(temp_dir.path(), &options(), &Lz4, 2, &mut lz4_bytes).unwrap();
    let mut zstd_bytes = Vec::new();
    write_file(temp_dir.path(), &options(), &zstd, 2, &mut zstd_bytes).unwrap();

    assert!(ManifestFile::parse(&zstd, &lz4_bytes).is_err());
    assert!(ManifestFile::parse(&Lz4, &zstd_bytes).is_err());
}
