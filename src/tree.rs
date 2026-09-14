// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! The results of a walk.
//!
//! - rows split by kind, not tagged: every consumer wants one kind at a time
//! - frame planner reads sizes; extractor makes directories first; the index
//!   interleaves all three exactly once

use std::fmt;

use error_stack::{Report, ResultExt as _};

use crate::tape::StrTape;

/// The tree outgrew the `u32` it addresses its text with: >4 GiB of paths, or
/// >`u32::MAX` of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeFull;

impl fmt::Display for TreeFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // review: is this a good error string? i feel we can do better
        f.write_str("source tree outgrew its u32 addressing")
    }
}

impl std::error::Error for TreeFull {}

/// A file.
///
/// - `path`: index into the tree's tape, holding the path relative to the
///   walk root, forward slashes.
/// - `mode`: on Unix these are the real Unix bits, on Windows they are derived from the read-only attribute, matching `tar`
/// - `mtime`: unix seconds (0 if unknown)
#[derive(Clone, Copy)]
pub struct FileRow {
    pub path: u32,
    pub mtime: i64,
    /// Size at walk time.
    pub size: u64,
    pub mode: u32,
}

/// A directory. Fields as [`FileRow`].
#[derive(Clone, Copy)]
pub struct DirRow {
    pub path: u32,
    pub mtime: i64,
    pub mode: u32,
}

/// A symbolic link. Path here is simply where the symbolic link lives.
#[derive(Clone, Copy)]
pub struct LinkRow {
    pub path: u32,
    /// Target as stored, forward slashes. Also in the tape.
    pub target: u32,
    pub mtime: i64,
}

/// Records entries the walk could not record and skipped instead.
///
/// - found *during* the walk, so the entry never enters the tree at all
/// - these failures are fatal when they occur in the later stage when bytes have been reserved
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skips {
    /// Sockets, fifos, devices: no bytes and no target.
    pub special: u32,
    /// Not UTF-8. The index is JSON, with nowhere to put them.
    pub non_utf8: u32,
    /// Unreadable, usually permissions. The subtree below is missing too.
    pub unreadable: u32,
}

impl Skips {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.special as u64 + self.non_utf8 as u64 + self.unreadable as u64
    }

    #[must_use]
    pub const fn any(self) -> bool {
        self.total() > 0
    }

    pub const fn add(&mut self, other: Self) {
        self.special += other.special;
        self.non_utf8 += other.non_utf8;
        self.unreadable += other.unreadable;
    }
}

/// Everything a walk found.
#[derive(Default)]
pub struct SourceTree {
    text: StrTape,
    pub files: Vec<FileRow>,
    pub dirs: Vec<DirRow>,
    pub links: Vec<LinkRow>,
    pub skips: Skips,
}

// review: progress until here

impl SourceTree {
    /// The string at tape index `index`: a row's `path`, or a link's `target`.
    ///
    /// # Panics
    /// If `index` is not from a row of this tree. Every row index came from a push
    /// below, so an invalid one is a bug here, not a caller's doing.
    #[must_use]
    pub fn text(&self, index: u32) -> &str {
        self.text
            .get(index as usize)
            .expect("source tree tape index in range")
    }

    /// Every path, sorted. The order the index stores and later stages plan
    /// in.
    #[must_use]
    pub fn paths(&self) -> Vec<&str> {
        let mut all: Vec<&str> = Vec::with_capacity(self.len());
        all.extend(self.dirs.iter().map(|row| self.text(row.path)));
        all.extend(self.files.iter().map(|row| self.text(row.path)));
        all.extend(self.links.iter().map(|row| self.text(row.path)));
        all.sort_unstable();
        all
    }

    /// Total entries across all three columns.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len() + self.dirs.len() + self.links.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of text held. Capacity hint for the index's path column.
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        self.text.bytes()
    }

    /// Sum of every file's size at walk time.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    fn intern(&mut self, text: &str) -> Result<u32, Report<TreeFull>> {
        let index = u32::try_from(self.text.len()).change_context(TreeFull)?;
        self.text.push(text).change_context(TreeFull)?;
        Ok(index)
    }

    /// Record a file.
    ///
    /// # Errors
    /// [`TreeFull`]: text tape or entry count overflowed.
    pub fn push_file(
        &mut self,
        path: &str,
        mtime: i64,
        size: u64,
        mode: u32,
    ) -> Result<(), Report<TreeFull>> {
        let path = self.intern(path)?;
        self.files.push(FileRow {
            path,
            mtime,
            size,
            mode,
        });
        Ok(())
    }

    /// Record a directory.
    ///
    /// # Errors
    /// As [`SourceTree::push_file`].
    pub fn push_dir(&mut self, path: &str, mtime: i64, mode: u32) -> Result<(), Report<TreeFull>> {
        let path = self.intern(path)?;
        self.dirs.push(DirRow { path, mtime, mode });
        Ok(())
    }

    /// Record a symlink and its target.
    ///
    /// # Errors
    /// As [`SourceTree::push_file`].
    pub fn push_link(
        &mut self,
        path: &str,
        target: &str,
        mtime: i64,
    ) -> Result<(), Report<TreeFull>> {
        let path = self.intern(path)?;
        let target = self.intern(target)?;
        self.links.push(LinkRow {
            path,
            target,
            mtime,
        });
        Ok(())
    }
}
