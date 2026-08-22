//! End to end: build a tree on disk, walk it, and check what came back
//! against what was written.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tarseer::walk;

/// A directory that removes itself, named uniquely per process and call.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("tarseer-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create the fixture root");
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Files, directories and one symlink, deliberately not in sorted order on
/// disk.
fn fixture(tag: &str) -> TmpDir {
    let t = TmpDir::new(tag);
    let r = t.path();
    fs::create_dir_all(r.join("zed/deep")).unwrap();
    fs::create_dir_all(r.join("alpha")).unwrap();
    fs::write(r.join("top.txt"), b"top").unwrap();
    fs::write(r.join("zed/z.bin"), b"0123456789").unwrap();
    fs::write(r.join("zed/deep/d.txt"), b"deep!").unwrap();
    fs::write(r.join("alpha/a.txt"), b"a").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../top.txt", r.join("alpha/link")).unwrap();
    t
}

/// The oracle: `read_dir` plus a sort, which the walk cannot influence.
fn recurse(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let mut kids: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| (e.file_name(), e.file_type().unwrap()))
        .collect();
    kids.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, ft) in kids {
        let name = name.to_str().unwrap().to_owned();
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        out.push(path.clone());
        if ft.is_dir() {
            recurse(&dir.join(&name), &path, out);
        }
    }
}

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
