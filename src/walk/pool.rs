// Workers that read directories ahead of the walk and build the parts it
// plans.
//
// A worker takes the queued directory that comes first in walk order. While
// every worker is busy it goes on into the subdirectories it finds, depth
// first, as the walk would. A subtree that it reads to its end, and that fits a
// part, becomes one `Unit::Whole`: the walk takes it in without visiting its
// rows. Where the worker stops, each directory it is inside of becomes a
// `Unit::Dir`, and the subdirectories it did not enter are queued for any
// worker. Every directory the walk will ask for has a `Slot`, which the worker
// that reads it fills.
//
// The rows that have been read and not yet given to the sink are limited. At
// the limit the workers read only the directory the walk waits for, and below
// the outermost directory the walk is still measuring, up to one budget more.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use error_stack::{Report, ResultExt as _};

use super::assemble::Plan;
use super::cut::{Next, Unit};
use super::reader::Scratch;
use super::scan::{Added, Finding, Item, Job, Rows, Scanned, Scanner, Spares, joined, text_of};
use super::{WalkError, WalkOptions, estimate};
use crate::manifest::{EntryKind, TreePart};

// The entries a worker reads, in subtrees that fit a part, before it hands
// over what it read.
const BUNDLE: usize = 2048;
// The most queued directories it takes on in that time.
const STEPS: usize = 64;
// How many rows one budget is taken to hold, for the limit on rows in flight.
const ROW_BYTES: u64 = 64;

type Built = Result<TreePart, Report<WalkError>>;

// A place in walk order. A directory's key has one number for each level
// below the root: `2p + 1` for the subdirectory at place `p` in its parent's
// listing. A later scan of a directory, from place `p` on, has the
// directory's key and `2p`, since it finds the subdirectories from there on.
// Keys compare as walk order does.
type Key = Arc<[u32]>;

fn key_below(key: &[u32], place: usize, subdirectory: bool) -> Key {
    // A listing holds fewer than 2^31 entries: its names fit in 4 GiB.
    let place = u32::try_from(place).expect("a place in a listing fits 31 bits");
    key.iter()
        .copied()
        .chain([place * 2 + u32::from(subdirectory)])
        .collect()
}

/// Where the unit of one queued directory, or of a later scan of one, ends up.
pub(super) struct Slot {
    key: Key,
    unit: Mutex<Option<Unit>>,
}

impl Slot {
    fn new(key: Key) -> Arc<Self> {
        Arc::new(Self {
            key,
            unit: Mutex::new(None),
        })
    }

    fn take(&self) -> Option<Unit> {
        self.unit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn fill(&self, unit: Unit) {
        *self.unit.lock().unwrap_or_else(PoisonError::into_inner) = Some(unit);
    }
}

// A queued scan, with the path of its directory and the estimate of the
// directory's own row: 0 for the root and for a later scan of a directory,
// which are never read as one unit.
struct Task {
    job: Job,
    path: String,
    row: u64,
    slot: Arc<Slot>,
}

/// What [`Pool::wait`] ends with.
pub(super) enum Event {
    Unit(Unit),
    Part(Built),
    // Neither came in a while.
    Nothing,
}

enum Work {
    Build(usize, Plan),
    Read(Task),
}

#[derive(Default)]
struct State {
    queue: BTreeMap<Key, Task>,
    plans: VecDeque<(usize, Plan)>,
    built: HashMap<usize, Built>,
    // The key of the slot the walk last waited for, and how much of it is
    // the key of the outermost directory the walk was measuring then.
    head: Option<(Key, usize)>,
    // While the walk sleeps: the slot it waits for, by its address, and the
    // part it gives the sink next.
    asleep: bool,
    wants_slot: usize,
    wants_part: usize,
    idle: usize,
    // Workers between taking a directory and handing over what they read, and
    // those of them that wait for a descriptor.
    reading: usize,
    short_of_handles: usize,
    closed: bool,
    broken: bool,
}

pub(super) struct Pool<'a> {
    scanner: &'a Scanner<'a>,
    budget: u64,
    // The most rows that are read and not yet given to the sink, and how many
    // more there may be below the directory the walk measures.
    window: usize,
    measured: usize,
    held: AtomicUsize,
    // How many workers wait for work, for a worker that does not hold the
    // lock. It may be a moment behind.
    idle: AtomicUsize,
    state: Mutex<State>,
    work: Condvar,
    ready: Condvar,
    // Wakes the thread that adds workers, when the walk is closed.
    closing: Condvar,
    spares: Arc<Spares>,
}

impl<'a> Pool<'a> {
    pub fn new(scanner: &'a Scanner<'a>, options: &WalkOptions<'_>) -> Self {
        let rows = usize::try_from(options.budget / ROW_BYTES).unwrap_or(usize::MAX);
        Self {
            scanner,
            budget: options.budget,
            // Two budgets, as the walk without threads holds, and what every
            // worker may have read and not handed over.
            window: rows
                .max(1)
                .saturating_mul(2)
                .saturating_add(options.threads.saturating_mul(BUNDLE)),
            measured: rows.max(1),
            held: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            state: Mutex::default(),
            work: Condvar::new(),
            ready: Condvar::new(),
            closing: Condvar::new(),
            spares: Arc::default(),
        }
    }

    // A panic elsewhere must not turn into a second one here, least of all
    // in a destructor, so a poisoned lock is taken as it is.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep<'s>(&self, state: MutexGuard<'s, State>) -> MutexGuard<'s, State> {
        self.work
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops the workers. Each returns once its current work is done.
    pub fn close(&self) {
        self.lock().closed = true;
        self.work.notify_all();
        self.closing.notify_all();
    }

    /// Waits up to `tick`, then says whether directories are queued and no
    /// worker is idle to take them. `None` once the pool is closed.
    pub fn short_of_workers(&self, tick: Duration) -> Option<bool> {
        let mut state = self.lock();
        if !state.closed {
            state = self
                .closing
                .wait_timeout(state, tick)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.closed {
            return None;
        }
        Some(state.idle == 0 && !state.queue.is_empty())
    }

    /// Queues the root, and returns the slot of its first scan.
    pub fn root(&self) -> Arc<Slot> {
        let slot = Slot::new(Key::default());
        let task = Task {
            job: Job::root(),
            path: String::new(),
            row: 0,
            slot: Arc::clone(&slot),
        };
        self.lock().queue.insert(Key::default(), task);
        self.work.notify_one();
        slot
    }

    /// Waits a while for `slot` to be filled, or for part `part` to be built,
    /// and returns what came first. Called by the walk, which measures the
    /// directory at depth `measuring` and below.
    ///
    /// # Panics
    /// If a worker panicked.
    pub fn wait(&self, slot: Option<&Arc<Slot>>, part: usize, measuring: usize) -> Event {
        let mut state = self.lock();
        for waited in [false, true] {
            if state.broken {
                drop(state);
                panic!("a thread of the walk panicked");
            }
            // Parts first, so that the sink has them as soon as it can.
            if let Some(built) = state.built.remove(&part) {
                return Event::Part(built);
            }
            // A worker fills a slot before it takes this lock to say so.
            if let Some(unit) = slot.and_then(|slot| slot.take()) {
                return Event::Unit(unit);
            }
            if waited {
                break;
            }
            if let Some(slot) = slot
                && state.head.as_ref().is_none_or(|(key, _)| *key != slot.key)
            {
                state.head = Some((Arc::clone(&slot.key), measuring.min(slot.key.len())));
                // Workers stopped at the limit may read below the new head.
                if state.idle > 0 && self.held.load(Ordering::Relaxed) >= self.window {
                    self.work.notify_all();
                }
            }
            state.asleep = true;
            state.wants_slot = slot.map_or(0, |slot| Arc::as_ptr(slot) as usize);
            state.wants_part = part;
            state = self
                .ready
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            state.asleep = false;
        }
        Event::Nothing
    }

    /// Queues `plan` to be built into part `number`.
    pub fn plan(&self, number: usize, plan: Plan) {
        let mut state = self.lock();
        state.plans.push_back((number, plan));
        let wake = state.idle > 0;
        drop(state);
        if wake {
            self.work.notify_one();
        }
    }

    /// Part `number`, if it is built.
    pub fn built(&self, number: usize) -> Option<Built> {
        self.lock().built.remove(&number)
    }

    /// The sink took a part of `rows` rows.
    pub fn delivered(&self, rows: usize) {
        let before = self.held.fetch_sub(rows, Ordering::Relaxed);
        // Rows that have gone make room for the workers to read more.
        if before >= self.window {
            self.work.notify_all();
        }
    }

    /// Runs one worker until the pool is closed.
    pub fn work(&self) {
        let mut scratch = Scratch::default();
        while let Some(work) = self.next_work() {
            let broken = BrokenOnPanic(self);
            match work {
                Work::Build(number, plan) => {
                    let built = plan.assemble().change_context(WalkError);
                    // The rows go before the lock is taken.
                    drop(plan);
                    let mut state = self.lock();
                    state.built.insert(number, built);
                    let wake = state.asleep && state.wants_part == number;
                    drop(state);
                    if wake {
                        self.ready.notify_one();
                    }
                }
                Work::Read(task) => {
                    let mut read = Read {
                        found: Finding::from_spares(&self.spares),
                        ..Read::default()
                    };
                    let mut next = Some(task);
                    let mut steps = 0;
                    while let Some(task) = next.take() {
                        steps += 1;
                        let stopped = self.read(task, &mut scratch, &mut read);
                        if !stopped && read.entries < BUNDLE && steps < STEPS {
                            next = self.take_on();
                        }
                    }
                    self.hand_over(read);
                }
            }
            std::mem::forget(broken);
        }
    }

    // Waits for work this worker may take, and takes it. Parts come first,
    // since they free rows.
    fn next_work(&self) -> Option<Work> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some((number, plan)) = state.plans.pop_front() {
                return Some(Work::Build(number, plan));
            }
            if self.room(&state)
                && let Some((_, task)) = state.queue.pop_first()
            {
                state.reading += 1;
                return Some(Work::Read(task));
            }
            state.idle += 1;
            self.idle.store(state.idle, Ordering::Relaxed);
            state = self.sleep(state);
            state.idle -= 1;
            self.idle.store(state.idle, Ordering::Relaxed);
        }
    }

    // Whether the first queued directory may be read: below the limit on
    // rows; at it, the directory the walk waits for, and up to one budget more
    // below the directory it measures. What that adds is one subtree, and no
    // more than a part once it is known to fit one.
    fn room(&self, state: &State) -> bool {
        let held = self.held.load(Ordering::Relaxed);
        held < self.window
            || state
                .queue
                .first_key_value()
                .zip(state.head.as_ref())
                .is_some_and(|((first, _), (key, measured))| {
                    first.starts_with(key)
                        || (first.starts_with(&key[..*measured])
                            && held < self.window + self.measured)
                })
    }

    // One more queued directory for a worker that has read little so far,
    // while other workers have enough to do and no part waits to be built.
    fn take_on(&self) -> Option<Task> {
        let mut state = self.lock();
        if state.closed
            || !state.plans.is_empty()
            || state.queue.len() <= state.idle
            || !self.room(&state)
        {
            return None;
        }
        state.queue.pop_first().map(|(_, task)| task)
    }

    // Whether a worker may go on into a subdirectory: nobody waits for work,
    // and the rows have room.
    fn alone(&self) -> bool {
        self.idle.load(Ordering::Relaxed) == 0 && self.held.load(Ordering::Relaxed) < self.window
    }

    // Reads the directory of `task`, and while `alone` holds and its subtree
    // fits a part, everything below it, depth first. Returns `true` where it
    // stopped short of the end, which ends `read`.
    fn read(&self, task: Task, scratch: &mut Scratch, read: &mut Read) -> bool {
        let Task {
            job,
            mut path,
            row,
            slot,
        } = task;
        // The key of the directory: without the place of a later scan of it.
        let key = match slot.key.len() {
            len if row == 0 && len > 0 => slot.key[..len - 1].into(),
            _ => Arc::clone(&slot.key),
        };
        let mut frames: Vec<Frame> = Vec::new();
        // The jobs of the subdirectories of every frame, those of the
        // innermost last.
        let mut jobs: Vec<Option<Job>> = Vec::new();
        let mut total = row;
        let mut next = Some((job, 0));
        loop {
            if let Some((job, place)) = next.take() {
                read.make_room(&self.spares);
                let added = self.scanner.scan_into(
                    job,
                    &path,
                    scratch,
                    &mut || {
                        let mut released = false;
                        for job in jobs.iter_mut().flatten() {
                            released |= job.release(&self.scanner.held);
                        }
                        released || self.release()
                    },
                    &mut read.found,
                );
                read.entries += added.items.len();
                self.held.fetch_add(added.count, Ordering::Relaxed);
                total += added.bytes;
                let first = jobs.len();
                jobs.extend(read.found.directories.drain(..).map(Some));
                let plain = added.plain;
                frames.push(Frame {
                    scan: read.scans.len(),
                    jobs: first,
                    entered: 0,
                    next: added.items.start,
                    end: added.items.end,
                    path: path.len(),
                    place,
                    rest: std::mem::take(&mut read.found.rest),
                    rows: Below {
                        scans: 0..0,
                        bytes: added.bytes,
                        count: added.count,
                    },
                    done: Vec::new(),
                });
                read.scans.push((read.blocks.len(), added));
                // A scan with anything but rows is handed over as it is, since
                // the walk has to look at what it holds.
                if !plain || row == 0 || total > self.budget {
                    break;
                }
            }

            // Into the next subdirectory, up through those read to the end.
            let top = frames.last_mut().expect("a directory being read");
            let (items, text) = read.rows(read.scans[top.scan].0);
            if let Some(index) =
                (top.next..top.end).find(|&index| matches!(items[index], Item::Directory { .. }))
            {
                if !self.alone() {
                    break;
                }
                let Item::Directory { name, place, .. } = items[index] else {
                    unreachable!("found as a directory above");
                };
                top.next = index + 1;
                let job = jobs[top.jobs + top.entered]
                    .take()
                    .expect("a job for every directory");
                top.entered += 1;
                path.truncate(top.path);
                path.push('/');
                path.push_str(text_of(text, name));
                next = Some((job, place));
                continue;
            }
            let mut frame = frames.pop().expect("checked above");
            jobs.truncate(frame.jobs);
            frame.rows.scans = frame.scan..read.scans.len();
            let Some(above) = frames.last_mut() else {
                read.whole.push((slot, frame.rows));
                return false;
            };
            above.rows.bytes += frame.rows.bytes;
            above.rows.count += frame.rows.count;
            above.done.push(frame.rows);
        }
        read.stopped = Some(Stopped {
            slot,
            key,
            path,
            frames,
            jobs,
        });
        true
    }

    // Hands over what one worker read: fills the slots, and queues the
    // directories it did not enter.
    fn hand_over(&self, read: Read) {
        let Read {
            found,
            mut blocks,
            scans,
            whole,
            stopped,
            ..
        } = read;
        let (rows, errors, unreadable, next) = found.into_rows();
        blocks.push(rows);
        let read = Taken { blocks, scans };
        let mut filled: Vec<(Arc<Slot>, Unit)> = whole
            .into_iter()
            .map(|(slot, below)| (slot, read.whole(below)))
            .collect();
        let mut tasks: Vec<Task> = Vec::new();
        if let Some(stopped) = stopped {
            // Only the last scan can have failed.
            let last = (errors, unreadable, next);
            filled.push(read.stopped(stopped, last, &mut tasks));
        }

        let addresses: Vec<usize> = filled
            .iter()
            .map(|(slot, _)| Arc::as_ptr(slot) as usize)
            .collect();
        for (slot, unit) in filled {
            slot.fill(unit);
        }
        let mut state = self.lock();
        state.reading -= 1;
        let added = tasks.len();
        for task in tasks {
            state.queue.insert(Arc::clone(&task.slot.key), task);
        }
        let walk = state.asleep && addresses.contains(&state.wants_slot);
        // This worker takes one of the new directories itself. Those waiting
        // for a descriptor may find one now.
        let wake = added.saturating_sub(1).min(state.idle);
        let handles = state.short_of_handles > 0;
        drop(state);
        if walk {
            self.ready.notify_one();
        }
        if handles {
            self.work.notify_all();
        } else {
            for _ in 0..wake {
                self.work.notify_one();
            }
        }
    }

    // Closes the directories that queued jobs hold, for a scan that has no
    // handle left. With none to close, it waits for another worker to hand
    // over what it read, since a worker holds handles of its own until then.
    fn release(&self) -> bool {
        let mut state = self.lock();
        let mut released = false;
        for task in state.queue.values_mut() {
            released |= task.job.release(&self.scanner.held);
        }
        if released {
            return true;
        }
        if state.reading <= 1 || state.closed {
            return false;
        }
        // Not counted while it waits, so that two waiting workers do not wait
        // for each other.
        state.reading -= 1;
        state.short_of_handles += 1;
        state = self.sleep(state);
        state.short_of_handles -= 1;
        state.reading += 1;
        true
    }
}

// What one worker read, now that its rows are shared.
struct Taken {
    blocks: Vec<Arc<Rows>>,
    scans: Vec<(usize, Added)>,
}

impl Taken {
    fn scanned(&self, scan: usize) -> Scanned {
        let (block, added) = &self.scans[scan];
        Scanned {
            rows: Arc::clone(&self.blocks[*block]),
            items: added.items.clone(),
            errors: Vec::new(),
            unreadable: None,
            next: None,
        }
    }

    fn whole(&self, below: Below) -> Unit {
        Unit::Whole {
            scans: below.scans.map(|scan| self.scanned(scan)).collect(),
            bytes: below.bytes,
            count: below.count,
        }
    }

    // The directories a worker was inside of where it stopped, each a unit
    // inside its parent's, and the outermost for its slot. The subdirectories
    // not entered go to `tasks`. `last` is what the last scan could not read.
    fn stopped(
        &self,
        stopped: Stopped,
        mut last: (
            Vec<Option<std::io::Error>>,
            Option<std::io::Error>,
            Option<usize>,
        ),
        tasks: &mut Vec<Task>,
    ) -> (Arc<Slot>, Unit) {
        let Stopped {
            slot,
            key,
            path,
            frames,
            mut jobs,
        } = stopped;
        let mut keys = vec![key];
        for frame in &frames[1..] {
            let above = keys.last().expect("the first key");
            keys.push(key_below(above, frame.place, true));
        }
        // From the innermost directory out.
        let mut inner: Option<Unit> = None;
        for (frame, key) in frames.into_iter().zip(keys).rev() {
            let mut own = self.scanned(frame.scan);
            if inner.is_none() {
                (own.errors, own.unreadable, own.next) =
                    (std::mem::take(&mut last.0), last.1.take(), last.2.take());
            }
            let directory = &path[..frame.path];
            let rows = Arc::clone(&own.rows);
            let mut done = frame.done.into_iter();
            let mut subdirs = Vec::new();
            let found = own
                .items
                .clone()
                .filter_map(|index| match rows.items[index] {
                    Item::Directory { name, place, .. } => Some((name, place)),
                    _ => None,
                });
            for (nth, (name, place)) in found.enumerate() {
                if let Some(below) = done.next() {
                    subdirs.push(Next::Ready(Box::new(self.whole(below))));
                } else if nth < frame.entered {
                    let unit = inner.take().expect("the directory being read");
                    subdirs.push(Next::Ready(Box::new(unit)));
                } else {
                    let name = rows.text(name);
                    let task = Task {
                        job: jobs[frame.jobs + nth]
                            .take()
                            .expect("a job that was not entered"),
                        path: joined(directory, name),
                        row: estimate(EntryKind::Directory, name, ""),
                        slot: Slot::new(key_below(&key, place, true)),
                    };
                    subdirs.push(Next::Slot(Arc::clone(&task.slot)));
                    tasks.push(task);
                }
            }
            let rest = frame
                .rest
                .into_iter()
                .map(|job| {
                    let task = Task {
                        slot: Slot::new(key_below(&key, job.start(), false)),
                        job,
                        path: directory.to_owned(),
                        row: 0,
                    };
                    let next = Next::Slot(Arc::clone(&task.slot));
                    tasks.push(task);
                    next
                })
                .collect();
            inner = Some(Unit::Dir {
                scanned: own,
                subdirs,
                rest,
            });
        }
        (slot, inner.expect("a directory was read"))
    }
}

// What one worker read before it hands it over. Every scan shares one set of
// rows, so a directory costs no allocation of its own.
#[derive(Default)]
struct Read {
    // The rows being filled, and those filled before them.
    found: Finding,
    blocks: Vec<Arc<Rows>>,
    // Every scan with the rows it is in, in the order read, which within a
    // subtree is walk order.
    scans: Vec<(usize, Added)>,
    entries: usize,
    // The subtrees read to their end, each for its slot.
    whole: Vec<(Arc<Slot>, Below)>,
    // Where the worker stopped short, if it did.
    stopped: Option<Stopped>,
}

impl Read {
    // Starts new rows if a scan might not fit in those being filled. Only
    // plain scans came before, so the full rows hold no failure.
    fn make_room(&mut self, spares: &Arc<Spares>) {
        if !self.found.has_room() {
            let full = std::mem::replace(&mut self.found, Finding::from_spares(spares));
            self.blocks.push(full.into_rows().0);
        }
    }

    // The items and the text of the rows in `block`.
    fn rows(&self, block: usize) -> (&[Item], &str) {
        match self.blocks.get(block) {
            Some(rows) => (&rows.items, &rows.text),
            None => (self.found.items(), self.found.text()),
        }
    }
}

// The scans of a subtree, as a range of `Read::scans`, and the estimate and
// number of its rows below its own row.
struct Below {
    scans: Range<usize>,
    bytes: u64,
    count: usize,
}

struct Stopped {
    slot: Arc<Slot>,
    // The key of the outermost directory.
    key: Key,
    // The path of the innermost directory, which those of the others begin.
    path: String,
    frames: Vec<Frame>,
    jobs: Vec<Option<Job>>,
}

// A directory that `Pool::read` is inside of.
struct Frame {
    // Its scan in `Read::scans`, and where the jobs of its subdirectories
    // start in the reader's jobs.
    scan: usize,
    jobs: usize,
    // How many of its subdirectories were entered, and the next item to look
    // at, and the end of its items.
    entered: usize,
    next: usize,
    end: usize,
    // The length of its path, and its place in its parent's listing.
    path: usize,
    place: usize,
    // The scans of the rest of a directory that one scan did not read.
    rest: Vec<Job>,
    // Its rows and those below it read so far, and each subdirectory read to
    // its end, in order.
    rows: Below,
    done: Vec<Below>,
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
        self.0.closing.notify_all();
    }
}
