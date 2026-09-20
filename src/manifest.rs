//! The logical manifest: how a walked tree is represented, apart from how it
//! is written.
//!
//! A manifest is a sequence of [`TreePart`]s in walk order and one [`Index`]
//! that says, for each part, where in the tree it starts and how many rows it
//! holds. The [`json`](crate::json) module gives both a text form, and the
//! [`frames`](crate::frames) module turns that text into bytes.

pub mod part;

pub use self::part::{
    DirectoryRow, EntryKind, FileRow, SymlinkRow, Timestamp, TreePart, TreePartFull, walk_order,
};
use crate::walk::Skips;

/// One row of the index: where a part starts in the tree and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartEntry {
    /// The path of the part's first row in walk order. Empty if the part
    /// holds no rows.
    pub first: String,
    /// The number of directory rows in the part.
    pub directories: u64,
    /// The number of file rows in the part.
    pub files: u64,
    /// The number of symlink rows in the part.
    pub symlinks: u64,
}

impl PartEntry {
    /// Returns the index entry for `part`.
    #[must_use]
    pub fn of(part: &TreePart) -> Self {
        Self {
            first: part.first_path().unwrap_or_default(),
            directories: part.directories.len() as u64,
            files: part.files.len() as u64,
            symlinks: part.symlinks.len() as u64,
        }
    }
}

/// The index of a manifest: one entry per part, in walk order, and what the
/// walk skipped.
///
/// A directory is one contiguous run of the walk, so a search over
/// [`PartEntry::first`] with [`walk_order`] finds the parts it spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    /// One entry per part, in walk order.
    pub parts: Vec<PartEntry>,
    /// What the walk skipped.
    pub skips: Skips,
}

impl Index {
    /// Returns the number of rows over every part.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.parts
            .iter()
            .map(|part| part.directories + part.files + part.symlinks)
            .sum()
    }
}
