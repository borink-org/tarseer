// TODO(docs): scaffold. Public docs in this file are notes, not prose.
// TODO(docs): the shape itself is unreviewed too — group names, `count`, and
// which columns each group carries are all still open.

//! The JSON form of a walk: one array per column, all the same length.
//!
//! - columns, not an array of objects, for the reason the tree is columns: a
//!   reader after sizes should not parse every path to reach them
//! - arrays also compress far better than values interleaved with field names
//! - no second copy of the document: columns written straight from the rows,
//!   paths straight from the tape

use std::fmt;

use error_stack::{Report, ResultExt as _};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::tree::{Skips, SourceTree};

/// The tree could not be rendered as JSON.
///
/// - own context, not a walk failure: the walk is over and the tree in hand,
///   so nothing about the filesystem is implicated
/// - in practice ruled out — every value is a string or an integer — so read
///   the source if it ever does fire
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not render the source tree as JSON")
    }
}

impl std::error::Error for JsonError {}

// A column, serialized from a fresh iterator over the rows it lives in. The
// closure is there because `serialize_field` needs something it can borrow and
// serialize, and an iterator is consumed by being one.
struct Column<F>(F);

impl<F, I> Serialize for Column<F>
where
    F: Fn() -> I,
    I: IntoIterator,
    I::Item: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq((self.0)())
    }
}

// The file columns: `path`, `size`, `mode`, `mtime`.
struct Files<'a>(&'a SourceTree);

impl Serialize for Files<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let tree = self.0;
        let mut out = serializer.serialize_struct("files", 4)?;
        out.serialize_field(
            "path",
            &Column(|| tree.files.iter().map(|row| tree.text(row.path))),
        )?;
        out.serialize_field("size", &Column(|| tree.files.iter().map(|row| row.size)))?;
        out.serialize_field("mode", &Column(|| tree.files.iter().map(|row| row.mode)))?;
        out.serialize_field("mtime", &Column(|| tree.files.iter().map(|row| row.mtime)))?;
        out.end()
    }
}

// The directory columns: `path`, `mode`, `mtime`. No size: a directory's own
// size is the filesystem's bookkeeping, not anything to write back out.
struct Dirs<'a>(&'a SourceTree);

impl Serialize for Dirs<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let tree = self.0;
        let mut out = serializer.serialize_struct("dirs", 3)?;
        out.serialize_field(
            "path",
            &Column(|| tree.dirs.iter().map(|row| tree.text(row.path))),
        )?;
        out.serialize_field("mode", &Column(|| tree.dirs.iter().map(|row| row.mode)))?;
        out.serialize_field("mtime", &Column(|| tree.dirs.iter().map(|row| row.mtime)))?;
        out.end()
    }
}

// The symlink columns: `path`, `target`, `mtime`. No mode: a symlink's own
// bits are platform folklore, and nothing restores them.
struct Links<'a>(&'a SourceTree);

impl Serialize for Links<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let tree = self.0;
        let mut out = serializer.serialize_struct("links", 3)?;
        out.serialize_field(
            "path",
            &Column(|| tree.links.iter().map(|row| tree.text(row.path))),
        )?;
        out.serialize_field(
            "target",
            &Column(|| tree.links.iter().map(|row| tree.text(row.target))),
        )?;
        out.serialize_field("mtime", &Column(|| tree.links.iter().map(|row| row.mtime)))?;
        out.end()
    }
}

impl Serialize for Skips {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("skips", 3)?;
        out.serialize_field("special", &self.special)?;
        out.serialize_field("non_utf8", &self.non_utf8)?;
        out.serialize_field("unreadable", &self.unreadable)?;
        out.end()
    }
}

impl Serialize for SourceTree {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("tree", 5)?;
        out.serialize_field("count", &self.len())?;
        out.serialize_field("files", &Files(self))?;
        out.serialize_field("dirs", &Dirs(self))?;
        out.serialize_field("links", &Links(self))?;
        out.serialize_field("skips", &self.skips)?;
        out.end()
    }
}

impl SourceTree {
    /// Serialize to the JSON form.
    ///
    /// # Errors
    /// [`JsonError`] if a column cannot be serialized — which the types here
    /// rule out.
    pub fn to_json(&self) -> Result<String, Report<JsonError>> {
        serde_json::to_string(self).change_context(JsonError)
    }

    /// As [`SourceTree::to_json`], indented for a human.
    ///
    /// # Errors
    /// As [`SourceTree::to_json`].
    pub fn to_json_pretty(&self) -> Result<String, Report<JsonError>> {
        serde_json::to_string_pretty(self).change_context(JsonError)
    }
}
