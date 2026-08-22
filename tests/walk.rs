//! End to end: build a tree on disk, walk it, and check what came back
//! against what was written.

mod common;

use std::fs;

use common::{TmpDir, fixture, recurse};
use tarseer::walk;

#[test]
fn every_entry_is_found_in_path_order() {
    let t = fixture("order");
    let tree = walk(t.path()).unwrap();

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
    let t = fixture("kinds");
    let tree = walk(t.path()).unwrap();

    let mut files: Vec<(&str, u64)> = tree
        .files
        .iter()
        .map(|f| (tree.text(f.rel), f.size))
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
    let t = fixture("symlink");
    let tree = walk(t.path()).unwrap();
    assert_eq!(tree.links.len(), 1);
    let l = tree.links[0];
    assert_eq!(tree.text(l.rel), "alpha/link");
    assert_eq!(tree.text(l.target), "../top.txt");
}

#[test]
fn the_walk_order_matches_a_plain_recursive_sorted_walk() {
    let t = fixture("oracle");
    let tree = walk(t.path()).unwrap();
    let mut want = Vec::new();
    recurse(t.path(), "", &mut want);
    want.sort();
    assert_eq!(tree.paths(), want);
}

#[test]
fn an_empty_tree_holds_nothing() {
    let t = TmpDir::new("empty");
    let tree = walk(t.path()).unwrap();
    assert!(tree.is_empty());
    assert_eq!(tree.paths(), Vec::<&str>::new());
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_counted_and_the_rest_still_walks() {
    use std::os::unix::fs::PermissionsExt;

    let t = fixture("locked");
    let locked = t.path().join("zed");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let tree = walk(t.path());
    // Restore before asserting, so a failure still lets the fixture clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let tree = tree.unwrap();
    assert_eq!(tree.skips.unreadable, 1);
    assert!(tree.paths().contains(&"top.txt"));
    assert!(!tree.paths().contains(&"zed/z.bin"));
}

#[test]
fn the_tree_is_sized_before_it_is_filled_and_nothing_grows() {
    // The fill pass is the walk's only serial one, so a buffer that grows
    // there memmoves on the critical path. The fixture has a symlink because a
    // link is the one row that interns two strings.
    let t = fixture("sizing");
    let tree = walk(t.path()).unwrap();

    assert_eq!(tree.text_bytes(), tree.text_capacity(), "text tape grew");
    assert_eq!(tree.files.len(), tree.files.capacity(), "files grew");
    assert_eq!(tree.dirs.len(), tree.dirs.capacity(), "dirs grew");
    assert_eq!(tree.links.len(), tree.links.capacity(), "links grew");
}

#[test]
fn a_wide_flat_directory_comes_back_whole() {
    // One directory of many entries is the case a per-directory scan cannot
    // spread, and so takes a different path through the walk.
    let t = TmpDir::new("wide");
    for i in 0..2000 {
        fs::write(t.path().join(format!("f{i:05}")), b"x").unwrap();
    }
    let tree = walk(t.path()).unwrap();
    assert_eq!(tree.files.len(), 2000);
    assert_eq!(tree.paths().first(), Some(&"f00000"));
    assert_eq!(tree.paths().last(), Some(&"f01999"));
}
