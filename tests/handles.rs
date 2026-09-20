//! A tree deeper than the process has file descriptors for. This test lowers
//! the limit of its whole process, so it has a test binary to itself.
//!
//! Only the linux reader recovers. Through `std::fs`, every listed entry keeps
//! its directory open, and std gives no way to let go of it.

#![cfg(all(target_os = "linux", not(tarseer_portable_reader)))]

mod common;

use std::fs;

use common::{TempDir, walk_default};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

#[test]
fn a_tree_deeper_than_the_descriptor_limit_is_walked_whole() {
    const DEPTH: usize = 200;

    let temp_dir = TempDir::new("handles");
    let mut directory = temp_dir.path().to_path_buf();
    for level in 0..DEPTH {
        directory.push(format!("d{level}"));
        fs::create_dir(&directory).expect("create a level");
        fs::write(directory.join("a-file"), b"x").expect("write");
        fs::write(directory.join("z-file"), b"x").expect("write");
    }

    let limit = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(64),
            maximum: limit.maximum,
        },
    )
    .expect("lower the descriptor limit");

    let walked = walk_default(temp_dir.path());
    setrlimit(Resource::Nofile, limit).expect("restore the descriptor limit");

    // Each level holds a file that sorts before its subdirectory and one that
    // sorts after, so the walk returns to every level once its handle is gone.
    assert_eq!(walked.len(), DEPTH * 3);
    assert!(!walked.skips.any());
}
