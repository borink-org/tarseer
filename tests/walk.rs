//! End to end: build a tree on disk, walk it, and check the index against what
//! was written.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tarseer::{Kind, PathScratch, index_tree, walk};

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

/// files, dirs and one symlink, deliberately not in sorted order on disk.
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

fn paths(index: &tarseer::Index) -> Vec<String> {
    let mut out = Vec::new();
    index.for_each_path(|_, p| out.push(p.to_owned()));
    out
}

#[test]
fn every_entry_is_indexed_in_path_order() {
    let t = fixture("order");
    let tree = walk(t.path()).unwrap();
    let index = index_tree(&tree).unwrap();

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
    assert_eq!(paths(&index), want);

    let mut sorted = paths(&index);
    sorted.sort();
    assert_eq!(paths(&index), sorted, "rows must be sorted by path");
}

#[test]
fn kinds_and_sizes_survive_the_round_trip() {
    let t = fixture("kinds");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();

    let mut sc = PathScratch::default();
    let mut seen = Vec::new();
    for i in 0..index.len() {
        let p = index.write_path(i, &mut sc).to_owned();
        seen.push((p, index.kind(i).unwrap(), index.size[i]));
    }

    assert!(seen.contains(&("top.txt".into(), Kind::File, 3)));
    assert!(seen.contains(&("zed/z.bin".into(), Kind::File, 10)));
    assert!(seen.contains(&("zed/deep/d.txt".into(), Kind::File, 5)));
    assert!(seen.contains(&("zed/deep".into(), Kind::Dir, 0)));

    assert_eq!(index.num_files, 4);
    assert_eq!(index.num_dirs, 3);
    assert_eq!(index.total_bytes, 3 + 10 + 5 + 1);
}

#[cfg(unix)]
#[test]
fn a_symlink_keeps_its_target_and_is_not_followed() {
    let t = fixture("symlink");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();

    let mut sc = PathScratch::default();
    let i = (0..index.len())
        .find(|&i| index.write_path(i, &mut sc) == "alpha/link")
        .expect("the symlink is indexed");
    assert_eq!(index.kind(i), Some(Kind::Symlink));
    assert_eq!(index.link(i), "../top.txt");
    assert_eq!(index.size[i], 0);
    assert_eq!(index.num_links(), 1);
}

// The oracle is `read_dir` plus a sort, which the walk cannot influence.
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
fn the_walk_order_matches_a_plain_recursive_sorted_walk() {
    let t = fixture("oracle");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();

    let mut want = Vec::new();
    recurse(t.path(), "", &mut want);
    want.sort();
    assert_eq!(paths(&index), want);
}

#[test]
fn an_empty_tree_indexes_to_nothing() {
    let t = TmpDir::new("empty");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    assert_eq!(index.count, 0);
    assert!(index.is_empty());
    assert_eq!(paths(&index), Vec::<String>::new());
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
    let index = index_tree(&tree).unwrap();
    assert!(paths(&index).contains(&"top.txt".to_owned()));
    assert!(!paths(&index).contains(&"zed/z.bin".to_owned()));
}
