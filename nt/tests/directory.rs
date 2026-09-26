#![cfg(windows)]

use std::fs;
use std::io;
use std::path::PathBuf;

use tarseer_nt::{Buffer, Directory};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

// A directory of its own under the system's temporary directory, removed
// when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tarseer-nt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create the directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

// Every entry's name, and whether it is a directory, and its size.
fn listed(directory: &mut Directory) -> Vec<(String, bool, u64)> {
    let mut entries = Vec::new();
    directory
        .list(&mut Buffer::default(), |entry| {
            let name = String::from_utf16(&entry.name().collect::<Vec<_>>()).expect("UTF-16");
            let is_directory = entry.metadata.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            entries.push((name, is_directory, entry.metadata.size));
            true
        })
        .expect("list");
    entries.sort();
    entries
}

#[test]
fn a_listing_gives_each_entry_with_its_metadata() {
    let temp_dir = TempDir::new("listing");
    fs::write(temp_dir.0.join("file"), b"hello").expect("write");
    fs::create_dir(temp_dir.0.join("sub")).expect("create a directory");

    let mut directory = Directory::open(&temp_dir.0).expect("open");
    assert_eq!(
        listed(&mut directory),
        [("file".to_owned(), false, 5), ("sub".to_owned(), true, 0)]
    );
}

#[test]
fn a_listing_larger_than_the_buffer_is_read_whole() {
    const FILES: usize = 3000;
    let temp_dir = TempDir::new("large");
    // About 190 bytes a record, so several reads of 64 KiB.
    for index in 0..FILES {
        fs::write(temp_dir.0.join(format!("{index:0>60}")), b"").expect("write");
    }

    let mut directory = Directory::open(&temp_dir.0).expect("open");
    let names: Vec<String> = listed(&mut directory)
        .into_iter()
        .map(|entry| entry.0)
        .collect();
    let want: Vec<String> = (0..FILES).map(|index| format!("{index:0>60}")).collect();
    assert_eq!(names, want);
}

#[test]
fn a_listing_stops_when_asked() {
    let temp_dir = TempDir::new("stop");
    for name in ["a", "b", "c"] {
        fs::write(temp_dir.0.join(name), b"").expect("write");
    }

    let mut directory = Directory::open(&temp_dir.0).expect("open");
    let mut seen = 0;
    directory
        .list(&mut Buffer::default(), |_| {
            seen += 1;
            false
        })
        .expect("list");
    assert_eq!(seen, 1);
}

#[test]
fn a_directory_opens_relative_to_its_parent() {
    let temp_dir = TempDir::new("relative");
    fs::create_dir_all(temp_dir.0.join("sub/deeper")).expect("create directories");
    fs::write(temp_dir.0.join("sub/deeper/file"), b"x").expect("write");
    fs::write(temp_dir.0.join("file"), b"x").expect("write");

    let root = Directory::open(&temp_dir.0).expect("open");
    let sub = root
        .open_dir(&wide("SUB"))
        .expect("open, whatever the case");
    let mut deeper = sub.open_dir(&wide("deeper")).expect("open");
    assert_eq!(listed(&mut deeper), [("file".to_owned(), false, 1)]);
    let metadata = deeper.metadata().expect("metadata");
    assert_ne!(metadata.attributes & FILE_ATTRIBUTE_DIRECTORY, 0);
    assert_eq!(metadata.reparse_tag, 0);

    assert!(root.open_dir(&wide("file")).is_err());
    assert!(root.open_dir(&wide("missing")).is_err());
}

#[test]
fn only_a_directory_opens_as_one() {
    let temp_dir = TempDir::new("file");
    let file = temp_dir.0.join("file");
    fs::write(&file, b"x").expect("write");

    let error = Directory::open(&file)
        .err()
        .expect("a file is no directory");
    assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
}

#[test]
fn a_junction_reads_as_std_reads_it() {
    let temp_dir = TempDir::new("junction");
    let target = temp_dir.0.join("target");
    fs::create_dir(&target).expect("create a directory");
    // A junction needs no privilege, where a symlink can.
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(temp_dir.0.join("link"))
        .arg(&target)
        .output()
        .expect("run mklink");
    assert!(made.status.success(), "{made:?}");

    let root = Directory::open(&temp_dir.0).expect("open");
    let read = root.read_link(&wide("link")).expect("read the junction");
    let want = fs::read_link(temp_dir.0.join("link")).expect("std reads the junction");
    assert_eq!(
        String::from_utf16(&read).expect("UTF-16"),
        want.to_str().expect("UTF-8")
    );

    let link = root
        .open_dir(&wide("link"))
        .expect("open the junction itself");
    assert_ne!(link.metadata().expect("metadata").reparse_tag, 0);
}
