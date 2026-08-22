//! Content digests: that they are blake3 of the bytes on disk, that they land
//! on the right rows, and that a file changing under the walk is an error.

mod common;

use common::{TmpDir, fixture};
use tarseer::{PathScratch, hash_tree, index_tree, index_tree_with, walk};

#[test]
fn every_file_gets_the_blake3_of_its_bytes() {
    let t = fixture("hash");
    let tree = walk(t.path()).unwrap();
    let sums = hash_tree(t.path(), &tree).unwrap();
    assert_eq!(sums.len(), tree.files.len());

    for (row, sum) in tree.files.iter().zip(&sums) {
        let bytes = std::fs::read(t.path().join(tree.text(row.rel))).unwrap();
        assert_eq!(
            sum,
            blake3::hash(&bytes).as_bytes(),
            "{}",
            tree.text(row.rel)
        );
    }
}

#[test]
fn digests_land_on_the_files_and_nowhere_else() {
    let t = fixture("hash-rows");
    let tree = walk(t.path()).unwrap();
    let sums = hash_tree(t.path(), &tree).unwrap();
    let index = index_tree_with(&tree, &sums).unwrap();

    assert_eq!(index.checksum_algo, "blake3");
    let mut sc = PathScratch::default();
    for i in 0..index.len() {
        let path = index.write_path(i, &mut sc).to_owned();
        let sum = index.checksum(i);
        match index.kind(i) {
            Some(tarseer::Kind::File) => {
                assert_eq!(sum.len(), 64, "{path}");
                assert!(sum.bytes().all(|b| b.is_ascii_hexdigit()), "{path}");
                let want = blake3::hash(&std::fs::read(t.path().join(&path)).unwrap());
                assert_eq!(sum, want.to_hex().as_str(), "{path}");
            }
            _ => assert!(sum.is_empty(), "{path} is not a file but carries a digest"),
        }
    }
}

#[test]
fn an_index_without_digests_says_so() {
    let t = fixture("hash-none");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    assert!(index.checksum_algo.is_empty());
    for i in 0..index.len() {
        assert!(index.checksum(i).is_empty());
    }
}

#[test]
fn a_file_that_changed_since_the_walk_is_an_error() {
    // The size in the tree is what later stages reserve space for, so a digest
    // certifying different bytes than the ones stored is worse than none.
    let t = TmpDir::new("hash-changed");
    let f = t.path().join("grows.txt");
    std::fs::write(&f, b"short").unwrap();
    let tree = walk(t.path()).unwrap();
    std::fs::write(&f, b"much longer than before").unwrap();

    let e = hash_tree(t.path(), &tree).unwrap_err().to_string();
    assert!(e.contains("grows.txt"), "{e}");
    assert!(e.contains("walk time"), "{e}");
}

#[test]
fn a_file_too_big_to_hold_whole_hashes_the_same() {
    // Past the whole-read threshold the file is read in chunks instead, which
    // must not change the digest.
    let t = TmpDir::new("hash-big");
    let big = vec![0xab; (9 << 20) + 12345];
    std::fs::write(t.path().join("big.bin"), &big).unwrap();

    let tree = walk(t.path()).unwrap();
    let sums = hash_tree(t.path(), &tree).unwrap();
    assert_eq!(sums.len(), 1);
    assert_eq!(&sums[0], blake3::hash(&big).as_bytes());
}

#[test]
fn a_mismatched_digest_count_is_refused() {
    let t = fixture("hash-count");
    let tree = walk(t.path()).unwrap();
    let e = index_tree_with(&tree, &[[0u8; 32]])
        .unwrap_err()
        .to_string();
    assert!(e.contains("digests for"), "{e}");
}
