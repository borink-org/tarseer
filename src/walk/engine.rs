// The walk on worker threads. Nothing in it is one thread's job alone, so that
// parts of a tree that have nothing to do with each other do not wait for
// each other.
//
// 1. A scan reads one directory. A worker takes the waiting scan that comes
//    first in walk order, and goes on into the subdirectories it finds until it
//    has read `BUNDLE` entries.
//    While every worker is busy, it reads a subdirectory's whole subtree
//    itself, and that subtree is one unit with nothing of its own in the
//    engine: see `Engine::deep`.
// 2. Every other directory is a node that adds up its subtree as the scans arrive.
//    A node whose subtree fits a part is whole: its parent takes it as one, by
//    its totals. A node is over as soon as its total passes the budget, which
//    is long before its subtree has been read.
// 3. A node that is over gets a sequencer, which any worker runs. It puts the
//    directory's children into groups by their sizes, as the rules in the
//    module doc say, and each group is the plan of a part. A child that is
//    over has a sequencer of its own, and the two do not wait for each other.
// 4. A worker builds a part from its plan.
// 5. The calling thread gives the parts to the sink: in walk order, by
//    following the tree of sequencers, or as they are built.
//
// The rows that have been read and not yet given to the sink are limited. At
// the limit the workers stop scanning, except for one scan at a time, which
// is what the part that the sink waits for may need.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use error_stack::{Report, ResultExt as _};

use super::assemble::{Piece, Plan, Segment, Subtree};
use super::reader::Scratch;
use super::scan::{CHUNK, Item, Job, Rows, Scanned, Scanner, key_below};
use super::{Cancelled, OnError, PartOrder, SkipReason, Skips, WalkError, WalkOptions, estimate};
use crate::manifest::{EntryKind, TreePart};

// The entries a worker reads before it hands its scans over.
const BUNDLE: usize = 2048;

// How many rows one budget is taken to hold, for the limit on rows in flight.
const ROW_BYTES: u64 = 64;

type Built = Result<TreePart, Report<WalkError>>;

// Where a directory's row is in its parent: the scan that found it, by where
// that scan starts in the listing, and the item in it.
type Place = (usize, usize);

// What a parent knows of a child directory once the child has made up its
// mind.
enum Child {
    // The subtree fits a part. `bytes` and `count` are of its rows, without
    // the directory's own row.
    Whole {
        bytes: u64,
        count: usize,
        scans: Vec<Scanned>,
    },
    Over(Arc<Node>),
    Failed(Report<WalkError>),
}

// The rows that go into a directory's first part ahead of its own: the rows of
// the directories above it that are not in a part yet, its own row last.
struct Pending {
    // The names of the directories above the first of the rows.
    stem: Vec<String>,
    pieces: Vec<Piece>,
}

struct Node {
    parent: Option<(Arc<Node>, Place)>,
    // The key of the directory's first scan. See `Job::key`.
    key: Box<[u32]>,
    // Relative to the root, and empty for the root.
    path: String,
    // How many directories lie between the root and this directory's entries.
    depth: usize,
    // The estimate of the directory's own row. 0 for the root, which has none.
    row_bytes: u64,
    state: Mutex<NodeState>,
}

#[derive(Default)]
struct NodeState {
    // The directory's scans that have arrived, by where they start.
    scans: BTreeMap<usize, Scanned>,
    arrived: usize,
    // Where the scan that reached the end of the listing starts.
    last: Option<usize>,
    children: HashMap<Place, Child>,
    // Child directories that have not made up their minds.
    unresolved: usize,
    child_is_over: bool,
    // The first child in walk order that failed.
    failed_child: Option<Place>,
    // The rows of the subtree so far, without the directory's own.
    bytes: u64,
    count: usize,
    // The first thing in the directory's own scans that could not be read.
    failure: Option<(Place, Report<WalkError>)>,
    stage: Stage,
    // Set for a node that is over.
    cut: Option<Cut>,
    // From the parent's sequencer. `None` until that has come this far.
    pending: Option<Pending>,
    // In the queue of sequencers to run.
    queued: bool,
    // What the sequencer has made, in walk order. That is all of it once the
    // stage is `Done`.
    slots: Vec<Slot>,
}

// How far a node has come.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Stage {
    // It does not know yet whether its subtree fits a part.
    #[default]
    Open,
    // It told its parent what it is. A node that is over has a sequencer from
    // here on.
    Decided,
    // Its sequencer has made its last part.
    Done,
}

enum Slot {
    Part(usize),
    Child(Arc<Node>),
    Failed(Option<Report<WalkError>>),
}

// A sequencer's place in its directory, and its open group.
#[derive(Default)]
struct Cut {
    start: usize,
    item: usize,
    group: Vec<Piece>,
    group_bytes: u64,
    // The pending rows have gone: into a part, or to a child that is over.
    carried: bool,
    // The first part, made before the pending rows were known.
    first: Option<(usize, Vec<Piece>)>,
    // The key of the scan, or of the child's first scan, that the sequencer
    // waits for. `State::heads` holds it too.
    waits_for: Option<Box<[u32]>>,
}

// A directory that `Engine::deep` is inside of.
struct Frame {
    scanned: Scanned,
    // The jobs of the subdirectories not entered yet, and of the rest of a
    // directory that one scan did not read.
    jobs: std::vec::IntoIter<Job>,
    rest: Vec<Job>,
    // The first item that has not been looked at.
    next: usize,
    // Where the directory's row is in its parent, and that row: a scan of
    // the parent, and the item in it.
    place: Place,
    from: (Arc<Rows>, usize),
    // Whether `Engine::report` has had the scan.
    reported: bool,
    // The rows read below the directory's own row.
    bytes: u64,
    count: usize,
    // The subdirectories that are read to their end, in walk order.
    done: Vec<(Place, Child)>,
}

struct Task {
    job: Job,
    node: Arc<Node>,
}

enum Work {
    Build(usize, Plan),
    Sequence(Arc<Node>),
    Scan(Task),
}

#[derive(Default)]
struct State {
    // By `Job::key`, so the first is the first in walk order.
    waiting: BTreeMap<Box<[u32]>, Task>,
    sequence: VecDeque<Arc<Node>>,
    plans: VecDeque<(usize, Plan)>,
    // What the sequencers wait for. The first is what the walk as a whole
    // waits for, since everything before it in walk order is done.
    heads: BTreeSet<Box<[u32]>>,
    // The parts that are built, by their number, for the sink in walk order.
    parts: HashMap<usize, Built>,
    // The same for the sink in the order they were built.
    built: VecDeque<Built>,
    idle: usize,
    // Workers inside a scan that are not waiting for a handle.
    scanning: usize,
    closed: bool,
    broken: bool,
    // Goes up whenever the calling thread may have something to do.
    events: u64,
}

pub(super) struct Engine<'a> {
    scanner: &'a Scanner<'a>,
    options: &'a WalkOptions<'a>,
    root: &'a Path,
    // The most rows that are read and not yet given to the sink.
    window: usize,
    held: AtomicUsize,
    // How many workers wait for work, for a worker that does not hold the
    // lock. It may be a moment behind.
    idle: AtomicUsize,
    // Sequencers that are not done, and parts planned and given to the sink.
    open: AtomicUsize,
    planned: AtomicUsize,
    skips: [AtomicU32; 4],
    state: Mutex<State>,
    work: Condvar,
    ready: Condvar,
}

impl<'a> Engine<'a> {
    pub fn new(scanner: &'a Scanner<'a>, options: &'a WalkOptions<'a>, root: &'a Path) -> Self {
        let rows = usize::try_from(options.budget / ROW_BYTES).unwrap_or(usize::MAX);
        Self {
            scanner,
            options,
            root,
            // Two budgets, as the walk without threads holds, and what every
            // thread may have read and not handed over.
            window: rows
                .max(1)
                .saturating_mul(2)
                .saturating_add(options.threads.saturating_mul(BUNDLE)),
            held: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
            planned: AtomicUsize::new(0),
            skips: Default::default(),
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

    fn lock_node(node: &Node) -> MutexGuard<'_, NodeState> {
        node.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'s>(condvar: &Condvar, state: MutexGuard<'s, State>) -> MutexGuard<'s, State> {
        condvar.wait(state).unwrap_or_else(PoisonError::into_inner)
    }

    // Tells the calling thread that there may be something for the sink.
    fn announce(&self) {
        self.lock().events += 1;
        self.ready.notify_one();
    }

    /// Stops the workers. Each returns once its current work is done.
    pub fn close(&self) {
        self.lock().closed = true;
        self.work.notify_all();
    }

    pub fn skips(&self) -> Skips {
        let count = |reason: usize| self.skips[reason].load(Ordering::Relaxed);
        Skips {
            special: count(0),
            non_utf8: count(1),
            unreadable: count(2),
            failed: count(3),
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
        if let Some(progress) = self.options.progress {
            progress.skipped(path, reason);
        }
    }

    /// Runs the walk from the calling thread, which gives the parts to `sink`.
    /// The workers run [`Engine::work`] meanwhile.
    pub fn run(
        &self,
        sink: &mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
    ) -> Result<(), Report<WalkError>> {
        // The root is cut like a directory that is over, whatever it holds,
        // and nothing goes ahead of its rows.
        let root = Arc::new(Node {
            parent: None,
            key: Box::default(),
            path: String::new(),
            depth: 0,
            row_bytes: 0,
            state: Mutex::new(NodeState {
                stage: Stage::Decided,
                cut: Some(Cut::default()),
                pending: Some(Pending {
                    stem: Vec::new(),
                    pieces: Vec::new(),
                }),
                ..NodeState::default()
            }),
        });
        self.open.fetch_add(1, Ordering::Relaxed);
        let first = Task {
            job: Job::root(),
            node: Arc::clone(&root),
        };
        self.lock().waiting.insert(first.job.key.clone(), first);
        self.work.notify_one();

        match self.options.order {
            PartOrder::Walk => self.deliver_in_walk_order(&root, sink),
            PartOrder::Completion => self.deliver_as_built(sink),
        }
    }

    fn cancelled(&self) -> Result<(), Report<WalkError>> {
        let raised = self
            .options
            .cancel
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed));
        if raised {
            return Err(Report::new(Cancelled).change_context(WalkError));
        }
        Ok(())
    }

    // Waits until `events` has moved on from `seen`. It looks at the cancel
    // flag now and then, since raising that wakes nobody.
    fn wait_for_events(&self, seen: u64) -> Result<(), Report<WalkError>> {
        let mut state = self.lock();
        while state.events == seen {
            if state.broken {
                drop(state);
                panic!("a thread of the walk panicked");
            }
            drop(state);
            self.cancelled()?;
            state = self.lock();
            if state.events != seen {
                break;
            }
            state = self
                .ready
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    fn hand_to_sink(
        &self,
        built: Built,
        sink: &mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
    ) -> Result<(), Report<WalkError>> {
        let part = built?;
        let rows = part.len();
        sink(part)?;
        // Rows that have gone make room for the workers to scan more.
        let before = self.held.fetch_sub(rows, Ordering::Relaxed);
        if before >= self.window && before - rows < self.window {
            self.work.notify_all();
        }
        Ok(())
    }

    fn deliver_as_built(
        &self,
        sink: &mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
    ) -> Result<(), Report<WalkError>> {
        let mut delivered = 0;
        loop {
            self.cancelled()?;
            let mut state = self.lock();
            let seen = state.events;
            let built = state.built.pop_front();
            drop(state);
            if let Some(built) = built {
                delivered += 1;
                self.hand_to_sink(built, sink)?;
                continue;
            }
            // No sequencer is left to plan a part, and every plan became a
            // part that the sink has.
            if self.open.load(Ordering::Relaxed) == 0
                && self.planned.load(Ordering::Relaxed) == delivered
            {
                return Ok(());
            }
            self.wait_for_events(seen)?;
        }
    }

    fn deliver_in_walk_order(
        &self,
        root: &Arc<Node>,
        sink: &mut dyn FnMut(TreePart) -> Result<(), Report<WalkError>>,
    ) -> Result<(), Report<WalkError>> {
        // The sequencers being followed, each with the next of its slots.
        let mut open = vec![(Arc::clone(root), 0)];
        loop {
            self.cancelled()?;
            let seen = self.lock().events;
            let Some((node, next)) = open.last_mut() else {
                return Ok(());
            };
            let mut state = Self::lock_node(node);
            let closed = state.stage == Stage::Done;
            let waits_for = match state.slots.get_mut(*next) {
                Some(Slot::Part(number)) => Some(*number),
                Some(Slot::Child(child)) => {
                    let child = Arc::clone(child);
                    *next += 1;
                    drop(state);
                    open.push((child, 0));
                    continue;
                }
                Some(Slot::Failed(report)) => {
                    return Err(report.take().expect("a failure is reported once"));
                }
                None if closed => {
                    drop(state);
                    open.pop();
                    continue;
                }
                None => None,
            };
            drop(state);
            if let Some(number) = waits_for
                && let Some(built) = self.lock().parts.remove(&number)
            {
                *next += 1;
                self.hand_to_sink(built, sink)?;
                continue;
            }
            self.wait_for_events(seen)?;
        }
    }

    /// Runs one worker until the engine is closed.
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
                    match self.options.order {
                        PartOrder::Walk => drop(state.parts.insert(number, built)),
                        PartOrder::Completion => state.built.push_back(built),
                    }
                    state.events += 1;
                    drop(state);
                    self.ready.notify_one();
                }
                Work::Sequence(node) => self.sequence(&node),
                Work::Scan(first) => self.scan_from(first, &mut scratch),
            }
            std::mem::forget(broken);
        }
    }

    // Waits for work this worker may take, and takes it. Parts come first,
    // since they free rows, and then sequencers, since they plan parts.
    fn next_work(&self) -> Option<Work> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some((number, plan)) = state.plans.pop_front() {
                return Some(Work::Build(number, plan));
            }
            if let Some(node) = state.sequence.pop_front() {
                return Some(Work::Sequence(node));
            }
            // At the limit the workers still scan what the walk waits for: the
            // scans below its first head, which come first in the queue. What
            // that adds is one subtree, and no more than a part once it is
            // known to fit one. Scans past it would add without end.
            let needed = || {
                let first = state.waiting.first_key_value().map(|(key, _)| key);
                let head = state.heads.first();
                first
                    .zip(head)
                    .is_some_and(|(first, head)| first.starts_with(head))
            };
            let room =
                self.held.load(Ordering::Relaxed) < self.window || state.scanning == 0 || needed();
            if room && let Some((_, task)) = state.waiting.pop_first() {
                state.scanning += 1;
                return Some(Work::Scan(task));
            }
            state.idle += 1;
            self.idle.store(state.idle, Ordering::Relaxed);
            state = Self::wait(&self.work, state);
            state.idle -= 1;
            self.idle.store(state.idle, Ordering::Relaxed);
        }
    }

    fn enqueue(&self, add: impl FnOnce(&mut State) -> usize) {
        let mut state = self.lock();
        let added = add(&mut state);
        let wake = added.min(state.idle);
        drop(state);
        for _ in 0..wake {
            self.work.notify_one();
        }
    }

    fn plan(&self, number: usize, plan: Plan) {
        self.enqueue(|state| {
            state.plans.push_back((number, plan));
            1
        });
    }

    // Puts `node`'s sequencer in the queue, unless it is there already. The
    // caller holds the node's lock, so a sequencer that is running has
    // finished its step, or has not started it, by the time this is seen.
    fn schedule(&self, node: &Arc<Node>, state: &mut NodeState) {
        if state.cut.is_none() || state.stage == Stage::Done || state.queued {
            return;
        }
        state.queued = true;
        let node = Arc::clone(node);
        self.enqueue(|state| {
            state.sequence.push_back(node);
            1
        });
    }

    // Scans `first`, and then the subdirectories it finds, in walk order, until
    // `BUNDLE` entries are read. The jobs left over go to the other workers.
    fn scan_from(&self, first: Task, scratch: &mut Scratch) {
        // The tasks this worker will scan itself, the next one last, and the
        // ones it leaves to the others.
        let mut mine = vec![first];
        let mut others: Vec<Task> = Vec::new();
        let mut read = 0;
        while let Some(Task { job, node }) = mine.pop() {
            let start = job.start();
            if let (Some(progress), Some(path)) = (self.options.progress, job.path()) {
                progress.entered(path);
            }
            // With no handle left, this worker first closes the directories
            // that its own jobs hold.
            let found = self.scanner.scan(job, scratch, &mut || {
                let mut released = false;
                for waiting in mine.iter_mut().chain(others.iter_mut()) {
                    released |= waiting.job.release(&self.scanner.held);
                }
                released || self.release()
            });
            read += found.scanned.rows.items.len();
            self.report(&node.path, &found.scanned);

            let rows = Arc::clone(&found.scanned.rows);
            let rest: Vec<Task> = found
                .rest
                .into_iter()
                .map(|job| Task {
                    job,
                    node: Arc::clone(&node),
                })
                .collect();
            // The node hears of its subdirectories before any of them can
            // answer.
            let jobs = found.directories;
            self.arrived(&node, start, found.scanned, jobs.len(), true);

            // The nth directory among the items belongs to the nth job.
            let places = rows
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| matches!(item, Item::Directory { .. }));
            let mut children: Vec<Task> = Vec::new();
            for (job, (index, _)) in jobs.into_iter().zip(places) {
                let from = (Arc::clone(&rows), index);
                // A subtree is started within the bundle and read to its end
                // whatever its size, up to a part.
                if rest.is_empty() && read < BUNDLE && self.alone() {
                    let mut release = || {
                        let mut released = false;
                        for waiting in mine.iter_mut().chain(others.iter_mut()) {
                            released |= waiting.job.release(&self.scanner.held);
                        }
                        released
                    };
                    let place = (start, index);
                    self.deep(job, &node, place, from, &mut read, scratch, &mut release, &mut children);
                } else {
                    let below = node_below(&node, (start, index), &from.0, from.1);
                    children.push(Task { job, node: below });
                }
            }

            // The scans of the rest of a directory are for other workers,
            // and they come before anything else this worker would do.
            let done = read >= BUNDLE || !rest.is_empty();
            others.extend(rest);
            if done {
                others.extend(children);
                break;
            }
            // Going on alone saves trips through the lock while every worker
            // is busy. With workers waiting it would only make them wait
            // longer, which is how it goes when reading is slow and the
            // threads are many: then the others get all but the first.
            let mut children = children.into_iter();
            mine.extend(children.next());
            if self.idle.load(Ordering::Relaxed) > 0 {
                others.extend(children);
                self.give(std::mem::take(&mut others));
            } else {
                let mut later: Vec<Task> = children.collect();
                later.reverse();
                let first = mine.pop();
                mine.extend(later);
                mine.extend(first);
            }
        }
        others.extend(mine);

        let mut state = self.lock();
        let added = others.len();
        for task in others {
            state.waiting.insert(task.job.key.clone(), task);
        }
        state.scanning -= 1;
        // This worker takes one of the new jobs itself. At the limit the
        // others wait for the last scan to end, which this may have been.
        let next = usize::from(state.scanning == 0 && !state.waiting.is_empty());
        let wake = added.saturating_sub(1).max(next).min(state.idle);
        drop(state);
        for _ in 0..wake {
            self.work.notify_one();
        }
    }

    // Puts `tasks` in the queue for the other workers.
    fn give(&self, tasks: Vec<Task>) {
        self.enqueue(|state| {
            let added = tasks.len();
            for task in tasks {
                state.waiting.insert(task.job.key.clone(), task);
            }
            added
        });
    }

    // Counts what a scan skipped, and tells the progress receiver of what it
    // found. With worker threads both happen when a directory is read.
    fn report(&self, path: &str, scanned: &Scanned) {
        if scanned.unreadable.is_some() && self.options.on_error == OnError::Skip {
            self.skip(path, SkipReason::Unreadable);
        }
        if scanned.is_clean() && self.options.progress.is_none() {
            return;
        }
        let rows = &scanned.rows;
        for item in &rows.items {
            match *item {
                Item::Skipped { name, reason } => {
                    self.skip(&joined(path, rows.text(name)), reason);
                }
                Item::Failed { name, .. } if self.options.on_error == OnError::Skip => {
                    let path = match name {
                        Some(name) => joined(path, rows.text(name)),
                        None => path.to_owned(),
                    };
                    self.skip(&path, SkipReason::Failed);
                }
                Item::Failed { .. } => {}
                Item::File { size, .. } => self.recorded(EntryKind::File, size),
                Item::Symlink { .. } => self.recorded(EntryKind::Symlink, 0),
                Item::Directory { .. } => self.recorded(EntryKind::Directory, 0),
            }
        }
    }

    fn recorded(&self, kind: EntryKind, size: u64) {
        if let Some(progress) = self.options.progress {
            progress.recorded(kind, size);
        }
    }

    // Closes the directories that waiting jobs hold, for a scan that has no
    // handle left. With none to close, it waits for another scan to end,
    // since a scan holds handles of its own until then.
    fn release(&self) -> bool {
        let mut state = self.lock();
        let mut released = false;
        for task in state.waiting.values_mut() {
            released |= task.job.release(&self.scanner.held);
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

    // Takes in the scan of `node` that starts at `start`, which found
    // `directories` subdirectories.
    // `count` is `false` for a scan whose rows `deep` counted when it read them.
    fn arrived(
        &self,
        node: &Arc<Node>,
        start: usize,
        mut scanned: Scanned,
        directories: usize,
        count: bool,
    ) {
        if count {
            self.held.fetch_add(scanned.count, Ordering::Relaxed);
        }
        let failure = self.failure_of(node, start, &mut scanned);

        let mut state = Self::lock_node(node);
        state.bytes += scanned.bytes;
        state.count += scanned.count;
        state.unresolved += directories;
        state.arrived += 1;
        if scanned.next.is_none() {
            state.last = Some(start);
        }
        if let Some(failure) = failure
            && state.failure.as_ref().is_none_or(|(at, _)| failure.0 < *at)
        {
            state.failure = Some(failure);
        }
        state.scans.insert(start, scanned);
        self.settled(node, state);
    }

    // The first thing a scan could not read, as the failure of the walk, under
    // the policy that makes it one. The root is one under either policy:
    // counting it as a skip would report an empty walk as a success.
    fn failure_of(
        &self,
        node: &Node,
        start: usize,
        scanned: &mut Scanned,
    ) -> Option<(Place, Report<WalkError>)> {
        if let Some(error) = scanned
            .unreadable
            .take_if(|_| self.options.on_error == OnError::Fail || node.parent.is_none())
        {
            let listed = match node.parent {
                Some(_) => node.path.clone(),
                None => self.root.display().to_string(),
            };
            let report = Report::new(error)
                .attach(format!("listing {listed}"))
                .change_context(WalkError);
            return Some(((start, 0), report));
        }
        if self.options.on_error != OnError::Fail {
            return None;
        }
        let rows = Arc::clone(&scanned.rows);
        rows.items.iter().enumerate().find_map(|(index, item)| {
            let Item::Failed { name, what, error } = *item else {
                return None;
            };
            let error = scanned.errors[error as usize].take()?;
            let path = match name {
                Some(name) => joined(&node.path, rows.text(name)),
                None => node.path.clone(),
            };
            let report = Report::new(error)
                .attach(format!("{what} of {path}"))
                .change_context(WalkError);
            Some(((start, index), report))
        })
    }

    // After something about `node` has changed: lets it make up its mind if it
    // can, tells its parent, and so on upwards. A node that is over has its
    // sequencer run instead, since that may have waited for the change.
    fn settled(&self, node: &Arc<Node>, state: MutexGuard<'_, NodeState>) {
        let Some(decision) = self.settle(node, state) else {
            return;
        };
        if let Some((parent, place)) = node.parent.clone() {
            self.decided(parent, place, decision);
        }
    }

    // Tells `parent` what its child at `place` is, and everything above it
    // that this decides in turn.
    fn decided(&self, mut parent: Arc<Node>, mut place: Place, mut decision: Child) {
        // Never two node locks at once on the way up.
        loop {
            let mut above = Self::lock_node(&parent);
            above.unresolved -= 1;
            match &decision {
                Child::Whole { bytes, count, .. } => {
                    above.bytes += bytes;
                    above.count += count;
                }
                Child::Over(_) => above.child_is_over = true,
                Child::Failed(_) => {
                    above.failed_child = Some(above.failed_child.map_or(place, |at| at.min(place)));
                }
            }
            above.children.insert(place, decision);
            let Some(next) = self.settle(&parent, above) else {
                return;
            };
            decision = next;
            let Some((up, at)) = parent.parent.clone() else {
                return;
            };
            (parent, place) = (up, at);
        }
    }

    // Whether a worker may read a subtree by itself: nobody waits for work,
    // and the rows have room.
    fn alone(&self) -> bool {
        self.idle.load(Ordering::Relaxed) == 0
            && self.held.load(Ordering::Relaxed) < self.window
    }

    // Reads the subtree of the directory of `job` on this thread, depth-first,
    // for as long as `alone` holds and the subtree fits a part. A subtree read
    // to its end is whole, and `parent` takes it as one unit: nothing in it
    // had a node, a task or a trip through the queue.
    //
    // Where that stops, the directories it is inside of become nodes, which
    // take what has been read, and the scans not made go to `out` in walk
    // order. Nothing is read twice.
    //
    // `from` is the row of the directory: a scan of its parent, and the item.
    #[allow(clippy::too_many_arguments)]
    fn deep(
        &self,
        job: Job,
        parent: &Arc<Node>,
        place: Place,
        from: (Arc<Rows>, usize),
        read: &mut usize,
        scratch: &mut Scratch,
        release: &mut dyn FnMut() -> bool,
        out: &mut Vec<Task>,
    ) {
        let mut frames: Vec<Frame> = Vec::new();
        // The estimate of the subtree with the row of its root.
        let mut total = row_estimate(&from.0, &from.0.items[from.1]);
        let mut enter = Some((job, place, from));
        while let Some((job, place, from)) = enter.take() {
            // With a receiver of progress the path is needed at once.
            let path = self
                .options
                .progress
                .and_then(|_| job.path().map(str::to_owned));
            if let (Some(progress), Some(path)) = (self.options.progress, &path) {
                progress.entered(path);
            }
            let found = self.scanner.scan(job, scratch, &mut || {
                let mut released = false;
                for frame in &mut frames {
                    for waiting in frame.jobs.as_mut_slice() {
                        released |= waiting.release(&self.scanner.held);
                    }
                }
                released || release() || self.release()
            });
            *read += found.scanned.rows.items.len();
            self.held.fetch_add(found.scanned.count, Ordering::Relaxed);
            total += found.scanned.bytes;
            // A scan with anything but rows goes the ordinary way, which
            // knows what to do with a failure.
            let plain = found.rest.is_empty() && found.scanned.is_clean();
            if let Some(path) = &path {
                self.report(path, &found.scanned);
            }
            frames.push(Frame {
                bytes: found.scanned.bytes,
                count: found.scanned.count,
                scanned: found.scanned,
                jobs: found.directories.into_iter(),
                rest: found.rest,
                next: 0,
                place,
                from,
                reported: path.is_some(),
                done: Vec::new(),
            });
            if !plain || total > self.options.budget {
                return self.hand_back(frames, parent, out);
            }

            // Up through the directories that are read to their end, to the
            // next subdirectory to enter.
            while let Some(top) = frames.last_mut() {
                let rows = &top.scanned.rows;
                let next = (top.next..rows.items.len())
                    .find(|&index| matches!(rows.items[index], Item::Directory { .. }));
                if let Some(index) = next {
                    if !self.alone() {
                        return self.hand_back(frames, parent, out);
                    }
                    top.next = index + 1;
                    let job = top.jobs.next().expect("a job for every directory");
                    enter = Some((job, (0, index), (Arc::clone(rows), index)));
                    break;
                }
                let frame = frames.pop().expect("checked above");
                let mut scans = vec![frame.scanned];
                for (_, child) in frame.done {
                    if let Child::Whole { scans: below, .. } = child {
                        scans.extend(below);
                    }
                }
                let whole = Child::Whole {
                    bytes: frame.bytes,
                    count: frame.count,
                    scans,
                };
                match frames.last_mut() {
                    Some(above) => {
                        above.bytes += frame.bytes;
                        above.count += frame.count;
                        above.done.push((frame.place, whole));
                    }
                    None => return self.decided(Arc::clone(parent), frame.place, whole),
                }
            }
        }
    }

    // Makes nodes of the directories `deep` was inside of, the outermost
    // first, and gives each what was read of it.
    fn hand_back(&self, frames: Vec<Frame>, parent: &Arc<Node>, out: &mut Vec<Task>) {
        let mut above = Arc::clone(parent);
        // The scans not made, of each directory, innermost last.
        let mut left: Vec<Vec<Task>> = Vec::new();
        for frame in frames {
            let node = node_below(&above, frame.place, &frame.from.0, frame.from.1);
            if !frame.reported {
                self.report(&node.path, &frame.scanned);
            }
            let rows = Arc::clone(&frame.scanned.rows);
            let directories = rows
                .items
                .iter()
                .filter(|item| matches!(item, Item::Directory { .. }))
                .count();
            self.arrived(&node, 0, frame.scanned, directories, false);
            for (place, whole) in frame.done {
                self.decided(Arc::clone(&node), place, whole);
            }
            let places = (frame.next..rows.items.len())
                .filter(|&index| matches!(rows.items[index], Item::Directory { .. }));
            let mut tasks: Vec<Task> = frame
                .jobs
                .zip(places)
                .map(|(job, index)| Task {
                    job,
                    node: node_below(&node, (0, index), &rows, index),
                })
                .collect();
            tasks.extend(frame.rest.into_iter().map(|job| Task {
                job,
                node: Arc::clone(&node),
            }));
            left.push(tasks);
            above = node;
        }
        // In walk order the innermost directory's scans come first.
        out.extend(left.into_iter().rev().flatten());
    }

    // One node's part in `settled`. Returns what the node has decided, if it
    // decided now.
    fn settle(&self, node: &Arc<Node>, mut state: MutexGuard<'_, NodeState>) -> Option<Child> {
        if state.stage != Stage::Open {
            self.schedule(node, &mut state);
            return None;
        }
        let decision = self.decide(node, &mut state)?;
        state.stage = Stage::Decided;
        if matches!(decision, Child::Over(_)) {
            state.cut = Some(Cut::default());
            self.open.fetch_add(1, Ordering::Relaxed);
            self.schedule(node, &mut state);
        }
        Some(decision)
    }

    // What `node` is, if that can be said yet.
    fn decide(&self, node: &Arc<Node>, state: &mut NodeState) -> Option<Child> {
        let failed_child = state.failed_child;
        let own = state.failure.as_ref().map(|(place, _)| *place);
        if own.is_some() || failed_child.is_some() {
            // The earlier of the two in walk order.
            let report = if own.is_some_and(|own| failed_child.is_none_or(|child| own <= child)) {
                state.failure.take().expect("checked above").1
            } else {
                match state.children.remove(&failed_child.expect("checked above")) {
                    Some(Child::Failed(report)) => report,
                    _ => unreachable!("filtered above"),
                }
            };
            return Some(Child::Failed(report));
        }
        if state.child_is_over || node.row_bytes + state.bytes > self.options.budget {
            return Some(Child::Over(Arc::clone(node)));
        }
        let complete = state
            .last
            .is_some_and(|last| state.arrived == last / CHUNK + 1);
        if !complete || state.unresolved > 0 {
            return None;
        }
        // The scans in the order the walk would enter their directories.
        let mut scans = Vec::new();
        for (start, scanned) in std::mem::take(&mut state.scans) {
            let rows = Arc::clone(&scanned.rows);
            scans.push(scanned);
            for (index, item) in rows.items.iter().enumerate() {
                if !matches!(item, Item::Directory { .. }) {
                    continue;
                }
                match state.children.remove(&(start, index)) {
                    Some(Child::Whole { scans: below, .. }) => scans.extend(below),
                    _ => unreachable!("every child of a whole directory is whole"),
                }
            }
        }
        Some(Child::Whole {
            bytes: state.bytes,
            count: state.count,
            scans,
        })
    }

    // Runs the sequencer of `node` as far as it can go.
    fn sequence(&self, node: &Arc<Node>) {
        let mut state = Self::lock_node(node);
        state.queued = false;
        if state.stage == Stage::Done {
            return;
        }
        let mut cut = state
            .cut
            .take()
            .expect("a sequencer for a node that is over");
        if let Some(head) = cut.waits_for.take() {
            self.lock().heads.remove(&head);
        }
        let done = self.cut(node, &mut state, &mut cut);
        if let Some(head) = cut.waits_for.clone() {
            // Workers that stopped at the limit may scan below the new head.
            let mut all = self.lock();
            all.heads.insert(head);
            let wake = all.idle > 0;
            drop(all);
            if wake {
                self.work.notify_all();
            }
        }
        state.cut = Some(cut);
        if done {
            state.stage = Stage::Done;
            // What is left belongs to parts now, or to nothing.
            state.scans.clear();
            state.children.clear();
            drop(state);
            self.open.fetch_sub(1, Ordering::Relaxed);
        } else {
            drop(state);
        }
        self.announce();
    }

    // Goes through the children of `node` in walk order and groups them. It
    // returns `false` where it has to wait: for a scan, for a child to make
    // up its mind, or for the pending rows. Returns `true` at the end.
    fn cut(&self, node: &Arc<Node>, state: &mut NodeState, cut: &mut Cut) -> bool {
        loop {
            let Some(scanned) = state.scans.get(&cut.start) else {
                cut.waits_for = Some(key_below(&node.key, cut.start, false));
                return false;
            };
            let rows = Arc::clone(&scanned.rows);
            let next = scanned.next;
            // Under `Fail` the scan stopped at what it could not read.
            let fails_at = state
                .failure
                .as_ref()
                .filter(|(place, _)| place.0 == cut.start)
                .map(|(place, _)| place.1);

            while cut.item < rows.items.len() {
                let index = cut.item;
                if fails_at == Some(index) {
                    let (_, report) = state.failure.take().expect("checked above");
                    return self.fail(state, report);
                }
                let item = &rows.items[index];
                match *item {
                    Item::Skipped { .. } | Item::Failed { .. } => {}
                    Item::File { name, .. } => {
                        let bytes = estimate(EntryKind::File, rows.text(name), "");
                        self.add(node, state, cut, &rows, index, None, bytes);
                    }
                    Item::Symlink { name, target, .. } => {
                        let bytes =
                            estimate(EntryKind::Symlink, rows.text(name), rows.text(target));
                        self.add(node, state, cut, &rows, index, None, bytes);
                    }
                    Item::Directory { name, place, .. } => {
                        let row = estimate(EntryKind::Directory, rows.text(name), "");
                        match state.children.remove(&(cut.start, index)) {
                            None => {
                                cut.waits_for = Some(key_below(&node.key, place, true));
                                return false;
                            }
                            Some(Child::Failed(report)) => return self.fail(state, report),
                            Some(Child::Whole {
                                bytes,
                                count,
                                scans,
                            }) => {
                                let subtree = (count > 0).then(|| Subtree {
                                    scans,
                                    depth: node.depth + 1,
                                    count,
                                });
                                self.add(node, state, cut, &rows, index, subtree, row + bytes);
                            }
                            Some(Child::Over(child)) => {
                                if !self.hand_over(node, state, cut, &rows, index, &child) {
                                    state
                                        .children
                                        .insert((cut.start, index), Child::Over(child));
                                    return false;
                                }
                            }
                        }
                    }
                }
                cut.item += 1;
            }
            // A failure that is not at an item is the directory's own: it
            // could not be listed.
            if fails_at.is_some() {
                let (_, report) = state.failure.take().expect("checked above");
                return self.fail(state, report);
            }
            state.scans.remove(&cut.start);
            let Some(next) = next else {
                break;
            };
            (cut.start, cut.item) = (next, 0);
        }
        // The open group is the last part. A directory with nothing in a part
        // yet still owes the pending rows one.
        if !cut.group.is_empty() || !cut.carried {
            self.emit(node, state, cut);
        }
        true
    }

    fn fail(&self, state: &mut NodeState, report: Report<WalkError>) -> bool {
        match self.options.order {
            PartOrder::Walk => state.slots.push(Slot::Failed(Some(report))),
            PartOrder::Completion => self.lock().built.push_back(Err(report)),
        }
        true
    }

    // Adds a child to the open group: the row at `index`, and the subtree
    // below it if it is a directory with one. A child that does not fit
    // closes the group first, unless the group is empty.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &self,
        node: &Arc<Node>,
        state: &mut NodeState,
        cut: &mut Cut,
        rows: &Arc<Rows>,
        index: usize,
        subtree: Option<Subtree>,
        bytes: u64,
    ) {
        if !cut.group.is_empty() && cut.group_bytes + bytes > self.options.budget {
            self.emit(node, state, cut);
        }
        push_row(&mut cut.group, rows, index, node.depth);
        cut.group.extend(subtree.map(Piece::Subtree));
        cut.group_bytes += bytes;
    }

    // Makes the open group a part, with the pending rows ahead of it if they
    // are still here.
    fn emit(&self, node: &Arc<Node>, state: &mut NodeState, cut: &mut Cut) {
        let group = std::mem::take(&mut cut.group);
        cut.group_bytes = 0;
        if cut.carried {
            self.plan_part(state, node.stem(), group);
            return;
        }
        cut.carried = true;
        if let Some(pending) = state.pending.take() {
            let mut pieces = pending.pieces;
            pieces.extend(group);
            self.plan_part(state, pending.stem, pieces);
        } else {
            // The parent's sequencer has not come this far. The part keeps
            // its place, and is planned when the pending rows come.
            let number = self.planned.fetch_add(1, Ordering::Relaxed);
            state.slots.push(Slot::Part(number));
            cut.first = Some((number, group));
        }
    }

    fn plan_part(&self, state: &mut NodeState, stem: Vec<String>, pieces: Vec<Piece>) {
        // Only the root can have nothing here: a tree with no rows.
        if pieces.is_empty() {
            return;
        }
        let number = self.planned.fetch_add(1, Ordering::Relaxed);
        state.slots.push(Slot::Part(number));
        self.plan(number, Plan { stem, pieces });
    }

    // Reaches a child that is over: closes the open group, and gives the
    // child the rows that go ahead of its own. Returns `false` if those are
    // not known yet.
    fn hand_over(
        &self,
        node: &Arc<Node>,
        state: &mut NodeState,
        cut: &mut Cut,
        rows: &Arc<Rows>,
        index: usize,
        child: &Arc<Node>,
    ) -> bool {
        if !cut.group.is_empty() {
            self.emit(node, state, cut);
        }
        let mut pending = if cut.carried {
            Pending {
                stem: node.stem(),
                pieces: Vec::new(),
            }
        } else {
            // This directory has nothing in a part yet, so its own pending
            // rows go along into the child's first part.
            let Some(pending) = state.pending.take() else {
                return false;
            };
            cut.carried = true;
            pending
        };
        push_row(&mut pending.pieces, rows, index, node.depth);
        state.slots.push(Slot::Child(Arc::clone(child)));

        // The child's lock inside the parent's: the way down. Nothing takes
        // two node locks on the way up.
        let mut below = Self::lock_node(child);
        let first = below.cut.as_mut().and_then(|cut| cut.first.take());
        if let Some((number, group)) = first {
            pending.pieces.extend(group);
            drop(below);
            let plan = Plan {
                stem: pending.stem,
                pieces: pending.pieces,
            };
            self.plan(number, plan);
        } else {
            below.pending = Some(pending);
            self.schedule(child, &mut below);
        }
        true
    }
}

impl Node {
    // The names of the directories above this directory's entries.
    fn stem(&self) -> Vec<String> {
        if self.path.is_empty() {
            return Vec::new();
        }
        self.path.split('/').map(str::to_owned).collect()
    }
}

// The node of the directory whose row is item `index` of `rows`, a scan of
// `parent` that `place` names.
fn node_below(parent: &Arc<Node>, place: Place, rows: &Rows, index: usize) -> Arc<Node> {
    let item = &rows.items[index];
    let Item::Directory { place: listed, .. } = *item else {
        unreachable!("the caller gives a directory");
    };
    Arc::new(Node {
        parent: Some((Arc::clone(parent), place)),
        key: key_below(&parent.key, listed, true),
        path: job_path(&parent.path, rows, item),
        depth: parent.depth + 1,
        row_bytes: row_estimate(rows, item),
        state: Mutex::default(),
    })
}

fn joined(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        return name.to_owned();
    }
    format!("{directory}/{name}")
}

fn job_path(parent: &str, rows: &Rows, item: &Item) -> String {
    let Item::Directory { name, .. } = *item else {
        unreachable!("filtered by the caller");
    };
    joined(parent, rows.text(name))
}

fn row_estimate(rows: &Rows, item: &Item) -> u64 {
    let Item::Directory { name, .. } = *item else {
        unreachable!("filtered by the caller");
    };
    estimate(EntryKind::Directory, rows.text(name), "")
}

// Adds item `index` of `rows`, an entry of a directory at `depth`, to `pieces`.
fn push_row(pieces: &mut Vec<Piece>, rows: &Arc<Rows>, index: usize, depth: usize) {
    // The last segment goes on if nothing came between: a subtree would be a
    // piece of its own.
    if let Some(Piece::Rows(last)) = pieces.last_mut()
        && Arc::ptr_eq(&last.rows, rows)
        && last.depth == depth
    {
        last.to = index + 1;
        last.count += 1;
        return;
    }
    pieces.push(Piece::Rows(Segment {
        rows: Arc::clone(rows),
        from: index,
        to: index + 1,
        depth,
        count: 1,
    }));
}

// Tells the calling thread that a worker panicked, so that it does not wait
// for what will not come. Forgotten when the work returns.
struct BrokenOnPanic<'e, 'a>(&'e Engine<'a>);

impl Drop for BrokenOnPanic<'_, '_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.broken = true;
        state.closed = true;
        state.events += 1;
        drop(state);
        self.0.ready.notify_all();
        self.0.work.notify_all();
    }
}
