// The reader for every other platform, through `std::fs`. It keeps std's
// entries until they have been visited, because their metadata is read
// through them.

use std::fs::{self, DirEntry, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use super::{Kind, Listed, Listing, Spare, Stat};
use crate::manifest::Timestamp;

/// This reader needs no buffer of its own.
#[derive(Default)]
pub struct Scratch {
    /// See [`Spare`].
    pub spare: Spare,
}

#[derive(Default)]
pub struct Held {
    entries: Vec<DirEntry>,
}

/// A directory, by its path. It holds nothing open.
pub struct Directory {
    path: PathBuf,
}

// The methods share the linux reader's signatures, which read through the
// open directory.
#[allow(clippy::unused_self)]
impl Directory {
    /// Whether the walk reads a directory's metadata from the directory itself
    /// and not from its parent's listing. On Windows the listing holds a copy
    /// that the filesystem updates late, so a directory that was just written
    /// into shows an old mtime there.
    pub const STATS_ITSELF: bool = cfg!(windows);

    /// Opens the root of a walk. A root that is a symlink is followed.
    // The signature is the linux reader's, which can fail here.
    #[allow(clippy::unnecessary_wraps)]
    pub fn open_root(root: &Path) -> io::Result<Self> {
        Ok(Self {
            path: root.to_path_buf(),
        })
    }

    /// Opens the directory `listed` names inside this one.
    #[allow(clippy::unnecessary_wraps)]
    pub fn open_child(&self, listed: &Listed, listing: &Listing) -> io::Result<Self> {
        Ok(Self {
            path: listing.held.entries[listed.slot as usize].path(),
        })
    }

    /// Returns `true` if `error` says the process has no handle left. This
    /// reader holds none.
    pub fn out_of_handles(_: &io::Error) -> bool {
        false
    }

    /// Reads and sorts the directory's entries.
    pub fn list(&self, scratch: &mut Scratch) -> io::Result<Listing> {
        let mut listing = Listing::from_spare(&mut scratch.spare);
        for entry in fs::read_dir(&self.path)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing.failures.push((error, None, "listing"));
                    continue;
                }
            };
            let name = entry.file_name();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    let name = name.to_string_lossy().into_owned();
                    listing.failures.push((error, Some(name), "kind"));
                    continue;
                }
            };
            let slot = listing.held.entries.len();
            let listed = match name.to_str() {
                Some(name) => Listed::new(
                    listing.names_mut(),
                    name.as_bytes(),
                    kind_of(file_type),
                    slot,
                ),
                None => Listed::new(
                    listing.names_mut(),
                    name.to_string_lossy().as_bytes(),
                    Kind::NonUtf8,
                    slot,
                ),
            };
            let Some(listed) = listed else {
                let error = io::Error::other("the listing is larger than 4 GiB");
                listing.failures.push((error, None, "listing"));
                break;
            };
            listing.entries.push(listed);
            listing.held.entries.push(entry);
        }
        listing.sort();
        Ok(listing)
    }

    /// Reads the metadata of `listed`, without following a symlink.
    pub fn stat(&self, listed: &Listed, listing: &Listing) -> io::Result<Stat> {
        let metadata = listing.held.entries[listed.slot as usize].metadata()?;
        Ok(converted(&metadata))
    }

    /// Reads the metadata of this directory.
    pub fn stat_self(&self) -> io::Result<Stat> {
        Ok(converted(&fs::symlink_metadata(&self.path)?))
    }

    /// Reads the target of the symlink `listed`. `None` if it is not UTF-8.
    pub fn read_link(&self, listed: &Listed, listing: &Listing) -> io::Result<Option<String>> {
        let target = fs::read_link(listing.held.entries[listed.slot as usize].path())?;
        Ok(target.to_str().map(str::to_owned))
    }
}

fn kind_of(file_type: FileType) -> Kind {
    if file_type.is_symlink() {
        Kind::Symlink {
            directory: symlink_is_directory(file_type),
        }
    } else if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Special
    }
}

// Whether a symlink is a directory link. Only Windows has the distinction.
#[cfg_attr(windows, allow(clippy::unnecessary_wraps))]
fn symlink_is_directory(file_type: FileType) -> Option<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        Some(file_type.is_symlink_dir())
    }
    #[cfg(not(windows))]
    {
        let _ = file_type;
        None
    }
}

fn converted(metadata: &Metadata) -> Stat {
    Stat {
        kind: kind_of(metadata.file_type()),
        size: metadata.len(),
        mtime: mtime_of(metadata),
        mode: mode_of(metadata),
    }
}

#[cfg_attr(unix, allow(clippy::unnecessary_wraps))]
fn mtime_of(metadata: &Metadata) -> Option<Timestamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(Timestamp {
            secs: metadata.mtime(),
            nanos: u32::try_from(metadata.mtime_nsec()).unwrap_or(0),
        })
    }
    #[cfg(not(unix))]
    {
        metadata.modified().ok().map(Timestamp::from_system_time)
    }
}

fn mode_of(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        match (metadata.is_dir(), metadata.permissions().readonly()) {
            (true, true) => 0o555,
            (true, false) => 0o755,
            (false, true) => 0o444,
            (false, false) => 0o644,
        }
    }
}
