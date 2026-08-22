//! The index: one row per entry, stored as columns.
//!
//! # Why columns
//!
//! Everything that reads an index reads one field of every row — the extractor
//! wants sizes, a listing wants paths, a planner wants kinds. A `Vec<Entry>`
//! makes each of those a strided walk over rows many times its width.
//!
//! # Why paths are not stored
//!
//! Ancestry is stored once, in a directory table of `(parent, name)`, and each
//! row names its directory and its own last component — so every path but the
//! first in a directory costs only its leaf. [`Index::write_path`] walks back
//! to the root and writes down again, allocation-free after the first row if
//! one [`PathScratch`] is reused.
//!
//! Rows are sorted by path, so a reader can stream the columns in order.

use std::collections::BTreeMap;

use serde::{Serialize, Serializer};

use crate::bail;
use crate::error::{BoxError, Result};
use crate::hash::{ALGO, Digest, HexDigest};
use crate::tape::StrTape;
use crate::tree::SourceTree;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    File = 0,
    Dir = 1,
    Symlink = 2,
}

impl Kind {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parse a stored discriminant; `None` for one this build does not know.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::File),
            1 => Some(Self::Dir),
            2 => Some(Self::Symlink),
            _ => None,
        }
    }
}

/// One row, pointing at text the caller already holds.
pub struct EntryRef<'a> {
    /// Relative path, forward slashes.
    pub path: &'a str,
    pub kind: Kind,
    pub size: u64,
    /// Unix-ish mode; advisory, and not applied on Windows.
    pub mode: u32,
    /// mtime as unix seconds; 0 if unknown.
    pub mtime: i64,
    /// Lowercase hex content digest. Empty for anything not hashed, which
    /// includes every directory and symlink.
    pub checksum: &'a str,
    /// A symlink's target, as stored. Empty for everything else.
    pub link: &'a str,
}

/// How much a [`Builder`]'s columns should hold, so each allocates once.
#[derive(Debug, Default, Clone, Copy)]
pub struct Capacity {
    pub entries: usize,
    pub path_bytes: usize,
    /// Total bytes of all digests: 64 per hashed file.
    pub sum_bytes: usize,
    pub link_bytes: usize,
}

/// An index under construction: see [`Index::builder`].
pub struct Builder {
    m: Index,
    /// Directory path to id. Only the writing side pays for this; a reader
    /// rebuilds the table straight from the columns.
    seen: BTreeMap<String, u32>,
    /// The row before, to check ordering without a path column to look back at.
    last: String,
}

impl Builder {
    /// Append one row. Rows must arrive in ascending path order.
    ///
    /// # Errors
    /// If `row.path` is not strictly greater than the previous one, or a
    /// string column overflows its tape.
    pub fn push(&mut self, row: &EntryRef<'_>) -> Result<()> {
        if !self.last.is_empty() && self.last.as_str() >= row.path {
            bail!(
                "index rows out of order: {:?} pushed after {:?}",
                row.path,
                self.last
            );
        }
        self.last.clear();
        self.last.push_str(row.path);

        // Rows arrive in path order, so a row's directory is almost always the
        // one the row before used, which `seen` makes free.
        let (parent, name) = split_last(row.path);
        match row.kind {
            Kind::Dir => {
                let me = self.intern(row.path)?;
                self.m.dir.push(me);
                self.m.name.push("").map_err(tape_err("name"))?;
                self.m.num_dirs += 1;
            }
            Kind::File | Kind::Symlink => {
                let d = self.intern(parent)?;
                self.m.dir.push(d);
                self.m.name.push(name).map_err(tape_err("name"))?;
                if row.kind == Kind::File {
                    self.m.num_files += 1;
                }
            }
        }
        self.m
            .checksum
            .push(row.checksum)
            .map_err(tape_err("checksum"))?;
        self.m.link.push(row.link).map_err(tape_err("link"))?;
        self.m.kind.push(row.kind.as_u8());
        self.m.size.push(row.size);
        self.m.mode.push(row.mode);
        self.m.mtime.push(row.mtime);
        self.m.count += 1;
        Ok(())
    }

    /// The id of `path` in the directory table, adding it and any ancestor not
    /// yet there. `""` is the root, id 0.
    fn intern(&mut self, path: &str) -> Result<u32> {
        if path.is_empty() {
            return Ok(0);
        }
        if let Some(&id) = self.seen.get(path) {
            return Ok(id);
        }
        let (parent, name) = split_last(path);
        let pid = self.intern(parent)?;
        let Ok(id) = u32::try_from(self.m.dir_parent.len()) else {
            bail!("directory table exceeds u32 ids");
        };
        self.m.dir_parent.push(pid);
        self.m.dir_name.push(name).map_err(tape_err("dir_name"))?;
        self.seen.insert(path.into(), id);
        Ok(id)
    }

    #[must_use]
    pub fn finish(self) -> Index {
        self.m
    }
}

fn tape_err(col: &'static str) -> impl Fn(crate::tape::TapeFull) -> BoxError {
    move |e| -> BoxError { format!("{col} column: {e}").into() }
}

/// A path split at its last separator: `("dir/sub", "leaf")`, or `("", "leaf")`
/// for a path with no separator at all.
fn split_last(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Every entry a walk found, in path order.
#[derive(Debug, Default, Serialize)]
pub struct Index {
    /// Number of entries — the common length of every entry column.
    pub count: u64,
    pub num_files: u64,
    pub num_dirs: u64,
    /// Sum of every file's size.
    pub total_bytes: u64,

    /// Parent of each directory in the table; `u32::MAX` for row 0, the root.
    /// With `dir_name` this is the tree itself — see [`Index::write_path`].
    pub dir_parent: Vec<u32>,
    /// Each directory's own component; row 0, the root, is empty.
    #[serde(serialize_with = "serialize_tape")]
    dir_name: StrTape,
    /// The directory each entry lives in. A directory entry names *itself*, so
    /// its `name` is empty and its path is its own row in the table.
    pub dir: Vec<u32>,
    /// Each entry's final component; empty exactly for directory entries.
    #[serde(serialize_with = "serialize_tape")]
    name: StrTape,
    /// [`Kind`] discriminants; read with [`Index::kind`].
    pub kind: Vec<u8>,
    pub size: Vec<u64>,
    pub mode: Vec<u32>,
    pub mtime: Vec<i64>,
    /// The algorithm the checksum column was produced with, or empty if the
    /// tree was indexed without hashing.
    pub checksum_algo: String,
    #[serde(serialize_with = "serialize_tape")]
    checksum: StrTape,
    #[serde(serialize_with = "serialize_tape")]
    link: StrTape,
}

/// A tape serializes as the array of its strings — the reader wants the
/// column, not the arena it happens to be packed into.
fn serialize_tape<S: Serializer>(t: &StrTape, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_seq(t.iter())
}

impl Index {
    /// An empty index sized for `cap`.
    #[must_use]
    pub fn builder(cap: Capacity) -> Builder {
        Builder {
            m: Self {
                // Row 0 is the root: no parent, and an empty name that
                // `write_path` skips rather than writing a leading separator.
                dir_parent: vec![u32::MAX],
                dir_name: root_names(cap),
                dir: Vec::with_capacity(cap.entries),
                name: StrTape::with_capacity(cap.path_bytes, cap.entries),
                kind: Vec::with_capacity(cap.entries),
                size: Vec::with_capacity(cap.entries),
                mode: Vec::with_capacity(cap.entries),
                mtime: Vec::with_capacity(cap.entries),
                checksum: StrTape::with_capacity(cap.sum_bytes, cap.entries),
                link: StrTape::with_capacity(cap.link_bytes, cap.entries),
                ..Self::default()
            },
            seen: BTreeMap::new(),
            last: String::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.kind.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn num_links(&self) -> u64 {
        self.count - self.num_files - self.num_dirs
    }

    /// The kind at row `i`, or `None` if the stored discriminant is unknown.
    #[must_use]
    pub fn kind(&self, i: usize) -> Option<Kind> {
        Kind::from_u8(*self.kind.get(i)?)
    }

    /// The hex digest at row `i`; empty for a row that was not hashed.
    ///
    /// # Panics
    /// If `i` is past the end.
    #[must_use]
    pub fn checksum(&self, i: usize) -> &str {
        self.checksum.get(i).expect("index row in range")
    }

    /// A symlink's target at row `i`; empty for everything else.
    ///
    /// # Panics
    /// If `i` is past the end.
    #[must_use]
    pub fn link(&self, i: usize) -> &str {
        self.link.get(i).expect("index row in range")
    }

    /// Write row `i`'s path into `sc` and return it, forward slashes.
    ///
    /// # Panics
    /// If `i` is past the end, or the directory table does not reach a root
    /// from row `i` — neither of which a [`Builder`] can produce.
    pub fn write_path<'a>(&self, i: usize, sc: &'a mut PathScratch) -> &'a str {
        sc.buf.clear();
        let mut at = self.dir[i];
        sc.up.clear();
        while at != u32::MAX {
            sc.up.push(at);
            at = self.dir_parent[at as usize];
        }
        // Collected leaf-first; the path reads the other way.
        while let Some(d) = sc.up.pop() {
            let c = self.dir_name.get(d as usize).expect("valid directory id");
            if !c.is_empty() {
                push_component(&mut sc.buf, c);
            }
        }
        let name = self.name.get(i).expect("index row in range");
        if !name.is_empty() {
            push_component(&mut sc.buf, name);
        }
        &sc.buf
    }

    /// Every path, in order.
    pub fn for_each_path(&self, mut f: impl FnMut(usize, &str)) {
        let mut sc = PathScratch::default();
        for i in 0..self.len() {
            f(i, self.write_path(i, &mut sc));
        }
    }

    /// Serialize to the JSON form: one array per column, plus the scalars.
    ///
    /// Columns rather than an array of objects, for the same reason the index
    /// is columns in the first place — a reader that wants sizes should not
    /// have to parse every path to find them.
    ///
    /// # Errors
    /// Only if a column cannot be serialized, which the types here rule out.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| -> BoxError { format!("index JSON: {e}").into() })
    }

    /// As [`Index::to_json`], indented for a human.
    ///
    /// # Errors
    /// As [`Index::to_json`].
    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| -> BoxError { format!("index JSON: {e}").into() })
    }

    /// A one-line human summary.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} entries: {} files, {} dirs, {} symlinks, {} bytes",
            self.count,
            self.num_files,
            self.num_dirs,
            self.num_links(),
            self.total_bytes,
        )
    }
}

fn root_names(cap: Capacity) -> StrTape {
    let mut t = StrTape::with_capacity(cap.path_bytes, cap.entries);
    // Infallible: the tape was just created, so it is nowhere near its limit.
    t.push("").unwrap_or_default();
    t
}

fn push_component(buf: &mut String, c: &str) {
    if !buf.is_empty() {
        buf.push('/');
    }
    buf.push_str(c);
}

/// Reusable room for reconstructing a path: the string, and the ancestry
/// walked on the way to it. One per loop (or per thread) is what keeps
/// [`Index::write_path`] from allocating.
#[derive(Default)]
pub struct PathScratch {
    buf: String,
    up: Vec<u32>,
}

/// Build the index of a walked tree, without content digests.
///
/// # Errors
/// As [`index_tree_with`].
pub fn index_tree(tree: &SourceTree) -> Result<Index> {
    index_tree_with(tree, &[])
}

/// Build the index of a walked tree, with one digest per file.
///
/// `sums` is in `tree.files` order — what [`crate::hash::hash_tree`] returns —
/// or empty for an index without them.
///
/// The tree's three columns are interleaved into one path-sorted order first,
/// because that order is the index's whole contract.
///
/// # Errors
/// If `sums` is neither empty nor one digest per file, if the tree holds more
/// entries than a `u32` can index, or if a string column overflows its tape.
pub fn index_tree_with(tree: &SourceTree, sums: &[Digest]) -> Result<Index> {
    if !sums.is_empty() && sums.len() != tree.files.len() {
        bail!("{} digests for {} files", sums.len(), tree.files.len());
    }
    let n = tree.len();
    u32::try_from(n)?;
    let mut order: Vec<(&str, Slot)> = Vec::with_capacity(n);
    #[expect(clippy::cast_possible_truncation, reason = "checked as a u32 above")]
    {
        let d = tree.dirs.iter().enumerate();
        order.extend(d.map(|(i, r)| (tree.text(r.rel), Slot::Dir(i as u32))));
        let f = tree.files.iter().enumerate();
        order.extend(f.map(|(i, r)| (tree.text(r.rel), Slot::File(i as u32))));
        let l = tree.links.iter().enumerate();
        order.extend(l.map(|(i, r)| (tree.text(r.rel), Slot::Link(i as u32))));
    }
    order.sort_unstable_by(|a, b| a.0.cmp(b.0));

    let mut b = Index::builder(Capacity {
        entries: n,
        path_bytes: tree.text_bytes(),
        sum_bytes: sums.len() * 64,
        link_bytes: tree.links.iter().map(|l| tree.text(l.target).len()).sum(),
    });
    for (path, slot) in order {
        // Held out here because the row below borrows it.
        let hex = match slot {
            Slot::File(i) if !sums.is_empty() => Some(HexDigest::of(&sums[i as usize])),
            _ => None,
        };
        let row = match slot {
            Slot::File(i) => {
                let f = tree.files[i as usize];
                EntryRef {
                    path,
                    kind: Kind::File,
                    size: f.size,
                    mode: f.mode,
                    mtime: f.mtime,
                    checksum: hex.as_ref().map_or("", HexDigest::as_str),
                    link: "",
                }
            }
            Slot::Dir(i) => {
                let d = tree.dirs[i as usize];
                EntryRef {
                    path,
                    kind: Kind::Dir,
                    size: 0,
                    mode: d.mode,
                    mtime: d.mtime,
                    checksum: "",
                    link: "",
                }
            }
            Slot::Link(i) => {
                let l = tree.links[i as usize];
                EntryRef {
                    path,
                    kind: Kind::Symlink,
                    size: 0,
                    mode: 0o777,
                    mtime: l.mtime,
                    checksum: "",
                    link: tree.text(l.target),
                }
            }
        };
        b.push(&row)?;
    }
    let mut m = b.finish();
    m.total_bytes = tree.total_bytes();
    if !sums.is_empty() {
        m.checksum_algo = ALGO.into();
    }
    Ok(m)
}

/// Which of a tree's three columns a path-ordered entry came from.
enum Slot {
    Dir(u32),
    File(u32),
    Link(u32),
}
