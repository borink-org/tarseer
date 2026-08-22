//! Hard links: two names, one file. The index says so without pretending
//! either name is less real than the other.

#![cfg(unix)]

mod common;

use common::TmpDir;
use tarseer::{PathScratch, index_tree, walk};

fn row_of(index: &tarseer::Index, want: &str) -> usize {
    let mut sc = PathScratch::default();
    (0..index.len())
        .find(|&i| index.write_path(i, &mut sc) == want)
        .unwrap_or_else(|| panic!("{want} is indexed"))
}

fn linked_fixture(tag: &str) -> TmpDir {
    let t = TmpDir::new(tag);
    std::fs::write(t.path().join("a.txt"), b"shared bytes").unwrap();
    std::fs::hard_link(t.path().join("a.txt"), t.path().join("b.txt")).unwrap();
    std::fs::write(t.path().join("alone.txt"), b"not shared").unwrap();
    t
}

#[test]
fn a_second_name_points_back_at_the_first() {
    let t = linked_fixture("hl");
    let tree = walk(t.path()).unwrap();
    assert_eq!(tree.linked.len(), 2, "both names carry an identity");

    let index = index_tree(&tree).unwrap();
    let (a, b) = (row_of(&index, "a.txt"), row_of(&index, "b.txt"));
    assert!(a < b, "rows are in path order");
    assert_eq!(index.same_as(a), None, "the first name is the target");
    assert_eq!(index.same_as(b), Some(a));
    assert_eq!(index.num_hardlinks(), 1);

    // Both rows are still complete files: a reader that ignores `same_as` gets
    // a correct tree with the bytes stored twice.
    assert_eq!(index.size[a], index.size[b]);
    assert_eq!(index.kind(a), index.kind(b));
}

#[test]
fn a_tree_with_no_hard_links_carries_no_side_table() {
    let t = TmpDir::new("hl-none");
    std::fs::write(t.path().join("only.txt"), b"x").unwrap();
    let tree = walk(t.path()).unwrap();
    assert!(tree.linked.is_empty());

    let index = index_tree(&tree).unwrap();
    assert_eq!(index.num_hardlinks(), 0);
    assert!((0..index.len()).all(|i| index.same_as(i).is_none()));
}

#[test]
fn a_file_linked_only_outside_the_tree_stays_ordinary() {
    // nlink > 1 is a filter, not the decision: what matters is whether two
    // *recorded* paths share an inode.
    let outer = TmpDir::new("hl-outer");
    let inner = TmpDir::new("hl-inner");
    std::fs::write(outer.path().join("target.txt"), b"y").unwrap();
    std::fs::hard_link(
        outer.path().join("target.txt"),
        inner.path().join("seen.txt"),
    )
    .unwrap();

    let index = index_tree(&walk(inner.path()).unwrap()).unwrap();
    assert_eq!(index.num_hardlinks(), 0);
    assert_eq!(index.same_as(row_of(&index, "seen.txt")), None);
}
