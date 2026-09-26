// How the walk reads directories. Three readers share one interface: `linux`
// and `windows` ask the kernel directly, and `portable` goes through
// `std::fs`.
//
// A listing keeps every name in one buffer and one small record per entry, so
// that listing a directory costs no allocation per entry.

use crate::manifest::Timestamp;

// The `tarseer_portable_reader` cfg builds the portable reader on any platform.
cfg_select! {
    all(target_os = "linux", not(tarseer_portable_reader)) => {
        mod linux;
        use linux as imp;
    }
    all(windows, not(tarseer_portable_reader)) => {
        mod windows;
        use windows as imp;
    }
    _ => {
        mod portable;
        use portable as imp;
    }
}

pub(super) use imp::{Directory, Scratch};

/// The buffers of a listing that has been read, for the next one to fill. A
/// walk then allocates for a listing only when one is larger than any before.
#[derive(Default)]
pub(super) struct Spare {
    entries: Vec<Listed>,
    names: Vec<u8>,
    sorted: Vec<Listed>,
    keys: Vec<u128>,
}

// The names of a listing. They are checked for UTF-8 once, all together, and
// held as text if they pass. A name is then a slice of that text, and costs no
// check of its own. NUL, which ends each name for the linux reader, is UTF-8.
enum Names {
    Bytes(Vec<u8>),
    Text(String),
}

impl Default for Names {
    fn default() -> Self {
        Self::Bytes(Vec::new())
    }
}

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
    /// A socket, a device or another kind the walk does not record. Windows
    /// has none: every entry is a file, a directory or a link.
    #[cfg_attr(all(windows, not(tarseer_portable_reader)), allow(dead_code))]
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
    #[cfg_attr(
        all(target_os = "linux", not(tarseer_portable_reader)),
        allow(dead_code)
    )]
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
        // The linux reader passes names to the kernel, which wants this NUL.
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
}

/// The entries of one directory, sorted by name, bytewise.
#[derive(Default)]
pub(super) struct Listing {
    pub entries: Vec<Listed>,
    names: Names,
    // What the sort works in. See `Listing::sort`.
    sorted: Vec<Listed>,
    keys: Vec<u128>,
    // What the reader keeps until the entries have been visited.
    #[cfg_attr(
        all(target_os = "linux", not(tarseer_portable_reader)),
        allow(dead_code)
    )]
    held: imp::Held,
    /// Entries the reader could not list: the error, the name if it is
    /// known, and what could not be read.
    pub failures: Vec<(std::io::Error, Option<String>, &'static str)>,
}

impl Listing {
    /// An empty listing that fills the buffers of `spare`.
    fn from_spare(spare: &mut Spare) -> Self {
        Self {
            entries: std::mem::take(&mut spare.entries),
            names: Names::Bytes(std::mem::take(&mut spare.names)),
            sorted: std::mem::take(&mut spare.sorted),
            keys: std::mem::take(&mut spare.keys),
            ..Self::default()
        }
    }

    /// The buffer that holds every name.
    pub fn names(&self) -> &[u8] {
        match &self.names {
            Names::Bytes(bytes) => bytes,
            Names::Text(text) => text.as_bytes(),
        }
    }

    // While the listing is being read.
    fn names_mut(&mut self) -> &mut Vec<u8> {
        match &mut self.names {
            Names::Bytes(bytes) => bytes,
            Names::Text(_) => unreachable!("names are text only once the listing is sorted"),
        }
    }

    /// The name of `listed` as text, or `None` if it is not UTF-8.
    // Only the Windows reader uses it.
    #[cfg_attr(not(all(windows, not(tarseer_portable_reader))), allow(dead_code))]
    pub fn name_text(&self, listed: &Listed) -> Option<&str> {
        match &self.names {
            Names::Text(text) => {
                text.get(listed.start as usize..(listed.start + listed.len) as usize)
            }
            Names::Bytes(bytes) => std::str::from_utf8(listed.name(bytes)).ok(),
        }
    }

    // Sorts the entries, and checks the names for UTF-8 all at once.
    fn sort(&mut self) {
        let names = match &self.names {
            Names::Bytes(bytes) => bytes.as_slice(),
            Names::Text(text) => text.as_bytes(),
        };
        // The sort is of one integer for each entry: the first eight bytes
        // of its name, and its place. Names that share their first eight
        // bytes are then put in order among themselves.
        let entries = &self.entries;
        self.keys.clear();
        self.keys.extend(
            entries
                .iter()
                .enumerate()
                .map(|(place, entry)| (u128::from(entry.prefix) << 64) | place as u128),
        );
        self.keys.sort_unstable();
        let place = |key: u128| (key & u128::from(u64::MAX)) as usize;
        let mut from = 0;
        while from < self.keys.len() {
            let prefix = self.keys[from] >> 64;
            let run = self.keys[from..]
                .iter()
                .take_while(|&&key| key >> 64 == prefix)
                .count();
            if run > 1 {
                self.keys[from..from + run].sort_unstable_by(|&left, &right| {
                    entries[place(left)]
                        .name(names)
                        .cmp(entries[place(right)].name(names))
                });
            }
            from += run;
        }
        self.sorted.clear();
        self.sorted
            .extend(self.keys.iter().map(|&key| entries[place(key)]));
        std::mem::swap(&mut self.entries, &mut self.sorted);
        self.names = match std::mem::take(&mut self.names) {
            Names::Bytes(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Names::Text(text),
                Err(error) => Names::Bytes(error.into_bytes()),
            },
            text @ Names::Text(_) => text,
        };
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
