//! The manifest on disk: that it reads back as the parts the walk gave, and
//! that it holds up against what a file can do wrong.

mod common;

use std::path::Path;

use common::{TempDir, fixture, wide_fixture};
use tarseer::manifest::FOOTER_LEN;
use tarseer::{Manifest, ReadError, WalkOptions, WriteOptions, Written, walk, write_manifest};

fn write(root: &Path, budget: u64, threads: usize) -> (Vec<u8>, Written) {
    let mut out = Vec::new();
    let options = WalkOptions {
        budget,
        ..WalkOptions::default()
    };
    let write_options = WriteOptions {
        threads,
        ..WriteOptions::default()
    };
    let written = write_manifest(root, &options, &write_options, &mut out).unwrap();
    (out, written)
}

#[test]
fn every_part_reads_back_as_the_json_the_walk_gave() {
    let temp_dir = wide_fixture("manifest-roundtrip");
    let budget = 300;
    let (bytes, written) = write(temp_dir.path(), budget, 4);
    let options = WalkOptions {
        budget,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options).unwrap();

    let manifest = Manifest::parse(&bytes).unwrap();
    assert_eq!(manifest.index, written.index);
    assert_eq!(written.len, bytes.len() as u64);
    assert_eq!(manifest.index.parts.len(), walked.parts.len());
    assert!(walked.parts.len() > 3, "the fixture is cut at this budget");

    for (number, part) in walked.parts.iter().enumerate() {
        let entry = &manifest.index.parts[number];
        assert_eq!(
            manifest.part_json(number).unwrap(),
            part.to_json().unwrap().into_bytes(),
            "part {number}"
        );
        assert_eq!(Some(&entry.first), part.first_path().as_ref());
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
    assert_eq!(manifest.index.entries(), walked.len() as u64);
}

#[test]
fn the_bytes_do_not_depend_on_the_thread_count() {
    let temp_dir = wide_fixture("manifest-threads");
    let (one, _) = write(temp_dir.path(), 150, 1);
    for threads in [2, 3, 16] {
        let (many, _) = write(temp_dir.path(), 150, threads);
        assert!(one == many, "{threads} threads wrote different bytes");
    }
}

#[test]
fn the_match_window_changes_the_bytes_but_not_what_they_say() {
    let temp_dir = wide_fixture("manifest-window");
    let options = WalkOptions {
        budget: 300,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options).unwrap();

    // 0 is zstd's own choice, 10 the smallest window it accepts.
    for window_log in [0, 10, 17, 19, 27] {
        let mut bytes = Vec::new();
        let write_options = WriteOptions {
            window_log,
            ..WriteOptions::default()
        };
        write_manifest(temp_dir.path(), &options, &write_options, &mut bytes).unwrap();

        let manifest = Manifest::parse(&bytes).unwrap();
        assert_eq!(manifest.index.parts.len(), walked.parts.len());
        for (number, part) in walked.parts.iter().enumerate() {
            assert_eq!(
                manifest.part_json(number).unwrap(),
                part.to_json().unwrap().into_bytes(),
                "window {window_log}, part {number}"
            );
        }
    }
}

#[test]
fn a_manifest_after_other_bytes_still_opens() {
    let temp_dir = fixture("manifest-prefix");
    let (bytes, _) = write(temp_dir.path(), 4 << 20, 2);
    let mut archive = b"whatever payload comes first".to_vec();
    archive.extend_from_slice(&bytes);

    let alone = Manifest::parse(&bytes).unwrap();
    let after = Manifest::parse(&archive).unwrap();
    assert_eq!(after.index, alone.index);
    assert_eq!(after.part_json(0).unwrap(), alone.part_json(0).unwrap());
}

#[test]
fn a_plain_zstd_decoder_skips_the_whole_manifest() {
    let temp_dir = wide_fixture("manifest-skippable");
    let (bytes, _) = write(temp_dir.path(), 300, 2);
    let payload = b"exactly these bytes, and nothing of the manifest";
    // zstd-safe writes into spare capacity only, so the buffer is sized first.
    let mut archive = Vec::with_capacity(zstd_safe::compress_bound(payload.len()));
    zstd_safe::compress(&mut archive, payload, 3).unwrap();
    archive.reserve(bytes.len());
    archive.extend_from_slice(&bytes);

    let mut decoded = Vec::with_capacity(payload.len() * 4);
    zstd_safe::decompress(&mut decoded, &archive).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn an_empty_tree_has_an_index_and_no_parts() {
    let temp_dir = TempDir::new("manifest-empty");
    let (bytes, written) = write(temp_dir.path(), 4 << 20, 2);
    let manifest = Manifest::parse(&bytes).unwrap();
    assert!(manifest.index.parts.is_empty());
    assert_eq!(manifest.index, written.index);
}

fn refused(bytes: &[u8]) -> String {
    let report = Manifest::parse(bytes).expect_err("damaged bytes are refused");
    assert_eq!(report.current_context(), &ReadError);
    format!("{report:?}")
}

#[test]
fn damage_anywhere_is_refused_rather_than_misread() {
    let temp_dir = wide_fixture("manifest-damage");
    let (bytes, _) = write(temp_dir.path(), 300, 2);

    // Too short to hold a footer at all.
    refused(&bytes[..FOOTER_LEN - 1]);

    // The magic at the very end.
    let mut magic = bytes.clone();
    *magic.last_mut().unwrap() ^= 0xff;
    assert!(refused(&magic).contains("no tarseer footer"));

    // A truncated tail no longer ends in a footer.
    refused(&bytes[..bytes.len() - 1]);

    // A lie about the manifest's length.
    let mut long = bytes.clone();
    let at = bytes.len() - FOOTER_LEN + 8;
    long[at..at + 8].copy_from_slice(&(bytes.len() as u64 + 1).to_le_bytes());
    refused(&long);

    // A byte inside the index's compressed JSON.
    let index_len = usize::try_from(u64::from_le_bytes(
        bytes[bytes.len() - FOOTER_LEN + 16..bytes.len() - FOOTER_LEN + 24]
            .try_into()
            .unwrap(),
    ))
    .unwrap();
    let mut index = bytes.clone();
    let inside = bytes.len() - FOOTER_LEN - index_len / 2;
    index[inside] ^= 0x55;
    refused(&index);
}

#[test]
fn a_damaged_part_is_refused_when_it_is_read() {
    let temp_dir = wide_fixture("manifest-part");
    let (mut bytes, _) = write(temp_dir.path(), 300, 2);
    let manifest = Manifest::parse(&bytes).unwrap();
    let second = manifest.index.parts[1].clone();

    // The tag of part 1: the index still opens, the part does not.
    bytes[usize::try_from(second.offset).unwrap() + 8] ^= 0xff;
    let manifest = Manifest::parse(&bytes).unwrap();
    manifest.part_json(0).unwrap();
    let report = manifest.part_json(1).expect_err("a bad tag is refused");
    assert!(format!("{report:?}").contains("TSPT"));
}

#[test]
fn a_flipped_byte_inside_a_part_is_refused_rather_than_decoded() {
    let temp_dir = wide_fixture("manifest-flip");
    let (bytes, _) = write(temp_dir.path(), 300, 2);
    let part = Manifest::parse(&bytes).unwrap().index.parts[1].clone();

    // Inside the compressed payload, past the frame's own header. Nothing but
    // the frame checksum notices this: the damage decodes, to JSON that may
    // well parse.
    let start = usize::try_from(part.offset).unwrap();
    for at in [
        start + 20,
        start + usize::try_from(part.frame_len).unwrap() / 2,
    ] {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x55;
        let manifest = Manifest::parse(&damaged).unwrap();
        let report = manifest
            .part_json(1)
            .expect_err("a damaged part is refused");
        assert_eq!(report.current_context(), &ReadError);
    }
}
