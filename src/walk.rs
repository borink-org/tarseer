//! The source walk: every entry under a root, deterministically ordered.
//!
//! # Two passes, and why the parallel one is by directory
//!
//! Scanning time is almost all per-entry `stat`. Measured on a 181,240-entry
//! tree, enumeration is 26 ms and the `stat`s a further 105 ms. So the scan
//! runs on a thread pool — but at the granularity of a whole **directory**,
//! which is not the obvious choice and is the only one that pays:
//!
//! ```text
//! serial                        135 ms
//! one task per stat             194 ms   (worse — the syscall is ~0.5 us and
//!                                         per-item fan-out costs more than that)
//! one task per directory         14 ms
//! ```
//!
//! Levels are then walked breadth-first, one `par_iter` per level over the
//! whole frontier, rather than recursing inside each task: recursion would mean
//! a task blocking on its children's `par_iter`, which lets rayon run another
//! such task on the same stack, and that nests without bound. A frontier of one
//! falls back to spreading `stat`s *within* the directory, in `STAT_CHUNK`
//! chunks — the only parallelism a single flat directory of 100,000 files
//! offers.
//!
//! # Order
//!
//! `flatten` is serial and depth-first over name-sorted entries, so the
//! result is exactly what a plain recursive sorted walk would produce — a
//! guarantee, because downstream layouts depend on it. Node order needs no
//! recorded index: rayon's `fold` keeps a task's directories in the order it
//! was handed them and `collect` returns tasks in split order, so concatenating
//! the `Out`s *is* frontier order.
//!
//! # Per-task storage
//!
//! A directory costs appends, not allocations: its task owns the arenas and
//! vectors in `Out`. `kids` holds *full* paths because the next level opens
//! them and the frontier borrows them directly, so it outlives the frontier by
//! exactly one level and is freed as soon as that level has been scanned.

use std::path::Path;

use rayon::prelude::*;

use crate::arena::{Arena, Span};
use crate::error::{Context, Result};
use crate::tree::{Skips, SourceTree};

/// Entries per task when a single wide directory spreads its own `stat` calls.
/// Big enough to amortize the fan-out that per-item parallelism fails to.
const STAT_CHUNK: usize = 512;

/// Directories per task when a level has many. Small, because a task is cheap
/// and directories vary wildly in size.
const SCAN_CHUNK: usize = 4;

/// Walk `root` recursively and deterministically, collecting files,
/// directories and symlinks with relative forward-slash paths.
///
/// Entries that cannot be recorded are counted in [`SourceTree::skips`] rather
/// than failing the walk — an unreadable directory costs its subtree, but the
/// rest of the tree still gets scanned, which is what makes walking `$HOME` or
/// `/var` possible at all.
///
/// # Errors
/// If the root's path is not UTF-8, if a directory entry's type or metadata
/// cannot be read, or if the tree outgrows the `u32` indices used throughout.
pub fn walk(root: &Path) -> Result<SourceTree> {
    let root_str = root
        .to_str()
        .ctx(|| format!("non-Unicode source path {}", root.display()))?;

    let mut levels: Vec<Vec<Out>> = Vec::new();
    let mut nodes: Vec<NodeAt> = Vec::new();

    let mut root_out = Out::default();
    scan_one(root_str, true, &mut root_out)?;
    root_out.release_scratch();
    levels.push(vec![root_out]);
    nodes.push(NodeAt {
        level: 0,
        task: 0,
        meta: 0,
        children: (0, 0),
        rel: 0,
    });

    let root_prefix = root_str.len() + usize::from(!root_str.ends_with('/'));
    let mut level_start = 0usize;
    loop {
        let level_end = nodes.len();
        let base = u32::try_from(level_end).ctx(|| "source tree exceeds u32 directories".into())?;
        let mut frontier: Vec<&str> = Vec::new();
        for node in &mut nodes[level_start..level_end] {
            let out = &levels[node.level as usize][node.task as usize];
            let meta = &out.metas[node.meta as usize];
            let first = u32::try_from(frontier.len()).ctx(|| "frontier exceeds u32".into())?;
            for k in meta.kids.0..meta.kids.1 {
                frontier.push(out.kid(k as usize));
            }
            let count =
                u32::try_from(frontier.len()).ctx(|| "frontier exceeds u32".into())? - first;
            node.children = (base + first, count);
        }
        if frontier.is_empty() {
            break;
        }

        // Taken here because this is the one moment these paths are still
        // alive: the frontier borrows `kids`, which the next iteration frees.
        let rels: Vec<u32> = frontier
            .iter()
            .map(|p| u32::try_from(p.len() - root_prefix).ctx(|| "source path exceeds u32".into()))
            .collect::<Result<_>>()?;

        let mut outs = scan_frontier(&frontier)?;
        for out in &mut outs {
            out.release_scratch();
        }
        drop(frontier);
        if let Some(scanned) = levels.last_mut() {
            for out in scanned {
                out.release_kids();
            }
        }

        let level_no = u32::try_from(levels.len()).ctx(|| "source tree too deep".into())?;
        let mut rel = rels.iter();
        for (task, out) in outs.iter().enumerate() {
            let task = u32::try_from(task).ctx(|| "too many scan tasks".into())?;
            for slot in 0..out.metas.len() {
                let meta = u32::try_from(slot).ctx(|| "too many directories in a task".into())?;
                let rel = *rel
                    .next()
                    .ctx(|| "scan returned more directories than it was given".into())?;
                nodes.push(NodeAt {
                    level: level_no,
                    task,
                    meta,
                    children: (0, 0),
                    rel,
                });
            }
        }
        levels.push(outs);
        level_start = level_end;
    }

    let mut tree = size_tree(&levels, &nodes);
    let mut rel = String::new();
    flatten(&levels, &nodes, 0, &mut rel, &mut tree)?;
    Ok(tree)
}

/// Count what the tasks kept, so the tree allocates once.
///
/// The text total is interned name bytes plus one parent-path-and-separator per
/// entry, which is why `NodeAt` carries its directory's `rel` length.
fn size_tree(levels: &[Vec<Out>], nodes: &[NodeAt]) -> SourceTree {
    let mut links = 0usize;
    let mut found = 0usize;
    let mut text_bytes = 0usize;
    for out in levels.iter().flatten() {
        links += out.links as usize;
        found += out.entries.len();
        text_bytes += out.names.bytes() + out.targets.bytes();
    }
    for node in nodes {
        let out = &levels[node.level as usize][node.task as usize];
        let meta = &out.metas[node.meta as usize];
        let entries = (meta.entries.1 - meta.entries.0) as usize;
        text_bytes += entries
            * if node.rel == 0 {
                0
            } else {
                node.rel as usize + 1
            };
    }
    let dirs = nodes.len() - 1;
    let files = found - dirs - links;
    SourceTree::with_capacity(files, dirs, links, text_bytes)
}

fn scan_frontier(frontier: &[&str]) -> Result<Vec<Out>> {
    if let [only] = *frontier {
        let mut out = Out::default();
        scan_one(only, true, &mut out)?;
        return Ok(vec![out]);
    }

    (0..frontier.len())
        .into_par_iter()
        .with_min_len(SCAN_CHUNK)
        .fold(
            || Ok(Out::default()),
            |acc: Result<Out>, i| {
                let mut out = acc?;
                scan_one(frontier[i], false, &mut out)?;
                Ok(out)
            },
        )
        .collect::<Result<Vec<Out>>>()
}

/// One entry as the scan found it, before it has a path.
enum Found {
    File {
        name: Span,
        mtime: i64,
        size: u64,
        mode: u32,
    },
    Dir {
        name: Span,
        mtime: i64,
        mode: u32,
    },
    Link {
        name: Span,
        target: Span,
        mtime: i64,
    },
}

/// What one `stat` yielded, before it is sorted into a `Found`.
#[derive(Clone, Copy, Default)]
struct Stat {
    mtime: i64,
    size: u64,
    mode: u32,
}

/// One directory's place in the buffers its task produced.
struct Meta {
    /// Range into [`Out::entries`], sorted by name.
    entries: (u32, u32),
    /// Range into [`Out::kid_spans`], in `Found::Dir` order.
    kids: (u32, u32),
    skips: Skips,
}

/// Everything one task produced while scanning its slice of one level.
#[derive(Default)]
struct Out {
    /// Every entry name of every directory this task scanned.
    names: Arena,
    /// Every entry, contiguous per directory.
    entries: Vec<Found>,
    /// Subdirectory paths; the next level's frontier borrows these.
    kids: Arena,
    /// Symlink targets. Its own arena rather than a corner of `names` because
    /// it is nearly always empty and is read by exactly one match arm.
    targets: Arena,
    /// One span per subdirectory path, in the order the directories were found.
    kid_spans: Vec<Span>,
    /// One per directory this task scanned.
    metas: Vec<Meta>,
    /// Symlinks found. Kept as a count because `entries` cannot be asked
    /// without walking it, and sizing the tree needs it: a link is the one row
    /// that interns two strings.
    links: u32,
    /// Reusable buffer for the one path a symlink still needs built.
    tmp: String,
}

impl Out {
    /// The text of a name this task's scan interned. The sole resolver, so a
    /// span cannot be read against the wrong arena.
    fn name(&self, span: Span) -> &str {
        self.names.get(span)
    }

    fn target(&self, span: Span) -> &str {
        self.targets.get(span)
    }

    /// The *i*-th subdirectory path, `i` indexing `kid_spans`.
    fn kid(&self, i: usize) -> &str {
        self.kids.get(self.kid_spans[i])
    }

    fn release_scratch(&mut self) {
        self.tmp = String::new();
    }

    fn release_kids(&mut self) {
        self.kids = Arena::default();
        self.kid_spans = Vec::new();
    }
}

/// Where a node's data lives, plus where its children are.
#[derive(Clone, Copy)]
struct NodeAt {
    level: u32,
    task: u32,
    meta: u32,
    children: (u32, u32),
    /// Length of this directory's path relative to the root.
    rel: u32,
}

#[derive(Clone, Copy)]
struct Dir<'a> {
    out: &'a Out,
    meta: &'a Meta,
    children: (u32, u32),
}

impl<'a> Dir<'a> {
    fn at(levels: &'a [Vec<Out>], nodes: &[NodeAt], id: u32) -> Self {
        let node = nodes[id as usize];
        let out = &levels[node.level as usize][node.task as usize];
        Self {
            out,
            meta: &out.metas[node.meta as usize],
            children: node.children,
        }
    }

    fn entries(&self) -> &'a [Found] {
        &self.out.entries[self.meta.entries.0 as usize..self.meta.entries.1 as usize]
    }
}

/// Enumerate exactly one directory: its entries, sorted, plus the paths of its
/// subdirectories in the same order.
///
/// `wide` says this is the only directory being scanned right now, so it is
/// worth spreading its own `stat`s across the pool.
fn scan_one(dir: &str, wide: bool, out: &mut Out) -> Result<()> {
    let dir_str = dir;
    let dir = Path::new(dir);
    let stat_of = |e: &std::fs::DirEntry, ft: std::fs::FileType| -> Result<Stat> {
        if !(ft.is_file() || ft.is_dir()) {
            return Ok(Stat::default());
        }
        let m = e
            .metadata()
            .ctx(|| format!("metadata {}", e.path().display()))?;
        Ok(Stat {
            mtime: m.modified().ok().map_or(0, system_time_to_unix),
            size: if ft.is_file() { m.len() } else { 0 },
            mode: mode_of(&m, ft.is_dir()),
        })
    };

    let entries_from = u32::try_from(out.entries.len()).ctx(|| "entry arena overflow".into())?;
    let kids_from = u32::try_from(out.kid_spans.len()).ctx(|| "child arena overflow".into())?;

    // A directory that cannot be read costs its whole subtree, but it is
    // counted and the rest of the tree is still walked.
    let Ok(rd) = std::fs::read_dir(dir) else {
        return finish(
            out,
            entries_from,
            kids_from,
            Skips {
                unreadable: 1,
                ..Skips::default()
            },
        );
    };

    let mut ents: Vec<(std::ffi::OsString, std::fs::FileType, std::fs::DirEntry)> = Vec::new();
    for e in rd {
        let e = e.ctx(|| format!("dir entry in {}", dir.display()))?;
        let ft = e
            .file_type()
            .ctx(|| format!("file_type {}", e.path().display()))?;
        ents.push((e.file_name(), ft, e));
    }
    ents.sort_by(|a, b| a.0.cmp(&b.0));

    let stats: Vec<Stat> = if wide && ents.len() >= STAT_CHUNK * 2 {
        ents.par_chunks(STAT_CHUNK)
            .map(|c| {
                c.iter()
                    .map(|(_, ft, e)| stat_of(e, *ft))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    } else {
        ents.iter()
            .map(|(_, ft, e)| stat_of(e, *ft))
            .collect::<Result<_>>()?
    };

    let mut skips = Skips::default();
    for ((name, ft, _), st) in ents.into_iter().zip(stats) {
        let Some(name_str) = name.to_str() else {
            skips.non_utf8 += 1;
            continue;
        };
        let span = out.names.push(&[name_str])?;
        if ft.is_symlink() {
            join_into(&mut out.tmp, dir_str, name_str);
            match scan_link(Path::new(&out.tmp))? {
                Some((target, mtime)) => {
                    let target = out.targets.push(&[&target])?;
                    out.entries.push(Found::Link {
                        name: span,
                        target,
                        mtime,
                    });
                    out.links += 1;
                }
                None => skips.non_utf8 += 1,
            }
        } else if ft.is_dir() {
            push_kid(out, dir_str, name_str)?;
            out.entries.push(Found::Dir {
                name: span,
                mtime: st.mtime,
                mode: st.mode,
            });
        } else if ft.is_file() {
            out.entries.push(Found::File {
                name: span,
                mtime: st.mtime,
                size: st.size,
                mode: st.mode,
            });
        } else {
            skips.special += 1;
        }
    }
    finish(out, entries_from, kids_from, skips)
}

/// Close off one directory's contribution to its task's buffers.
fn finish(out: &mut Out, entries_from: u32, kids_from: u32, skips: Skips) -> Result<()> {
    let entries_to = u32::try_from(out.entries.len()).ctx(|| "entry arena overflow".into())?;
    let kids_to = u32::try_from(out.kid_spans.len()).ctx(|| "child arena overflow".into())?;
    out.metas.push(Meta {
        entries: (entries_from, entries_to),
        kids: (kids_from, kids_to),
        skips,
    });
    Ok(())
}

/// Append `dir/name` to `buf`, replacing whatever it held.
fn join_into(buf: &mut String, dir: &str, name: &str) {
    buf.clear();
    buf.push_str(dir);
    if !dir.ends_with('/') {
        buf.push('/');
    }
    buf.push_str(name);
}

/// Record a subdirectory's full path in the task's arena.
fn push_kid(out: &mut Out, dir: &str, name: &str) -> Result<()> {
    let sep = if dir.ends_with('/') { "" } else { "/" };
    let span = out.kids.push(&[dir, sep, name])?;
    out.kid_spans.push(span);
    Ok(())
}

/// Read the symlink at `abs`: its stored target and its own mtime, or `None`
/// if the target is not UTF-8 and so cannot go in the index.
fn scan_link(abs: &Path) -> Result<Option<(String, i64)>> {
    let target = std::fs::read_link(abs).ctx(|| format!("read link {}", abs.display()))?;
    let Some(target) = target.to_str() else {
        return Ok(None);
    };
    let mtime = std::fs::symlink_metadata(abs)
        .ok()
        .and_then(|m| m.modified().ok())
        .map_or(0, system_time_to_unix);
    let target = if target.contains('\\') {
        target.replace('\\', "/")
    } else {
        String::from(target)
    };
    Ok(Some((target, mtime)))
}

/// Serial, depth-first: the order everything downstream depends on.
fn flatten(
    levels: &[Vec<Out>],
    nodes: &[NodeAt],
    id: u32,
    rel: &mut String,
    tree: &mut SourceTree,
) -> Result<()> {
    let dir = Dir::at(levels, nodes, id);
    tree.skips.add(dir.meta.skips);
    let mut child = 0u32;
    for found in dir.entries() {
        let name = match found {
            Found::File { name, .. } | Found::Dir { name, .. } | Found::Link { name, .. } => name,
        };
        let mark = rel.len();
        if mark > 0 {
            rel.push('/');
        }
        rel.push_str(dir.out.name(*name));

        match found {
            Found::File {
                mtime, size, mode, ..
            } => tree.push_file(rel, *mtime, *size, *mode)?,
            Found::Link { target, mtime, .. } => {
                tree.push_link(rel, dir.out.target(*target), *mtime)?;
            }
            Found::Dir { mtime, mode, .. } => {
                tree.push_dir(rel, *mtime, *mode)?;
                flatten(levels, nodes, dir.children.0 + child, rel, tree)?;
                child += 1;
            }
        }

        rel.truncate(mark);
    }
    Ok(())
}

fn system_time_to_unix(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn mode_of(m: &std::fs::Metadata, is_dir: bool) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = is_dir;
        m.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (is_dir, m.permissions().readonly()) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}
