//! End to end: build a tree on disk, walk it, and check what came back
//! against what was written.

mod common;

use std::fs;

use common::{TempDir, fixture, recurse};
use tarseer::{WalkError, walk};

#[test]
fn every_entry_is_found_in_path_order() {
    let temp_dir = fixture("order");
    let tree = walk(temp_dir.path()).unwrap();

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
    assert_eq!(tree.paths(), want);
}

#[test]
fn rows_carry_the_sizes_and_kinds_that_were_written() {
    let temp_dir = fixture("kinds");
    let tree = walk(temp_dir.path()).unwrap();

    let mut files: Vec<(&str, u64)> = tree
        .files
        .iter()
        .map(|row| (tree.text(row.path), row.size))
        .collect();
    files.sort_unstable();
    assert_eq!(
        files,
        [
            ("alpha/a.txt", 1),
            ("top.txt", 3),
            ("zed/deep/d.txt", 5),
            ("zed/z.bin", 10),
        ]
    );
    assert_eq!(tree.dirs.len(), 3);
    assert_eq!(tree.total_bytes(), 19);
}

#[cfg(unix)]
#[test]
fn a_symlink_keeps_its_target_and_is_not_followed() {
    let temp_dir = fixture("symlink");
    let tree = walk(temp_dir.path()).unwrap();
    assert_eq!(tree.links.len(), 1);
    let link = tree.links[0];
    assert_eq!(tree.text(link.path), "alpha/link");
    assert_eq!(tree.text(link.target), "../top.txt");
}

#[test]
fn the_walk_order_matches_a_plain_recursive_sorted_walk() {
    let temp_dir = fixture("oracle");
    let tree = walk(temp_dir.path()).unwrap();
    let mut want = Vec::new();
    recurse(temp_dir.path(), "", &mut want);
    want.sort();
    assert_eq!(tree.paths(), want);
}

#[test]
fn an_empty_tree_holds_nothing() {
    let temp_dir = TempDir::new("empty");
    let tree = walk(temp_dir.path()).unwrap();
    assert!(tree.is_empty());
    assert_eq!(tree.paths(), Vec::<&str>::new());
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_counted_and_the_rest_still_walks() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = fixture("locked");
    let locked = temp_dir.path().join("zed");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let tree = walk(temp_dir.path());
    // Restore before asserting, so a failure still lets the fixture clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let tree = tree.unwrap();
    assert_eq!(tree.skips.unreadable, 1);
    assert!(tree.paths().contains(&"top.txt"));
    assert!(!tree.paths().contains(&"zed/z.bin"));
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
    let got = walk(temp_dir.path());
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();

    let report = got.err().expect("a denied stat fails the walk");
    assert_eq!(report.current_context(), &WalkError);
    // The context is asserted on above; what the text has to carry is the one
    // thing the caller could not have worked out — which entry it was. The
    // walk is sorted, so the first denied entry is `zed`'s first child.
    assert!(
        format!("{report:?}").contains("zed/deep"),
        "the report should name the entry it tripped over: {report:?}"
    );
}
