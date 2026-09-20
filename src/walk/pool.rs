// The worker threads of a walk. They do two kinds of work, and what the walk
// produces does not depend on either: they decide only how long it waits.
//
// Scanning ahead:
//
// 1. A worker takes the waiting job that comes first in walk order, scans it,
//    and puts the jobs of its subdirectories back. Those sort directly after
//    their parent, so the workers run down the tree just ahead of the walk.
// 2. The scans that wait for the walk are limited in bytes. A worker whose
//    next job would pass the limit waits, unless the walk is waiting for that
//    very job.
// 3. The walk asks for the scan of the directory it enters and waits until it
//    is there.
// 4. A scan of a few entries is too little work for a trip through the lock.
//    A worker therefore goes on into the subdirectories it found, in walk
//    order, until it has read `BUNDLE` entries, and hands the scans over
//    together.
//
// Building parts: the walk decides which rows make up a part and hands over a
// plan. A worker builds the part, which comes before scanning, since it frees
// the rows and feeds whatever takes the parts. The walk takes the parts back
// in the order it planned them.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use error_stack::Report;

use super::assemble::Plan;
use super::reader::Scratch;
use super::scan::{Job, Scanned, Scanner};
use crate::manifest::{TreePart, TreePartFull};

// The entries a worker reads before it hands its scans over.
const BUNDLE: usize = 2048;

/// Scans that follow one another in walk order, each with its key. The walk
/// asks for the first.
pub(super) struct Bundle {
    pub scans: Vec<(Box<[u32]>, Scanned)>,
    /// Set when the scans are one whole subtree, every directory in it was
    /// read, and every item is a row. The walk can then take the subtree as
    /// one, by these two totals, and look at none of its rows.
    pub whole: Option<Whole>,
}

/// The rows of a whole subtree below its root: their estimate and number.
#[derive(Clone, Copy)]
pub(super) struct Whole {
    pub bytes: u64,
    pub count: usize,
}

type Built = Result<TreePart, Report<TreePartFull>>;

fn held_bytes(bundle: &Bundle) -> u64 {
    bundle
        .scans
        .iter()
        .map(|(_, scanned)| scanned.held_bytes())
        .sum()
}

/// What the walk was waiting for.
pub(super) enum Got {
    Scans(Bundle),
    Part(Built),
}

enum Work {
    Scan(Job),
    Build(usize, Plan),
}

#[derive(Default)]
struct State {
    // By `Job::key`, so the first is the one the walk needs soonest.
    waiting: BTreeMap<Box<[u32]>, Job>,
    // By the key of each bundle's first scan.
    scans: HashMap<Box<[u32]>, Bundle>,
    // The bytes of `scans`.
    held_bytes: u64,
    // The key of the scan the walk is waiting for, if it is.
    wanted: Option<Box<[u32]>>,
    // The plans no worker has taken yet, and the parts that were built, each
    // by its number.
    plans: VecDeque<(usize, Plan)>,
    parts: BTreeMap<usize, Built>,
    // Workers waiting for work.
    idle: usize,
    // Workers inside a scan that are not waiting for a handle.
    scanning: usize,
    closed: bool,
    // A worker panicked, so what the walk waits for may never come.
    broken: bool,
}

pub(super) struct Pool<'a> {
    scanner: &'a Scanner<'a>,
    // The most bytes of scans that wait for the walk.
    window: u64,
    state: Mutex<State>,
    // Signalled when a worker may have something to do.
    work: Condvar,
    // Signalled when the walk may have what it waits for.
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

    /// Hands over the plan of part `number` for a worker to build.
    pub fn plan(&self, number: usize, plan: Plan) {
        let mut state = self.lock();
        state.plans.push_back((number, plan));
        let wake = state.idle > 0;
        drop(state);
        if wake {
            self.work.notify_one();
        }
    }

    /// Returns part `number` if it is built.
    pub fn part_if_built(&self, number: usize) -> Option<Built> {
        self.lock().parts.remove(&number)
    }

    /// Waits for part `part`, or for the scan with `key`, whichever is there
    /// first. Either may be `None`, for a walk that has no use for it now. A
    /// scan comes with the scans that follow it in the same bundle.
    ///
    /// # Panics
    ///
    /// Panics if a worker panicked.
    pub fn wait_for(&self, part: Option<usize>, key: Option<&[u32]>) -> Got {
        let mut state = self.lock();
        loop {
            if let Some(built) = part.and_then(|number| state.parts.remove(&number)) {
                return Got::Part(built);
            }
            if let Some(scans) = key.and_then(|key| state.scans.remove(key)) {
                let was_full = state.held_bytes >= self.window;
                state.held_bytes -= held_bytes(&scans);
                state.wanted = None;
                let wake = was_full && state.idle > 0;
                drop(state);
                if wake {
                    self.work.notify_all();
                }
                return Got::Scans(scans);
            }
            if state.broken {
                drop(state);
                panic!("a thread of the walk panicked");
            }
            // A full window lets a worker through only for the wanted job, so
            // the workers have to hear that there is one.
            if let (None, Some(key)) = (&state.wanted, key) {
                state.wanted = Some(key.into());
                if state.idle > 0 {
                    self.work.notify_all();
                }
            }
            state = Self::wait(&self.ready, state);
        }
    }

    /// Stops the workers. Each returns once its current work is done.
    pub fn close(&self) {
        self.lock().closed = true;
        self.work.notify_all();
    }

    /// Runs one worker until the pool is closed.
    pub fn work(&self) {
        let mut scratch = Scratch::default();
        while let Some(work) = self.next_work() {
            let broken = BrokenOnPanic(self);
            match work {
                Work::Build(number, plan) => {
                    let built = plan.assemble();
                    // The rows go before the lock is taken.
                    drop(plan);
                    std::mem::forget(broken);
                    self.lock().parts.insert(number, built);
                    self.ready.notify_one();
                }
                Work::Scan(first) => {
                    let (first_key, bundle, others) = self.scan_from(first, &mut scratch);
                    std::mem::forget(broken);
                    self.hand_over(first_key, bundle, others);
                }
            }
        }
    }

    // Scans `first`, and then the subdirectories it finds, in walk order, until
    // `BUNDLE` entries are read. Returns the scans and the jobs left over.
    fn scan_from(&self, first: Job, scratch: &mut Scratch) -> (Box<[u32]>, Bundle, Vec<Job>) {
        let first_key = first.key.clone();
        let mut scans = Vec::new();
        // The jobs this worker will scan itself, the next one last, and the
        // ones it leaves to the pool.
        let mut mine = vec![first];
        let mut others = Vec::new();
        let mut read = 0;
        while let Some(job) = mine.pop() {
            let key = job.key.clone();
            // With no handle left, this worker first closes the directories
            // that its own jobs hold.
            let found = self.scanner.scan(job, scratch, &mut || {
                let mut released = false;
                for waiting in mine.iter_mut().chain(others.iter_mut()) {
                    released |= waiting.release(&self.scanner.held);
                }
                released || self.release()
            });
            read += found.scanned.rows.items.len();
            scans.push((key, found.scanned));
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
        // With no job left over, the scans are all of the first one's subtree.
        let whole = (others.is_empty() && scans.iter().all(|(_, scanned)| scanned.is_clean()))
            .then(|| Whole {
                bytes: scans.iter().map(|(_, scanned)| scanned.bytes).sum(),
                count: scans.iter().map(|(_, scanned)| scanned.count).sum(),
            });
        (first_key, Bundle { scans, whole }, others)
    }

    fn hand_over(&self, first_key: Box<[u32]>, bundle: Bundle, others: Vec<Job>) {
        let mut state = self.lock();
        let added = others.len();
        for job in others {
            state.waiting.insert(job.key.clone(), job);
        }
        state.held_bytes += held_bytes(&bundle);
        let wanted = state.wanted.as_deref() == Some(&*first_key);
        state.scans.insert(first_key, bundle);
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

    // Waits for work this worker may take, and takes it.
    fn next_work(&self) -> Option<Work> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some((number, plan)) = state.plans.pop_front() {
                return Some(Work::Build(number, plan));
            }
            let room = state.held_bytes < self.window;
            let first = state.waiting.first_key_value().map(|(key, _)| key);
            if first.is_some_and(|first| room || state.wanted.as_ref() == Some(first)) {
                let (_, job) = state.waiting.pop_first().expect("a first job");
                state.scanning += 1;
                return Some(Work::Scan(job));
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

// Tells the walk that a worker panicked, so that it does not wait for what
// will not come. Forgotten when the work returns.
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
