//! The JSON form of a part.
//!
//! [`TreePart::to_json`] writes one document per part:
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
//! and every array in a group has one value per row. `parent` is a node id and
//! `name` a component, as in [`TreePart`]. A symlink's `directory` is `true` or
//! `false` for a Windows directory or file link, and `null` where a symlink has
//! no kind. `mtime` is whole seconds since the Unix epoch, or `null` when the
//! filesystem reported no time; `mtime_nanos` is the nanoseconds after it, and
//! `0` when `mtime` is `null`.
//!
//! The document is written straight from the rows, without a second copy of
//! the part in memory.

use std::fmt;

use error_stack::{Report, ResultExt as _};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::manifest::{Index, PartEntry, Timestamp, TreePart};
use crate::walk::Skips;

/// A part or an index could not be written as JSON, or an index could not be
/// read from it. The report says which value was at fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError;

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not write or read the JSON form")
    }
}

impl std::error::Error for JsonError {}

// A column, serialized from a fresh iterator over its rows. The closure is
// there because `serialize_field` takes a value it can borrow, and serializing
// an iterator consumes it.
struct Column<F>(F);

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
struct Directories<'a>(&'a TreePart);

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

struct Files<'a>(&'a TreePart);

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
struct Symlinks<'a>(&'a TreePart);

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

impl Serialize for TreePart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("part", 4)?;
        out.serialize_field("stem", &Column(|| self.stem()))?;
        out.serialize_field("directories", &Directories(self))?;
        out.serialize_field("files", &Files(self))?;
        out.serialize_field("symlinks", &Symlinks(self))?;
        out.end()
    }
}

impl TreePart {
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
    /// As [`TreePart::to_json`].
    pub fn write_json(&self, out: &mut Vec<u8>) -> Result<(), Report<JsonError>> {
        out.clear();
        serde_json::to_writer(&mut *out, self).change_context(JsonError)
    }
}

/// The version of the JSON forms that this build writes and reads. The index
/// carries it as `format_version`, and [`Index::from_json`] refuses any other.
pub const FORMAT_VERSION: u16 = 1;

struct IndexJson<'a>(&'a Index);

impl Serialize for IndexJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let index = self.0;
        let mut out = serializer.serialize_struct("index", 3)?;
        out.serialize_field("format_version", &FORMAT_VERSION)?;
        out.serialize_field("parts", &Parts(&index.parts))?;
        out.serialize_field("skips", &SkipsJson(index.skips))?;
        out.end()
    }
}

struct Parts<'a>(&'a [PartEntry]);

impl Serialize for Parts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let rows = self.0;
        // Left out when every part is in the frame of its own number, which is
        // so whenever the parts were written in walk order.
        let moved = rows
            .iter()
            .enumerate()
            .any(|(place, row)| row.frame != place as u64);
        let mut out = serializer.serialize_struct("parts", 4 + usize::from(moved))?;
        out.serialize_field("first", &Column(|| rows.iter().map(|row| &row.first)))?;
        out.serialize_field(
            "directories",
            &Column(|| rows.iter().map(|row| row.directories)),
        )?;
        out.serialize_field("files", &Column(|| rows.iter().map(|row| row.files)))?;
        out.serialize_field("symlinks", &Column(|| rows.iter().map(|row| row.symlinks)))?;
        if moved {
            out.serialize_field("frame", &Column(|| rows.iter().map(|row| row.frame)))?;
        }
        out.end()
    }
}

struct SkipsJson(Skips);

impl Serialize for SkipsJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = serializer.serialize_struct("skips", 4)?;
        out.serialize_field("special", &self.0.special)?;
        out.serialize_field("non_utf8", &self.0.non_utf8)?;
        out.serialize_field("unreadable", &self.0.unreadable)?;
        out.serialize_field("failed", &self.0.failed)?;
        out.end()
    }
}

fn malformed<T>(what: impl FnOnce() -> String) -> Result<T, Report<JsonError>> {
    Err(JsonError).attach_with(what)
}

impl Index {
    /// Writes the index as a JSON document: `format_version`, then `parts`
    /// with one array per field of [`PartEntry`], then `skips`.
    ///
    /// # Errors
    /// [`JsonError`] if a column cannot be serialized. The types of the
    /// columns rule that out.
    pub fn to_json(&self) -> Result<String, Report<JsonError>> {
        serde_json::to_string(&IndexJson(self)).change_context(JsonError)
    }

    /// Reads an index from the JSON that [`Index::to_json`] writes.
    ///
    /// # Errors
    /// [`JsonError`] if `raw` is not JSON, names another
    /// [`FORMAT_VERSION`], lacks a column or a skip count, holds a value of
    /// the wrong type, or has columns of different lengths.
    pub fn from_json(raw: &[u8]) -> Result<Self, Report<JsonError>> {
        let document: serde_json::Value = serde_json::from_slice(raw).change_context(JsonError)?;
        if document["format_version"].as_u64() != Some(u64::from(FORMAT_VERSION)) {
            return malformed(|| "the index names another format version".to_owned());
        }
        let parts = &document["parts"];
        let column = |name: &str| -> Result<Vec<u64>, Report<JsonError>> {
            let Some(values) = parts[name].as_array() else {
                return malformed(|| format!("index column {name} is missing"));
            };
            values
                .iter()
                .map(|value| match value.as_u64() {
                    Some(number) => Ok(number),
                    None => malformed(|| format!("index column {name} holds a non-integer")),
                })
                .collect()
        };
        let directories = column("directories")?;
        let files = column("files")?;
        let symlinks = column("symlinks")?;
        let Some(first) = parts["first"].as_array() else {
            return malformed(|| "index column first is missing".to_owned());
        };
        let count = first.len();
        if [directories.len(), files.len(), symlinks.len()]
            .iter()
            .any(|&len| len != count)
        {
            return malformed(|| "index columns disagree on the number of parts".to_owned());
        }
        // Without the column, each part is in the frame of its own number.
        let frames = match parts.get("frame") {
            Some(_) => column("frame")?,
            None => (0..count as u64).collect(),
        };
        let mut seen = vec![false; count];
        let each_once = frames.len() == count
            && frames.iter().all(|&frame| {
                usize::try_from(frame)
                    .ok()
                    .and_then(|frame| seen.get_mut(frame))
                    .is_some_and(|seen| !std::mem::replace(seen, true))
            });
        if !each_once {
            return malformed(|| "index column frame does not name every frame once".to_owned());
        }
        let mut entries = Vec::with_capacity(count);
        for row in 0..count {
            let Some(first) = first[row].as_str() else {
                return malformed(|| "index column first holds a non-string".to_owned());
            };
            entries.push(PartEntry {
                first: first.to_owned(),
                directories: directories[row],
                files: files[row],
                symlinks: symlinks[row],
                frame: frames[row],
            });
        }

        let skip = |name: &str| -> Result<u32, Report<JsonError>> {
            match document["skips"][name].as_u64().map(u32::try_from) {
                Some(Ok(count)) => Ok(count),
                _ => malformed(|| format!("index skip count {name} is missing")),
            }
        };
        let skips = Skips {
            special: skip("special")?,
            non_utf8: skip("non_utf8")?,
            unreadable: skip("unreadable")?,
            failed: skip("failed")?,
        };
        Ok(Self {
            parts: entries,
            skips,
        })
    }
}
