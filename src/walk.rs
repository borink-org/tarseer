// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! The source walk: every entry under a root, in walk order, cut into parts.
//!
//! - walk order: depth-first, each directory's entries sorted by name
//! - serial for now; the parallel walk lands later and must cut identically
//!
//! # Where parts are cut
//!
//! Sizes are *estimated* JSON bytes ([`estimate`]), from names and fixed
//! per-row widths only, so a cut never depends on anything decided later.
//!
//! - a subtree within the budget is never split
//! - an over-budget directory groups its children in order, each group as
//!   large as fits
//! - an over-budget child closes the group before it and is split the same way
//! - a directory's own row goes with the first part of its contents
//!
//! Where a cut falls depends only on the subtree and its siblings, never on
//! what came before, which is what lets a task that walks a subtree produce
//! that subtree's parts by itself.
//!
//! # Holding only what is undecided
//!
//! Rows wait in one queue until their part is sealed. A directory is
//! *measuring* until its subtree passes the budget, and *split* after. The
//! outermost measuring directory is always the one to cross first, and when it
//! does, every group before it is sealed, outermost first, so parts leave in
//! walk order. Waiting rows stay around two budgets.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirEntry, FileType};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use error_stack::{Report, ResultExt as _};

use crate::part::{EntryKind, Part, Timestamp};

/// Default part budget, in estimated JSON bytes.
pub const DEFAULT_BUDGET: u64 = 4 << 20;

// Fixed per-row estimates: every column a row adds to its part's JSON except
// its strings, including the ones filled in after the walk (checksum, frame,
// offset). Fixed on purpose: the real digit widths are either not known yet or
// changed by `--reproducible`, and a cut must not move with them.
const FILE_ROW: u64 = 140;
const DIR_ROW: u64 = 45;
const LINK_ROW: u64 = 40;

/// The walk could not finish.
///
/// - one context for the whole walk; what went wrong is attached
/// - distinguishable causes: `report.contains::<Cancelled>()`,
///   `report.contains::<PartFull>()`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkError;

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not walk the source tree")
    }
}

impl std::error::Error for WalkError {}

/// The cancel flag was raised. Parts already handed to the sink stay handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the walk was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Decides which entries the walk records.
///
/// - asked before anything is stated or descended into
/// - a refused directory costs its whole subtree, uncounted
/// - parts depend on the filter: it is part of the input, like the tree
pub trait Filter: Send + Sync {
    fn keep(&self, candidate: &Candidate<'_>) -> bool;
}

/// An entry offered to a [`Filter`].
pub struct Candidate<'a> {
    /// Path of the directory holding it, relative to the root; empty at the
    /// root.
    pub parent: &'a str,
    pub name: &'a str,
    pub kind: EntryKind,
    /// Everything else in the same directory, for rules that depend on
    /// siblings.
    pub listing: Listing<'a>,
}

/// One directory's entries, sorted by name.
#[derive(Clone, Copy)]
pub struct Listing<'a> {
    entries: &'a [Listed],
}

impl<'a> Listing<'a> {
    /// Whether an entry named `name` is in the directory. A binary search.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries
            .binary_search_by(|entry| entry.name.as_os_str().cmp(name.as_ref()))
            .is_ok()
    }

    /// Every UTF-8 name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        self.entries.iter().filter_map(|entry| entry.name.to_str())
    }

    /// Entries, including ones whose names are not UTF-8.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Told what the walk is doing. Every method defaults to nothing.
pub trait Progress: Send + Sync {
    /// About to list a directory; empty for the root.
    fn entered(&self, _dir: &str) {}
    /// An entry was recorded. `size` is 0 for anything but a file.
    fn recorded(&self, _kind: EntryKind, _size: u64) {}
    /// An entry was counted in [`Skips`] instead. `path` is lossy for a
    /// non-UTF-8 name.
    fn skipped(&self, _path: &str, _reason: SkipReason) {}
}

/// Why an entry was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Socket, fifo, device: no bytes and no target.
    Special,
    /// Name or symlink target is not UTF-8.
    NonUtf8,
    /// A directory that could not be listed; its subtree is missing too.
    Unreadable,
    /// Type, metadata or target unreadable, under [`OnError::Skip`].
    Failed,
}

/// What an entry the walk fails to read costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// The walk fails, naming the entry. What an archive wants: a hole there
    /// would be silent.
    #[default]
    Fail,
    /// Counted in [`Skips::failed`] and reported to [`Progress::skipped`].
    Skip,
}

/// How to walk.
#[derive(Clone, Copy)]
pub struct WalkOptions<'a> {
    /// Estimated JSON bytes per part.
    pub budget: u64,
    pub filter: Option<&'a dyn Filter>,
    pub progress: Option<&'a dyn Progress>,
    /// Checked before every entry.
    pub cancel: Option<&'a AtomicBool>,
    pub on_error: OnError,
}

impl Default for WalkOptions<'_> {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            filter: None,
            progress: None,
            cancel: None,
            on_error: OnError::Fail,
        }
    }
}

/// Entries the walk met and did not record, by reason. A filter's refusals
/// are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skips {
    pub special: u32,
    pub non_utf8: u32,
    pub unreadable: u32,
    pub failed: u32,
}

impl Skips {
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.special + self.non_utf8 + self.unreadable + self.failed
    }

    #[must_use]
    pub const fn any(&self) -> bool {
        self.total() > 0
    }
}

/// A whole walk, collected: see [`walk`].
#[derive(Debug, Clone, Default)]
pub struct Walk {
    pub parts: Vec<Part>,
    pub skips: Skips,
}

impl Walk {
    /// Every entry with its path, in walk order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, EntryKind)> {
        self.parts.iter().flat_map(Part::entries).collect()
    }

    /// Every path, in walk order.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.parts
            .iter()
            .flat_map(Part::entries)
            .map(|(path, _)| path)
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.iter().map(Part::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.parts.iter().map(Part::total_bytes).sum()
    }
}

/// Estimated JSON bytes of one row. `target` is empty for all but a symlink.
#[must_use]
pub fn estimate(kind: EntryKind, name: &str, target: &str) -> u64 {
    let fixed = match kind {
        EntryKind::File => FILE_ROW,
        EntryKind::Dir => DIR_ROW,
        EntryKind::Symlink => LINK_ROW,
    };
    fixed + name.len() as u64 + target.len() as u64
}

/// Walk `root`, collecting every part.
///
/// # Errors
/// As [`walk_parts`].
pub fn walk(root: &Path, options: &WalkOptions<'_>) -> Result<Walk, Report<WalkError>> {
    let mut parts = Vec::new();
    let skips = walk_parts(root, options, &mut |part| {
        parts.push(part);
        Ok(())
    })?;
    Ok(Walk { parts, skips })
}

/// Walk `root`, handing each part to `sink` as it is sealed, in walk order.
///
/// - `root` itself is not recorded; paths are relative to it
/// - an unreadable directory costs its subtree; the rest still walks
/// - a sink error stops the walk and is returned as is
///
/// # Errors
/// [`WalkError`]: an entry unreadable under [`OnError::Fail`], cancelled, a
/// part outgrew its `u32` addressing, or the sink failed.
pub fn walk_parts(
    root: &Path,
    options: &WalkOptions<'_>,
    sink: &mut dyn FnMut(Part) -> Result<(), Report<WalkError>>,
) -> Result<Skips, Report<WalkError>> {
    let mut walker = Walker {
        options,
        sink,
        rows: Vec::new(),
        text: String::new(),
        base: 0,
        total: 0,
        path: String::new(),
        stack: Vec::new(),
        measuring: 1,
        skips: Skips::default(),
    };
    walker.run(root)?;
    Ok(walker.skips)
}

struct Listed {
    name: OsString,
    file_type: FileType,
    entry: DirEntry,
}

// A row waiting for its part. Its strings sit in `Walker::text`, in row order.
struct Row {
    depth: u32,
    name_len: u32,
    target_len: u32,
    meta: Meta,
}

enum Meta {
    Dir {
        mtime: Option<Timestamp>,
        mode: u32,
    },
    File {
        size: u64,
        mtime: Option<Timestamp>,
        mode: u32,
    },
    Link {
        mtime: Option<Timestamp>,
    },
}

// An open directory. Row positions are absolute: `Walker::base` plus an index
// into `Walker::rows`.
struct Level {
    listing: Vec<Listed>,
    next: usize,
    // Length of this directory's relative path in `Walker::path`.
    path_len: usize,
    // Position of its own row; unused at the root, which has none.
    row: usize,
    // `Walker::total` before its own row: its subtree so far is the difference.
    start_total: u64,
    split: bool,
    // Split only: where the open group starts, and its estimate. Empty when
    // `group_first` is the position of whatever comes next; `usize::MAX` while
    // an over-budget child is being walked.
    group_first: usize,
    group_bytes: u64,
    // Measuring only: each finished child's position and estimate.
    completed: Vec<(usize, u64)>,
}

struct Walker<'o, 's> {
    options: &'o WalkOptions<'o>,
    sink: &'s mut dyn FnMut(Part) -> Result<(), Report<WalkError>>,
    rows: Vec<Row>,
    text: String,
    // Absolute position of `rows[0]`; everything before it has been sealed.
    base: usize,
    // Running estimate of every row so far.
    total: u64,
    path: String,
    stack: Vec<Level>,
    // Index of the outermost measuring level; `stack.len()` if none.
    measuring: usize,
    skips: Skips,
}

impl Walker<'_, '_> {
    fn run(&mut self, root: &Path) -> Result<(), Report<WalkError>> {
        if let Some(progress) = self.options.progress {
            progress.entered("");
        }
        let listing = self.list(root)?;
        self.stack.push(Level {
            listing,
            next: 0,
            path_len: 0,
            row: 0,
            start_total: 0,
            split: true,
            group_first: 0,
            group_bytes: 0,
            completed: Vec::new(),
        });

        while let Some(top) = self.stack.last_mut() {
            if top.next == top.listing.len() {
                self.finish_level()?;
                continue;
            }
            let index = top.next;
            top.next += 1;
            if self
                .options
                .cancel
                .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
            {
                return Err(Report::new(Cancelled).change_context(WalkError));
            }
            // The listing is moved out while its entry is handled, so the rest
            // of the walker stays free to change.
            let level = self.stack.len() - 1;
            let listing = std::mem::take(&mut self.stack[level].listing);
            let visited = self.visit(level, &listing, index);
            self.stack[level].listing = listing;
            visited?;
        }
        Ok(())
    }

    // One entry's whole handling, top to bottom: split up, it only moves the
    // shared bookkeeping into more signatures.
    #[allow(clippy::too_many_lines)]
    fn visit(
        &mut self,
        level: usize,
        listing: &[Listed],
        index: usize,
    ) -> Result<(), Report<WalkError>> {
        let listed = &listing[index];
        let parent_len = self.stack[level].path_len;
        self.path.truncate(parent_len);

        let Some(name) = listed.name.to_str() else {
            self.skip_lossy(&listed.name, SkipReason::NonUtf8);
            return Ok(());
        };
        let kind = if listed.file_type.is_symlink() {
            EntryKind::Symlink
        } else if listed.file_type.is_dir() {
            EntryKind::Dir
        } else if listed.file_type.is_file() {
            EntryKind::File
        } else {
            self.skip_lossy(&listed.name, SkipReason::Special);
            return Ok(());
        };
        if let Some(filter) = self.options.filter {
            let candidate = Candidate {
                parent: &self.path,
                name,
                kind,
                listing: Listing { entries: listing },
            };
            if !filter.keep(&candidate) {
                return Ok(());
            }
        }
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(name);

        let metadata = match listed.entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return self.failed(error, "metadata"),
        };
        let mtime = metadata.modified().ok().map(Timestamp::from_system_time);
        let depth = u32::try_from(level).change_context(WalkError)?;
        let name_len = u32::try_from(name.len()).change_context(WalkError)?;

        match kind {
            EntryKind::Symlink => {
                let target = match fs::read_link(listed.entry.path()) {
                    Ok(target) => target,
                    Err(error) => return self.failed(error, "target"),
                };
                let Some(target) = target.to_str() else {
                    self.skip(SkipReason::NonUtf8);
                    return Ok(());
                };
                let target = target.replace('\\', "/");
                let target_len = u32::try_from(target.len()).change_context(WalkError)?;
                let bytes = estimate(kind, name, &target);
                self.text.push_str(name);
                self.text.push_str(&target);
                self.push_leaf(
                    Row {
                        depth,
                        name_len,
                        target_len,
                        meta: Meta::Link { mtime },
                    },
                    bytes,
                )?;
                self.recorded(kind, 0);
            }
            EntryKind::File => {
                let bytes = estimate(kind, name, "");
                self.text.push_str(name);
                self.push_leaf(
                    Row {
                        depth,
                        name_len,
                        target_len: 0,
                        meta: Meta::File {
                            size: metadata.len(),
                            mtime,
                            mode: mode_of(&metadata, false),
                        },
                    },
                    bytes,
                )?;
                self.recorded(kind, metadata.len());
            }
            EntryKind::Dir => {
                let bytes = estimate(kind, name, "");
                self.text.push_str(name);
                let row = self.base + self.rows.len();
                self.rows.push(Row {
                    depth,
                    name_len,
                    target_len: 0,
                    meta: Meta::Dir {
                        mtime,
                        mode: mode_of(&metadata, true),
                    },
                });
                self.recorded(kind, 0);
                if let Some(progress) = self.options.progress {
                    progress.entered(&self.path);
                }
                let listing = self.list(&listed.entry.path())?;
                self.stack.push(Level {
                    listing,
                    next: 0,
                    path_len: self.path.len(),
                    row,
                    start_total: self.total,
                    split: false,
                    group_first: 0,
                    group_bytes: 0,
                    completed: Vec::new(),
                });
                self.total += bytes;
                self.check_budget()?;
            }
        }
        Ok(())
    }

    // Read and sort a directory. An unreadable one is a skip, not an error.
    fn list(&mut self, dir: &Path) -> Result<Vec<Listed>, Report<WalkError>> {
        let Ok(read) = fs::read_dir(dir) else {
            self.skip(SkipReason::Unreadable);
            return Ok(Vec::new());
        };
        let mut listing = Vec::new();
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.failed(error, "listing")?;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    let parent_len = self.path.len();
                    if parent_len > 0 {
                        self.path.push('/');
                    }
                    self.path.push_str(&entry.file_name().to_string_lossy());
                    let failed = self.failed(error, "kind");
                    self.path.truncate(parent_len);
                    failed?;
                    continue;
                }
            };
            listing.push(Listed {
                name: entry.file_name(),
                file_type,
                entry,
            });
        }
        listing.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(listing)
    }

    // A file or symlink row, whose strings are already in `text`.
    fn push_leaf(&mut self, row: Row, bytes: u64) -> Result<(), Report<WalkError>> {
        let position = self.base + self.rows.len();
        self.rows.push(row);
        self.total += bytes;
        let level = self.stack.len() - 1;
        if self.stack[level].split {
            self.add_to_group(level, position, bytes)?;
        } else {
            self.stack[level].completed.push((position, bytes));
        }
        self.check_budget()
    }

    // Offer a finished child of split `level` to its open group.
    fn add_to_group(
        &mut self,
        level: usize,
        position: usize,
        bytes: u64,
    ) -> Result<(), Report<WalkError>> {
        let open = &self.stack[level];
        if open.group_first < position && open.group_bytes + bytes > self.options.budget {
            self.seal(position)?;
            let open = &mut self.stack[level];
            open.group_first = position;
            open.group_bytes = 0;
        }
        self.stack[level].group_bytes += bytes;
        Ok(())
    }

    // Split every measuring level whose subtree has passed the budget,
    // outermost first. Only the outermost can be the first to cross.
    fn check_budget(&mut self) -> Result<(), Report<WalkError>> {
        while self.measuring < self.stack.len()
            && self.total - self.stack[self.measuring].start_total > self.options.budget
        {
            self.split(self.measuring)?;
            self.measuring += 1;
        }
        Ok(())
    }

    fn split(&mut self, level: usize) -> Result<(), Report<WalkError>> {
        let row = self.stack[level].row;
        // The parent is already split. This child closes its open group.
        if self.stack[level - 1].group_first < row {
            self.seal(row)?;
        }
        let parent = &mut self.stack[level - 1];
        parent.group_first = usize::MAX;
        parent.group_bytes = 0;

        let this = &mut self.stack[level];
        this.split = true;
        this.group_first = row + 1;
        this.group_bytes = 0;
        for (position, bytes) in std::mem::take(&mut this.completed) {
            self.add_to_group(level, position, bytes)?;
        }
        Ok(())
    }

    fn finish_level(&mut self) -> Result<(), Report<WalkError>> {
        let level = self.stack.len() - 1;
        let end = self.base + self.rows.len();
        if level == 0 {
            self.seal(end)?;
            self.stack.pop();
            return Ok(());
        }

        // Sealed before the pop: a part starting inside this directory needs it
        // for its stem.
        let this = &self.stack[level];
        // A directory whose row never left — possible only when the row alone
        // passes the budget — takes it out now rather than letting it drift into
        // a later sibling's part.
        if this.split && (this.group_first < end || self.base <= this.row) {
            self.seal(end)?;
        }
        let done = self.stack.pop().expect("a level to finish");
        self.measuring = self.measuring.min(level);
        if done.split {
            let parent = &mut self.stack[level - 1];
            parent.group_first = end;
            parent.group_bytes = 0;
        } else {
            let bytes = self.total - done.start_total;
            if self.stack[level - 1].split {
                self.add_to_group(level - 1, done.row, bytes)?;
            } else {
                self.stack[level - 1].completed.push((done.row, bytes));
            }
        }
        self.path.truncate(self.stack[level - 1].path_len);
        Ok(())
    }

    // Seal every waiting row before absolute position `end` into a part.
    fn seal(&mut self, end: usize) -> Result<(), Report<WalkError>> {
        let count = end - self.base;
        if count == 0 {
            return Ok(());
        }
        let mut part = Part::default();

        // The stem is the open directories above the first row.
        let stem_len = self.rows[0].depth as usize;
        for level in 1..=stem_len {
            let start = match self.stack[level - 1].path_len {
                0 => 0,
                parent_len => parent_len + 1,
            };
            part.push_stem(&self.path[start..self.stack[level].path_len])
                .change_context(WalkError)?;
        }
        let mut nodes: Vec<u32> = (0..=stem_len)
            .map(|node| u32::try_from(node).expect("stem depth fits a u32"))
            .collect();

        let mut cursor = 0;
        for row in &self.rows[..count] {
            let name_end = cursor + row.name_len as usize;
            let name = &self.text[cursor..name_end];
            let target = &self.text[name_end..name_end + row.target_len as usize];
            cursor = name_end + row.target_len as usize;
            let depth = row.depth as usize;
            let parent = nodes[depth];
            match row.meta {
                Meta::Dir { mtime, mode } => {
                    let node = part
                        .push_dir(parent, name, mtime, mode)
                        .change_context(WalkError)?;
                    nodes.truncate(depth + 1);
                    nodes.push(node);
                }
                Meta::File { size, mtime, mode } => part
                    .push_file(parent, name, size, mtime, mode)
                    .change_context(WalkError)?,
                Meta::Link { mtime } => part
                    .push_link(parent, name, target, mtime)
                    .change_context(WalkError)?,
            }
        }
        self.rows.drain(..count);
        self.text.drain(..cursor);
        self.base = end;
        (self.sink)(part)
    }

    fn failed(&mut self, error: std::io::Error, what: &str) -> Result<(), Report<WalkError>> {
        match self.options.on_error {
            OnError::Fail => Err(error)
                .attach_with(|| format!("{what} of {}", self.path))
                .change_context(WalkError),
            OnError::Skip => {
                self.skip(SkipReason::Failed);
                Ok(())
            }
        }
    }

    // Count a skip of the entry `self.path` names.
    fn skip(&mut self, reason: SkipReason) {
        let count = match reason {
            SkipReason::Special => &mut self.skips.special,
            SkipReason::NonUtf8 => &mut self.skips.non_utf8,
            SkipReason::Unreadable => &mut self.skips.unreadable,
            SkipReason::Failed => &mut self.skips.failed,
        };
        *count += 1;
        if let Some(progress) = self.options.progress {
            progress.skipped(&self.path, reason);
        }
    }

    // Count a skip of `name` inside the directory `self.path` names.
    fn skip_lossy(&mut self, name: &std::ffi::OsStr, reason: SkipReason) {
        let parent_len = self.path.len();
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(&name.to_string_lossy());
        self.skip(reason);
        self.path.truncate(parent_len);
    }

    fn recorded(&self, kind: EntryKind, size: u64) {
        if let Some(progress) = self.options.progress {
            progress.recorded(kind, size);
        }
    }
}

fn mode_of(metadata: &fs::Metadata, is_dir: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = is_dir;
        metadata.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (is_dir, metadata.permissions().readonly()) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}
