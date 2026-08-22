//! The source walk: every entry under a root, deterministically ordered.
//!
//! Depth-first, over each directory's entries sorted by name. That order is a
//! guarantee rather than an accident — later stages pack and extract in it, so
//! the archive's bytes depend on it, and `the_walk_order_matches_a_plain_
//! recursive_sorted_walk` pins it against an oracle that this code cannot
//! influence.
//!
//! Nothing here is parallel yet. It is the shape the parallel walk has to
//! reproduce exactly.

use std::path::Path;

use crate::error::{Context, Result};
use crate::tree::{Skips, SourceTree};

/// Walk `root` recursively, collecting files, directories and symlinks with
/// relative forward-slash paths.
///
/// Entries that cannot be recorded are counted in [`SourceTree::skips`] rather
/// than failing the walk — an unreadable directory costs its subtree, but the
/// rest still gets walked, which is what makes walking `$HOME` or `/var`
/// possible at all.
///
/// # Errors
/// If a directory entry's type or metadata cannot be read, or the tree
/// outgrows the `u32` indices used throughout.
pub fn walk(root: &Path) -> Result<SourceTree> {
    let mut tree = SourceTree::default();
    let mut rel = String::new();
    scan(root, &mut rel, &mut tree)?;
    Ok(tree)
}

fn scan(dir: &Path, rel: &mut String, tree: &mut SourceTree) -> Result<()> {
    // A directory that cannot be read costs its whole subtree, but it is
    // counted and the rest of the tree is still walked.
    let Ok(rd) = std::fs::read_dir(dir) else {
        tree.skips.add(Skips {
            unreadable: 1,
            ..Skips::default()
        });
        return Ok(());
    };

    let mut ents: Vec<(std::ffi::OsString, std::fs::FileType, std::fs::DirEntry)> = Vec::new();
    for e in rd {
        let e = e.ctx(|| format!("dir entry in {}", dir.display()))?;
        let ft = e
            .file_type()
            .ctx(|| format!("file_type {}", e.path().display()))?;
        ents.push((e.file_name(), ft, e));
    }
    ents.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, ft, e) in ents {
        let Some(name) = name.to_str() else {
            tree.skips.non_utf8 += 1;
            continue;
        };
        let mark = rel.len();
        if mark > 0 {
            rel.push('/');
        }
        rel.push_str(name);

        if ft.is_symlink() {
            match read_link(&e.path())? {
                Some((target, mtime)) => tree.push_link(rel, &target, mtime)?,
                None => tree.skips.non_utf8 += 1,
            }
        } else if ft.is_dir() || ft.is_file() {
            let m = e
                .metadata()
                .ctx(|| format!("metadata {}", e.path().display()))?;
            let mtime = m.modified().ok().map_or(0, system_time_to_unix);
            let mode = mode_of(&m, ft.is_dir());
            if ft.is_dir() {
                tree.push_dir(rel, mtime, mode)?;
                scan(&e.path(), rel, tree)?;
            } else {
                tree.push_file(rel, mtime, m.len(), mode)?;
            }
        } else {
            tree.skips.special += 1;
        }

        rel.truncate(mark);
    }
    Ok(())
}

/// Read the symlink at `abs`: its stored target and its own mtime, or `None`
/// if the target is not UTF-8 and so cannot go in the tree.
fn read_link(abs: &Path) -> Result<Option<(String, i64)>> {
    let target = std::fs::read_link(abs).ctx(|| format!("read link {}", abs.display()))?;
    let Some(target) = target.to_str() else {
        return Ok(None);
    };
    let mtime = std::fs::symlink_metadata(abs)
        .ok()
        .and_then(|m| m.modified().ok())
        .map_or(0, system_time_to_unix);
    let target = if target.contains('\\') {
        target.replace('\\', "/")
    } else {
        String::from(target)
    };
    Ok(Some((target, mtime)))
}

fn system_time_to_unix(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn mode_of(m: &std::fs::Metadata, is_dir: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = is_dir;
        m.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (is_dir, m.permissions().readonly()) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}
