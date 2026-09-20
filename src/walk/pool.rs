// Scanning ahead of the walk. Worker threads scan the directories the walk
// will enter next, in walk order, and the walk takes each result when it
// gets there. What the walk produces does not depend on the workers: they
// decide only how long it waits.
//
// 1. A worker takes the waiting job that comes first in walk order, scans it,
//    and puts the jobs of its subdirectories back. Those sort directly after
//    their parent, so the workers run down the tree just ahead of the walk.
// 2. The results that wait for the walk are limited in bytes. A worker whose
//    next job would pass the limit waits, unless the walk is waiting for that
//    very job.
// 3. The walk asks for the result of the directory it enters and waits until
//    it is there.
// 4. A scan of a few entries is too little work for a trip through the lock.
//    A worker therefore goes on into the subdirectories it found, in walk
//    order, until it has read `BUNDLE` entries, and hands the scans over
//    together.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use super::reader::Scratch;
use super::scan::{Job, Scanned, Scanner};

// The entries a worker reads before it hands its scans over.
const BUNDLE: usize = 256;

// Scans that follow one another in walk order, each with its key. The walk
// asks for the first.
type Bundle = Vec<(Box<[u32]>, Scanned)>;

fn held_bytes(bundle: &Bundle) -> u64 {
    bundle.iter().map(|(_, scanned)| scanned.held_bytes()).sum()
}

#[derive(Default)]
struct State {
    // By `Job::key`, so the first is the one the walk needs soonest.
    waiting: BTreeMap<Box<[u32]>, Job>,
    // By the key of each bundle's first scan.
    results: HashMap<Box<[u32]>, Bundle>,
    // The bytes of `results`.
    held_bytes: u64,
    // The key of the directory the walk is waiting for, if it is.
    wanted: Option<Box<[u32]>>,
    // Workers waiting for a job.
    idle: usize,
    // Workers inside a scan that are not waiting for a handle.
    scanning: usize,
    closed: bool,
    // A worker panicked, so a result the walk waits for may never come.
    broken: bool,
}

pub(super) struct Pool<'a> {
    scanner: &'a Scanner<'a>,
    // The most bytes of results that wait for the walk.
    window: u64,
    state: Mutex<State>,
    // Signalled when a worker may have something to do.
    work: Condvar,
    // Signalled when the walk has its result.
    ready: Condvar,
}

impl<'a> Pool<'a> {
    pub fn new(scanner: &'a Scanner<'a>, window: u64) -> Self {
        Self {
            scanner,
            window,
            state: Mutex::default(),
            work: Condvar::new(),
            ready: Condvar::new(),
        }
    }

    // A panic elsewhere must not turn into a second one here, least of all
    // in a destructor, so a poisoned lock is taken as it is.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'s>(condvar: &Condvar, state: MutexGuard<'s, State>) -> MutexGuard<'s, State> {
        condvar.wait(state).unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds the first job of a walk.
    pub fn submit(&self, job: Job) {
        self.lock().waiting.insert(job.key.clone(), job);
        self.work.notify_one();
    }

    /// Waits for the scan with `key`, and returns it with the scans that
    /// follow it in the same bundle.
    ///
    /// # Panics
    ///
    /// Panics if a worker panicked.
    pub fn take(&self, key: &[u32]) -> Bundle {
        let mut state = self.lock();
        loop {
            if let Some(scanned) = state.results.remove(key) {
                let was_full = state.held_bytes >= self.window;
                state.held_bytes -= held_bytes(&scanned);
                state.wanted = None;
                let wake = was_full && state.idle > 0;
                drop(state);
                if wake {
                    self.work.notify_all();
                }
                return scanned;
            }
            if state.broken {
                drop(state);
                panic!("a thread of the walk panicked");
            }
            // A full window lets a worker through only for the wanted job, so
            // the workers have to hear that there is one.
            if state.wanted.is_none() {
                state.wanted = Some(key.into());
                if state.idle > 0 {
                    self.work.notify_all();
                }
            }
            state = Self::wait(&self.ready, state);
        }
    }

    /// Stops the workers. Each returns once its current scan is done.
    pub fn close(&self) {
        self.lock().closed = true;
        self.work.notify_all();
    }

    /// Runs one worker until the pool is closed.
    pub fn work(&self) {
        let mut scratch = Scratch::default();
        while let Some(first) = self.next_job() {
            let broken = BrokenOnPanic(self);
            let first_key = first.key.clone();
            let mut bundle = Bundle::new();
            // The jobs this worker will scan itself, the next one last, and
            // the ones it leaves to the pool.
            let mut mine = vec![first];
            let mut others = Vec::new();
            let mut read = 0;
            while let Some(job) = mine.pop() {
                let key = job.key.clone();
                // With no handle left, this worker first closes the directories
                // that its own jobs hold.
                let found = self.scanner.scan(job, &mut scratch, &mut || {
                    let mut released = false;
                    for waiting in mine.iter_mut().chain(others.iter_mut()) {
                        released |= waiting.release(&self.scanner.held);
                    }
                    released || self.release()
                });
                read += found.scanned.items.len();
                bundle.push((key, found.scanned));
                // The scans of the rest of a directory are for other workers,
                // and they come before anything else this worker would do.
                let done = read >= BUNDLE || !found.rest.is_empty();
                others.extend(found.rest);
                if done {
                    others.extend(found.directories);
                    break;
                }
                mine.extend(found.directories.into_iter().rev());
            }
            others.extend(mine);
            std::mem::forget(broken);

            let mut state = self.lock();
            let added = others.len();
            for job in others {
                state.waiting.insert(job.key.clone(), job);
            }
            state.held_bytes += held_bytes(&bundle);
            let wanted = state.wanted.as_deref() == Some(&*first_key);
            state.results.insert(first_key, bundle);
            state.scanning -= 1;
            // This worker takes one of the new jobs itself.
            let wake = added.saturating_sub(1).min(state.idle);
            drop(state);
            if wanted {
                self.ready.notify_one();
            }
            for _ in 0..wake {
                self.work.notify_one();
            }
        }
    }

    // Waits for a job this worker may take, and takes it.
    fn next_job(&self) -> Option<Job> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            let room = state.held_bytes < self.window;
            let first = state.waiting.first_key_value().map(|(key, _)| key);
            if first.is_some_and(|first| room || state.wanted.as_ref() == Some(first)) {
                let (_, job) = state.waiting.pop_first().expect("a first job");
                state.scanning += 1;
                return Some(job);
            }
            state.idle += 1;
            state = Self::wait(&self.work, state);
            state.idle -= 1;
        }
    }

    // Closes the directories that waiting jobs hold, for a scan that has no
    // handle left. With none to close, it waits for another scan to end,
    // since a scan holds handles of its own until then.
    fn release(&self) -> bool {
        let mut state = self.lock();
        let mut released = false;
        for job in state.waiting.values_mut() {
            released |= job.release(&self.scanner.held);
        }
        if released {
            return true;
        }
        if state.scanning <= 1 || state.closed {
            return false;
        }
        // Not counted while it waits, so that two waiting scans do not wait
        // for each other.
        state.scanning -= 1;
        state.idle += 1;
        state = Self::wait(&self.work, state);
        state.idle -= 1;
        state.scanning += 1;
        true
    }
}

// Tells the walk that a scan panicked, so that it does not wait for a result
// that will not come. Forgotten when the scan returns.
struct BrokenOnPanic<'p, 'a>(&'p Pool<'a>);

impl Drop for BrokenOnPanic<'_, '_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.broken = true;
        state.closed = true;
        drop(state);
        self.0.ready.notify_all();
        self.0.work.notify_all();
    }
}
