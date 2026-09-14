// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! One part of a walk: a contiguous run of walk order, readable on its own.
//!
//! - rows split by kind, not tagged: every consumer wants one kind at a time
//! - a row names its parent directory by node id plus its own last component;
//!   no row stores a whole path, so text is linear in depth, not quadratic
//! - nodes: `0` is the walk root, `1..=stem` the stem (the directories above
//!   the part's first row), `stem + 1 + i` is `dirs[i]`
//! - every id is local to the part: a part needs nothing outside itself

use std::cmp::Ordering;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use error_stack::{Report, ResultExt as _};

use crate::tape::StrTape;

/// The part outgrew the `u32` it addresses its text and nodes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartFull;

impl fmt::Display for PartFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("part outgrew its u32 addressing")
    }
}

impl std::error::Error for PartFull {}

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// A point in time at the precision the filesystem gave.
///
/// - `secs` from the Unix epoch, negative before it
/// - `nanos` always `0..1_000_000_000`, counting forward from `secs`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

impl Timestamp {
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

/// A file.
///
/// - `parent`: node id of the directory holding it
/// - `name`: tape index of its last component
/// - `mode`: real Unix bits on Unix; on Windows derived from the read-only
///   attribute, matching `tar`
/// - `mtime`: `None` if the filesystem would not say
#[derive(Debug, Clone, Copy)]
pub struct FileRow {
    pub parent: u32,
    pub name: u32,
    /// Size at walk time.
    pub size: u64,
    pub mtime: Option<Timestamp>,
    pub mode: u32,
}

/// A directory. Fields as [`FileRow`]; its own node id is
/// [`Part::dir_node`].
#[derive(Debug, Clone, Copy)]
pub struct DirRow {
    pub parent: u32,
    pub name: u32,
    pub mtime: Option<Timestamp>,
    pub mode: u32,
}

/// A symbolic link, never followed. Fields as [`FileRow`].
#[derive(Debug, Clone, Copy)]
pub struct LinkRow {
    pub parent: u32,
    pub name: u32,
    /// Target as stored, forward slashes. Also in the tape.
    pub target: u32,
    pub mtime: Option<Timestamp>,
}

/// A contiguous run of walk order with its stem.
#[derive(Debug, Clone, Default)]
pub struct Part {
    text: StrTape,
    stem: Vec<u32>,
    pub dirs: Vec<DirRow>,
    pub files: Vec<FileRow>,
    pub links: Vec<LinkRow>,
}

impl Part {
    /// The string at tape index `index`: a row's `name`, or a link's `target`.
    ///
    /// # Panics
    /// If `index` is not from a row of this part. Every row index came from a
    /// push below, so an invalid one is a bug here, not a caller's doing.
    #[must_use]
    pub fn text(&self, index: u32) -> &str {
        self.text
            .get(index as usize)
            .expect("part tape index in range")
    }

    /// The stem's components, outermost first.
    #[must_use]
    pub fn stem(&self) -> impl ExactSizeIterator<Item = &str> {
        self.stem.iter().map(|&index| self.text(index))
    }

    /// Node id of `dirs[index]`.
    ///
    /// # Panics
    /// Past `u32`, which pushing a row already refuses.
    #[must_use]
    pub fn dir_node(&self, index: usize) -> u32 {
        u32::try_from(1 + self.stem.len() + index).expect("node ids fit a u32")
    }

    /// The path of the entry `name` inside node `parent`, relative to the walk
    /// root, forward slashes.
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
            let row = self.dirs[node - self.stem.len() - 1];
            (row.parent, self.text(row.name))
        }
    }

    /// Every entry with its path, in walk order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, EntryKind)> {
        let mut all = Vec::with_capacity(self.len());
        all.extend(
            self.dirs
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::Dir)),
        );
        all.extend(
            self.files
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::File)),
        );
        all.extend(
            self.links
                .iter()
                .map(|row| (self.path(row.parent, row.name), EntryKind::Symlink)),
        );
        all.sort_unstable_by(|left, right| walk_order(&left.0, &right.0));
        all
    }

    /// Rows in the part; the stem is not counted.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.dirs.len() + self.files.len() + self.links.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sum of every file's size at walk time.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    fn intern(&mut self, text: &str) -> Result<u32, Report<PartFull>> {
        let index = u32::try_from(self.text.len()).change_context(PartFull)?;
        self.text.push(text).change_context(PartFull)?;
        Ok(index)
    }

    /// Append a stem component, below the previous one.
    ///
    /// # Errors
    /// [`PartFull`]: text tape overflowed.
    ///
    /// # Panics
    /// If a directory row was already pushed: the stem's node ids come first.
    pub fn push_stem(&mut self, name: &str) -> Result<(), Report<PartFull>> {
        assert!(
            self.dirs.is_empty(),
            "the stem precedes every directory row"
        );
        let name = self.intern(name)?;
        self.stem.push(name);
        Ok(())
    }

    /// Record a directory, returning its node id.
    ///
    /// # Errors
    /// [`PartFull`]: text tape or node ids overflowed.
    pub fn push_dir(
        &mut self,
        parent: u32,
        name: &str,
        mtime: Option<Timestamp>,
        mode: u32,
    ) -> Result<u32, Report<PartFull>> {
        let node = u32::try_from(1 + self.stem.len() + self.dirs.len()).change_context(PartFull)?;
        let name = self.intern(name)?;
        self.dirs.push(DirRow {
            parent,
            name,
            mtime,
            mode,
        });
        Ok(node)
    }

    /// Record a file.
    ///
    /// # Errors
    /// [`PartFull`]: text tape overflowed.
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

    /// Record a symlink.
    ///
    /// # Errors
    /// As [`Part::push_file`].
    pub fn push_link(
        &mut self,
        parent: u32,
        name: &str,
        target: &str,
        mtime: Option<Timestamp>,
    ) -> Result<(), Report<PartFull>> {
        let name = self.intern(name)?;
        let target = self.intern(target)?;
        self.links.push(LinkRow {
            parent,
            name,
            target,
            mtime,
        });
        Ok(())
    }
}

/// Walk order over two relative paths: component by component, each compared
/// bytewise.
///
/// - a directory sorts before its contents, and its contents before its next
///   sibling — unlike a plain string sort, where `a!` falls between `a` and
///   `a/sub`
/// - the same as mapping `/` to the lowest byte, since a name holds neither
///   `/` nor NUL
#[must_use]
pub fn walk_order(left: &str, right: &str) -> Ordering {
    let key = |byte: u8| if byte == b'/' { 0 } else { byte };
    left.bytes().map(key).cmp(right.bytes().map(key))
}
