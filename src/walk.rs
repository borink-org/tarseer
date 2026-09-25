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
//! # Metadata
//!
//! By default the walk reads each entry's size, modification time and
//! permission bits. With [`Metadata::Kinds`] it reads only what listing a
//! directory gives, the names and kinds, and the targets of symlinks; that
//! saves a lookup of every entry. The parts are the same either way, and
//! [`read_metadata`] fills one in later. A consumer that opens each file, such
//! as a copy, can instead record a file's metadata from the open file with
//! `read_file_metadata` (Unix), which needs no second lookup.
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
//! estimates and cuts parts against [`WalkOptions::budget`].
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
//! With [`WalkOptions::threads`] above 0, that many threads read directories
//! ahead of the walk, and the parts are the same as without threads, row for
//! row.
//!
//! 1. The threads read directories several at once, first those that come
//!    first in walk order. A directory of more than 1,024 entries, or of more
//!    than 64 subdirectories, is read in pieces, which several threads read
//!    too, and open the subdirectories of.
//! 2. While every thread is busy, a thread goes on into the subdirectories it
//!    finds. A subtree that it reads to its end, and that fits a part, is taken
//!    in as one piece, without a look at its rows.
//! 3. The calling thread cuts the parts, as it does without threads, from what
//!    the threads have read, and gives them to the sink in walk order.
//! 4. The threads build the parts.
//!
//! On Linux the walk adds threads while its threads wait on storage, up to
//! [`WalkOptions::max_threads`]. Every 2 ms it looks at how much CPU time the
//! process used and whether it read from storage. It adds threads only while
//! directories are queued, no thread is idle, and either the process read
//! from storage without filling its processors or it used less than half of
//! them, as on a remote filesystem. A walk of a warm cache keeps its
//! processors busy and reads nothing, so it keeps the threads it started with.
//! When the walk has at least as many threads as the processors it may use,
//! each thread keeps to one processor.
//!
//! [`Progress`] hears of a directory's entries when the directory is read, and
//! with threads a [`Filter`] and a [`Progress`] are called from the threads,
//! several at once and in no particular order. A walk that fails reports the
//! first entry in walk order that it could not read.
//!
//! # Memory
//!
//! Without threads, the walk holds the rows that are not yet in a sealed
//! part. Their estimates add up to less than about two budgets. It also
//! holds entries it has read and not yet visited. Those are at most 1,024 for
//! each directory between the root and the entry being visited. A directory
//! larger than that costs the names of its whole listing while the walk is
//! inside it.
//!
//! With threads, the limit is on the rows that have been read and not yet
//! given to the sink: two budgets at 64 bytes a row, plus 2,048 rows for each
//! thread. A thread that is reading may pass it by what it reads before it
//! looks again. It is not a limit on bytes: names, targets, listings and
//! buffers take their own. At the limit the threads read only the directory
//! the walk waits for, and what lies below the directory whose size it is
//! still adding up, one budget more at most.
//!
//! On Linux the walk keeps some of the directories it is about to read open,
//! at most 128. When the process runs out of file descriptors, the walk closes
//! them. It then opens each by its path when it reads it.
//! With threads, the walk first makes the process's table of descriptors large
//! enough for those. The kernel would otherwise grow the table during the walk,
//! and with threads that stops every one of them for some milliseconds.

use std::fmt;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use error_stack::Report;

use crate::manifest::{EntryKind, TreePart};

mod assemble;
mod cut;
mod fill;
mod pool;
mod reader;
mod scan;

use self::cut::Walker;
#[cfg(unix)]
pub use self::fill::read_file_metadata;
pub use self::fill::read_metadata;
use self::pool::Pool;
use self::reader::{Kind, Listed};
use self::scan::Scanner;

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
///
/// Without threads the calls come in walk order.
/// With [`WalkOptions::threads`] above 0 they come from those threads, when
/// a directory is read, several at once and in no particular order.
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

/// What the walk reads of each entry besides its name and kind.
///
/// Where parts are cut depends on the kinds, names and link targets alone,
/// so both give the same parts, row for row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Metadata {
    /// Size, modification time and permission bits, one `statx` per entry
    /// on Linux.
    ///
    /// On Windows a file's size and times come from its directory's
    /// listing, which costs no call. NTFS keeps a copy of them in the entry
    /// of each of the file's names, and when the file is written through
    /// one name, the entries of its other names keep the old size and times
    /// until the file is opened through them. A file with several hard
    /// links, changed through another name, is then recorded as it was.
    /// Directories are read from the directory itself and are not affected.
    #[default]
    Full,
    /// Only what the listing of a directory says: the name and the kind, and
    /// the target of a symlink. Every row has size 0, no modification time
    /// and mode 0, except those of entries listed without a kind, whose
    /// metadata is read to find it. A walk that needs no metadata saves a
    /// lookup of every entry. [`read_metadata`] fills a part in later, on
    /// any thread, for example while its files are being copied.
    Kinds,
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
    /// What the walk reads of each entry. See [`Metadata`].
    pub metadata: Metadata,
    /// The number of threads that walk. With 0 the calling thread walks alone.
    /// The parts are the same either way.
    pub threads: usize,
    /// The most threads the walk runs when its threads wait on storage. It
    /// starts [`threads`](Self::threads) and adds more while those are
    /// blocked, a cold cache or a remote filesystem, and not while they keep
    /// their processors busy. 0 means four times `threads`. Linux only.
    pub max_threads: usize,
}

impl Default for WalkOptions<'_> {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            filter: None,
            progress: None,
            cancel: None,
            on_error: OnError::Fail,
            metadata: Metadata::Full,
            threads: 0,
            max_threads: 0,
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

/// Walks `root` and hands each part to `sink` as soon as it is complete,
/// in walk order, on the calling thread.
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
    let scanner = Scanner::new(root, options);
    if options.threads == 0 {
        Walker::new(options, &scanner, None, sink).run(root)?;
        return Ok(scanner.skips());
    }

    let most = most_threads(options);
    reserve_walk_handles(most);
    let pool = Pool::new(&scanner, options);
    std::thread::scope(|scope| {
        // Closed on every way out, so that the scope can join the workers.
        let _closed = CloseOnDrop(&pool);
        start_workers(scope, options.threads, most, &pool);
        Walker::new(options, &scanner, Some(&pool), sink).run(root)?;
        Ok(scanner.skips())
    })
}

/// The most threads a walk with threads may run: [`WalkOptions::max_threads`],
/// or four times [`WalkOptions::threads`].
pub(crate) fn most_threads(options: &WalkOptions<'_>) -> usize {
    if options.max_threads == 0 {
        options.threads.saturating_mul(4)
    } else {
        options.max_threads.max(options.threads)
    }
}

// Starts `threads` workers, and the thread that adds more up to `most`.
fn start_workers<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    threads: usize,
    most: usize,
    pool: &'scope Pool<'_>,
) {
    // Workers that have every processor to themselves each keep to one,
    // and find their caches as they left them, which walks of small
    // directories gain most from. On a machine it shares, a worker must be
    // free to move away from a busy processor, so fewer are not pinned.
    let processors = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let pinned = threads >= processors;
    for worker in 0..threads {
        scope.spawn(move || {
            if pinned {
                reader::pin(worker);
            }
            pool.work();
        });
    }
    if most > threads && reader::Usage::MEASURED {
        scope.spawn(move || add_workers_while_blocked(scope, pool, threads, most));
    }
}

// Adds workers while the ones there wait on storage. Scans must be queued
// with no worker idle, and either the process read from storage and its
// workers do not fill their processors, or it uses less than half of them,
// as on a remote filesystem that reads nothing from local storage. On a warm
// cache neither holds, and no worker is added.
fn add_workers_while_blocked<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    pool: &'scope Pool<'_>,
    mut workers: usize,
    most: usize,
) {
    const TICK: std::time::Duration = std::time::Duration::from_millis(2);
    let processors = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mut usage = reader::Usage::new();
    let mut before = (std::time::Instant::now(), usage.sample());
    while let Some(short) = pool.short_of_workers(TICK) {
        let now = (std::time::Instant::now(), usage.sample());
        let elapsed = now.0.duration_since(before.0).as_secs_f64();
        let busy = (now.1).0.saturating_sub((before.1).0).as_secs_f64() / elapsed;
        let read = (now.1)
            .1
            .zip((before.1).1)
            .is_some_and(|(now, was)| now > was);
        before = now;
        #[allow(clippy::cast_precision_loss)]
        let processors = workers.min(processors) as f64;
        let blocked = (read && busy < 0.9 * processors) || busy < 0.5 * processors;
        if short && blocked && workers < most {
            let add = (workers / 2).clamp(1, most - workers);
            for _ in 0..add {
                scope.spawn(|| pool.work());
            }
            workers += add;
        }
    }
}

// Reserve before starting any threads that will coexist with the walk,
// including encoders. Growing Linux's descriptor table with threads alive
// can wait for an RCU grace period.
pub(crate) fn reserve_walk_handles(threads: usize) {
    reader::reserve_handles(scan::MAX_HELD + 4 * threads + 64);
}

struct CloseOnDrop<'p, 'a>(&'p Pool<'a>);

impl Drop for CloseOnDrop<'_, '_> {
    fn drop(&mut self) {
        self.0.close();
    }
}
