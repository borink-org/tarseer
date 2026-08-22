//! The Linux directory reader: `getdents64` and a masked `statx`, through
//! rustix.
//!
//! Not fewer syscalls — `strace -c` counts both scanners equal on a 180,000
//! entry tree (181,246 `statx`, 2,482 `getdents64`, 1,253 open/close), because
//! `std` reaches the same calls underneath. What differs is that names are
//! borrowed out of the kernel's buffer rather than copied into an owned
//! `CString` and then copied again by `file_name()`, and that `statx` is asked
//! for four fields instead of a whole `stat`: 0.02 allocations per entry
//! against 2.14, and ~6% off the walk.
//!
//! `statx` reports in `stx_mask` which fields it actually filled, and may fill
//! fewer than were asked for. Reading `SIZE` when the filesystem declined to
//! supply it would record the file as empty, so a shortfall is an error rather
//! than a default. `AT_STATX_DONT_SYNC` is deliberately not passed: a stale
//! size on a network filesystem is a corrupt archive, not a fast walk.
//!
//! The 16 KiB `getdents64` buffer lives in a `thread_local`, not in `Out`.
//! `Out` outlives its task, so a buffer per task kept thousands alive until
//! the level ended — 152 MiB peak. Only as many exist as there are threads.

use std::cell::RefCell;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, RawDir, StatxFlags};

use crate::arena::{Arena, Span};
use crate::bail;
use crate::error::{Context, Result};
use crate::tree::Skips;
use crate::walk::{Found, Out, STAT_CHUNK, Stat, finish, join_into, scan_link};

const DIR_BUF: usize = 16 << 10;

/// Buffers one worker refills for each directory it scans, instead of the scan
/// allocating them per directory.
#[derive(Default)]
struct Scratch {
    buf: Vec<u8>,
    ents: Vec<(Span, FileType)>,
    stats: Vec<Stat>,
}

// INO and NLINK ride along for hard-link grouping: statx fills them from the
// inode it already had to read, so this widens the mask without adding a
// syscall or a round trip.
const WANT: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::INO)
    .union(StatxFlags::NLINK);

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// Every name in one directory, interned, sorted, and paired with its type.
fn read_names(
    dirfd: &rustix::fd::OwnedFd,
    dir: &str,
    buf: &mut Vec<u8>,
    names: &mut Arena,
    ents: &mut Vec<(Span, FileType)>,
    skips: &mut Skips,
) -> Result<()> {
    buf.clear();
    ents.clear();
    if buf.capacity() < DIR_BUF {
        buf.reserve(DIR_BUF);
    }
    let mut iter = RawDir::new(dirfd, buf.spare_capacity_mut());
    while let Some(entry) = iter.next() {
        let entry = entry.ctx(|| format!("read dir {dir}"))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let Ok(name) = core::str::from_utf8(name) else {
            skips.non_utf8 += 1;
            continue;
        };
        ents.push((names.push(&[name])?, entry.file_type()));
    }
    ents.sort_by(|a, b| names.get(a.0).cmp(names.get(b.0)));
    Ok(())
}

/// One masked `statx` per entry that has bytes or children, spread across the
/// pool when this is the only directory being scanned.
fn stat_all(
    dirfd: &rustix::fd::OwnedFd,
    dir: &str,
    names: &Arena,
    ents: &[(Span, FileType)],
    stats: &mut Vec<Stat>,
    wide: bool,
) -> Result<()> {
    let stat_of = |&(span, ft): &(Span, FileType)| -> Result<Stat> {
        if !(ft == FileType::RegularFile || ft == FileType::Directory) {
            return Ok(Stat::default());
        }
        let name = names.get(span);
        let st = rustix::fs::statx(dirfd, name, AtFlags::SYMLINK_NOFOLLOW, WANT)
            .ctx(|| format!("statx {dir}/{name}"))?;
        if st.stx_mask & WANT.bits() != WANT.bits() {
            bail!(
                "statx {dir}/{name} returned mask {:#x}, missing {:#x} of the requested fields",
                st.stx_mask,
                WANT.bits() & !st.stx_mask
            );
        }
        Ok(Stat {
            mtime: st.stx_mtime.tv_sec,
            size: if ft == FileType::RegularFile {
                st.stx_size
            } else {
                0
            },
            mode: u32::from(st.stx_mode) & 0o7777,
            // See `walk::link_ident`: identity only for a file with a second
            // name.
            ino: if ft == FileType::RegularFile && st.stx_nlink > 1 {
                st.stx_ino
            } else {
                0
            },
        })
    };

    stats.clear();
    stats.resize(ents.len(), Stat::default());
    if wide && ents.len() >= STAT_CHUNK * 2 {
        use rayon::prelude::*;
        ents.par_chunks(STAT_CHUNK)
            .zip(stats.par_chunks_mut(STAT_CHUNK))
            .try_for_each(|(es, ss)| -> Result<()> {
                for (e, s) in es.iter().zip(ss) {
                    *s = stat_of(e)?;
                }
                Ok(())
            })?;
    } else {
        for (e, s) in ents.iter().zip(stats.iter_mut()) {
            *s = stat_of(e)?;
        }
    }
    Ok(())
}

/// Enumerate one directory, as [`crate::walk`] does, using `getdents64`.
///
/// # Errors
/// If the directory cannot be read, a name is not UTF-8, or `statx` declines
/// to fill a field that was asked for.
pub(crate) fn scan_one(dir: &str, wide: bool, out: &mut Out) -> Result<()> {
    SCRATCH.with(|cell| scan_into(dir, wide, out, &mut cell.borrow_mut()))
}

fn scan_into(dir: &str, wide: bool, out: &mut Out, scratch: &mut Scratch) -> Result<()> {
    let entries_from = u32::try_from(out.entries.len()).ctx(|| "entry arena overflow".into())?;
    let kids_from = u32::try_from(out.kid_spans.len()).ctx(|| "child arena overflow".into())?;
    let Scratch { buf, ents, stats } = scratch;

    // An unreadable directory costs its subtree and is counted, exactly as in
    // the portable scanner.
    let Ok(dirfd) = rustix::fs::open(
        dir,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) else {
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

    let mut skips = Skips::default();
    read_names(&dirfd, dir, buf, &mut out.names, ents, &mut skips)?;
    stat_all(&dirfd, dir, &out.names, ents, stats, wide)?;

    out.entries.reserve(ents.len());
    out.kid_spans.reserve(
        ents.iter()
            .filter(|(_, ft)| *ft == FileType::Directory)
            .count(),
    );
    for (
        &(span, ft),
        &Stat {
            mtime,
            size,
            mode,
            ino,
        },
    ) in ents.iter().zip(stats.iter())
    {
        let name = out.names.get(span);
        match ft {
            FileType::Symlink => {
                join_into(&mut out.tmp, dir, name);
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
            }
            FileType::Directory => {
                let sep = if dir.ends_with('/') { "" } else { "/" };
                let kid = out.kids.push(&[dir, sep, name])?;
                out.kid_spans.push(kid);
                out.entries.push(Found::Dir {
                    name: span,
                    mtime,
                    mode,
                });
            }
            FileType::RegularFile => {
                if ino != 0 {
                    let at =
                        u32::try_from(out.entries.len()).ctx(|| "entry arena overflow".into())?;
                    out.linked.push((at, ino));
                }
                out.entries.push(Found::File {
                    name: span,
                    mtime,
                    size,
                    mode,
                });
            }
            _ => skips.special += 1,
        }
    }
    finish(out, entries_from, kids_from, skips)
}
