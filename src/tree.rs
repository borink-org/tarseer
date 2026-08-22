//! What a walk found: three columns of fixed-size `Copy` rows over one tape.
//!
//! Rows are separated by kind rather than tagged, because every consumer wants
//! one kind at a time: the frame planner reads sizes, the extractor creates
//! directories first, and the index interleaves all three exactly once.

use crate::error::{Context, Result};
use crate::tape::StrTape;

/// A file, minus where it lives — the path a driver opens is its own business.
///
/// `rel` indexes the tree's tape; `mode` is real Unix bits on Unix and derived
/// from the read-only attribute on Windows, the mapping `tar` itself uses.
/// `mtime` is unix seconds, 0 if the filesystem would not say.
#[derive(Clone, Copy)]
pub struct FileRow {
    pub rel: u32,
    pub mtime: i64,
    /// Size at walk time.
    pub size: u64,
    pub mode: u32,
}

/// A directory. Fields as [`FileRow`].
#[derive(Clone, Copy)]
pub struct DirRow {
    pub rel: u32,
    pub mtime: i64,
    pub mode: u32,
}

/// A symbolic link. Never followed: the link is the thing being recorded, not
/// whatever it points at.
#[derive(Clone, Copy)]
pub struct LinkRow {
    pub rel: u32,
    /// The target exactly as stored, with forward slashes — also in the tape.
    pub target: u32,
    pub mtime: i64,
}

/// What a walk met and could not record, by reason.
///
/// Everything counted here is found *during* the walk, which is what makes
/// skipping it clean — the entry simply never enters the tree. A failure
/// discovered later, once a plan has reserved bytes for an entry, is fatal
/// instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skips {
    /// Sockets, fifos, devices — anything with no bytes and no target.
    pub special: u32,
    /// Names that are not UTF-8. The index is JSON, with nowhere to put them.
    pub non_utf8: u32,
    /// Directories that could not be read, usually for want of permission.
    /// The whole subtree below one is missing.
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

    pub const fn add(&mut self, o: Self) {
        self.special += o.special;
        self.non_utf8 += o.non_utf8;
        self.unreadable += o.unreadable;
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

impl SourceTree {
    /// A tree sized for what the walk already counted.
    ///
    /// This exists because the walk fills it in one *serial* pass, so a buffer
    /// that grows there memmoves on the critical path — around 10 MiB on a
    /// 180,000-file tree, plus the doubled peak while each old buffer is live
    /// beside its replacement.
    #[must_use]
    pub fn with_capacity(files: usize, dirs: usize, links: usize, text_bytes: usize) -> Self {
        Self {
            // A link is the one row that interns two strings.
            text: StrTape::with_capacity(text_bytes, files + dirs + 2 * links),
            files: Vec::with_capacity(files),
            dirs: Vec::with_capacity(dirs),
            links: Vec::with_capacity(links),
            skips: Skips::default(),
        }
    }

    /// The string at tape index `i` — a row's `rel`, or a link's `target`.
    ///
    /// # Panics
    /// If `i` did not come from a row of this tree. Every index in a row was
    /// returned by a push below, so an invalid one is a bug here rather than
    /// anything a caller can cause.
    #[must_use]
    pub fn text(&self, i: u32) -> &str {
        self.text
            .get(i as usize)
            .expect("source tree tape index in range")
    }

    /// Every path in the tree, sorted — the order the index will store and
    /// every later stage will plan in.
    #[must_use]
    pub fn paths(&self) -> Vec<&str> {
        let mut all: Vec<&str> = Vec::with_capacity(self.len());
        all.extend(self.dirs.iter().map(|r| self.text(r.rel)));
        all.extend(self.files.iter().map(|r| self.text(r.rel)));
        all.extend(self.links.iter().map(|r| self.text(r.rel)));
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

    /// Bytes the text tape can hold before it grows — see
    /// [`StrTape::text_capacity`].
    #[must_use]
    pub const fn text_capacity(&self) -> usize {
        self.text.text_capacity()
    }

    /// Bytes of text held — the capacity hint for the index's path column.
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        self.text.bytes()
    }

    /// Sum of every file's size at walk time.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    fn intern(&mut self, s: &str) -> Result<u32> {
        let i = u32::try_from(self.text.len()).ctx(|| "source tree exceeds u32 entries".into())?;
        self.text
            .push(s)
            .map_err(|e| -> crate::error::BoxError { format!("source tree: {e}").into() })?;
        Ok(i)
    }

    /// Record a file.
    ///
    /// # Errors
    /// If the tree's text tape or entry count overflows.
    pub fn push_file(&mut self, rel: &str, mtime: i64, size: u64, mode: u32) -> Result<()> {
        let rel = self.intern(rel)?;
        self.files.push(FileRow {
            rel,
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
    pub fn push_dir(&mut self, rel: &str, mtime: i64, mode: u32) -> Result<()> {
        let rel = self.intern(rel)?;
        self.dirs.push(DirRow { rel, mtime, mode });
        Ok(())
    }

    /// Record a symlink and its target.
    ///
    /// # Errors
    /// As [`SourceTree::push_file`].
    pub fn push_link(&mut self, rel: &str, target: &str, mtime: i64) -> Result<()> {
        let rel = self.intern(rel)?;
        let target = self.intern(target)?;
        self.links.push(LinkRow { rel, target, mtime });
        Ok(())
    }
}
