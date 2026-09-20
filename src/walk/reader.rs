// How the walk reads directories. Two readers share one interface: `unix`
// asks the kernel directly, and `portable` goes through `std::fs`.
//
// A listing keeps every name in one buffer and one small record per entry, so
// that listing a directory costs no allocation per entry.

use std::cmp::Ordering;

use crate::manifest::Timestamp;

#[cfg(all(unix, not(tarseer_portable_reader)))]
mod unix;
#[cfg(all(unix, not(tarseer_portable_reader)))]
use unix as imp;

#[cfg(not(all(unix, not(tarseer_portable_reader))))]
mod portable;
#[cfg(not(all(unix, not(tarseer_portable_reader))))]
use portable as imp;

pub(super) use imp::{Directory, Scratch};

/// What a listing says an entry is, before its metadata is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
    /// `directory` is `Some` where the platform tells file links from
    /// directory links.
    Symlink {
        directory: Option<bool>,
    },
    /// A socket, a device or another kind the walk does not record.
    Special,
    /// The listing gave no kind. [`Directory::stat`] finds it.
    Unknown,
    /// The name is not UTF-8. The listing holds a lossy copy of it.
    NonUtf8,
}

/// One listed entry. Its name is in [`Listing::names`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Listed {
    // The first eight bytes of the name, big-endian and zero-padded. Most
    // comparisons in the sort stop at this integer.
    prefix: u64,
    start: u32,
    len: u32,
    pub kind: Kind,
    // The entry's place in `Held`, for the reader that needs it.
    #[cfg_attr(all(unix, not(tarseer_portable_reader)), allow(dead_code))]
    slot: u32,
}

impl Listed {
    fn new(names: &mut Vec<u8>, name: &[u8], kind: Kind, slot: usize) -> Option<Self> {
        let start = u32::try_from(names.len()).ok()?;
        let len = u32::try_from(name.len()).ok()?;
        let slot = u32::try_from(slot).ok()?;
        start.checked_add(len)?.checked_add(1)?;
        let mut prefix = [0; 8];
        let head = name.len().min(8);
        prefix[..head].copy_from_slice(&name[..head]);
        names.extend_from_slice(name);
        // The unix reader passes names to the kernel, which wants this NUL.
        names.push(0);
        Some(Self {
            prefix: u64::from_be_bytes(prefix),
            start,
            len,
            kind,
            slot,
        })
    }

    /// Returns the entry's name, given the buffer of its listing.
    pub fn name<'n>(&self, names: &'n [u8]) -> &'n [u8] {
        &names[self.start as usize..(self.start + self.len) as usize]
    }

    fn order(&self, other: &Self, names: &[u8]) -> Ordering {
        self.prefix
            .cmp(&other.prefix)
            .then_with(|| self.name(names).cmp(other.name(names)))
    }
}

/// The entries of one directory, sorted by name, bytewise.
#[derive(Default)]
pub(super) struct Listing {
    pub entries: Vec<Listed>,
    pub names: Vec<u8>,
    // What the reader keeps until the entries have been visited.
    #[cfg_attr(all(unix, not(tarseer_portable_reader)), allow(dead_code))]
    held: imp::Held,
    /// Entries the reader could not list: the error, the name if it is
    /// known, and what could not be read.
    pub failures: Vec<(std::io::Error, Option<String>, &'static str)>,
}

impl Listing {
    fn sort(&mut self) {
        let names = &self.names;
        self.entries
            .sort_unstable_by(|left, right| left.order(right, names));
    }

    /// Returns the position of the entry named `name`, by binary search.
    pub fn find(entries: &[Listed], names: &[u8], name: &[u8]) -> Option<usize> {
        let mut prefix = [0; 8];
        let head = name.len().min(8);
        prefix[..head].copy_from_slice(&name[..head]);
        let prefix = u64::from_be_bytes(prefix);
        entries
            .binary_search_by(|entry| {
                entry
                    .prefix
                    .cmp(&prefix)
                    .then_with(|| entry.name(names).cmp(name))
            })
            .ok()
    }
}

/// What the walk records of an entry.
pub(super) struct Stat {
    /// The entry's kind, for an entry listed as [`Kind::Unknown`].
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<Timestamp>,
    /// The permission bits.
    pub mode: u32,
}
