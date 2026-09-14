// TODO(docs): scaffold. Public docs in this file are notes, not prose.
// TODO(docs): the shape itself is unreviewed too — group names and which
// columns each group carries are all still open.

//! The JSON form of a part: its stem, then one array per column, all the same
//! length within a group.
//!
//! - columns, not an array of objects, for the reason a part is columns: a
//!   reader after sizes should not parse every name to reach them
//! - arrays also compress far better than values interleaved with field names
//! - no second copy of the document: columns written straight from the rows
//! - `mtime` is seconds or `null` when unknown; `mtime_nanos` carries the rest

use std::fmt;

use error_stack::{Report, ResultExt as _};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::part::{Part, Timestamp};

/// A part could not be rendered as JSON.
///
/// - in practice ruled out — every value is a string, an integer or `null` —
///   so read the source if it ever does fire
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not render the part as JSON")
    }
}

impl std::error::Error for JsonError {}

// A column, serialized from a fresh iterator over the rows it lives in. The
// closure is there because `serialize_field` needs something it can borrow and
// serialize, and an iterator is consumed by being one.
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

// The directory columns. No size: a directory's own size is the filesystem's
// bookkeeping, not anything to write back out.
struct Dirs<'a>(&'a Part);

impl Serialize for Dirs<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let part = self.0;
        let rows = &part.dirs;
        let mut out = serializer.serialize_struct("dirs", 5)?;
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

// No mode: a symlink's own bits are platform folklore, and nothing restores
// them.
struct Links<'a>(&'a Part);

impl Serialize for Links<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let part = self.0;
        let rows = &part.links;
        let mut out = serializer.serialize_struct("links", 5)?;
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
        out.end()
    }
}

impl Serialize for Part {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("part", 4)?;
        out.serialize_field("stem", &Column(|| self.stem()))?;
        out.serialize_field("dirs", &Dirs(self))?;
        out.serialize_field("files", &Files(self))?;
        out.serialize_field("links", &Links(self))?;
        out.end()
    }
}

impl Part {
    /// Serialize to the JSON form.
    ///
    /// # Errors
    /// [`JsonError`] if a column cannot be serialized — which the types here
    /// rule out.
    pub fn to_json(&self) -> Result<String, Report<JsonError>> {
        serde_json::to_string(self).change_context(JsonError)
    }

    /// As [`Part::to_json`], into `out` after clearing it, so a caller that
    /// renders many parts reuses one buffer.
    ///
    /// # Errors
    /// As [`Part::to_json`].
    pub fn write_json(&self, out: &mut Vec<u8>) -> Result<(), Report<JsonError>> {
        out.clear();
        serde_json::to_writer(&mut *out, self).change_context(JsonError)
    }
}
