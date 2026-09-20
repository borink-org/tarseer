// Scanning: everything the walk reads about one directory, read at once. A
// scan lists the directory, asks the filter about each entry, and reads the
// metadata of those it keeps. It touches nothing but its own directory, so
// scans of different directories can run on different threads.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::reader::{self, Directory, Kind, Listed, Scratch};
use super::{Candidate, Filter, Listing, OnError, SkipReason, estimate};
use crate::manifest::{EntryKind, Timestamp};

/// The most open directories that waiting jobs may hold between them.
const MAX_HELD: usize = 128;

/// The most entries one scan reads. A directory with more is read by several
/// scans, which can run on different threads. The walk never holds the
/// metadata of more than this many of its entries at once.
pub(super) const CHUNK: usize = 1024;

/// A scan waiting to be made.
pub(super) struct Job {
    /// Where the scan's first entry is in the tree. From the root down, each
    /// level adds one number: twice the entry's place in its listing, plus
    /// one for a subdirectory. Keys compare in the order the walk needs the
    /// scans, and a comparison reads a few integers where one of paths would
    /// read every byte.
    pub key: Box<[u32]>,
    pub work: Work,
}

pub(super) enum Work {
    /// Open and list a directory, and read its first [`CHUNK`] entries.
    Directory {
        /// The path relative to the root. Empty for the root.
        path: String,
        /// The directory, if the scan of its parent could open it and the
        /// limit on held directories allowed. Otherwise this scan opens it by
        /// its path.
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
            key: Box::default(),
            work: Work::Directory {
                path: String::new(),
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

    /// The path of the directory, for a scan that opens one.
    pub fn path(&self) -> Option<&str> {
        match &self.work {
            Work::Directory { path, .. } => Some(path),
            Work::Rest { .. } => None,
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
    key: Box<[u32]>,
    path: String,
    directory: Directory,
    listing: reader::Listing,
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
struct Finding {
    scanned: Reading,
    /// One job for each directory among the items, in walk order.
    pub directories: Vec<Job>,
    /// The scans of the rest of the directory, in walk order.
    pub rest: Vec<Job>,
}

/// A range of [`Scanned::text`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Span {
    start: u32,
    len: u32,
}

/// One thing a scan found, in walk order.
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
    /// `place` is where the listing has it, which its job's key is made of.
    Directory {
        name: Span,
        place: usize,
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
}

impl Rows {
    pub fn text(&self, span: Span) -> &str {
        &self.text[span.start as usize..(span.start + span.len) as usize]
    }
}

/// What a scan found in one directory.
#[derive(Default)]
pub(super) struct Scanned {
    pub rows: Arc<Rows>,
    /// The estimate of the rows, as the walk adds it up, and their number.
    pub bytes: u64,
    pub count: usize,
    pub errors: Vec<Option<io::Error>>,
    /// Set when the directory could not be opened or listed. It then holds no
    /// items.
    pub unreadable: Option<io::Error>,
    /// Where in the listing the next scan of this directory starts, if this
    /// scan did not reach the end.
    pub next: Option<usize>,
}

impl Scanned {
    /// Returns `true` if the directory was read and every item is a row, so
    /// that the walk has nothing to count or report for it.
    pub fn is_clean(&self) -> bool {
        self.unreadable.is_none() && self.count == self.rows.items.len()
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
        let too_large = || io::Error::other("the directory's names are larger than 4 GiB");
        let start = u32::try_from(self.text.len()).map_err(|_| too_large())?;
        let len = u32::try_from(text.len()).map_err(|_| too_large())?;
        start.checked_add(len).ok_or_else(too_large)?;
        self.text.push_str(text);
        Ok(Span { start, len })
    }

    // Records a symlink, given what reading its target returned.
    fn symlink(
        &mut self,
        name: Span,
        target: io::Result<Option<String>>,
        mtime: Option<Timestamp>,
        directory: Option<bool>,
    ) -> io::Result<()> {
        let target = match target {
            Ok(Some(target)) => target,
            Ok(None) => {
                self.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::NonUtf8,
                });
                return Ok(());
            }
            Err(error) => {
                self.failed(Some(name), "target", error);
                return Ok(());
            }
        };
        let target = if target.contains('\\') {
            target.replace('\\', "/")
        } else {
            target
        };
        let target = self.span(&target)?;
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

impl Finding {
    fn finish(self) -> Found {
        let Reading {
            text,
            items,
            bytes,
            count,
            errors,
            unreadable,
            next,
        } = self.scanned;
        Found {
            scanned: Scanned {
                rows: Arc::new(Rows { text, items }),
                bytes,
                count,
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
    pub on_error: OnError,
    pub held: Held,
}

impl Scanner<'_> {
    /// Makes the scan of `job`.
    ///
    /// `release` is called when the process has no handle left to open the
    /// directory with. It closes directories that waiting jobs hold and
    /// returns `true`, or returns `false` if there was nothing to close.
    pub fn scan(
        &self,
        job: Job,
        scratch: &mut Scratch,
        release: &mut dyn FnMut() -> bool,
    ) -> Found {
        let mut found = Finding {
            scanned: Reading::default(),
            directories: Vec::new(),
            rest: Vec::new(),
        };
        let (path, opened, refused) = match job.work {
            Work::Rest { listed, start } => {
                self.entries(&listed, start, &mut found);
                return found.finish();
            }
            Work::Directory {
                path,
                opened,
                refused,
            } => (path, opened, refused),
        };
        if let Some(error) = refused {
            found.scanned.unreadable = Some(error);
            return found.finish();
        }
        let directory = match opened {
            Some(directory) => {
                if Directory::HOLDS_A_HANDLE {
                    self.held.let_go();
                }
                Ok(directory)
            }
            None => self.open(&path, release),
        };
        let listing = directory.and_then(|directory| Ok((directory.list(scratch)?, directory)));
        let (mut listing, directory) = match listing {
            Ok(listed) => listed,
            Err(error) => {
                found.scanned.unreadable = Some(error);
                return found.finish();
            }
        };

        for (error, name, what) in std::mem::take(&mut listing.failures) {
            let name = name.and_then(|name| found.scanned.span(&name).ok());
            found.scanned.failed(name, what, error);
            if self.on_error == OnError::Fail {
                return found.finish();
            }
        }
        let listed = ListedDirectory {
            key: job.key,
            path,
            directory,
            listing,
        };
        if listed.listing.entries.len() <= CHUNK {
            self.entries(&listed, 0, &mut found);
            return found.finish();
        }
        let listed = Arc::new(listed);
        for start in (CHUNK..listed.listing.entries.len()).step_by(CHUNK) {
            found.rest.push(Job {
                key: key_below(&listed.key, start, false),
                work: Work::Rest {
                    listed: Arc::clone(&listed),
                    start,
                },
            });
        }
        self.entries(&listed, 0, &mut found);
        found.finish()
    }

    // Reads up to `CHUNK` entries of `listed`, from `start`.
    fn entries(&self, listed: &ListedDirectory, start: usize, found: &mut Finding) {
        let end = listed.listing.entries.len().min(start + CHUNK);
        if end < listed.listing.entries.len() {
            found.scanned.next = Some(end);
        }
        for place in start..end {
            let failed = found.scanned.errors.len();
            if let Err(error) = self.entry(listed, place, found) {
                found.scanned.failed(None, "listing", error);
            }
            // Under `Fail` the walk stops at the first failure, so nothing
            // after it is read.
            if self.on_error == OnError::Fail && found.scanned.errors.len() > failed {
                break;
            }
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
        within: &'l ListedDirectory,
        listed: &Listed,
        scanned: &mut Reading,
    ) -> io::Result<Option<Offered<'l>>> {
        let ListedDirectory {
            path: parent,
            directory,
            listing,
            ..
        } = within;
        let parent = parent.as_str();
        let raw = listed.name(&listing.names);
        let name = match std::str::from_utf8(raw) {
            Ok(name) if listed.kind != Kind::NonUtf8 => name,
            Ok(lossy) => {
                let name = scanned.span(lossy)?;
                scanned.items.push(Item::Skipped {
                    name,
                    reason: SkipReason::NonUtf8,
                });
                return Ok(None);
            }
            Err(_) => {
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
                    names: &listing.names,
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
    fn entry(&self, within: &ListedDirectory, place: usize, found: &mut Finding) -> io::Result<()> {
        let ListedDirectory {
            key,
            path: parent,
            directory,
            listing,
        } = within;
        let listed: &Listed = &listing.entries[place];
        let scanned = &mut found.scanned;
        let jobs = &mut found.directories;
        let parent = parent.as_str();
        let Some(Offered {
            name,
            kind,
            listed_kind,
            stat,
        }) = self.offered(within, listed, scanned)?
        else {
            return Ok(());
        };
        let name_span = scanned.span(name)?;

        let (opened, refused) = if kind == EntryKind::Directory {
            self.open_ahead(directory, listed, listing)
        } else {
            (None, None)
        };
        let stat = match (stat, &opened) {
            (Some(stat), _) => Ok(stat),
            (None, Some(child)) if Directory::STATS_ITSELF => child.stat_self(),
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
                let target = directory.read_link(listed, listing);
                scanned.symlink(name_span, target, stat.mtime, is_directory)?;
            }
            Kind::Directory => {
                let mut path = String::with_capacity(parent.len() + 1 + name.len());
                if !parent.is_empty() {
                    path.push_str(parent);
                    path.push('/');
                }
                path.push_str(name);
                jobs.push(Job {
                    key: key_below(key, place, true),
                    work: Work::Directory {
                        path,
                        opened,
                        refused,
                    },
                });
                scanned.row(EntryKind::Directory, name, "");
                scanned.items.push(Item::Directory {
                    name: name_span,
                    place,
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

/// The key of a scan that starts at `place` in the listing of the directory
/// with `key`. That is the scan of the subdirectory there, or the scan that
/// reads the directory's own entries from there on. The second comes first,
/// since it is the one that finds the subdirectory.
pub(super) fn key_below(key: &[u32], place: usize, subdirectory: bool) -> Box<[u32]> {
    // A listing holds fewer than 2^31 entries: its names fit in 4 GiB.
    let place = u32::try_from(place).expect("a place in a listing fits 31 bits");
    let mut below = Vec::with_capacity(key.len() + 1);
    below.extend_from_slice(key);
    below.push(place * 2 + u32::from(subdirectory));
    below.into_boxed_slice()
}

// Lowers the descriptor limit of the whole test process, so it is the only
// unit test of this crate that opens files.
#[cfg(all(test, target_os = "linux", not(tarseer_portable_reader)))]
mod tests {
    use std::fs::{self, File};

    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    use super::{Held, Job, OnError, Scanner, Scratch};

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

        let scanner = Scanner {
            root: &root,
            filter: None,
            on_error: OnError::Fail,
            held: Held::new(),
        };
        let mut asked = 0;
        let found = scanner.scan(Job::root(), &mut Scratch::default(), &mut || {
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
