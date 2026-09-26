// The reader for Windows, through `tarseer-nt`: `NtQueryDirectoryFile` into
// one buffer, which gives every entry's size, times and attributes with its
// name, and `NtCreateFile` relative to the open directory, as the linux reader
// uses `openat`. `std` opens every directory by its full path, which Windows
// resolves from the root each time, and a directory's own metadata takes it
// another open.
//
// A file's metadata from the listing is the copy NTFS keeps in the entry of
// the name listed. Writing through one name refreshes that name's entry
// only: the entries of a hard-linked file's other names keep the old size
// and times until it is opened through them.
// `NtQueryInformationByName` (`FileStatInformation`) would read the file's
// own, with its link count, at one call per file, as `statx` on Linux.

use std::io;
use std::path::Path;

use tarseer_nt::{Buffer, Metadata};
use windows_sys::Win32::Foundation::ERROR_TOO_MANY_OPEN_FILES;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
};

use super::{Kind, Listed, Listing, Spare, Stat};
use crate::manifest::Timestamp;

// A reparse point whose tag says it names another file: a symlink or a
// junction. `std` counts exactly these as symlinks; the others (deduplicated
// or cloud files, ...) are files and directories like any other.
const NAME_SURROGATE: u32 = 0x2000_0000;

// 100-nanosecond intervals from 1601, the Windows epoch, to 1970.
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// Where a listing is read. One buffer serves every directory, because a
/// listing is copied out of it before the next directory is read.
#[derive(Default)]
pub struct Scratch {
    buffer: Buffer,
    name: String,
    /// See [`Spare`].
    pub spare: Spare,
}

/// What the listing said of each entry, by slot: its metadata costs no call.
#[derive(Default)]
pub struct Held {
    entries: Vec<Metadata>,
}

/// An open directory.
pub struct Directory {
    directory: tarseer_nt::Directory,
}

impl Directory {
    /// Whether the walk reads a directory's metadata from the directory itself
    /// and not from its parent's listing. The listing holds a copy that the
    /// filesystem updates late, so a directory that was just written into
    /// shows an old mtime there; the open directory costs one call.
    pub const STATS_ITSELF: bool = true;

    /// Opens the root of a walk. A root that is a symlink is followed.
    pub fn open_root(root: &Path) -> io::Result<Self> {
        let directory = tarseer_nt::Directory::open(root)?;
        Ok(Self { directory })
    }

    /// Opens the directory `listed` names inside this one.
    pub fn open_child(&self, listed: &Listed, listing: &Listing) -> io::Result<Self> {
        self.open_name(name_of(listed, listing))
    }

    /// Opens the directory `name` inside this one, not following a link: a
    /// name that became a link since it was listed is opened as the link.
    pub fn open_name(&self, name: &str) -> io::Result<Self> {
        let directory = self.directory.open_dir(&wide(name))?;
        Ok(Self { directory })
    }

    /// Returns `true` if `error` says the process has no handle left.
    pub fn out_of_handles(error: &io::Error) -> bool {
        error.raw_os_error() == Some(ERROR_TOO_MANY_OPEN_FILES.cast_signed())
    }

    /// Reads and sorts the directory's entries.
    // The signature is the portable reader's, which can fail here.
    #[allow(clippy::unnecessary_wraps)]
    pub fn list(&self, scratch: &mut Scratch) -> io::Result<Listing> {
        let mut listing = Listing::from_spare(&mut scratch.spare);
        let name = &mut scratch.name;
        let read = self.directory.list(&mut scratch.buffer, |entry| {
            name.clear();
            let mut lossy = false;
            for unit in char::decode_utf16(entry.name()) {
                name.push(unit.unwrap_or_else(|_| {
                    lossy = true;
                    char::REPLACEMENT_CHARACTER
                }));
            }
            let kind = if lossy {
                Kind::NonUtf8
            } else {
                kind_of(&entry.metadata)
            };
            let slot = listing.held.entries.len();
            let Some(listed) = Listed::new(listing.names_mut(), name.as_bytes(), kind, slot) else {
                let error = io::Error::other("the listing is larger than 4 GiB");
                listing.failures.push((error, None, "listing"));
                return false;
            };
            listing.entries.push(listed);
            listing.held.entries.push(entry.metadata);
            true
        });
        if let Err(error) = read {
            listing.failures.push((error, None, "listing"));
        }
        listing.sort();
        Ok(listing)
    }

    /// Reads the metadata of `listed`, as its listing gave it.
    // The signature is the other readers', which make a call here.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub fn stat(&self, listed: &Listed, listing: &Listing) -> io::Result<Stat> {
        Ok(converted(&listing.held.entries[listed.slot as usize]))
    }

    /// Reads the metadata of this directory.
    pub fn stat_self(&self) -> io::Result<Stat> {
        Ok(converted(&self.directory.metadata()?))
    }

    /// Reads the target of the symlink `listed`. `None` if it is not UTF-8.
    pub fn read_link(&self, listed: &Listed, listing: &Listing) -> io::Result<Option<String>> {
        let target = self.directory.read_link(&wide(name_of(listed, listing)))?;
        Ok(String::from_utf16(&target).ok())
    }
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

fn name_of<'l>(listed: &Listed, listing: &'l Listing) -> &'l str {
    listing
        .name_text(listed)
        .expect("only entries with UTF-8 names are opened")
}

fn kind_of(metadata: &Metadata) -> Kind {
    let directory = metadata.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if metadata.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && metadata.reparse_tag & NAME_SURROGATE != 0
    {
        Kind::Symlink {
            directory: Some(directory),
        }
    } else if directory {
        Kind::Directory
    } else {
        Kind::File
    }
}

// What the portable reader records through `std`, from the same fields.
fn converted(metadata: &Metadata) -> Stat {
    let kind = kind_of(metadata);
    let readonly = metadata.attributes & FILE_ATTRIBUTE_READONLY != 0;
    Stat {
        kind,
        size: metadata.size,
        mtime: Some(timestamp(metadata.last_write)),
        mode: match (kind == Kind::Directory, readonly) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        },
    }
}

fn timestamp(ticks: i64) -> Timestamp {
    let since = ticks - UNIX_EPOCH_TICKS;
    Timestamp {
        secs: since.div_euclid(10_000_000),
        nanos: u32::try_from(since.rem_euclid(10_000_000) * 100).expect("under a second"),
    }
}
