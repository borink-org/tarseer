// Cutting: the walk in walk order, which decides where parts end. It runs on
// the calling thread. Without threads it reads each directory itself when it
// comes to it.
// With threads it takes what the workers of a `Pool` have read ahead, and the
// workers build the parts it plans.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use error_stack::{Report, ResultExt as _};

use super::assemble::{Piece, Plan, Segment, Subtree};
use super::pool::{Event, Pool, Slot};
use super::reader::Scratch;
use super::scan::{Item, Job, Scanned, Scanner, joined};
use super::{Cancelled, OnError, WalkError, WalkOptions, estimate};
use crate::manifest::{EntryKind, TreePart};

pub(super) type Sink<'s> = dyn FnMut(TreePart) -> Result<(), Report<WalkError>> + 's;

/// What the walk takes in for a directory, or for a later scan of a large one.
pub(super) enum Unit {
    /// One scan of a directory, and where the unit of each subdirectory among
    /// its items comes from, in order. For a directory's first scan, also the
    /// later scans of the rest of it.
    Dir {
        scanned: Scanned,
        subdirs: Vec<Next>,
        rest: Vec<Next>,
    },
    /// A directory whose subtree, with its own row, fits a part, read to its
    /// end: the scans of its directories in walk order, its own first, and the
    /// estimate and number of the rows below it.
    Whole {
        scans: Vec<Scanned>,
        bytes: u64,
        count: usize,
    },
}

/// Where a unit comes from.
pub(super) enum Next {
    /// Nothing has read it: the walk reads it when it comes to it.
    Job(Job),
    /// A worker reads it, or has.
    Slot(Arc<Slot>),
    /// Read with its parent.
    Ready(Box<Unit>),
}

// An open directory. Row positions are absolute: the first row of the walk is
// at 0.
struct Level {
    scanned: Scanned,
    // The next item of `scanned`.
    next: usize,
    // The units of the subdirectories among the items not visited yet, and
    // those of the rest of this directory.
    subdirs: std::vec::IntoIter<Next>,
    rest: std::vec::IntoIter<Next>,
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
    // Measuring only: where this level's finished children start in
    // `Walker::completed`.
    completed: usize,
}

pub(super) struct Walker<'w> {
    options: &'w WalkOptions<'w>,
    scanner: &'w Scanner<'w>,
    pool: Option<&'w Pool<'w>>,
    sink: &'w mut Sink<'w>,
    // The rows that are not in a part yet, in walk order. The walk keeps where
    // they are and never copies them.
    waiting: VecDeque<Piece>,
    // The position of the first waiting row, and of the next row to come.
    base: usize,
    position: usize,
    // Running estimate of every row so far.
    total: u64,
    path: String,
    stack: Vec<Level>,
    // Index of the outermost measuring level; `stack.len()` if none.
    measuring: usize,
    // Each finished child of the measuring levels, its position and estimate,
    // the outer levels' first.
    completed: Vec<(usize, u64)>,
    scratch: Scratch,
    // Parts planned and parts given to the sink.
    planned: usize,
    delivered: usize,
}

impl<'w> Walker<'w> {
    pub fn new(
        options: &'w WalkOptions<'w>,
        scanner: &'w Scanner<'w>,
        pool: Option<&'w Pool<'w>>,
        sink: &'w mut Sink<'w>,
    ) -> Self {
        Self {
            options,
            scanner,
            pool,
            sink,
            waiting: VecDeque::new(),
            base: 0,
            position: 0,
            total: 0,
            path: String::new(),
            stack: Vec::new(),
            measuring: 1,
            completed: Vec::new(),
            scratch: Scratch::default(),
            planned: 0,
            delivered: 0,
        }
    }

    pub fn run(&mut self, root: &Path) -> Result<(), Report<WalkError>> {
        let first = match self.pool {
            Some(pool) => Next::Slot(pool.root()),
            None => Next::Job(Job::root()),
        };
        let Unit::Dir {
            mut scanned,
            subdirs,
            rest,
        } = self.take(first)?
        else {
            unreachable!("the root is never read as one unit");
        };
        // An error under either policy: counting the root as a skip would
        // report an empty walk as a success.
        if let Some(error) = scanned.unreadable.take() {
            return Err(error)
                .attach_with(|| format!("listing {}", root.display()))
                .change_context(WalkError);
        }
        self.stack.push(Level {
            next: scanned.items.start,
            scanned,
            subdirs: subdirs.into_iter(),
            rest: rest.into_iter(),
            path_len: 0,
            row: 0,
            start_total: 0,
            split: true,
            group_first: 0,
            group_bytes: 0,
            completed: 0,
        });

        while let Some(top) = self.stack.last_mut() {
            let index = top.next;
            if index == top.scanned.items.end {
                if top.rest.len() > 0 {
                    self.read_on()?;
                } else {
                    self.finish_level()?;
                }
                continue;
            }
            top.next += 1;
            if self
                .options
                .cancel
                .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
            {
                return Err(Report::new(Cancelled).change_context(WalkError));
            }
            self.visit(index)?;
        }
        // Every part that is planned goes to the sink before the walk ends.
        while self.delivered < self.planned {
            self.wait(None)?;
        }
        Ok(())
    }

    // Handles item `index` of the top level.
    fn visit(&mut self, index: usize) -> Result<(), Report<WalkError>> {
        let level = self.stack.len() - 1;
        let top = &mut self.stack[level];
        match top.scanned.rows.items[index] {
            Item::File { name, .. } => {
                let bytes = estimate(EntryKind::File, "", "") + name.len() as u64;
                self.leaf(level, index, bytes)
            }
            Item::Symlink { name, target, .. } => {
                let bytes =
                    estimate(EntryKind::Symlink, "", "") + (name.len() + target.len()) as u64;
                self.leaf(level, index, bytes)
            }
            Item::Directory { .. } => self.enter(level, index),
            // Counted by the scan.
            Item::Skipped { .. } => Ok(()),
            Item::Failed { .. } if self.options.on_error == OnError::Skip => Ok(()),
            Item::Failed { name, what, error } => {
                let error = top.scanned.errors[error as usize]
                    .take()
                    .expect("each error is reported once");
                self.path.truncate(top.path_len);
                let path = match name {
                    Some(name) => joined(&self.path, top.scanned.rows.text(name)),
                    None => self.path.clone(),
                };
                Err(error)
                    .attach_with(|| format!("{what} of {path}"))
                    .change_context(WalkError)
            }
        }
    }

    // A file or symlink row.
    fn leaf(&mut self, level: usize, index: usize, bytes: u64) -> Result<(), Report<WalkError>> {
        self.extend(level, index);
        let position = self.position;
        self.position += 1;
        self.total += bytes;
        self.finished(level, position, bytes)?;
        self.check_budget()
    }

    // Enters the directory at item `index` of `level`.
    fn enter(&mut self, level: usize, index: usize) -> Result<(), Report<WalkError>> {
        let top = &mut self.stack[level];
        let next = top
            .subdirs
            .next()
            .expect("a unit for every directory a scan found");
        let Item::Directory { name, .. } = top.scanned.rows.items[index] else {
            unreachable!("the caller found a directory");
        };
        let name_len = name.len();
        let parent_len = top.path_len;
        self.path.truncate(parent_len);
        if parent_len > 0 {
            self.path.push('/');
        }
        self.path.push_str(top.scanned.rows.text(name));
        let bytes = estimate(EntryKind::Directory, "", "") + name_len as u64;
        self.extend(level, index);
        let row = self.position;
        self.position += 1;

        match self.take(next)? {
            // Nothing below it can be cut, so it goes into its parent as one.
            Unit::Whole {
                scans,
                bytes: below,
                count,
            } => {
                self.position += count;
                self.total += bytes + below;
                if count > 0 {
                    self.waiting.push_back(Piece::Subtree(Subtree {
                        scans,
                        depth: level + 1,
                        count,
                    }));
                }
                self.finished(level, row, bytes + below)?;
                self.check_budget()
            }
            Unit::Dir {
                mut scanned,
                subdirs,
                rest,
            } => {
                // Under `Skip` the scan counted it, and it holds nothing.
                if let Some(error) = scanned.unreadable.take()
                    && self.options.on_error == OnError::Fail
                {
                    return Err(error)
                        .attach_with(|| format!("listing {}", self.path))
                        .change_context(WalkError);
                }
                self.stack.push(Level {
                    next: scanned.items.start,
                    scanned,
                    subdirs: subdirs.into_iter(),
                    rest: rest.into_iter(),
                    path_len: self.path.len(),
                    row,
                    start_total: self.total,
                    split: false,
                    group_first: 0,
                    group_bytes: 0,
                    completed: self.completed.len(),
                });
                self.total += bytes;
                self.check_budget()
            }
        }
    }

    // The unit of `next`, from wherever it comes.
    fn take(&mut self, next: Next) -> Result<Unit, Report<WalkError>> {
        match next {
            Next::Ready(unit) => Ok(*unit),
            Next::Slot(slot) => loop {
                if let Some(unit) = self.wait(Some(&slot))? {
                    return Ok(unit);
                }
            },
            Next::Job(job) => {
                let scanner = self.scanner;
                let stack = &mut self.stack;
                // With no handle left, close the directories that the jobs of
                // the open levels hold.
                let found = scanner.scan(job, &self.path, &mut self.scratch, &mut || {
                    let mut released = false;
                    for level in stack.iter_mut() {
                        let (subdirs, rest) =
                            (level.subdirs.as_mut_slice(), level.rest.as_mut_slice());
                        for next in subdirs.iter_mut().chain(rest) {
                            if let Next::Job(job) = next {
                                released |= job.release(&scanner.held);
                            }
                        }
                    }
                    released
                });
                Ok(Unit::Dir {
                    scanned: found.scanned,
                    subdirs: found.directories.into_iter().map(Next::Job).collect(),
                    rest: found.rest.into_iter().map(Next::Job).collect(),
                })
            }
        }
    }

    // Waits for `slot` to be filled, or with no slot for the next part, and
    // gives the sink each part that is ready meanwhile. Returns the unit once
    // it is there.
    fn wait(&mut self, slot: Option<&Arc<Slot>>) -> Result<Option<Unit>, Report<WalkError>> {
        let pool = self.pool.expect("only a walk with threads waits");
        match pool.wait(slot, self.delivered, self.measuring) {
            Event::Unit(unit) => Ok(Some(unit)),
            Event::Part(built) => {
                self.deliver(pool, built)?;
                Ok(None)
            }
            // Raising the flag wakes nobody, so a wait looks at it now and then.
            Event::Nothing => match self.options.cancel {
                Some(cancel) if cancel.load(Ordering::Relaxed) => {
                    Err(Report::new(Cancelled).change_context(WalkError))
                }
                _ => Ok(None),
            },
        }
    }

    fn deliver(
        &mut self,
        pool: &Pool<'_>,
        built: Result<TreePart, Report<WalkError>>,
    ) -> Result<(), Report<WalkError>> {
        let part = built?;
        let rows = part.len();
        self.delivered += 1;
        (self.sink)(part)?;
        pool.delivered(rows);
        Ok(())
    }

    // Replaces the scan of the top level, which the walk has used up, with
    // the scan of the rest of the same directory.
    fn read_on(&mut self) -> Result<(), Report<WalkError>> {
        let level = self.stack.len() - 1;
        self.path.truncate(self.stack[level].path_len);
        let next = self.stack[level]
            .rest
            .next()
            .expect("a unit for the rest of a directory that has more");
        let Unit::Dir {
            scanned, subdirs, ..
        } = self.take(next)?
        else {
            unreachable!("a later scan of a directory is never one unit");
        };
        let top = &mut self.stack[level];
        top.next = scanned.items.start;
        top.scanned = scanned;
        top.subdirs = subdirs.into_iter();
        Ok(())
    }

    // Adds item `index` of the top level to the waiting rows.
    fn extend(&mut self, depth: usize, index: usize) {
        let rows = &self.stack[depth].scanned.rows;
        // The last segment goes on if it ends right before this item of the
        // same directory. Scans of several directories can share their rows.
        if let Some(Piece::Rows(last)) = self.waiting.back_mut()
            && last.to == index
            && last.depth == depth
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

    // A child of `level` is finished: its row at `position`, and everything
    // below it, `bytes` in all.
    fn finished(
        &mut self,
        level: usize,
        position: usize,
        bytes: u64,
    ) -> Result<(), Report<WalkError>> {
        if self.stack[level].split {
            self.add_to_group(level, position, bytes)
        } else {
            self.completed.push((position, bytes));
            Ok(())
        }
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
        // Its finished children, which the levels below it follow in
        // `completed`.
        let from = this.completed;
        let to = self
            .stack
            .get(level + 1)
            .map_or(self.completed.len(), |below| below.completed);
        let children: Vec<(usize, u64)> = self.completed.drain(from..to).collect();
        for below in &mut self.stack[level + 1..] {
            below.completed -= to - from;
        }
        for (position, bytes) in children {
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
        self.completed.truncate(done.completed);
        self.measuring = self.measuring.min(level);
        if done.split {
            let parent = &mut self.stack[level - 1];
            parent.group_first = end;
            parent.group_bytes = 0;
        } else {
            self.finished(level - 1, done.row, self.total - done.start_total)?;
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
        let plan = Plan { stem, pieces };
        let Some(pool) = self.pool else {
            return (self.sink)(plan.assemble().change_context(WalkError)?);
        };
        pool.plan(self.planned, plan);
        self.planned += 1;
        // The parts built by now go to the sink before more are planned.
        while let Some(built) = pool.built(self.delivered) {
            self.deliver(pool, built)?;
        }
        Ok(())
    }
}
