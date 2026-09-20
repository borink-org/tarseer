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
//! # Threads
//!
//! Reading a directory, which is listing it and reading the metadata of its
//! entries, is most of a walk. With [`WalkOptions::threads`] above 0, that many
//! threads read directories ahead of the walk, in walk order. The walk itself
//! stays on the calling thread, takes what they read, and cuts the same parts
//! as it does without them. A directory of more than 1,024 entries is read in
//! pieces, which several threads read at once.
//!
//! # Memory
//!
//! The walk holds the rows that are not yet in a sealed part, and their
//! estimates add up to less than about two budgets. It also holds entries it
//! has read and not yet visited. Those are at most 1,024 for each directory
//! between the root and the entry being visited. A directory larger than that
//! costs the names of its whole listing while the walk is inside it.
//!
//! Threads add what they have read ahead of the walk, which is limited to one
//! more budget.
//!
//! On Linux the walk keeps some of the directories it is about to read open,
//! at most 128. When the process runs out of file descriptors, the walk closes
//! them. It then opens each by its path when it reads it.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use error_stack::{Report, ResultExt as _};

use crate::manifest::{EntryKind, TreePart};

mod assemble;
mod pool;
mod reader;
mod scan;

use self::assemble::{Piece, Plan, Segment, Subtree};
use self::pool::{Got, Pool, Whole};
use self::reader::{Kind, Listed, Scratch};
use self::scan::{Found, Held, Item, Job, Rows, Scanned, Scanner, key_below};

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
/// no kinds: there the walk reads the metadata first, to learn the kind. A
/// refused directory is not listed, so nothing under it is offered or counted.
/// The filter changes where parts are cut, in the same way that the tree does.
///
/// With [`WalkOptions::threads`] above 0, the calls come from those threads,
/// several at once, and in no particular order between directories.
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
    /// The number of threads that read directories ahead of the walk. With 0,
    /// the walk reads each directory itself when it enters it. The parts are
    /// the same either way.
    pub threads: usize,
}

impl Default for WalkOptions<'_> {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            filter: None,
            progress: None,
            cancel: None,
            on_error: OnError::Fail,
            threads: 0,
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
    let scanner = Scanner {
        root,
        filter: options.filter,
        on_error: options.on_error,
        held: Held::new(),
    };
    if options.threads == 0 {
        let mut walker = Walker::new(options, sink, &scanner, Source::Inline(Scratch::default()));
        walker.run(root, Some(Job::root()))?;
        return Ok(walker.skips);
    }

    // The scans that wait for the walk are limited to one budget for each
    // thread, so that the budget and the threads bound the walk's memory.
    let threads = u64::try_from(options.threads).unwrap_or(u64::MAX);
    let pool = Pool::new(&scanner, options.budget.saturating_mul(threads));
    std::thread::scope(|scope| {
        // Closed on every way out, so that the scope can join the workers.
        let _closed = CloseOnDrop(&pool);
        for _ in 0..options.threads {
            scope.spawn(|| pool.work());
        }
        pool.submit(Job::root());
        let ahead = Source::Ahead {
            pool: &pool,
            taken: VecDeque::new(),
            whole: None,
        };
        let mut walker = Walker::new(options, sink, &scanner, ahead);
        walker.run(root, None)?;
        Ok(walker.skips)
    })
}

struct CloseOnDrop<'p, 'a>(&'p Pool<'a>);

impl Drop for CloseOnDrop<'_, '_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

// Where the walk gets the scan of a directory it enters.
enum Source<'p> {
    // It scans the directory itself.
    Inline(Scratch),
    // Worker threads scan ahead of it. `taken` holds the scans that came along
    // with one it asked for, in the order it will need them.
    // `whole` is set while the first scans of `taken` are a whole subtree,
    // with how many of them.
    Ahead {
        pool: &'p Pool<'p>,
        taken: VecDeque<(Box<[u32]>, Scanned)>,
        whole: Option<(Whole, usize)>,
    },
}

// An open directory. Row positions are absolute: the first row of the walk is
// at 0.
struct Level {
    scanned: Scanned,
    next: usize,
    // The jobs of the subdirectories in `scanned` that the walk has not
    // entered yet, and the scans of the rest of this directory. Both are empty
    // when worker threads scan ahead, since they hold them.
    jobs: std::vec::IntoIter<Job>,
    rest: std::vec::IntoIter<Job>,
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
    // The rows that are not in a part yet, in walk order. The walk keeps where
    // they are and never copies them.
    waiting: VecDeque<Piece>,
    // The position of the first waiting row, and of the next row to come.
    base: usize,
    position: usize,
    // With worker threads, the parts that were planned and the parts the sink
    // has been given. A worker builds each part between the two.
    planned: usize,
    delivered: usize,
    // Running estimate of every row so far.
    total: u64,
    path: String,
    stack: Vec<Level>,
    // Index of the outermost measuring level; `stack.len()` if none.
    measuring: usize,
    skips: Skips,
    // The key of the directory `path` names. See `Job::key`.
    key: Vec<u32>,
    scanner: &'s Scanner<'s>,
    source: Source<'s>,
}

impl<'o, 's> Walker<'o, 's> {
    fn new(
        options: &'o WalkOptions<'o>,
        sink: &'s mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
        scanner: &'s Scanner<'s>,
        source: Source<'s>,
    ) -> Self {
        Self {
            options,
            sink,
            waiting: VecDeque::new(),
            base: 0,
            position: 0,
            planned: 0,
            delivered: 0,
            total: 0,
            path: String::new(),
            stack: Vec::new(),
            measuring: 1,
            skips: Skips::default(),
            key: Vec::new(),
            scanner,
            source,
        }
    }

    // `first` is the root's job when the walk scans for itself. Worker threads
    // were given it otherwise.
    fn run(&mut self, root: &Path, first: Option<Job>) -> Result<(), Report<WalkError>> {
        if let Some(progress) = self.options.progress {
            progress.entered("");
        }
        let Found {
            mut scanned,
            directories,
            rest,
        } = self.scan(first)?;
        // An error under either policy: counting the root as a skip would
        // report an empty walk as a success.
        if let Some(error) = scanned.unreadable.take() {
            return Err(error)
                .attach_with(|| format!("listing {}", root.display()))
                .change_context(WalkError);
        }
        self.stack.push(Level {
            scanned,
            next: 0,
            jobs: directories.into_iter(),
            rest: rest.into_iter(),
            path_len: 0,
            row: 0,
            start_total: 0,
            split: true,
            group_first: 0,
            group_bytes: 0,
            completed: Vec::new(),
        });

        while let Some(top) = self.stack.last_mut() {
            if top.next == top.scanned.rows.items.len() {
                match top.scanned.next {
                    Some(start) => self.read_on(start)?,
                    None => self.finish_level()?,
                }
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
            // The scan is moved out while its item is handled, so that
            // `visit` can borrow the walker mutably.
            let level = self.stack.len() - 1;
            let mut scanned = std::mem::take(&mut self.stack[level].scanned);
            let visited = self.visit(level, &mut scanned, index);
            self.stack[level].scanned = scanned;
            visited?;
        }
        self.deliver(true)
    }

    // Gives the sink the parts that workers have built, in the order they were
    // planned. With `all` it waits for every part. Otherwise it waits only
    // while more parts are planned than the workers can be building.
    fn deliver(&mut self, all: bool) -> Result<(), Report<WalkError>> {
        let Source::Ahead { pool, .. } = &self.source else {
            return Ok(());
        };
        while self.delivered < self.planned {
            let behind = self.planned - self.delivered;
            let built = if all || behind > self.options.threads + 1 {
                match pool.wait_for(Some(self.delivered), None) {
                    Got::Part(built) => built,
                    Got::Scans(_) => unreachable!("no scan was asked for"),
                }
            } else {
                match pool.part_if_built(self.delivered) {
                    Some(built) => built,
                    None => return Ok(()),
                }
            };
            self.delivered += 1;
            (self.sink)(built.change_context(WalkError)?)?;
        }
        Ok(())
    }

    // With worker threads, waits until the scan of the directory with
    // `self.key` is the first of `taken`. While it waits, the walk gives the
    // sink the parts the workers have built.
    fn fetch(&mut self) -> Result<(), Report<WalkError>> {
        let Self {
            source,
            sink,
            key,
            planned,
            delivered,
            ..
        } = self;
        let Source::Ahead { pool, taken, whole } = source else {
            return Ok(());
        };
        while taken.front().is_none_or(|(first, _)| **first != **key) {
            debug_assert!(taken.is_empty(), "scans are taken in walk order");
            let part = (*delivered < *planned).then_some(*delivered);
            match pool.wait_for(part, Some(key)) {
                Got::Scans(bundle) => {
                    *whole = bundle.whole.map(|totals| (totals, bundle.scans.len()));
                    taken.extend(bundle.scans);
                }
                Got::Part(built) => {
                    *delivered += 1;
                    sink(built.change_context(WalkError)?)?;
                }
            }
        }
        Ok(())
    }

    // Returns the scan of the directory `self.path` names. `job` is its job
    // when the walk scans for itself.
    fn scan(&mut self, job: Option<Job>) -> Result<Found, Report<WalkError>> {
        self.fetch()?;
        let Self {
            source,
            scanner,
            stack,
            ..
        } = self;
        match source {
            Source::Ahead { taken, whole, .. } => {
                // The walk goes into this subtree, so it is not taken as one.
                *whole = None;
                let (_, scanned) = taken.pop_front().expect("the scan that was fetched");
                Ok(Found {
                    scanned,
                    directories: Vec::new(),
                    rest: Vec::new(),
                })
            }
            Source::Inline(scratch) => {
                let job = job.expect("the walk holds the jobs when it scans for itself");
                let scanner = *scanner;
                // With no handle left, close the directories that the jobs of
                // the open levels hold.
                Ok(scanner.scan(job, scratch, &mut || {
                    let mut released = false;
                    for waiting in stack.iter_mut().flat_map(|level| level.jobs.as_mut_slice()) {
                        released |= waiting.release(&scanner.held);
                    }
                    released
                }))
            }
        }
    }

    // Takes the subtree of the directory with `self.key` as one, if worker
    // threads scanned all of it and it fits a part together with the
    // directory's own row, which is `row_bytes`. A subtree that fits is never
    // split, so only its totals matter to where parts are cut.
    fn take_whole(
        &mut self,
        row_bytes: u64,
    ) -> Result<Option<(Whole, Vec<Scanned>)>, Report<WalkError>> {
        // Progress hears of every entry, which a subtree taken as one skips.
        if self.options.progress.is_some() {
            return Ok(None);
        }
        self.fetch()?;
        let Source::Ahead { taken, whole, .. } = &mut self.source else {
            return Ok(None);
        };
        let Some((totals, scans)) = *whole else {
            return Ok(None);
        };
        if row_bytes + totals.bytes > self.options.budget {
            return Ok(None);
        }
        *whole = None;
        let scans = taken.drain(..scans).map(|(_, scanned)| scanned).collect();
        Ok(Some((totals, scans)))
    }

    // Handles one item from start to finish. Splitting it would pass the
    // same walker state through several more functions.
    #[allow(clippy::too_many_lines)]
    fn visit(
        &mut self,
        level: usize,
        scanned: &mut Scanned,
        index: usize,
    ) -> Result<(), Report<WalkError>> {
        let parent_len = self.stack[level].path_len;
        self.path.truncate(parent_len);

        let rows = &scanned.rows;
        let (name, kind) = match rows.items[index] {
            Item::Skipped { name, reason } => {
                self.skip_named(rows.text(name), reason);
                return Ok(());
            }
            Item::Failed { name, what, error } => {
                let error = scanned.errors[error as usize]
                    .take()
                    .expect("each error is reported once");
                return match name {
                    Some(name) => self.failed_at(error, rows.text(name), what),
                    None => self.failed(error, what),
                };
            }
            Item::File { name, .. } => (name, EntryKind::File),
            Item::Symlink { name, .. } => (name, EntryKind::Symlink),
            Item::Directory { name, .. } => (name, EntryKind::Directory),
        };
        let name = rows.text(name);
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(name);
        self.extend(level, rows, index);

        match rows.items[index] {
            Item::Symlink { target, .. } => {
                self.push_leaf(estimate(kind, name, rows.text(target)))?;
                self.recorded(kind, 0);
            }
            Item::File { size, .. } => {
                self.push_leaf(estimate(kind, name, ""))?;
                self.recorded(kind, size);
            }
            Item::Directory { place, .. } => {
                let bytes = estimate(kind, name, "");
                let row = self.position;
                self.position += 1;
                self.recorded(kind, 0);
                if let Some(progress) = self.options.progress {
                    progress.entered(&self.path);
                }
                let job = self.stack[level].jobs.next();
                self.key = key_below(&self.key[..level], place, true).into_vec();
                if let Some((totals, scans)) = self.take_whole(bytes)? {
                    // What `finish_level` does for a directory that was not
                    // split, without the level.
                    self.position += totals.count;
                    self.total += bytes + totals.bytes;
                    // An empty subtree has no rows to build, and a piece
                    // without rows would give the next part its stem.
                    if totals.count > 0 {
                        self.waiting.push_back(Piece::Subtree(Subtree {
                            scans,
                            depth: level + 1,
                            count: totals.count,
                        }));
                    }
                    let subtree = bytes + totals.bytes;
                    if self.stack[level].split {
                        self.add_to_group(level, row, subtree)?;
                    } else {
                        self.stack[level].completed.push((row, subtree));
                    }
                    return self.check_budget();
                }
                let Found {
                    mut scanned,
                    directories,
                    rest,
                } = self.scan(job)?;
                if let Some(error) = scanned.unreadable.take() {
                    self.unreadable(error)?;
                }
                self.stack.push(Level {
                    scanned,
                    next: 0,
                    jobs: directories.into_iter(),
                    rest: rest.into_iter(),
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
            Item::Skipped { .. } | Item::Failed { .. } => unreachable!("handled above"),
        }
        Ok(())
    }

    // Adds item `index` of `rows`, an entry of the directory at `depth`, to the
    // waiting rows.
    fn extend(&mut self, depth: usize, rows: &Arc<Rows>, index: usize) {
        // The last segment goes on if nothing came between: the entries of a
        // subdirectory would have started a segment of their own.
        if let Some(Piece::Rows(last)) = self.waiting.back_mut()
            && Arc::ptr_eq(&last.rows, rows)
        {
            last.to = index + 1;
            last.count += 1;
            return;
        }
        self.waiting.push_back(Piece::Rows(Segment {
            rows: Arc::clone(rows),
            from: index,
            to: index + 1,
            depth,
            count: 1,
        }));
    }

    // Replaces the scan of the top level, which the walk has used up, with
    // the scan of the same directory from `start` on.
    fn read_on(&mut self, start: usize) -> Result<(), Report<WalkError>> {
        let level = self.stack.len() - 1;
        self.path.truncate(self.stack[level].path_len);
        self.key = key_below(&self.key[..level], start, false).into_vec();
        let job = self.stack[level].rest.next();
        let found = self.scan(job)?;
        let top = &mut self.stack[level];
        top.scanned = found.scanned;
        top.next = 0;
        top.jobs = found.directories.into_iter();
        Ok(())
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
    fn push_leaf(&mut self, bytes: u64) -> Result<(), Report<WalkError>> {
        let position = self.position;
        self.position += 1;
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
        let end = self.position;
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
        // The stem is the open directories above the first row.
        let first = self.waiting.front().expect("a row for every position");
        let stem = (1..=first.depth())
            .map(|level| {
                let start = match self.stack[level - 1].path_len {
                    0 => 0,
                    parent_len => parent_len + 1,
                };
                self.path[start..self.stack[level].path_len].to_owned()
            })
            .collect();

        let mut pieces = Vec::new();
        let mut left = count;
        while left > 0 {
            let front = self.waiting.front_mut().expect("a row for every position");
            if front.count() <= left {
                left -= front.count();
                pieces.extend(self.waiting.pop_front());
            } else {
                let Piece::Rows(segment) = front else {
                    unreachable!("a part is never cut inside a subtree that fits one");
                };
                pieces.push(Piece::Rows(segment.split_off_front(left)));
                left = 0;
            }
        }
        self.base = end;
        self.emit(Plan { stem, pieces })
    }

    // Has the part of `plan` built and given to the sink: here and now, or by
    // a worker thread and in its turn.
    fn emit(&mut self, plan: Plan) -> Result<(), Report<WalkError>> {
        match &self.source {
            Source::Inline(_) => (self.sink)(plan.assemble().change_context(WalkError)?),
            Source::Ahead { pool, .. } => {
                pool.plan(self.planned, plan);
                self.planned += 1;
                self.deliver(false)
            }
        }
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
