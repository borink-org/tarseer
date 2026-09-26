// Building a part from the rows that scans found. The walk decides which rows
// make up a part, and writes that down as a plan. Building the part from the
// plan needs nothing else, so any thread can do it.

use std::sync::Arc;

use error_stack::Report;

use super::scan::{Item, Rows, Scanned};
use crate::manifest::{TreePart, TreePartFull};

/// Rows that follow one another in walk order: the items `from..to` of one
/// scan, which are entries of one directory.
pub(super) struct Segment {
    pub rows: Arc<Rows>,
    pub from: usize,
    pub to: usize,
    /// How many directories lie between the root and these entries.
    pub depth: usize,
    /// How many of the items are rows. The others are entries the walk
    /// skipped.
    pub count: usize,
}

impl Segment {
    /// Splits off the first `count` rows. `self` keeps the rest.
    pub fn split_off_front(&mut self, count: usize) -> Self {
        let mut seen = 0;
        let mut at = self.from;
        while seen < count {
            seen += usize::from(self.rows.items[at].is_row());
            at += 1;
        }
        let front = Self {
            rows: Arc::clone(&self.rows),
            from: self.from,
            to: at,
            depth: self.depth,
            count,
        };
        self.from = at;
        self.count -= count;
        front
    }
}

/// A whole subtree that the walk took as one: the scans of its directories in
/// walk order, the root's first. The root's own row is not among them, since
/// the scan of its parent found that.
pub(super) struct Subtree {
    pub scans: Vec<Scanned>,
    /// How many directories lie between the root of the walk and the entries
    /// of the subtree's root.
    pub depth: usize,
    /// How many rows the subtree holds.
    pub count: usize,
}

/// What a part is made of, piece by piece in walk order.
pub(super) enum Piece {
    Rows(Segment),
    Subtree(Subtree),
}

impl Piece {
    pub fn count(&self) -> usize {
        match self {
            Self::Rows(segment) => segment.count,
            Self::Subtree(subtree) => subtree.count,
        }
    }

    /// The depth of the piece's first row.
    pub fn depth(&self) -> usize {
        match self {
            Self::Rows(segment) => segment.depth,
            Self::Subtree(subtree) => subtree.depth,
        }
    }
}

/// Everything a part is built from.
pub(super) struct Plan {
    /// The names of the open directories above the first row, from the root
    /// down.
    pub stem: Vec<String>,
    pub pieces: Vec<Piece>,
}

impl Plan {
    pub fn assemble(&self) -> Result<TreePart, Report<TreePartFull>> {
        // Counted first, so that the part is allocated once and never grows.
        let mut room = Room {
            bytes: self.stem.iter().map(String::len).sum(),
            ..Room::default()
        };
        for piece in &self.pieces {
            match piece {
                Piece::Rows(segment) => room.add(&segment.rows, segment.from..segment.to),
                Piece::Subtree(subtree) => {
                    for scanned in &subtree.scans {
                        room.add(&scanned.rows, scanned.items.clone());
                    }
                }
            }
        }
        let mut part = TreePart::with_capacity(
            room.directories,
            room.files,
            room.symlinks,
            self.stem.len(),
            room.bytes,
        );
        for name in &self.stem {
            part.push_stem(name)?;
        }
        // The node of the directory that holds the rows of each depth.
        let mut nodes: Vec<u32> = (0..=self.stem.len())
            .map(|node| u32::try_from(node).expect("stem depth fits a u32"))
            .collect();

        for piece in &self.pieces {
            match piece {
                Piece::Rows(segment) => {
                    for item in &segment.rows.items[segment.from..segment.to] {
                        push(&mut part, &mut nodes, &segment.rows, item, segment.depth)?;
                    }
                }
                Piece::Subtree(subtree) => unfold(&mut part, &mut nodes, subtree)?,
            }
        }
        Ok(part)
    }
}

// What a part's rows take.
#[derive(Default)]
struct Room {
    directories: usize,
    files: usize,
    symlinks: usize,
    bytes: usize,
}

impl Room {
    fn add(&mut self, rows: &Rows, items: std::ops::Range<usize>) {
        for item in &rows.items[items] {
            match *item {
                Item::Directory { name, .. } => {
                    self.directories += 1;
                    self.bytes += name.len();
                }
                Item::File { name, .. } => {
                    self.files += 1;
                    self.bytes += name.len();
                }
                Item::Symlink { name, target, .. } => {
                    self.symlinks += 1;
                    self.bytes += name.len() + target.len();
                }
                Item::Skipped { .. } | Item::Failed { .. } => {}
            }
        }
    }
}

// Adds `item`, an entry of the directory at `depth`, to `part`. Returns `true`
// if it is a directory.
fn push(
    part: &mut TreePart,
    nodes: &mut Vec<u32>,
    rows: &Rows,
    item: &Item,
    depth: usize,
) -> Result<bool, Report<TreePartFull>> {
    let parent = nodes[depth];
    match *item {
        Item::Directory {
            name, mtime, mode, ..
        } => {
            let node = part.push_directory(parent, rows.text(name), mtime, mode)?;
            nodes.truncate(depth + 1);
            nodes.push(node);
            return Ok(true);
        }
        Item::File {
            name,
            size,
            mtime,
            mode,
        } => part.push_file(parent, rows.text(name), size, mtime, mode)?,
        Item::Symlink {
            name,
            target,
            mtime,
            directory,
        } => part.push_symlink(parent, rows.text(name), rows.text(target), mtime, directory)?,
        Item::Skipped { .. } | Item::Failed { .. } => {}
    }
    Ok(false)
}

// Adds the rows of a whole subtree in walk order. Its scans are in the order
// the walk would enter their directories, so a directory's scan is the next
// one that has not been read. No recursion: a subtree may be very deep.
fn unfold(
    part: &mut TreePart,
    nodes: &mut Vec<u32>,
    subtree: &Subtree,
) -> Result<(), Report<TreePartFull>> {
    // The directories being read: the scan, the next item in it, the depth.
    let first = subtree
        .scans
        .first()
        .map_or(0, |scanned| scanned.items.start);
    let mut open = vec![(0, first, subtree.depth)];
    let mut unread = 1;
    while let Some((scan, next, depth)) = open.last_mut() {
        let scanned = &subtree.scans[*scan];
        let Some(item) = scanned
            .items
            .contains(next)
            .then(|| &scanned.rows.items[*next])
        else {
            // The rest of a large directory is a scan of its own.
            if scanned.next.is_some() {
                (*scan, *next) = (unread, subtree.scans[unread].items.start);
                unread += 1;
            } else {
                open.pop();
            }
            continue;
        };
        *next += 1;
        let depth = *depth;
        if push(part, nodes, &scanned.rows, item, depth)? {
            open.push((unread, subtree.scans[unread].items.start, depth + 1));
            unread += 1;
        }
    }
    Ok(())
}
