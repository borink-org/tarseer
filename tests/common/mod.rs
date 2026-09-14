//! Fixtures shared by the integration tests: a self-removing directory, the
//! trees they walk, and the oracle they check the walk against.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tarseer::{Walk, WalkOptions, walk};

/// A directory that removes itself, named uniquely per process and call.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tarseer-{tag}-{}-{count}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create the fixture root");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Files, directories and one symlink, deliberately not in sorted order on
/// disk.
pub fn fixture(tag: &str) -> TempDir {
    let temp_dir = TempDir::new(tag);
    let root = temp_dir.path();
    fs::create_dir_all(root.join("zed/deep")).unwrap();
    fs::create_dir_all(root.join("alpha")).unwrap();
    fs::write(root.join("top.txt"), b"top").unwrap();
    fs::write(root.join("zed/z.bin"), b"0123456789").unwrap();
    fs::write(root.join("zed/deep/d.txt"), b"deep!").unwrap();
    fs::write(root.join("alpha/a.txt"), b"a").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../top.txt", root.join("alpha/link")).unwrap();
    temp_dir
}

/// A tree big enough to be cut many ways: wide and deep directories, empty
/// ones, names that sort differently as paths than as components (`a!` against
/// `a/`), and symlinks on Unix.
pub fn wide_fixture(tag: &str) -> TempDir {
    let temp_dir = TempDir::new(tag);
    let root = temp_dir.path();
    let mut chain = root.join("chain");
    for depth in 0..12 {
        chain = chain.join(format!("level{depth}"));
        fs::create_dir_all(&chain).unwrap();
        fs::write(chain.join("leaf.txt"), vec![b'x'; depth]).unwrap();
    }
    fs::create_dir_all(root.join("flat")).unwrap();
    for index in 0..60 {
        fs::write(root.join(format!("flat/file{index:03}.dat")), b"flat").unwrap();
    }
    for group in ["a", "a!", "a.b", "b"] {
        let dir = root.join(group);
        fs::create_dir_all(dir.join("inner")).unwrap();
        for index in 0..7 {
            fs::write(dir.join(format!("f{index}")), b"g").unwrap();
            fs::write(dir.join(format!("inner/g{index}")), b"h").unwrap();
        }
    }
    fs::create_dir_all(root.join("empty/also_empty")).unwrap();
    fs::write(root.join("a.txt"), b"top").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../a.txt", root.join("b/to_top")).unwrap();
        std::os::unix::fs::symlink("nowhere/at/all", root.join("dangling")).unwrap();
    }
    temp_dir
}

/// Walk with default options.
pub fn walk_default(root: &Path) -> Walk {
    walk(root, &WalkOptions::default()).unwrap()
}

/// The oracle: `read_dir` plus a sort per directory, which the walk cannot
/// influence. Paths come out in walk order.
pub fn recurse(dir: &Path, path: &str, out: &mut Vec<String>) {
    let mut kids: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .map(|entry| (entry.file_name(), entry.file_type().unwrap()))
        .collect();
    kids.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, ft) in kids {
        let name = name.to_str().unwrap().to_owned();
        let path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        out.push(path.clone());
        if ft.is_dir() {
            recurse(&dir.join(&name), &path, out);
        }
    }
}
