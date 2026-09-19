//! One part of a walk: a contiguous run of walk order that you can read
//! without any other part.
//!
//! A [`Part`] holds its rows in three tables, one per kind of entry:
//! [`Part::directories`], [`Part::files`] and [`Part::symlinks`]. A row does not store
//! its path. It stores the node id of its parent directory and its own name,
//! and [`Part::path`] joins them back into a path.
//!
//! # Node ids
//!
//! A node id names a directory within one part:
//!
//! - `0` is the walk root;
//! - `1..=stem` are the stem, the directories above the part's first row,
//!   outermost first ([`Part::stem`]);
//! - `stem + 1 + index` is `directories[index]` ([`Part::directory_node`]).
//!
//! Node ids and text indexes mean nothing outside the part they came from.

use std::cmp::Ordering;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use error_stack::{Report, ResultExt as _};

use crate::tape::StrTape;

/// The part's text or node ids passed what a `u32` can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartFull;

impl fmt::Display for PartFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("part outgrew its u32 addressing")
    }
}

impl std::error::Error for PartFull {}

/// The kind of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link. The walk records its target and does not follow it.
    Symlink,
}

/// A point in time, at the precision the filesystem reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    /// Whole seconds since the Unix epoch. Negative before it.
    pub secs: i64,
    /// Nanoseconds after `secs`, in `0..1_000_000_000`.
    pub nanos: u32,
}

impl Timestamp {
    /// Converts a [`SystemTime`] to seconds and nanoseconds since the Unix
    /// epoch. A time more than `i64::MAX` seconds from the epoch saturates.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self {
                secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                nanos: after.subsec_nanos(),
            },
            Err(before) => {
                let before = before.duration();
                let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                if before.subsec_nanos() == 0 {
                    Self {
                        secs: -secs,
                        nanos: 0,
                    }
                } else {
                    Self {
                        secs: (-secs).saturating_sub(1),
                        nanos: 1_000_000_000 - before.subsec_nanos(),
                    }
                }
            }
        }
    }
}

/// A regular file.
#[derive(Debug, Clone, Copy)]
pub struct FileRow {
    /// The node id of the directory that holds the file.
    pub parent: u32,
    /// The text index of the file's name, for [`Part::text`].
    pub name: u32,
    /// The file's size in bytes when the walk read its metadata.
    pub size: u64,
    /// The modification time, or `None` if the filesystem reported none.
    pub mtime: Option<Timestamp>,
    /// The permission bits. On Unix these are the low twelve bits of the
    /// mode. On other platforms they are `0o644`, or `0o444` for a read-only
    /// file.
    pub mode: u32,
}

/// A directory. Its own node id is [`Part::directory_node`] of its index in
/// [`Part::directories`].
#[derive(Debug, Clone, Copy)]
pub struct DirectoryRow {
    /// The node id of the directory that holds this one.
    pub parent: u32,
    /// The text index of the directory's name, for [`Part::text`].
    pub name: u32,
    /// The modification time, or `None` if the filesystem reported none.
    pub mtime: Option<Timestamp>,
    /// The permission bits. On Unix these are the low twelve bits of the
    /// mode. On other platforms they are `0o755`, or `0o555` for a read-only
    /// directory.
    pub mode: u32,
}

/// A symbolic link.
#[derive(Debug, Clone, Copy)]
pub struct SymlinkRow {
    /// The node id of the directory that holds the link.
    pub parent: u32,
    /// The text index of the link's name, for [`Part::text`].
    pub name: u32,
    /// The text index of the link's target, for [`Part::text`]. The target is
    /// stored as the filesystem gave it, with any backslash replaced by a
    /// forward slash.
    pub target: u32,
    /// The modification time of the link itself, or `None` if the filesystem
    /// reported none.
    pub mtime: Option<Timestamp>,
    /// Whether the link is a directory link. On Windows a symlink is created
    /// as a file link or a directory link, and this records which. `None` on
    /// a platform where a symlink has no kind, such as Linux and macOS.
    pub directory: Option<bool>,
}

/// A contiguous run of walk order, with the stem that places it in the tree.
///
/// The walk fills a part through [`Part::push_stem`], [`Part::push_directory`],
/// [`Part::push_file`] and [`Part::push_symlink`]. You read it through the row
/// tables and [`Part::text`], or as JSON through [`Part::to_json`].
#[derive(Debug, Clone, Default)]
pub struct Part {
    text: StrTape,
    stem: Vec<u32>,
    /// Every directory in the part, in walk order.
    pub directories: Vec<DirectoryRow>,
    /// Every regular file in the part, in walk order.
    pub files: Vec<FileRow>,
    /// Every symbolic link in the part, in walk order.
    pub symlinks: Vec<SymlinkRow>,
}

impl Part {
    /// Returns the string at text index `index`: a row's name, or a link's
    /// target.
    ///
    /// # Panics
    /// If `index` did not come from a row or the stem of this part.
    #[must_use]
    pub fn text(&self, index: u32) -> &str {
        self.text
            .get(index as usize)
            .expect("part tape index in range")
    }

    /// Returns the stem's components, outermost first. The stem is empty when
    /// the part starts at the walk root.
    #[must_use]
    pub fn stem(&self) -> impl ExactSizeIterator<Item = &str> {
        self.stem.iter().map(|&index| self.text(index))
    }

    /// Returns the node id of `directories[index]`.
    ///
    /// # Panics
    /// If the node id does not fit a `u32`. Pushing a directory row already
    /// refuses that, so a part built by this crate cannot panic here.
    #[must_use]
    pub fn directory_node(&self, index: usize) -> u32 {
        u32::try_from(1 + self.stem.len() + index).expect("node ids fit a u32")
    }

    /// Returns the path of the entry named by text index `name` inside node
    /// `parent`, relative to the walk root and joined with forward slashes.
    #[must_use]
    pub fn path(&self, parent: u32, name: u32) -> String {
        let mut components = vec![self.text(name)];
        let mut node = parent;
        while node != 0 {
            let (up, component) = self.node(node);
            components.push(component);
            node = up;
        }
        components.reverse();
        components.join("/")
    }

    // A node's parent and its own name.
    fn node(&self, node: u32) -> (u32, &str) {
        let node = node as usize;
        if node <= self.stem.len() {
            let parent = u32::try_from(node - 1).expect("node ids fit a u32");
            (parent, self.text(self.stem[node - 1]))
        } else {
            let row = self.directories[node - self.stem.len() - 1];
            (row.parent, self.text(row.name))
        }
    }

    /// Returns the path of the part's first row in walk order, or `None` if
    /// the part holds no rows.
    #[must_use]
    pub fn first_path(&self) -> Option<String> {
        let dir = self
            .directories
            .first()
            .map(|row| self.path(row.parent, row.name));
        let file = self
            .files
            .first()
            .map(|row| self.path(row.parent, row.name));
        let link = self
            .symlinks
            .first()
            .map(|row| self.path(row.parent, row.name));
        [dir, file, link]
            .into_iter()
            .flatten()
            .min_by(|left, right| walk_order(left, right))
    }

    /// Returns every entry with its path, in walk order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, EntryKind)> {
        let mut all = Vec::with_capacity(self.len());
        all.extend(
            self.directories
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::Directory)),
        );
        all.extend(
            self.files
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::File)),
        );
        all.extend(
            self.symlinks
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::Symlink)),
        );
        all.sort_unstable_by(|left, right| walk_order(&left.0, &right.0));
        all
    }

    /// Returns the number of rows. The stem is not counted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.directories.len() + self.files.len() + self.symlinks.len()
    }

    /// Returns `true` if the part holds no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the sum of every file's size.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    fn intern(&mut self, text: &str) -> Result<u32, Report<PartFull>> {
        let index = u32::try_from(self.text.len()).change_context(PartFull)?;
        self.text.push(text).change_context(PartFull)?;
        Ok(index)
    }

    /// Appends a stem component below the previous one.
    ///
    /// # Errors
    /// [`PartFull`] if the text passed 4 GiB.
    ///
    /// # Panics
    /// If a directory row was already pushed. The stem's node ids come before
    /// every directory's, so the stem must be complete first.
    pub fn push_stem(&mut self, name: &str) -> Result<(), Report<PartFull>> {
        assert!(
            self.directories.is_empty(),
            "the stem precedes every directory row"
        );
        let name = self.intern(name)?;
        self.stem.push(name);
        Ok(())
    }

    /// Appends a directory row and returns its node id.
    ///
    /// # Errors
    /// [`PartFull`] if the text passed 4 GiB or the node id does not fit a
    /// `u32`.
    pub fn push_directory(
        &mut self,
        parent: u32,
        name: &str,
        mtime: Option<Timestamp>,
        mode: u32,
    ) -> Result<u32, Report<PartFull>> {
        let node =
            u32::try_from(1 + self.stem.len() + self.directories.len()).change_context(PartFull)?;
        let name = self.intern(name)?;
        self.directories.push(DirectoryRow {
            parent,
            name,
            mtime,
            mode,
        });
        Ok(node)
    }

    /// Appends a file row.
    ///
    /// # Errors
    /// [`PartFull`] if the text passed 4 GiB.
    pub fn push_file(
        &mut self,
        parent: u32,
        name: &str,
        size: u64,
        mtime: Option<Timestamp>,
        mode: u32,
    ) -> Result<(), Report<PartFull>> {
        let name = self.intern(name)?;
        self.files.push(FileRow {
            parent,
            name,
            size,
            mtime,
            mode,
        });
        Ok(())
    }

    /// Appends a symbolic link row.
    ///
    /// # Errors
    /// [`PartFull`] if the text passed 4 GiB.
    pub fn push_symlink(
        &mut self,
        parent: u32,
        name: &str,
        target: &str,
        mtime: Option<Timestamp>,
        directory: Option<bool>,
    ) -> Result<(), Report<PartFull>> {
        let name = self.intern(name)?;
        let target = self.intern(target)?;
        self.symlinks.push(SymlinkRow {
            parent,
            name,
            target,
            mtime,
            directory,
        });
        Ok(())
    }
}

/// Compares two relative paths in walk order.
///
/// Walk order compares paths component by component, and each component
/// bytewise. A directory sorts before its contents, and its contents sort
/// before the directory's next sibling. A plain string comparison differs:
/// there, `a!` falls between `a` and `a/sub`, because `!` is below `/`.
///
/// This is the same as comparing the paths bytewise with `/` mapped to the
/// lowest byte, since a name holds neither `/` nor NUL.
#[must_use]
pub fn walk_order(left: &str, right: &str) -> Ordering {
    let key = |byte: u8| if byte == b'/' { 0 } else { byte };
    left.bytes().map(key).cmp(right.bytes().map(key))
}
