//! The source walk: every entry under a root, in walk order, handed out in
//! parts.
//!
//! # How a walk works
//!
//! 1. Fill a [`WalkOptions`]. [`WalkOptions::default()`] records everything
//!    under the root, fails on the first entry it cannot read, and cuts parts
//!    at [`DEFAULT_BUDGET`].
//! 2. Call [`walk_parts`] with the root and a sink. The walk hands the sink
//!    each [`TreePart`] as soon as the part is complete, in walk order, and
//!    holds nothing of it afterwards. [`walk`] does the same and collects the
//!    parts into a [`Walk`].
//! 3. Read each part's rows, or write it with [`TreePart::to_json`].
//!
//! The walk opens no file and reads no contents. Paths are relative to the
//! root, and the root itself has no row.
//!
//! # Walk order
//!
//! The walk is depth-first. It lists each directory, sorts the entries by
//! name, bytewise, and visits them in that order. A directory's row comes
//! directly before the rows of its contents.
//! [`walk_order`](crate::walk_order) compares two paths in this order.
//!
//! # Where parts are cut
//!
//! Each row has an estimated size in JSON bytes ([`estimate`]), computed from
//! its kind and the lengths of its name and target. The walk sums these
//! estimates and cuts parts against [`WalkOptions::budget`]:
//!
//! - a directory whose whole subtree fits the budget is never split;
//! - a directory whose subtree does not fit groups its children in order, and
//!   each group takes as many children as fit;
//! - a child whose own subtree does not fit closes the group before it and is
//!   cut by these same rules;
//! - a directory's own row goes into the first part of its contents.
//!
//! Where a cut falls depends only on the tree, the filter and the budget,
//! never on how the walk was scheduled.
//!
//! # Memory
//!
//! The walk holds the rows that are not yet in a sealed part, and their
//! estimates add up to less than about two budgets. It also holds the listing
//! of every directory between the root and the entry being visited, so a
//! directory with millions of children costs its whole listing while the walk
//! is inside it.
//!
//! On Unix the walk also keeps each of those directories open, one file
//! descriptor per level of depth. When the process runs out of descriptors, the
//! walk closes the shallowest one. It opens that directory again, by its path,
//! when it returns to it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use error_stack::{Report, ResultExt as _};

use crate::manifest::{EntryKind, Timestamp, TreePart};

mod reader;

use self::reader::{Directory, Kind, Listed, Scratch};

/// The budget of [`WalkOptions::default()`]: 4 MiB of estimated JSON per
/// part.
pub const DEFAULT_BUDGET: u64 = 4 << 20;

// Fixed per-row estimates: what a row adds to its part's JSON apart from its
// strings, rounded up. Fixed, because the digit widths of the real values are
// not known when a cut is decided, and a cut must not move with them.
const FILE_ROW: u64 = 140;
const DIRECTORY_ROW: u64 = 45;
const SYMLINK_ROW: u64 = 46;

/// The walk could not finish.
///
/// The report names the entry the walk failed on. Two causes have their own
/// types inside the report: [`Cancelled`] and
/// [`TreePartFull`](crate::TreePartFull). Test for them with
/// `report.contains::<Cancelled>()` and `report.contains::<TreePartFull>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkError;

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("could not walk the source tree")
    }
}

impl std::error::Error for WalkError {}

/// The walk stopped because [`WalkOptions::cancel`] was set. Parts the sink
/// already received are complete.
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
/// The walk asks the filter about every entry before it reads the entry's
/// metadata or lists it. The one exception is a filesystem whose listings give
/// no kinds: there the walk reads the metadata first, to learn the kind. A refused directory is not listed, so nothing under
/// it is offered or counted. The filter changes where parts are cut, in the
/// same way that the tree does.
pub trait Filter: Send + Sync {
    /// Returns `true` to record `candidate`, or `false` to leave it out.
    fn keep(&self, candidate: &Candidate<'_>) -> bool;
}

/// An entry offered to a [`Filter`].
pub struct Candidate<'a> {
    /// The path of the directory that holds the entry, relative to the root.
    /// Empty for an entry in the root.
    pub parent: &'a str,
    /// The entry's name.
    pub name: &'a str,
    /// The entry's kind.
    pub kind: EntryKind,
    /// Every entry of the same directory, the candidate included.
    pub listing: Listing<'a>,
}

/// The entries of one directory, sorted by name.
#[derive(Clone, Copy)]
pub struct Listing<'a> {
    entries: &'a [Listed],
    names: &'a [u8],
}

impl<'a> Listing<'a> {
    /// Returns `true` if the directory holds an entry named `name`. This is a
    /// binary search over the sorted names.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        reader::Listing::find(self.entries, self.names, name.as_bytes())
            .is_some_and(|found| self.entries[found].kind != Kind::NonUtf8)
    }

    /// Returns every name that is UTF-8, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let names = self.names;
        self.entries
            .iter()
            .filter(|entry| entry.kind != Kind::NonUtf8)
            .filter_map(move |entry| std::str::from_utf8(entry.name(names)).ok())
    }

    /// Returns the number of entries, including those whose names are not
    /// UTF-8.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the directory holds no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Receives a call for each step the walk takes. Every method does nothing
/// unless you override it.
pub trait Progress: Send + Sync {
    /// Called before the walk lists the directory at `directory`, which is
    /// empty for the root.
    fn entered(&self, _directory: &str) {}
    /// Called when the walk records an entry. `size` is the file's size, or 0
    /// for a directory or a link.
    fn recorded(&self, _kind: EntryKind, _size: u64) {}
    /// Called when the walk skips an entry and counts it in [`Skips`]. A name
    /// that is not UTF-8 appears in `path` with its invalid bytes replaced by
    /// U+FFFD.
    fn skipped(&self, _path: &str, _reason: SkipReason) {}
}

/// Why the walk skipped an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The entry is a socket, a fifo or a device.
    Special,
    /// The entry's name or link target is not UTF-8.
    NonUtf8,
    /// The entry is a directory that could not be opened, and the policy is
    /// [`OnError::Skip`]. Nothing under it was visited.
    Unreadable,
    /// The entry's type, metadata or link target could not be read, and the
    /// policy is [`OnError::Skip`].
    Failed,
}

/// What the walk does with an entry it cannot read: one whose type, metadata
/// or link target cannot be read, or a directory that cannot be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// The walk fails with a [`WalkError`] that names the entry.
    #[default]
    Fail,
    /// The walk counts the entry in [`Skips::failed`], or a directory it
    /// cannot open in [`Skips::unreadable`], reports it to
    /// [`Progress::skipped`], and goes on.
    Skip,
}

/// The settings of one walk.
#[derive(Clone, Copy)]
pub struct WalkOptions<'a> {
    /// The estimated JSON bytes a part may hold before the walk cuts it. See
    /// [the module doc](self#where-parts-are-cut) for how a cut is placed.
    pub budget: u64,
    /// The filter the walk consults before recording an entry, if any.
    pub filter: Option<&'a dyn Filter>,
    /// The receiver of progress calls, if any.
    pub progress: Option<&'a dyn Progress>,
    /// A flag the walk reads before every entry. Set it to `true` to stop the
    /// walk with [`Cancelled`].
    pub cancel: Option<&'a AtomicBool>,
    /// What the walk does with an entry it cannot read.
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

/// The entries the walk met and did not record, counted by [`SkipReason`].
/// Entries a [`Filter`] refused are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skips {
    /// Sockets, fifos and devices.
    pub special: u32,
    /// Entries whose name or link target is not UTF-8.
    pub non_utf8: u32,
    /// Directories that could not be opened under [`OnError::Skip`]. Each
    /// counts once, whatever it held.
    pub unreadable: u32,
    /// Entries whose type, metadata or link target could not be read under
    /// [`OnError::Skip`].
    pub failed: u32,
}

impl Skips {
    /// Returns the number of skipped entries over every reason.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.special + self.non_utf8 + self.unreadable + self.failed
    }

    /// Returns `true` if the walk skipped anything.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.total() > 0
    }
}

/// A whole walk, as [`walk`] returns it.
#[derive(Debug, Clone, Default)]
pub struct Walk {
    /// Every part, in walk order.
    pub parts: Vec<TreePart>,
    /// What the walk skipped.
    pub skips: Skips,
}

impl Walk {
    /// Returns every entry with its path, in walk order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, EntryKind)> {
        self.parts.iter().flat_map(TreePart::entries).collect()
    }

    /// Returns every path, in walk order.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.parts
            .iter()
            .flat_map(TreePart::entries)
            .map(|(path, _)| path)
            .collect()
    }

    /// Returns the number of rows over every part.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.iter().map(TreePart::len).sum()
    }

    /// Returns `true` if the walk recorded nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the sum of every file's size.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.parts.iter().map(TreePart::total_bytes).sum()
    }
}

/// Returns the estimated JSON bytes of one row: a fixed width for its kind,
/// plus the lengths of `name` and `target`. Pass an empty `target` for
/// anything but a link.
///
/// The estimate is what the walk cuts parts against. It is not the number of
/// bytes [`TreePart::to_json`] writes for the row.
#[must_use]
pub fn estimate(kind: EntryKind, name: &str, target: &str) -> u64 {
    let fixed = match kind {
        EntryKind::File => FILE_ROW,
        EntryKind::Directory => DIRECTORY_ROW,
        EntryKind::Symlink => SYMLINK_ROW,
    };
    fixed + name.len() as u64 + target.len() as u64
}

/// Walks `root` and collects every part.
///
/// The whole walk is in memory when this returns. Use [`walk_parts`] to
/// handle each part as it is sealed instead.
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

/// Walks `root` and hands each part to `sink` as soon as it is sealed, in
/// walk order.
///
/// Returns what the walk skipped. `root` itself gets no row, and every path
/// is relative to it. An error from `sink` stops the walk and is returned
/// unchanged.
///
/// # Errors
/// [`WalkError`] if `root` cannot be opened, if an entry cannot be read or a
/// directory cannot be opened under [`OnError::Fail`], if
/// [`WalkOptions::cancel`] was set ([`Cancelled`]), if a part passed the `u32`
/// it addresses its text and nodes with
/// ([`TreePartFull`](crate::TreePartFull)), or if `sink` returned an error.
pub fn walk_parts(
    root: &Path,
    options: &WalkOptions<'_>,
    sink: &mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
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
        root: root.to_path_buf(),
        scratch: Scratch::default(),
    };
    walker.run(root)?;
    Ok(walker.skips)
}

// A row waiting for its part. Its strings sit in `Walker::text`, in row order.
struct Row {
    depth: u32,
    name_len: u32,
    target_len: u32,
    meta: Meta,
}

enum Meta {
    Directory {
        mtime: Option<Timestamp>,
        mode: u32,
    },
    File {
        size: u64,
        mtime: Option<Timestamp>,
        mode: u32,
    },
    Symlink {
        mtime: Option<Timestamp>,
        directory: Option<bool>,
    },
}

// An open directory. Row positions are absolute: `Walker::base` plus an index
// into `Walker::rows`.
struct Level {
    listing: reader::Listing,
    // `None` for a directory that could not be opened, and for one whose
    // handle was given up when the process ran out of them.
    directory: Option<Directory>,
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
    sink: &'s mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
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
    root: PathBuf,
    scratch: Scratch,
}

impl Walker<'_, '_> {
    fn run(&mut self, root: &Path) -> Result<(), Report<WalkError>> {
        if let Some(progress) = self.options.progress {
            progress.entered("");
        }
        // An error under either policy: counting the root as a skip would
        // report an empty walk as a success.
        let directory = Directory::open_root(root)
            .attach_with(|| format!("listing {}", root.display()))
            .change_context(WalkError)?;
        let mut listing = directory
            .list(&mut self.scratch)
            .attach_with(|| format!("listing {}", root.display()))
            .change_context(WalkError)?;
        self.report(&mut listing)?;
        self.stack.push(Level {
            listing,
            directory: Some(directory),
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
            if top.next == top.listing.entries.len() {
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
            // The listing is moved out while its entry is handled, so that
            // `visit` can borrow the walker mutably.
            let level = self.stack.len() - 1;
            let listing = std::mem::take(&mut self.stack[level].listing);
            let visited = self.visit(level, &listing, index);
            self.stack[level].listing = listing;
            visited?;
        }
        Ok(())
    }

    // Handles one entry from start to finish. Splitting it would pass the
    // same walker state through several more functions.
    #[allow(clippy::too_many_lines)]
    fn visit(
        &mut self,
        level: usize,
        listing: &reader::Listing,
        index: usize,
    ) -> Result<(), Report<WalkError>> {
        let listed = &listing.entries[index];
        let parent_len = self.stack[level].path_len;
        self.path.truncate(parent_len);

        let name = listed.name(listing.names());
        let name = match std::str::from_utf8(name) {
            Ok(name) if listed.kind != Kind::NonUtf8 => name,
            Ok(lossy) => {
                self.skip_named(lossy, SkipReason::NonUtf8);
                return Ok(());
            }
            Err(_) => {
                self.skip_named(&String::from_utf8_lossy(name), SkipReason::NonUtf8);
                return Ok(());
            }
        };
        self.ensure_open(level)?;

        // A listing without kinds costs a read of the metadata before the
        // filter is asked.
        let mut stat = None;
        let mut listed_kind = listed.kind;
        if listed_kind == Kind::Unknown {
            match self.open(level).stat(listed, listing) {
                Ok(found) => {
                    listed_kind = found.kind;
                    stat = Some(found);
                }
                Err(error) => return self.failed_at(error, name, "kind"),
            }
        }
        let kind = match listed_kind {
            Kind::Symlink { .. } => EntryKind::Symlink,
            Kind::Directory => EntryKind::Directory,
            Kind::File => EntryKind::File,
            Kind::Special | Kind::Unknown | Kind::NonUtf8 => {
                self.skip_named(name, SkipReason::Special);
                return Ok(());
            }
        };
        if let Some(filter) = self.options.filter {
            let candidate = Candidate {
                parent: &self.path,
                name,
                kind,
                listing: Listing {
                    entries: &listing.entries,
                    names: listing.names(),
                },
            };
            if !filter.keep(&candidate) {
                return Ok(());
            }
        }
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(name);

        // A directory is opened before its metadata is read, because an open
        // directory can report its own.
        let opened =
            (kind == EntryKind::Directory).then(|| self.open_child(level, listed, listing));
        let stat = match (stat, &opened) {
            (Some(stat), _) => Ok(stat),
            (None, Some(Ok(directory))) if Directory::STATS_ITSELF => directory.stat_self(),
            _ => self.open(level).stat(listed, listing),
        };
        let stat = match stat {
            Ok(stat) => stat,
            Err(error) => return self.failed(error, "metadata"),
        };
        let depth = narrow(level)?;
        let name_len = narrow(name.len())?;

        match listed_kind {
            Kind::Symlink { directory } => {
                let target = match self.open(level).read_link(listed, listing) {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        self.skip(SkipReason::NonUtf8);
                        return Ok(());
                    }
                    Err(error) => return self.failed(error, "target"),
                };
                let target = if target.contains('\\') {
                    target.replace('\\', "/")
                } else {
                    target
                };
                let target_len = narrow(target.len())?;
                let bytes = estimate(kind, name, &target);
                self.text.push_str(name);
                self.text.push_str(&target);
                self.push_leaf(
                    Row {
                        depth,
                        name_len,
                        target_len,
                        meta: Meta::Symlink {
                            mtime: stat.mtime,
                            directory,
                        },
                    },
                    bytes,
                )?;
                self.recorded(kind, 0);
            }
            Kind::Directory => {
                let bytes = estimate(kind, name, "");
                self.text.push_str(name);
                let row = self.base + self.rows.len();
                self.rows.push(Row {
                    depth,
                    name_len,
                    target_len: 0,
                    meta: Meta::Directory {
                        mtime: stat.mtime,
                        mode: stat.mode,
                    },
                });
                self.recorded(kind, 0);
                if let Some(progress) = self.options.progress {
                    progress.entered(&self.path);
                }
                let read = opened
                    .expect("a directory was opened above")
                    .and_then(|directory| Ok((directory.list(&mut self.scratch)?, directory)));
                let (mut listing, directory) = match read {
                    Ok((listing, directory)) => (listing, Some(directory)),
                    Err(error) => {
                        self.unreadable(error)?;
                        (reader::Listing::default(), None)
                    }
                };
                self.report(&mut listing)?;
                self.stack.push(Level {
                    listing,
                    directory,
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
            _ => {
                let bytes = estimate(kind, name, "");
                self.text.push_str(name);
                self.push_leaf(
                    Row {
                        depth,
                        name_len,
                        target_len: 0,
                        meta: Meta::File {
                            size: stat.size,
                            mtime: stat.mtime,
                            mode: stat.mode,
                        },
                    },
                    bytes,
                )?;
                self.recorded(kind, stat.size);
            }
        }
        Ok(())
    }

    fn open(&self, level: usize) -> &Directory {
        self.stack[level]
            .directory
            .as_ref()
            .expect("`ensure_open` ran for this level")
    }

    // Opens a child of `level`. When the process has no handle left, the walk
    // gives up the handle of its shallowest open ancestor and tries again.
    // `ensure_open` opens that ancestor again when the walk returns to it.
    fn open_child(
        &mut self,
        level: usize,
        listed: &Listed,
        listing: &reader::Listing,
    ) -> std::io::Result<Directory> {
        loop {
            match self.open(level).open_child(listed, listing) {
                Err(error) if Directory::out_of_handles(&error) && self.release_above(level) => {}
                result => return result,
            }
        }
    }

    fn release_above(&mut self, level: usize) -> bool {
        let open = self.stack[..level]
            .iter_mut()
            .find(|above| above.directory.is_some());
        match open {
            Some(above) => {
                above.directory = None;
                true
            }
            None => false,
        }
    }

    // `self.path` must name the directory of `level` or something below it.
    fn ensure_open(&mut self, level: usize) -> Result<(), Report<WalkError>> {
        if self.stack[level].directory.is_some() {
            return Ok(());
        }
        let relative = &self.path[..self.stack[level].path_len];
        let path = self.root.join(relative);
        loop {
            match Directory::open_root(&path) {
                Ok(directory) => {
                    self.stack[level].directory = Some(directory);
                    return Ok(());
                }
                Err(error) if Directory::out_of_handles(&error) && self.release_above(level) => {}
                Err(error) => {
                    return Err(error)
                        .attach_with(|| format!("opening {} again", path.display()))
                        .change_context(WalkError);
                }
            }
        }
    }

    // A directory below the root that cannot be opened or listed costs its
    // whole subtree, so under `Skip` it has its own count.
    fn unreadable(&mut self, error: std::io::Error) -> Result<(), Report<WalkError>> {
        match self.options.on_error {
            OnError::Fail => Err(error)
                .attach_with(|| format!("listing {}", self.path))
                .change_context(WalkError),
            OnError::Skip => {
                self.skip(SkipReason::Unreadable);
                Ok(())
            }
        }
    }

    // Handles the entries a listing could not read. `self.path` names the
    // listed directory.
    fn report(&mut self, listing: &mut reader::Listing) -> Result<(), Report<WalkError>> {
        for (error, name, what) in std::mem::take(&mut listing.failures) {
            match name {
                Some(name) => self.failed_at(error, &name, what)?,
                None => self.failed(error, what)?,
            }
        }
        Ok(())
    }

    // `failed` for the entry `name` inside the directory `self.path` names.
    fn failed_at(
        &mut self,
        error: std::io::Error,
        name: &str,
        what: &str,
    ) -> Result<(), Report<WalkError>> {
        let parent_len = self.path.len();
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(name);
        let failed = self.failed(error, what);
        self.path.truncate(parent_len);
        failed
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
        // A directory's own row is still waiting here only when that row alone
        // passes the budget. Seal it now, so that it does not go into a later
        // sibling's part.
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
        let mut part = TreePart::default();

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
                Meta::Directory { mtime, mode } => {
                    let node = part
                        .push_directory(parent, name, mtime, mode)
                        .change_context(WalkError)?;
                    nodes.truncate(depth + 1);
                    nodes.push(node);
                }
                Meta::File { size, mtime, mode } => part
                    .push_file(parent, name, size, mtime, mode)
                    .change_context(WalkError)?,
                Meta::Symlink { mtime, directory } => part
                    .push_symlink(parent, name, target, mtime, directory)
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
    fn skip_named(&mut self, name: &str, reason: SkipReason) {
        let parent_len = self.path.len();
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(name);
        self.skip(reason);
        self.path.truncate(parent_len);
    }

    fn recorded(&self, kind: EntryKind, size: u64) {
        if let Some(progress) = self.options.progress {
            progress.recorded(kind, size);
        }
    }
}

// A length or a depth as the `u32` a row stores.
#[inline]
fn narrow(value: usize) -> Result<u32, Report<WalkError>> {
    match u32::try_from(value) {
        Ok(value) => Ok(value),
        Err(error) => Err(Report::new(error).change_context(WalkError)),
    }
}
