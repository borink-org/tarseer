//! A tree that is deeper, and wider, than the process has file descriptors
//! for. This test lowers the limit of its whole process, so it has a test
//! binary to itself.

#![cfg(unix)]

mod common;

use std::fs;

use common::{TempDir, walk_default};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tarseer::{WalkOptions, walk};

#[test]
fn a_tree_larger_than_the_descriptor_limit_is_walked_whole() {
    const DEPTH: usize = 200;
    const WIDTH: usize = 300;

    let temp_dir = TempDir::new("handles");
    let mut directory = temp_dir.path().join("deep");
    for level in 0..DEPTH {
        directory.push(format!("d{level}"));
        fs::create_dir_all(&directory).expect("create a level");
        // One file sorts before the subdirectory and one after, so the walk
        // comes back to every level.
        fs::write(directory.join("a-file"), b"x").expect("write");
        fs::write(directory.join("z-file"), b"x").expect("write");
    }
    // Every subdirectory here is found by one scan, which keeps as many of
    // them open as it can for the scans that follow. The first has directories
    // below it, and the walk reaches those while its later siblings still hold
    // every descriptor there is.
    for index in 0..WIDTH {
        let directory = temp_dir.path().join(format!("wide/w{index:03}"));
        fs::create_dir_all(&directory).expect("create a directory");
        fs::write(directory.join("file"), b"x").expect("write");
    }
    let below = temp_dir.path().join("wide/w000/below/further");
    fs::create_dir_all(&below).expect("create a directory");
    fs::write(below.join("file"), b"x").expect("write");
    let want = walk_default(temp_dir.path()).paths();
    assert_eq!(want.len(), 1 + DEPTH * 3 + 1 + WIDTH * 2 + 3);

    let limit = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(64),
            maximum: limit.maximum,
        },
    )
    .expect("lower the descriptor limit");
    let walked = walk(temp_dir.path(), &WalkOptions::default());
    setrlimit(Resource::Nofile, limit).expect("restore the descriptor limit");

    let walked = walked.expect("the walk gets by on the descriptors it has");
    assert!(!walked.skips.any());
    assert_eq!(walked.paths(), want);
}
