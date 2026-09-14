//! Fixtures shared by the integration tests: a self-removing directory, the
//! tree they all walk, and the oracle they check the walk against.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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

/// The oracle: `read_dir` plus a sort, which the walk cannot influence.
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
