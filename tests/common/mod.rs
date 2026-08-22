//! Fixtures shared by the test suites: a self-removing temp directory, the
//! tree they all walk, and the oracle the walk's order is checked against.
//!
//! Each suite imports only the helpers it uses.
#![allow(dead_code, clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory that removes itself, named uniquely per process and call.
pub struct TmpDir(PathBuf);

impl TmpDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("tarseer-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create the fixture root");
        Self(p)
    }

    pub fn path(&self) -> &Path {
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
pub fn fixture(tag: &str) -> TmpDir {
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
pub fn recurse(dir: &Path, rel: &str, out: &mut Vec<String>) {
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
