// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! The source walk: every entry under a root, deterministically ordered.
//!
//! - depth-first, each directory's entries sorted by name
//! - that order matters: later stages split work and order the archive by it
//! - serial for now; the parallel walk lands later and uses this as its oracle

use std::fmt;
use std::path::Path;

use error_stack::{Report, ResultExt as _};

use crate::tree::{Skips, SourceTree};

/// The walk could not finish.
///
/// - one context for the whole walk: every failure is the same to a caller —
///   the filesystem would not answer for an entry it had already named
/// - attached: which entry, since a caller cannot reconstruct it
/// - not attached: the root, which the caller passed in
/// - [`TreeFull`](crate::tree::TreeFull) is a limit, not a refusal, and stays
///   distinguishable: `report.contains::<TreeFull>()`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkError;

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not walk the source tree")
    }
}

impl std::error::Error for WalkError {}

/// Walk `root` recursively: files, directories and symlinks, as relative
/// forward-slash paths.
///
/// - unrecordable entries counted in [`SourceTree::skips`], not fatal
/// - an unreadable directory costs its subtree; the rest still walks, which is
///   what makes `$HOME` or `/var` walkable at all
///
/// # Errors
/// [`WalkError`]: an entry's type or metadata unreadable, or the tree outgrew
/// the `u32` indices used throughout.
pub fn walk(root: &Path) -> Result<SourceTree, Report<WalkError>> {
    let mut tree = SourceTree::default();
    let mut path = String::new();
    scan(root, &mut path, &mut tree)?;
    Ok(tree)
}

fn scan(dir: &Path, path: &mut String, tree: &mut SourceTree) -> Result<(), Report<WalkError>> {
    // A directory that cannot be read costs its whole subtree, but it is
    // counted and the rest of the tree is still walked.
    let Ok(listing) = std::fs::read_dir(dir) else {
        tree.skips.add(Skips {
            unreadable: 1,
            ..Skips::default()
        });
        return Ok(());
    };

    let mut entries: Vec<(std::ffi::OsString, std::fs::FileType, std::fs::DirEntry)> = Vec::new();
    for entry in listing {
        let entry = entry
            .attach_with(|| format!("listing {}", dir.display()))
            .change_context(WalkError)?;
        let file_type = entry
            .file_type()
            .attach_with(|| format!("kind of {}", entry.path().display()))
            .change_context(WalkError)?;
        entries.push((entry.file_name(), file_type, entry));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, file_type, entry) in entries {
        let Some(name) = name.to_str() else {
            tree.skips.non_utf8 += 1;
            continue;
        };
        let parent_len = path.len();
        if parent_len > 0 {
            path.push('/');
        }
        path.push_str(name);

        if file_type.is_symlink() {
            match read_link(&entry.path())? {
                Some((target, mtime)) => tree
                    .push_link(path, &target, mtime)
                    .attach_with(|| format!("recording {path}"))
                    .change_context(WalkError)?,
                None => tree.skips.non_utf8 += 1,
            }
        } else if file_type.is_dir() || file_type.is_file() {
            let metadata = entry
                .metadata()
                .attach_with(|| format!("metadata of {}", entry.path().display()))
                .change_context(WalkError)?;
            let mtime = metadata.modified().ok().map_or(0, system_time_to_unix);
            let mode = mode_of(&metadata, file_type.is_dir());
            if file_type.is_dir() {
                tree.push_dir(path, mtime, mode)
                    .attach_with(|| format!("recording {path}"))
                    .change_context(WalkError)?;
                scan(&entry.path(), path, tree)?;
            } else {
                tree.push_file(path, mtime, metadata.len(), mode)
                    .attach_with(|| format!("recording {path}"))
                    .change_context(WalkError)?;
            }
        } else {
            tree.skips.special += 1;
        }

        path.truncate(parent_len);
    }
    Ok(())
}

// Read the symlink at `absolute`: its stored target and its own mtime, or
// `None` if the target is not UTF-8 and so cannot go in the tree.
fn read_link(absolute: &Path) -> Result<Option<(String, i64)>, Report<WalkError>> {
    let target = std::fs::read_link(absolute)
        .attach_with(|| format!("target of {}", absolute.display()))
        .change_context(WalkError)?;
    let Some(target) = target.to_str() else {
        return Ok(None);
    };
    let mtime = std::fs::symlink_metadata(absolute)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map_or(0, system_time_to_unix);
    let target = if target.contains('\\') {
        target.replace('\\', "/")
    } else {
        String::from(target)
    };
    Ok(Some((target, mtime)))
}

fn system_time_to_unix(time: std::time::SystemTime) -> i64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn mode_of(metadata: &std::fs::Metadata, is_dir: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = is_dir;
        metadata.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (is_dir, metadata.permissions().readonly()) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}
