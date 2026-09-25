// The reader for Linux: `getdents64` into one buffer, and `statx`, `openat`
// and `readlinkat` relative to the open directory.

use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, Statx, StatxFlags};

use super::{Kind, Listed, Listing, Spare, Stat};
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
    /// See [`Spare`].
    pub spare: Spare,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            // 32 KiB, the size glibc reads directories with.
            buffer: vec![MaybeUninit::uninit(); 32 << 10],
            spare: Spare::default(),
        }
    }
}

#[derive(Default)]
pub struct Held;

/// Makes the process's table of descriptors large enough for `handles` more,
/// before the walk starts its threads.
///
/// The kernel grows that table when a descriptor does not fit. In a process
/// with threads it first waits for every processor to pass a quiet point.
/// That is several milliseconds each time, inside an `openat`. A walk that
/// holds directories open crosses 64, 128 and 256. With one thread the kernel
/// does not wait.
pub fn reserve_handles(handles: usize) {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC;
    let Ok(any) = rustix::fs::open("/", flags | OFlags::DIRECTORY, Mode::empty()) else {
        return;
    };
    let Ok(least) = i32::try_from(handles) else {
        return;
    };
    // A descriptor at `least` or above makes the table that large. It may be
    // refused, by a limit on descriptors below that. The walk works without.
    drop(rustix::io::fcntl_dupfd_cloexec(&any, least));
}

/// An open directory.
pub struct Directory {
    fd: OwnedFd,
}

impl Directory {
    /// Whether the walk reads a directory's metadata from the directory itself
    /// and not from its parent's listing. Here that saves the kernel a lookup.
    pub const STATS_ITSELF: bool = true;

    /// Whether a value of this type keeps a file descriptor open.
    pub const HOLDS_A_HANDLE: bool = true;

    /// Opens the root of a walk. A root that is a symlink is followed.
    pub fn open_root(root: &Path) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::open(root, flags, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Opens the directory `listed` names inside this one.
    pub fn open_child(&self, listed: &Listed, listing: &Listing) -> io::Result<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let name = c_name(listed, listing.names());
        let fd = rustix::fs::openat(&self.fd, name, flags, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Opens the directory `name` inside this one, to read the metadata of
    /// what it holds, and not to list it: `O_PATH` skips the rest of an open.
    pub fn open_name(&self, name: &str) -> io::Result<Self> {
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let fd = rustix::fs::openat(&self.fd, name, flags, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Reads the metadata of `name` inside this directory, without following
    /// a symlink.
    pub fn stat_name(&self, name: &str) -> io::Result<Stat> {
        let stat = rustix::fs::statx(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW, WANTED)?;
        converted(&stat)
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
        let mut listing = Listing::from_spare(&mut scratch.spare);
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
            let Some(listed) = Listed::new(listing.names_mut(), name, kind, 0) else {
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
    #[inline]
    pub fn stat(&self, listed: &Listed, listing: &Listing) -> io::Result<Stat> {
        let name = c_name(listed, listing.names());
        let stat = rustix::fs::statx(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW, WANTED)?;
        converted(&stat)
    }

    /// Reads the metadata of this directory.
    #[inline]
    pub fn stat_self(&self) -> io::Result<Stat> {
        let stat = rustix::fs::statx(&self.fd, c"", AtFlags::EMPTY_PATH, WANTED)?;
        converted(&stat)
    }

    /// Reads the target of the symlink `listed`. `None` if it is not UTF-8.
    pub fn read_link(&self, listed: &Listed, listing: &Listing) -> io::Result<Option<String>> {
        let name = c_name(listed, listing.names());
        let target = rustix::fs::readlinkat(&self.fd, name, Vec::new())?;
        Ok(target.into_string().ok())
    }
}

/// Reads the metadata of the file `fd` is open on, without a lookup.
pub fn stat_fd(fd: rustix::fd::BorrowedFd<'_>) -> io::Result<Stat> {
    let stat = rustix::fs::statx(fd, c"", AtFlags::EMPTY_PATH, WANTED)?;
    converted(&stat)
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
#[inline]
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

/// What the process has used so far: CPU time over all its threads, and bytes
/// read from storage.
pub struct Usage {
    io: Option<OwnedFd>,
    text: Vec<u8>,
}

impl Usage {
    /// Whether [`Usage::sample`] measures anything.
    pub const MEASURED: bool = true;

    pub fn new() -> Self {
        let flags = OFlags::RDONLY | OFlags::CLOEXEC;
        Self {
            io: rustix::fs::open("/proc/self/io", flags, Mode::empty()).ok(),
            text: vec![0; 512],
        }
    }

    /// The CPU time, and the bytes read from storage if the kernel says.
    pub fn sample(&mut self) -> (std::time::Duration, Option<u64>) {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
        let cpu = std::time::Duration::new(
            u64::try_from(time.tv_sec).unwrap_or(0),
            u32::try_from(time.tv_nsec).unwrap_or(0),
        );
        (cpu, self.read_bytes())
    }

    fn read_bytes(&mut self) -> Option<u64> {
        let io = self.io.as_ref()?;
        let read = rustix::io::pread(io, &mut self.text, 0).ok()?;
        let text = std::str::from_utf8(&self.text[..read]).ok()?;
        let line = text.lines().find(|line| line.starts_with("read_bytes:"))?;
        line["read_bytes:".len()..].trim().parse().ok()
    }
}

/// Keeps the calling thread on one processor of those the process may use,
/// the `worker`th of them, so that what it last used stays in that
/// processor's caches.
pub fn pin(worker: usize) {
    use rustix::thread::{CpuSet, sched_getaffinity, sched_setaffinity};
    let Ok(allowed) = sched_getaffinity(None) else {
        return;
    };
    let count = allowed.count() as usize;
    if count == 0 {
        return;
    }
    let Some(cpu) = (0..CpuSet::MAX_CPU)
        .filter(|&cpu| allowed.is_set(cpu))
        .nth(worker % count)
    else {
        return;
    };
    let mut one = CpuSet::new();
    one.set(cpu);
    // A thread that cannot be pinned walks all the same.
    let _ = sched_setaffinity(None, &one);
}
