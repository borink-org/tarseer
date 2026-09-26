// Scanning: everything the walk reads about one directory, read at once. A
// scan lists the directory, asks the filter about each entry, and reads the
// metadata of those it keeps. It touches nothing but its own directory, so
// scans of different directories can run on different threads. It also counts
// what it skipped and tells the progress receiver what it found.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::reader::{self, Directory, Kind, Listed, Scratch};
use super::{
    Candidate, Filter, Listing, Metadata, OnError, Progress, SkipReason, Skips, WalkOptions,
    estimate,
};
use crate::manifest::{EntryKind, Timestamp};

/// The most open directories that waiting jobs may hold between them.
pub(super) const MAX_HELD: usize = 128;

/// The most entries one scan reads. A directory with more is read by several
/// scans, which can run on different threads. The walk never holds the
/// metadata of more than this many of its entries at once.
pub(super) const CHUNK: usize = 1024;

/// The most subdirectories one scan opens. A scan opens each directory it
/// finds, and a directory with more is read by several scans, so that other
/// threads open the rest: opening a directory whose record is not cached
/// waits for storage, and one thread would open them one at a time.
pub(super) const DIRECTORIES: usize = 64;

// Where the scan of `listing` that starts at `start` ends: after `CHUNK`
// entries or `DIRECTORIES` directories, whichever comes first.
fn piece_end(listing: &reader::Listing, start: usize) -> usize {
    let mut directories = 0;
    let limit = listing.entries.len().min(start + CHUNK);
    for (place, entry) in listing.entries[start..limit].iter().enumerate() {
        if entry.kind == Kind::Directory {
            directories += 1;
            if directories == DIRECTORIES {
                return start + place + 1;
            }
        }
    }
    limit
}

/// A scan waiting to be made.
///
/// A job holds neither the path of its directory nor its place in the walk.
/// Whoever makes the scan knows both, and a walk would otherwise allocate
/// twice for every directory it finds.
pub(super) struct Job {
    pub work: Work,
}

pub(super) enum Work {
    /// Open and list a directory, and read its first [`CHUNK`] entries.
    Directory {
        /// The directory, if the scan of its parent could open it and the
        /// limit on held directories allowed. Otherwise this scan opens it by
        /// the path it is given.
        opened: Option<Directory>,
        /// Why the directory could not be opened, if its parent's scan found
        /// out.
        refused: Option<io::Error>,
    },
    /// Read the next [`CHUNK`] entries, from `start`, of a directory that an
    /// earlier scan listed.
    Rest {
        listed: Arc<ListedDirectory>,
        start: usize,
    },
}

impl Job {
    /// The job of the root.
    pub fn root() -> Self {
        Self {
            work: Work::Directory {
                opened: None,
                refused: None,
            },
        }
    }

    /// Where in its directory's listing this scan starts.
    pub fn start(&self) -> usize {
        match &self.work {
            Work::Directory { .. } => 0,
            Work::Rest { start, .. } => *start,
        }
    }

    /// Closes the directory this job holds for its scan, if it holds one that
    /// the scan can open again. Returns `true` if it did.
    pub fn release(&mut self, held: &Held) -> bool {
        match &mut self.work {
            Work::Directory { opened, .. } if Directory::HOLDS_A_HANDLE => {
                let released = opened.take().is_some();
                if released {
                    held.let_go();
                }
                released
            }
            _ => false,
        }
    }
}

/// A directory with more than [`CHUNK`] entries, shared by its scans.
pub(super) struct ListedDirectory {
    path: String,
    directory: Directory,
    listing: reader::Listing,
}

// A directory that is being read: its path relative to the root, which is
// empty for the root, and its listing.
struct Within<'a> {
    path: &'a str,
    directory: &'a Directory,
    listing: &'a reader::Listing,
    // Where the listing's names are in the text of the rows, if the scan
    // copied them there all at once. A name's span is then known without a
    // copy of its own.
    names_at: Option<u32>,
}

impl ListedDirectory {
    fn within(&self) -> Within<'_> {
        Within {
            path: &self.path,
            directory: &self.directory,
            listing: &self.listing,
            // A scan of a part of a large directory copies only its names.
            names_at: None,
        }
    }
}

/// What one scan returns.
pub(super) struct Found {
    pub scanned: Scanned,
    /// One job for each directory among the items, in walk order.
    pub directories: Vec<Job>,
    /// The scans of the rest of the directory, in walk order.
    pub rest: Vec<Job>,
}

// A scan's result while it is being made.
#[derive(Default)]
pub(super) struct Finding {
    scanned: Reading,
    /// One job for each directory among the items, in walk order.
    pub directories: Vec<Job>,
    /// The scans of the rest of the directory, in walk order.
    pub rest: Vec<Job>,
    // Where the buffers of the rows go when nothing needs them any more.
    spares: Option<Arc<Spares>>,
}

/// A range of [`Scanned::text`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Span {
    start: u32,
    len: u32,
}

impl Span {
    /// The length of the text, in bytes.
    pub const fn len(self) -> usize {
        self.len as usize
    }
}

/// One thing a scan found, in walk order.
#[derive(Clone, Copy)]
pub(super) enum Item {
    File {
        name: Span,
        size: u64,
        mtime: Option<Timestamp>,
        mode: u32,
    },
    Symlink {
        name: Span,
        target: Span,
        mtime: Option<Timestamp>,
        directory: Option<bool>,
    },
    /// The nth directory of a scan belongs to the nth job the scan returned.
    Directory {
        name: Span,
        mtime: Option<Timestamp>,
        mode: u32,
    },
    /// An entry the walk does not record. Its name is lossy if it was not
    /// UTF-8.
    Skipped { name: Span, reason: SkipReason },
    /// An entry that could not be read: its name if that is known, what could
    /// not be read, and the error's place in [`Scanned::errors`].
    Failed {
        name: Option<Span>,
        what: &'static str,
        error: u32,
    },
}

impl Item {
    /// Returns `true` if the walk records this item as a row.
    pub fn is_row(&self) -> bool {
        matches!(
            self,
            Self::File { .. } | Self::Symlink { .. } | Self::Directory { .. }
        )
    }
}

/// The rows a scan found, which parts are later built from. Several parts can
/// share them, so they do not change once the scan is done.
#[derive(Default)]
pub(super) struct Rows {
    pub text: String,
    pub items: Vec<Item>,
    spares: Option<Arc<Spares>>,
}

impl Drop for Rows {
    fn drop(&mut self) {
        if let Some(spares) = &self.spares {
            spares.keep(
                std::mem::take(&mut self.text),
                std::mem::take(&mut self.items),
            );
        }
    }
}

/// The buffers of rows that nothing needs any more, for the next read to fill.
/// A read's buffers grow up to a block, and a read that fills one goes on in
/// the next; kept buffers come back at their size, so they rarely grow again.
/// Growing copies what they hold, and an allocator may copy it into memory it
/// has not touched yet; a block allocated whole would cost a read of a few
/// rows as much as a large one. At most [`Spares::KEPT`] are kept.
#[derive(Default)]
pub(super) struct Spares(Mutex<Vec<(String, Vec<Item>)>>);

impl Spares {
    const KEPT: usize = 16;

    fn keep(&self, mut text: String, mut items: Vec<Item>) {
        text.clear();
        items.clear();
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.len() < Self::KEPT {
            kept.push((text, items));
        }
    }
}

impl Rows {
    pub fn text(&self, span: Span) -> &str {
        text_of(&self.text, span)
    }
}

/// The part of `text` that `span` names.
pub(super) fn text_of(text: &str, span: Span) -> &str {
    &text[span.start as usize..(span.start + span.len) as usize]
}

/// What a scan found in one directory.
pub(super) struct Scanned {
    pub rows: Arc<Rows>,
    /// Which of the rows' items are this scan's. The scans of a subtree that
    /// one thread read share their rows, so that a directory costs no
    /// allocation of its own. An item is named by its place in the rows.
    pub items: std::ops::Range<usize>,
    pub errors: Vec<Option<io::Error>>,
    /// Set when the directory could not be opened or listed. It then holds no
    /// items.
    pub unreadable: Option<io::Error>,
    /// Where in the listing the next scan of this directory starts, if this
    /// scan did not reach the end.
    pub next: Option<usize>,
}

impl Default for Scanned {
    fn default() -> Self {
        Self {
            rows: Arc::default(),
            items: 0..0,
            errors: Vec::new(),
            unreadable: None,
            next: None,
        }
    }
}

// A scan while it is being made.
#[derive(Default)]
struct Reading {
    text: String,
    items: Vec<Item>,
    bytes: u64,
    count: usize,
    errors: Vec<Option<io::Error>>,
    unreadable: Option<io::Error>,
    next: Option<usize>,
}

impl Reading {
    fn span(&mut self, text: &str) -> io::Result<Span> {
        let start = self.text.len();
        self.text.push_str(text);
        self.span_since(start)
    }

    // The span of what was appended to the text since `start`. If the text
    // would pass 4 GiB, it is cut back to `start` and this is an error.
    fn span_since(&mut self, start: usize) -> io::Result<Span> {
        let (Ok(from), Ok(to)) = (u32::try_from(start), u32::try_from(self.text.len())) else {
            self.text.truncate(start);
            return Err(io::Error::other(
                "the directory's names are larger than 4 GiB",
            ));
        };
        Ok(Span {
            start: from,
            len: to - from,
        })
    }

    // Records a symlink. `read` appends its target to the text, and returns
    // `false` if the target is not UTF-8.
    fn symlink(
        &mut self,
        name: Span,
        read: impl FnOnce(&mut String) -> io::Result<bool>,
        mtime: Option<Timestamp>,
        directory: Option<bool>,
    ) -> io::Result<()> {
        let start = self.text.len();
        match read(&mut self.text) {
            Ok(true) => {}
            Ok(false) => {
                self.text.truncate(start);
                self.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::NonUtf8,
                });
                return Ok(());
            }
            Err(error) => {
                self.text.truncate(start);
                self.failed(Some(name), "target", error);
                return Ok(());
            }
        }
        let target = self.span_since(start)?;
        let (name_len, target_len) = (name.len as usize, target.len as usize);
        self.bytes += estimate(EntryKind::Symlink, "", "") + (name_len + target_len) as u64;
        self.count += 1;
        self.items.push(Item::Symlink {
            name,
            target,
            mtime,
            directory,
        });
        Ok(())
    }

    // Counts a row that is about to be pushed.
    fn row(&mut self, kind: EntryKind, name: &str, target: &str) {
        self.bytes += estimate(kind, name, target);
        self.count += 1;
    }

    fn failed(&mut self, name: Option<Span>, what: &'static str, error: io::Error) {
        let slot = u32::try_from(self.errors.len()).expect("fewer errors than entries");
        self.errors.push(Some(error));
        self.items.push(Item::Failed {
            name,
            what,
            error: slot,
        });
    }
}

// An entry the filter kept: its name, its kind as the walk records it and as
// the listing gave it, and its metadata if that had to be read already.
struct Offered<'l> {
    name: &'l str,
    kind: EntryKind,
    listed_kind: Kind,
    stat: Option<reader::Stat>,
}

/// What one scan added to a [`Finding`] that takes several.
#[allow(dead_code)]
pub(super) struct Added {
    /// The scan's items, by their places in the rows.
    pub items: std::ops::Range<usize>,
    pub bytes: u64,
    pub count: usize,
    /// Whether the scan read its directory to the end and found nothing but
    /// rows.
    pub plain: bool,
}

impl Finding {
    fn finish(self) -> Found {
        let Reading {
            text,
            items,
            errors,
            unreadable,
            next,
            ..
        } = self.scanned;
        Found {
            scanned: Scanned {
                items: 0..items.len(),
                rows: Arc::new(Rows {
                    text,
                    items,
                    spares: self.spares,
                }),
                errors,
                unreadable,
                next,
            },
            directories: self.directories,
            rest: self.rest,
        }
    }
}

/// How many open directories the waiting jobs hold, and how many they may.
pub(super) struct Held {
    count: AtomicUsize,
    limit: AtomicUsize,
}

impl Held {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            limit: AtomicUsize::new(MAX_HELD),
        }
    }

    fn try_hold(&self) -> bool {
        self.count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < self.limit.load(Ordering::Relaxed)).then_some(count + 1)
            })
            .is_ok()
    }

    /// Records that a job gave its directory up, to a scan or to a release.
    pub fn let_go(&self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }

    // The process has fewer handles to spare than the limit assumed.
    fn ran_out(&self) {
        let count = self.count.load(Ordering::Relaxed);
        self.limit.fetch_min(count, Ordering::Relaxed);
    }
}

/// What every scan of one walk shares.
pub(super) struct Scanner<'a> {
    pub root: &'a Path,
    pub filter: Option<&'a dyn Filter>,
    pub progress: Option<&'a dyn Progress>,
    pub on_error: OnError,
    pub metadata: Metadata,
    pub held: Held,
    // By `SkipReason`, in the order of `Skips`' fields.
    skips: [AtomicU32; 4],
}

impl<'a> Scanner<'a> {
    pub fn new(root: &'a Path, options: &WalkOptions<'a>) -> Self {
        Self {
            root,
            filter: options.filter,
            progress: options.progress,
            on_error: options.on_error,
            metadata: options.metadata,
            held: Held::new(),
            skips: Default::default(),
        }
    }

    /// What the scans skipped so far.
    pub fn skips(&self) -> Skips {
        let count = |reason: usize| self.skips[reason].load(Ordering::Relaxed);
        Skips {
            special: count(0),
            non_utf8: count(1),
            unreadable: count(2),
            failed: count(3),
        }
    }

    /// Makes the scan of `job`, whose directory is at `path`, relative to the
    /// root.
    ///
    /// `release` is called when the process has no handle left to open the
    /// directory with. It closes directories that waiting jobs hold and
    /// returns `true`, or returns `false` if there was nothing to close.
    pub fn scan(
        &self,
        job: Job,
        path: &str,
        scratch: &mut Scratch,
        release: &mut dyn FnMut() -> bool,
    ) -> Found {
        let mut found = Finding::default();
        self.scan_into(job, path, scratch, release, &mut found);
        found.finish()
    }

    /// Makes the scan of `job` and adds what it finds to `found`, which may
    /// hold other scans already. The jobs of the subdirectories are added to
    /// [`Finding::directories`].
    pub fn scan_into(
        &self,
        job: Job,
        path: &str,
        scratch: &mut Scratch,
        release: &mut dyn FnMut() -> bool,
        found: &mut Finding,
    ) -> Added {
        let (items, bytes, count) = (
            found.scanned.items.len(),
            found.scanned.bytes,
            found.scanned.count,
        );
        let unreadable = found.scanned.unreadable.is_some();
        if job.start() == 0
            && let Some(progress) = self.progress
        {
            progress.entered(path);
        }
        self.fill(job, path, scratch, release, found);
        let scanned = &found.scanned;
        let added = items..scanned.items.len();
        let clean = scanned.count - count == added.len();
        if !clean || self.progress.is_some() || (!unreadable && scanned.unreadable.is_some()) {
            self.report(path, scanned, added.clone(), !unreadable);
        }
        Added {
            plain: found.rest.is_empty()
                && scanned.unreadable.is_none()
                && scanned.next.is_none()
                && scanned.count - count == added.len(),
            items: added,
            bytes: scanned.bytes - bytes,
            count: scanned.count - count,
        }
    }

    // Counts what a scan skipped, and tells the progress receiver of what it
    // found. `fresh` is `false` if the scan found `unreadable` set already.
    fn report(&self, path: &str, scanned: &Reading, items: std::ops::Range<usize>, fresh: bool) {
        // The root is never skipped: an empty walk would look like a success.
        if fresh
            && scanned.unreadable.is_some()
            && self.on_error == OnError::Skip
            && !path.is_empty()
        {
            self.skip(path, SkipReason::Unreadable);
        }
        for item in &scanned.items[items] {
            match *item {
                Item::Skipped { name, reason } => {
                    self.skip(&joined(path, text_of(&scanned.text, name)), reason);
                }
                Item::Failed { name, .. } if self.on_error == OnError::Skip => match name {
                    Some(name) => self.skip(
                        &joined(path, text_of(&scanned.text, name)),
                        SkipReason::Failed,
                    ),
                    None => self.skip(path, SkipReason::Failed),
                },
                Item::Failed { .. } => {}
                Item::File { size, .. } => self.recorded(EntryKind::File, size),
                Item::Symlink { .. } => self.recorded(EntryKind::Symlink, 0),
                Item::Directory { .. } => self.recorded(EntryKind::Directory, 0),
            }
        }
    }

    fn skip(&self, path: &str, reason: SkipReason) {
        let slot = match reason {
            SkipReason::Special => 0,
            SkipReason::NonUtf8 => 1,
            SkipReason::Unreadable => 2,
            SkipReason::Failed => 3,
        };
        self.skips[slot].fetch_add(1, Ordering::Relaxed);
        if let Some(progress) = self.progress {
            progress.skipped(path, reason);
        }
    }

    fn recorded(&self, kind: EntryKind, size: u64) {
        if let Some(progress) = self.progress {
            progress.recorded(kind, size);
        }
    }

    fn fill(
        &self,
        job: Job,
        path: &str,
        scratch: &mut Scratch,
        release: &mut dyn FnMut() -> bool,
        found: &mut Finding,
    ) {
        let (opened, refused) = match job.work {
            Work::Rest { listed, start } => {
                let entries = listed.listing.entries.len() - start;
                found.scanned.items.reserve(entries.min(CHUNK));
                self.entries(&listed.within(), start, found);
                return;
            }
            Work::Directory { opened, refused } => (opened, refused),
        };
        if let Some(error) = refused {
            found.scanned.unreadable = Some(error);
            return;
        }
        let directory = match opened {
            Some(directory) => {
                if Directory::HOLDS_A_HANDLE {
                    self.held.let_go();
                }
                Ok(directory)
            }
            None => self.open(path, release),
        };
        let listing = directory.and_then(|mut directory| Ok((directory.list(scratch)?, directory)));
        let (mut listing, directory) = match listing {
            Ok(listed) => listed,
            Err(error) => {
                found.scanned.unreadable = Some(error);
                return;
            }
        };

        for (error, name, what) in std::mem::take(&mut listing.failures) {
            let name = name.and_then(|name| found.scanned.span(&name).ok());
            found.scanned.failed(name, what, error);
            if self.on_error == OnError::Fail {
                return;
            }
        }
        // One allocation for the names and one for the items, where growing
        // them entry by entry would take several. A link's target comes on top.
        let entries = listing.entries.len();
        found.scanned.items.reserve(entries.min(CHUNK));
        if piece_end(&listing, 0) == entries {
            // One copy for all the names, where there was one for each.
            let names_at = listing.text().and_then(|text| {
                let at = u32::try_from(found.scanned.text.len()).ok()?;
                at.checked_add(u32::try_from(text.len()).ok()?)?;
                found.scanned.text.push_str(text);
                Some(at)
            });
            if names_at.is_none() {
                found.scanned.text.reserve(listing.names().len());
            }
            let within = Within {
                path,
                directory: &directory,
                listing: &listing,
                names_at,
            };
            self.entries(&within, 0, found);
            // The listing's buffers serve the next directory.
            scratch.spare = listing.into_spare();
            return;
        }
        // The scans of the rest need the path too, and outlive this call.
        let listed = Arc::new(ListedDirectory {
            path: path.to_owned(),
            directory,
            listing,
        });
        let mut start = piece_end(&listed.listing, 0);
        while start < listed.listing.entries.len() {
            found.rest.push(Job {
                work: Work::Rest {
                    listed: Arc::clone(&listed),
                    start,
                },
            });
            start = piece_end(&listed.listing, start);
        }
        self.entries(&listed.within(), 0, found);
    }

    // Reads the entries of `listed` from `start` to the end of that piece.
    fn entries(&self, listed: &Within<'_>, start: usize, found: &mut Finding) {
        let end = piece_end(listed.listing, start);
        if end < listed.listing.entries.len() {
            found.scanned.next = Some(end);
        }
        // With no filter and a validated, copied name block, regular files
        // need only their metadata. Decide this once for the scan, rather
        // than construct an Offered value and copy its optional Stat per row.
        let files_at = listed.names_at.filter(|_| self.filter.is_none());
        for place in start..end {
            let failed = found.scanned.errors.len();
            let entry = &listed.listing.entries[place];
            let result = if let Some(at) = files_at.filter(|_| entry.kind == Kind::File) {
                self.file(listed, entry, at, &mut found.scanned);
                Ok(())
            } else {
                self.entry(listed, place, found)
            };
            if let Err(error) = result {
                found.scanned.failed(None, "listing", error);
            }
            // Under `Fail` the walk stops at the first failure, so nothing
            // after it is read.
            if self.on_error == OnError::Fail && found.scanned.errors.len() > failed {
                break;
            }
        }
    }

    #[inline]
    fn file(&self, within: &Within<'_>, listed: &Listed, at: u32, scanned: &mut Reading) {
        let (offset, len) = listed.span();
        let name = Span {
            start: at + offset,
            len,
        };
        let stat = match self.metadata {
            Metadata::Full => within.directory.stat(listed, within.listing),
            Metadata::Kinds => Ok(reader::Stat::unread(Kind::File)),
        };
        match stat {
            Ok(stat) => {
                scanned.items.push(Item::File {
                    name,
                    size: stat.size,
                    mtime: stat.mtime,
                    mode: stat.mode,
                });
                scanned.bytes += estimate(EntryKind::File, "", "") + u64::from(len);
                scanned.count += 1;
            }
            Err(error) => scanned.failed(Some(name), "metadata", error),
        }
    }

    // Opens a directory by its path, for a job that holds none.
    fn open(&self, path: &str, release: &mut dyn FnMut() -> bool) -> io::Result<Directory> {
        let path = self.root.join(path);
        loop {
            match Directory::open_root(&path) {
                Err(error) if Directory::out_of_handles(&error) => {
                    self.held.ran_out();
                    if !release() {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }

    // Finds out what `listed` is and asks the filter about it. Returns its
    // name, its kind, and its metadata if that had to be read already. Returns
    // `None` if the entry is dealt with: skipped, failed or refused.
    fn offered<'l>(
        &self,
        within: &Within<'l>,
        listed: &Listed,
        scanned: &mut Reading,
    ) -> io::Result<Option<Offered<'l>>> {
        let Within {
            path: parent,
            directory,
            listing,
            ..
        } = *within;
        let raw = listed.name(listing.names());
        let name = match listing.name_text(listed) {
            Some(name) if listed.kind != Kind::NonUtf8 => name,
            Some(lossy) => {
                let name = scanned.span(lossy)?;
                scanned.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::NonUtf8,
                });
                return Ok(None);
            }
            None => {
                let name = scanned.span(&String::from_utf8_lossy(raw))?;
                scanned.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::NonUtf8,
                });
                return Ok(None);
            }
        };

        // A listing without kinds costs a read of the metadata before the
        // filter is asked.
        let mut stat = None;
        let mut listed_kind = listed.kind;
        if listed_kind == Kind::Unknown {
            match directory.stat(listed, listing) {
                Ok(found) => {
                    listed_kind = found.kind;
                    stat = Some(found);
                }
                Err(error) => {
                    let name = scanned.span(name)?;
                    scanned.failed(Some(name), "kind", error);
                    return Ok(None);
                }
            }
        }
        let kind = match listed_kind {
            Kind::Symlink { .. } => EntryKind::Symlink,
            Kind::Directory => EntryKind::Directory,
            Kind::File => EntryKind::File,
            Kind::Special | Kind::Unknown | Kind::NonUtf8 => {
                let name = scanned.span(name)?;
                scanned.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::Special,
                });
                return Ok(None);
            }
        };
        if let Some(filter) = self.filter {
            let candidate = Candidate {
                parent,
                name,
                kind,
                listing: Listing {
                    entries: &listing.entries,
                    names: listing.names(),
                },
            };
            if !filter.keep(&candidate) {
                return Ok(None);
            }
        }
        Ok(Some(Offered {
            name,
            kind,
            listed_kind,
            stat,
        }))
    }

    // A directory is opened before its metadata is read, because an open
    // directory can report its own. Its job then takes it along. Returns the
    // directory if it was opened, or why it cannot be.
    fn open_ahead(
        &self,
        directory: &Directory,
        listed: &Listed,
        listing: &reader::Listing,
    ) -> (Option<Directory>, Option<io::Error>) {
        if Directory::HOLDS_A_HANDLE && !self.held.try_hold() {
            return (None, None);
        }
        match directory.open_child(listed, listing) {
            Ok(child) => (Some(child), None),
            Err(error) => {
                if Directory::HOLDS_A_HANDLE {
                    self.held.let_go();
                }
                if Directory::out_of_handles(&error) {
                    self.held.ran_out();
                    (None, None)
                } else {
                    (None, Some(error))
                }
            }
        }
    }

    // Reads one entry into `scanned`. The error it returns is one that leaves
    // the scan unable to say which entry failed.
    fn entry(&self, within: &Within<'_>, place: usize, found: &mut Finding) -> io::Result<()> {
        let Within {
            directory, listing, ..
        } = *within;
        let listed: &Listed = &listing.entries[place];
        let scanned = &mut found.scanned;
        let jobs = &mut found.directories;
        let Some(Offered {
            name,
            kind,
            listed_kind,
            stat,
        }) = self.offered(within, listed, scanned)?
        else {
            return Ok(());
        };
        let name_span = match within.names_at {
            Some(at) => {
                let (start, len) = listed.span();
                Span {
                    start: at + start,
                    len,
                }
            }
            None => scanned.span(name)?,
        };

        let (opened, refused) = if kind == EntryKind::Directory {
            self.open_ahead(directory, listed, listing)
        } else {
            (None, None)
        };
        let stat = match (stat, &opened) {
            (Some(stat), _) => Ok(stat),
            (None, _) if self.metadata == Metadata::Kinds => Ok(reader::Stat::unread(listed_kind)),
            (None, Some(child)) if Directory::STATS_ITSELF => child.stat_self(),
            // Not opened ahead, because too many are held: still from the
            // directory itself, since where the listing's copy is old, which
            // copy a directory got would depend on the threads.
            (None, None) if Directory::STATS_ITSELF && kind == EntryKind::Directory => {
                directory.stat_name(name)
            }
            _ => directory.stat(listed, listing),
        };
        let stat = match stat {
            Ok(stat) => stat,
            Err(error) => {
                if opened.is_some() && Directory::HOLDS_A_HANDLE {
                    self.held.let_go();
                }
                scanned.failed(Some(name_span), "metadata", error);
                return Ok(());
            }
        };

        match listed_kind {
            Kind::Symlink {
                directory: is_directory,
            } => {
                scanned.symlink(
                    name_span,
                    |into| directory.read_link(listed, listing, into),
                    stat.mtime,
                    is_directory,
                )?;
            }
            Kind::Directory => {
                jobs.push(Job {
                    work: Work::Directory { opened, refused },
                });
                scanned.row(EntryKind::Directory, name, "");
                scanned.items.push(Item::Directory {
                    name: name_span,
                    mtime: stat.mtime,
                    mode: stat.mode,
                });
            }
            _ => {
                scanned.row(EntryKind::File, name, "");
                scanned.items.push(Item::File {
                    name: name_span,
                    size: stat.size,
                    mtime: stat.mtime,
                    mode: stat.mode,
                });
            }
        }
        Ok(())
    }
}

/// `name` inside `directory`, a path relative to the root.
pub(super) fn joined(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        return name.to_owned();
    }
    format!("{directory}/{name}")
}

// Lowers the descriptor limit of the whole test process, which no other unit
// test opens files in.
#[cfg(all(test, target_os = "linux", not(tarseer_portable_reader)))]
mod tests {
    use std::fs::{self, File};

    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    use super::{Job, Scanner, Scratch, WalkOptions};

    #[test]
    fn a_scan_with_no_descriptor_left_asks_for_a_release_and_goes_on() {
        let root = std::env::temp_dir().join(format!("tarseer-scan-{}", std::process::id()));
        fs::create_dir_all(root.join("inner")).unwrap();
        fs::write(root.join("file"), b"x").unwrap();

        let limit = getrlimit(Resource::Nofile);
        let lowered = Rlimit {
            current: Some(32),
            maximum: limit.maximum,
        };
        setrlimit(Resource::Nofile, lowered).unwrap();
        let mut taken = Vec::new();
        while let Ok(file) = File::open("/dev/null") {
            taken.push(file);
        }

        let scanner = Scanner::new(&root, &WalkOptions::default());
        let mut asked = 0;
        let found = scanner.scan(Job::root(), "", &mut Scratch::default(), &mut || {
            asked += 1;
            taken.pop().is_some()
        });
        drop(taken);
        setrlimit(Resource::Nofile, limit).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(asked, 1);
        assert!(found.scanned.unreadable.is_none());
        assert_eq!(found.scanned.rows.items.len(), 2);
        // The one descriptor that was released went to the directory itself,
        // so its subdirectory waits for a scan that opens it by its path.
        assert_eq!(found.directories.len(), 1);
    }
}
