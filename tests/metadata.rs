//! A walk that reads no metadata, and the metadata read in afterwards.

mod common;

use std::path::Path;

use common::{fixture, wide_fixture};
use tarseer::{Metadata, WalkOptions, read_metadata, walk};

fn parts(root: &Path, metadata: Metadata, budget: u64) -> Vec<tarseer::TreePart> {
    let options = WalkOptions {
        budget,
        metadata,
        ..WalkOptions::default()
    };
    walk(root, &options).unwrap().parts
}

#[test]
fn kinds_then_metadata_is_the_full_walk() {
    for temp_dir in [fixture("metadata-small"), wide_fixture("metadata-wide")] {
        let root = temp_dir.path();
        {
            for budget in [1 << 10, 1 << 20] {
                let full = parts(root, Metadata::Full, budget);
                let kinds = parts(root, Metadata::Kinds, budget);
                assert_eq!(full.len(), kinds.len(), "the same cuts");
                for (full, mut kinds) in full.into_iter().zip(kinds) {
                    assert!(
                        kinds
                            .files
                            .iter()
                            .all(|row| row.size == 0 && row.mtime.is_none())
                    );
                    assert!(kinds.directories.iter().all(|row| row.mtime.is_none()));
                    assert_eq!(read_metadata(root, &mut kinds).unwrap(), 0);
                    assert_eq!(full.to_json().unwrap(), kinds.to_json().unwrap());
                }
            }
        }
    }
}

#[test]
fn missing_rows_are_counted() {
    let temp_dir = fixture("metadata-missing");
    let root = temp_dir.path();
    let mut all = parts(root, Metadata::Kinds, 1 << 20);
    let mut part = all.remove(0);
    let files = part.files.len();
    for (path, kind) in part.entries() {
        if kind == tarseer::EntryKind::File {
            std::fs::remove_file(root.join(path)).unwrap();
        }
    }
    assert_eq!(read_metadata(root, &mut part).unwrap(), files);
}

#[cfg(unix)]
#[test]
fn open_files_give_the_full_walk() {
    let temp_dir = wide_fixture("metadata-open");
    let root = temp_dir.path();
    let full = parts(root, Metadata::Full, 1 << 10);
    let kinds = parts(root, Metadata::Kinds, 1 << 10);
    for (full, mut kinds) in full.into_iter().zip(kinds) {
        for index in 0..kinds.files.len() {
            let row = kinds.files[index];
            let file = std::fs::File::open(root.join(kinds.path(row.parent, row.name))).unwrap();
            tarseer::read_file_metadata(&file, &mut kinds.files[index]).unwrap();
        }
        for (full, kinds) in full.files.iter().zip(&kinds.files) {
            assert_eq!(
                (full.size, full.mtime, full.mode),
                (kinds.size, kinds.mtime, kinds.mode)
            );
        }
    }
    let directory = std::fs::File::open(root.join("flat")).unwrap();
    let mut row = kinds_row();
    assert!(tarseer::read_file_metadata(&directory, &mut row).is_err());
    assert_eq!(row.size, 7, "a refused file leaves the row as it was");
}

#[cfg(unix)]
fn kinds_row() -> tarseer::FileRow {
    tarseer::FileRow {
        parent: 0,
        name: 0,
        size: 7,
        mtime: None,
        mode: 0,
    }
}
