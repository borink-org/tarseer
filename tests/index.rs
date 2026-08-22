//! The index: what the columns say, and that a path can be rebuilt from them.

mod common;

use common::{TmpDir, fixture};
use tarseer::{Kind, PathScratch, index_tree, walk};

fn paths(index: &tarseer::Index) -> Vec<String> {
    let mut out = Vec::new();
    index.for_each_path(|_, p| out.push(p.to_owned()));
    out
}

#[test]
fn every_entry_is_indexed_in_path_order() {
    let t = fixture("index-order");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();

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
    let t = fixture("index-kinds");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();

    let mut sc = PathScratch::default();
    let mut seen = Vec::new();
    for i in 0..index.len() {
        let p = index.write_path(i, &mut sc).to_owned();
        seen.push((p, index.kind(i).unwrap(), index.size[i]));
    }

    assert!(seen.contains(&("top.txt".into(), Kind::File, 3)));
    assert!(seen.contains(&("zed/z.bin".into(), Kind::File, 10)));
    assert!(seen.contains(&("zed/deep".into(), Kind::Dir, 0)));
    assert_eq!(index.num_files, 4);
    assert_eq!(index.num_dirs, 3);
    assert_eq!(index.total_bytes, 3 + 10 + 5 + 1);
}

#[cfg(unix)]
#[test]
fn a_symlink_keeps_its_target_and_carries_no_size() {
    let t = fixture("index-symlink");
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

#[test]
fn a_deep_chain_interns_each_directory_once() {
    // The directory table is the index's whole space argument: a path costs
    // its own last component, not its ancestry.
    let t = TmpDir::new("index-deep");
    let deep = t.path().join("a/b/c/d/e");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("leaf.txt"), b"x").unwrap();
    std::fs::write(deep.join("other.txt"), b"y").unwrap();

    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    assert_eq!(
        paths(&index),
        [
            "a",
            "a/b",
            "a/b/c",
            "a/b/c/d",
            "a/b/c/d/e",
            "a/b/c/d/e/leaf.txt",
            "a/b/c/d/e/other.txt",
        ]
    );
    // Root plus a, b, c, d, e — each named once, and the two leaves share the
    // last of them rather than repeating the chain.
    assert_eq!(index.dir_parent.len(), 6);
}

#[test]
fn an_empty_tree_indexes_to_nothing() {
    let t = TmpDir::new("index-empty");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    assert_eq!(index.count, 0);
    assert!(index.is_empty());
    assert_eq!(paths(&index), Vec::<String>::new());
}
