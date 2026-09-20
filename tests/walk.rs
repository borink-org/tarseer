//! End to end: build a tree on disk, walk it, and check what came back
//! against what was written.

mod common;

use std::fs;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::{TempDir, fixture, recurse, walk_default};
use tarseer::{
    Cancelled, Candidate, EntryKind, Filter, OnError, Progress, SkipReason, WalkError, WalkOptions,
    walk,
};

#[test]
fn every_entry_is_found_in_walk_order() {
    let temp_dir = fixture("order");
    let walk = walk_default(temp_dir.path());

    let mut want = vec![
        "alpha",
        "alpha/a.txt",
        "top.txt",
        "zed",
        "zed/deep",
        "zed/deep/d.txt",
        "zed/z.bin",
    ];
    if cfg!(unix) {
        want.insert(2, "alpha/link");
    }
    assert_eq!(walk.paths(), want);
    assert_eq!(walk.parts.len(), 1, "a small tree is one part");
}

#[test]
fn rows_carry_the_sizes_and_kinds_that_were_written() {
    let temp_dir = fixture("kinds");
    let walk = walk_default(temp_dir.path());
    let part = &walk.parts[0];

    let files: Vec<(String, u64)> = part
        .files
        .iter()
        .map(|row| (part.path(row.parent, row.name), row.size))
        .collect();
    assert_eq!(
        files,
        [
            ("alpha/a.txt".to_owned(), 1),
            ("top.txt".to_owned(), 3),
            ("zed/deep/d.txt".to_owned(), 5),
            ("zed/z.bin".to_owned(), 10),
        ]
    );
    assert_eq!(part.directories.len(), 3);
    assert_eq!(walk.total_bytes(), 19);
}

#[cfg(unix)]
#[test]
fn a_symlink_keeps_its_target_and_is_not_followed() {
    let temp_dir = fixture("symlink");
    let walk = walk_default(temp_dir.path());
    let part = &walk.parts[0];
    assert_eq!(part.symlinks.len(), 1);
    let link = part.symlinks[0];
    assert_eq!(part.path(link.parent, link.name), "alpha/link");
    assert_eq!(part.text(link.target), "../top.txt");
    assert_eq!(link.directory, None, "a Unix symlink has no kind");
}

#[test]
fn the_walk_order_matches_a_plain_recursive_sorted_walk() {
    let temp_dir = fixture("oracle");
    let walk = walk_default(temp_dir.path());
    let mut want = Vec::new();
    recurse(temp_dir.path(), "", &mut want);
    assert_eq!(walk.paths(), want);
}

#[test]
fn a_root_that_cannot_be_listed_fails_the_walk() {
    let temp_dir = TempDir::new("noroot");
    let missing = temp_dir.path().join("missing");
    let report = walk(&missing, &WalkOptions::default()).expect_err("no root, no walk");
    assert_eq!(report.current_context(), &WalkError);
    assert!(
        format!("{report:?}").contains("missing"),
        "the report should name the root: {report:?}"
    );
}

#[test]
fn an_empty_tree_holds_nothing() {
    let temp_dir = TempDir::new("empty");
    let walk = walk_default(temp_dir.path());
    assert!(walk.is_empty());
    assert!(walk.parts.is_empty(), "no rows, no part");
}

#[test]
fn mtimes_keep_the_precision_the_filesystem_gave() {
    let temp_dir = TempDir::new("mtime");
    let path = temp_dir.path().join("precise");
    fs::write(&path, b"x").unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();

    let walk = walk_default(temp_dir.path());
    let mtime = walk.parts[0].files[0]
        .mtime
        .expect("the filesystem has mtimes");
    assert_eq!(mtime.secs, 1_700_000_000);
    // Filesystems differ in granularity; whatever it kept must not be rounded
    // away to whole seconds.
    let fs_nanos = fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    assert_eq!(mtime.nanos, fs_nanos);
}

struct NoZedNoLink;

impl Filter for NoZedNoLink {
    fn keep(&self, candidate: &Candidate<'_>) -> bool {
        assert!(candidate.listing.contains(candidate.name));
        !(candidate.parent.is_empty() && candidate.name == "zed")
            && candidate.kind != EntryKind::Symlink
    }
}

#[test]
fn a_filter_refuses_entries_and_whole_subtrees_before_they_are_read() {
    let temp_dir = fixture("filter");
    let options = WalkOptions {
        filter: Some(&NoZedNoLink),
        ..WalkOptions::default()
    };
    let walk = walk(temp_dir.path(), &options).unwrap();
    assert_eq!(walk.paths(), ["alpha", "alpha/a.txt", "top.txt"]);
    assert!(!walk.skips.any(), "a refusal is not a skip");
}

#[derive(Default)]
struct Counting {
    entered: Mutex<Vec<String>>,
    bytes: AtomicU64,
}

impl Progress for Counting {
    fn entered(&self, directory: &str) {
        self.entered.lock().unwrap().push(directory.to_owned());
    }

    fn recorded(&self, _kind: EntryKind, size: u64) {
        self.bytes.fetch_add(size, Ordering::Relaxed);
    }
}

#[test]
fn progress_hears_every_directory_and_every_byte() {
    let temp_dir = fixture("progress");
    let counting = Counting::default();
    let options = WalkOptions {
        progress: Some(&counting),
        ..WalkOptions::default()
    };
    walk(temp_dir.path(), &options).unwrap();
    assert_eq!(
        *counting.entered.lock().unwrap(),
        ["", "alpha", "zed", "zed/deep"]
    );
    assert_eq!(counting.bytes.load(Ordering::Relaxed), 19);
}

#[test]
fn a_raised_cancel_flag_stops_the_walk() {
    let temp_dir = fixture("cancel");
    let cancel = AtomicBool::new(true);
    let options = WalkOptions {
        cancel: Some(&cancel),
        ..WalkOptions::default()
    };
    let report = walk(temp_dir.path(), &options).expect_err("cancelled");
    assert!(report.contains::<Cancelled>());
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_fails_the_walk_and_says_which_one() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = fixture("locked-fail");
    let locked = temp_dir.path().join("zed");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let walked = walk(temp_dir.path(), &WalkOptions::default());
    // Restore before asserting, so a failure still lets the fixture clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let report = walked.expect_err("an unopenable directory fails the walk");
    assert_eq!(report.current_context(), &WalkError);
    assert!(
        format!("{report:?}").contains("listing zed"),
        "the report should name the directory: {report:?}"
    );
}

#[cfg(unix)]
#[test]
fn under_skip_an_unreadable_directory_is_counted_and_the_rest_still_walks() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = fixture("locked");
    let locked = temp_dir.path().join("zed");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let options = WalkOptions {
        on_error: OnError::Skip,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let walked = walked.unwrap();
    assert_eq!(walked.skips.unreadable, 1);
    let paths = walked.paths();
    assert!(paths.iter().any(|path| path == "top.txt"));
    assert!(paths.iter().any(|path| path == "zed"));
    assert!(!paths.iter().any(|path| path == "zed/z.bin"));
}

#[cfg(unix)]
#[test]
fn an_entry_that_cannot_be_stated_fails_the_walk_and_says_which_one() {
    use std::os::unix::fs::PermissionsExt;

    // Readable but not searchable: the names in it list, and then every
    // `metadata` inside it is denied. Unlike an unreadable directory there is
    // no whole subtree to write off, so this is a failure rather than a skip.
    let temp_dir = fixture("nostat");
    let inner = temp_dir.path().join("zed");
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o444)).unwrap();
    let got = walk(temp_dir.path(), &WalkOptions::default());
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();

    let report = got.expect_err("a denied stat fails the walk");
    assert_eq!(report.current_context(), &WalkError);
    // The walk is sorted, so the first denied entry is `zed`'s first child.
    assert!(
        format!("{report:?}").contains("zed/deep"),
        "the report should name the entry it tripped over: {report:?}"
    );
}

#[derive(Default)]
struct Skipped(Mutex<Vec<(String, SkipReason)>>);

impl Progress for Skipped {
    fn skipped(&self, path: &str, reason: SkipReason) {
        self.0.lock().unwrap().push((path.to_owned(), reason));
    }
}

#[cfg(unix)]
#[test]
fn under_skip_an_unstatable_entry_is_counted_and_named_and_the_walk_goes_on() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = fixture("skipstat");
    let inner = temp_dir.path().join("zed");
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o444)).unwrap();
    let skipped = Skipped::default();
    let options = WalkOptions {
        progress: Some(&skipped),
        on_error: OnError::Skip,
        ..WalkOptions::default()
    };
    let got = walk(temp_dir.path(), &options);
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();

    let walked = got.unwrap();
    assert_eq!(walked.skips.failed, 2, "zed/deep and zed/z.bin");
    assert_eq!(
        *skipped.0.lock().unwrap(),
        [
            ("zed/deep".to_owned(), SkipReason::Failed),
            ("zed/z.bin".to_owned(), SkipReason::Failed),
        ]
    );
    assert!(walked.paths().iter().any(|path| path == "top.txt"));
}
