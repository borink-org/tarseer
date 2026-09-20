// The reader for Linux: `getdents64` into one buffer, and `statx`, `openat`
// and `readlinkat` relative to the open directory.

use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, Statx, StatxFlags};

use super::{Kind, Listed, Listing, Stat};
use crate::manifest::Timestamp;

// Only what a row records, and the type for an entry listed without one.
const WANTED: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::MTIME);

/// Where `getdents64` writes. One buffer serves every directory, because a
/// listing is copied out of it before the next directory is read.
pub struct Scratch {
    buffer: Vec<MaybeUninit<u8>>,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            // 32 KiB, the size glibc reads directories with.
            buffer: vec![MaybeUninit::uninit(); 32 << 10],
        }
    }
}

#[derive(Default)]
pub struct Held;

/// An open directory.
pub struct Directory {
    fd: OwnedFd,
}

impl Directory {
    /// Whether [`Directory::stat_self`] is cheaper than [`Directory::stat`]
    /// on the parent.
    pub const STATS_ITSELF: bool = true;

    /// Opens the root of a walk. A root that is a symlink is followed.
    pub fn open_root(root: &Path) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::open(root, flags, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Opens the directory `listed` names inside this one.
    pub fn open_child(&self, listed: &Listed, listing: &Listing) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let name = c_name(listed, &listing.names);
        let fd = rustix::fs::openat(&self.fd, name, flags, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Returns `true` if `error` says the process has no descriptor left.
    pub fn out_of_handles(error: &io::Error) -> bool {
        let errno = error.raw_os_error();
        errno == Some(rustix::io::Errno::MFILE.raw_os_error())
            || errno == Some(rustix::io::Errno::NFILE.raw_os_error())
    }

    /// Reads and sorts the directory's entries.
    // The signature is the portable reader's, which can fail here.
    #[allow(clippy::unnecessary_wraps)]
    pub fn list(&self, scratch: &mut Scratch) -> io::Result<Listing> {
        let mut listing = Listing::default();
        let mut raw = RawDir::new(&self.fd, &mut scratch.buffer);
        while let Some(entry) = raw.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing.failures.push((error.into(), None, "listing"));
                    break;
                }
            };
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = kind_of(entry.file_type());
            let Some(listed) = Listed::new(&mut listing.names, name, kind, 0) else {
                let error = io::Error::other("the listing is larger than 4 GiB");
                listing.failures.push((error, None, "listing"));
                break;
            };
            listing.entries.push(listed);
        }
        listing.sort();
        Ok(listing)
    }

    /// Reads the metadata of `listed`, without following a symlink.
    pub fn stat(&self, listed: &Listed, listing: &Listing) -> io::Result<Stat> {
        let name = c_name(listed, &listing.names);
        let stat = rustix::fs::statx(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW, WANTED)?;
        converted(&stat)
    }

    /// Reads the metadata of this directory.
    pub fn stat_self(&self) -> io::Result<Stat> {
        let stat = rustix::fs::statx(&self.fd, c"", AtFlags::EMPTY_PATH, WANTED)?;
        converted(&stat)
    }

    /// Reads the target of the symlink `listed`. `None` if it is not UTF-8.
    pub fn read_link(&self, listed: &Listed, listing: &Listing) -> io::Result<Option<String>> {
        let name = c_name(listed, &listing.names);
        let target = rustix::fs::readlinkat(&self.fd, name, Vec::new())?;
        Ok(target.into_string().ok())
    }
}

fn c_name<'n>(listed: &Listed, names: &'n [u8]) -> &'n CStr {
    let start = listed.start as usize;
    let end = start + listed.len as usize + 1;
    CStr::from_bytes_with_nul(&names[start..end]).expect("a NUL after every listed name")
}

fn kind_of(file_type: FileType) -> Kind {
    match file_type {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink { directory: None },
        FileType::Unknown => Kind::Unknown,
        _ => Kind::Special,
    }
}

// `statx` says in `stx_mask` which fields it filled, and a filesystem may fill
// fewer than were asked for. Reading a size that was not filled would record
// the file as empty.
fn converted(stat: &Statx) -> io::Result<Stat> {
    if stat.stx_mask & WANTED.bits() != WANTED.bits() {
        return Err(io::Error::other(
            "the filesystem did not report the type, mode, size and mtime",
        ));
    }
    Ok(Stat {
        kind: kind_of(FileType::from_raw_mode(u32::from(stat.stx_mode))),
        size: stat.stx_size,
        mtime: Some(Timestamp {
            secs: stat.stx_mtime.tv_sec,
            nanos: stat.stx_mtime.tv_nsec,
        }),
        mode: u32::from(stat.stx_mode) & 0o7777,
    })
}
