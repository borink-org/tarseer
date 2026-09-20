//! The JSON form of a part.
//!
//! [`Part::to_json`] writes one document per part:
//!
//! ```json
//! {
//!   "stem": ["usr", "lib"],
//!   "directories": {"parent": [0], "name": ["x"], "mode": [493], "mtime": [1700000000], "mtime_nanos": [0]},
//!   "files": {"parent": [3], "name": ["y"], "size": [12], "mode": [420], "mtime": [null], "mtime_nanos": [0]},
//!   "symlinks": {"parent": [3], "name": ["z"], "target": ["y"], "mtime": [1700000000], "mtime_nanos": [500], "directory": [null]}
//! }
//! ```
//!
//! Each of `directories`, `files` and `symlinks` holds one array per column,
//! and every array in a group has one value per row. `parent` is a node id
//! and `name` a component, as in [`Part`]. A symlink's `directory` is `true`
//! or `false` for a Windows directory or file link, and `null` where a
//! symlink has no kind. `mtime` is whole seconds since the
//! Unix epoch, or `null` when the filesystem reported no time; `mtime_nanos`
//! is the nanoseconds after it, and `0` when `mtime` is `null`.
//!
//! The document is written straight from the rows, without a second copy of
//! the part in memory.

use std::fmt;

use error_stack::{Report, ResultExt as _};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::part::{Part, Timestamp};

/// The part could not be written as JSON.
///
/// Every value in the document is a string, an integer or `null`, so this
/// error is not expected to occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not render the part as JSON")
    }
}

impl std::error::Error for JsonError {}

// A column, serialized from a fresh iterator over its rows. The closure is
// there because `serialize_field` takes a value it can borrow, and serializing
// an iterator consumes it.
pub(crate) struct Column<F>(pub(crate) F);

impl<F, I> Serialize for Column<F>
where
    F: Fn() -> I,
    I: IntoIterator,
    I::Item: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq((self.0)())
    }
}

fn secs(mtime: Option<Timestamp>) -> Option<i64> {
    mtime.map(|time| time.secs)
}

fn nanos(mtime: Option<Timestamp>) -> u32 {
    mtime.map_or(0, |time| time.nanos)
}

// The directory columns. No size column: a directory's size is the space its
// entry list takes on this filesystem, and nothing restores it.
struct Directories<'a>(&'a Part);

impl Serialize for Directories<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let part = self.0;
        let rows = &part.directories;
        let mut out = serializer.serialize_struct("directories", 5)?;
        out.serialize_field("parent", &Column(|| rows.iter().map(|row| row.parent)))?;
        out.serialize_field(
            "name",
            &Column(|| rows.iter().map(|row| part.text(row.name))),
        )?;
        out.serialize_field("mode", &Column(|| rows.iter().map(|row| row.mode)))?;
        out.serialize_field("mtime", &Column(|| rows.iter().map(|row| secs(row.mtime))))?;
        out.serialize_field(
            "mtime_nanos",
            &Column(|| rows.iter().map(|row| nanos(row.mtime))),
        )?;
        out.end()
    }
}

struct Files<'a>(&'a Part);

impl Serialize for Files<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let part = self.0;
        let rows = &part.files;
        let mut out = serializer.serialize_struct("files", 6)?;
        out.serialize_field("parent", &Column(|| rows.iter().map(|row| row.parent)))?;
        out.serialize_field(
            "name",
            &Column(|| rows.iter().map(|row| part.text(row.name))),
        )?;
        out.serialize_field("size", &Column(|| rows.iter().map(|row| row.size)))?;
        out.serialize_field("mode", &Column(|| rows.iter().map(|row| row.mode)))?;
        out.serialize_field("mtime", &Column(|| rows.iter().map(|row| secs(row.mtime))))?;
        out.serialize_field(
            "mtime_nanos",
            &Column(|| rows.iter().map(|row| nanos(row.mtime))),
        )?;
        out.end()
    }
}

// No mode column: a symlink's permission bits are fixed at 0777 on Linux and
// unused on macOS, and nothing restores them.
struct Symlinks<'a>(&'a Part);

impl Serialize for Symlinks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let part = self.0;
        let rows = &part.symlinks;
        let mut out = serializer.serialize_struct("symlinks", 6)?;
        out.serialize_field("parent", &Column(|| rows.iter().map(|row| row.parent)))?;
        out.serialize_field(
            "name",
            &Column(|| rows.iter().map(|row| part.text(row.name))),
        )?;
        out.serialize_field(
            "target",
            &Column(|| rows.iter().map(|row| part.text(row.target))),
        )?;
        out.serialize_field("mtime", &Column(|| rows.iter().map(|row| secs(row.mtime))))?;
        out.serialize_field(
            "mtime_nanos",
            &Column(|| rows.iter().map(|row| nanos(row.mtime))),
        )?;
        out.serialize_field(
            "directory",
            &Column(|| rows.iter().map(|row| row.directory)),
        )?;
        out.end()
    }
}

impl Serialize for Part {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("part", 4)?;
        out.serialize_field("stem", &Column(|| self.stem()))?;
        out.serialize_field("directories", &Directories(self))?;
        out.serialize_field("files", &Files(self))?;
        out.serialize_field("symlinks", &Symlinks(self))?;
        out.end()
    }
}

impl Part {
    /// Writes the part as a JSON document.
    ///
    /// # Errors
    /// [`JsonError`] if a column cannot be serialized. The types of the
    /// columns rule that out.
    pub fn to_json(&self) -> Result<String, Report<JsonError>> {
        serde_json::to_string(self).change_context(JsonError)
    }

    /// Clears `out` and writes the part as a JSON document into it. Use this
    /// to write many parts through one buffer.
    ///
    /// # Errors
    /// As [`Part::to_json`].
    pub fn write_json(&self, out: &mut Vec<u8>) -> Result<(), Report<JsonError>> {
        out.clear();
        serde_json::to_writer(&mut *out, self).change_context(JsonError)
    }
}
