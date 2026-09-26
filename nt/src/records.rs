// What a listing gives: the records `NtQueryDirectoryFile` writes for
// `FileFullDirectoryInformation`, a chain of `FILE_FULL_DIR_INFORMATION`s in
// which each gives the offset of the next.
//
// They are parsed in safe code, as bytes, and every offset and length is
// checked against what the call said it wrote. The records come from a
// filesystem driver, and one that writes garbage gives wrong entries or ends
// the chain early: nothing is read outside what was written, and nothing
// panics.

use std::mem::offset_of;

use windows_sys::Wdk::Storage::FileSystem::FILE_FULL_DIR_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

type Record = FILE_FULL_DIR_INFORMATION;

// The fixed part of a record, which the name follows.
const HEADER: usize = offset_of!(Record, FileName);

/// What the system says of a file, a directory or a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// The `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    /// The tag of the reparse point, or 0 if it is not one.
    pub reparse_tag: u32,
    /// The size in bytes: the end of the file's data.
    pub size: u64,
    /// When the file was last written, in 100-nanosecond intervals since
    /// 1601.
    pub last_write: i64,
}

/// An entry of a listing, as the listing gave it.
pub struct Entry<'b> {
    // UTF-16, little-endian.
    name: &'b [u8],
    /// The entry's metadata. It is the copy the filesystem keeps in the
    /// directory, which on NTFS can be older than the file's own: see
    /// `Directory::list`.
    pub metadata: Metadata,
}

impl Entry<'_> {
    /// The entry's name, in UTF-16 units that need not be valid UTF-16.
    pub fn name(&self) -> impl Iterator<Item = u16> + '_ {
        self.name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
    }
}

// Calls `each` with the entries of `data`, all but `.` and `..`, until the
// chain ends or leaves `data`. Returns `false` if `each` did.
pub(crate) fn records(data: &[u8], each: &mut impl FnMut(&Entry<'_>) -> bool) -> bool {
    let mut at = 0;
    while let Some(fixed) = data.get(at..).and_then(|rest| rest.get(..HEADER)) {
        let length = u32_at(fixed, offset_of!(Record, FileNameLength)) as usize;
        let Some(name) = data[at + HEADER..].get(..length) else {
            break;
        };
        // UTF-16 `.` and `..`.
        if name != b".\0" && name != b".\0.\0" {
            let attributes = u32_at(fixed, offset_of!(Record, FileAttributes));
            let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                0
            } else {
                // With a reparse point, this field holds its tag.
                u32_at(fixed, offset_of!(Record, EaSize))
            };
            let entry = Entry {
                name,
                metadata: Metadata {
                    attributes,
                    reparse_tag,
                    size: i64_at(fixed, offset_of!(Record, EndOfFile)).cast_unsigned(),
                    last_write: i64_at(fixed, offset_of!(Record, LastWriteTime)),
                },
            };
            if !each(&entry) {
                return false;
            }
        }
        let next = u32_at(fixed, offset_of!(Record, NextEntryOffset)) as usize;
        if next == 0 {
            break;
        }
        let Some(following) = at.checked_add(next) else {
            break;
        };
        at = following;
    }
    true
}

// A field of the fixed part, which `records` has checked is all there.
fn u32_at(fixed: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*fixed[at..].first_chunk().expect("a field of the record"))
}

fn i64_at(fixed: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(*fixed[at..].first_chunk().expect("a field of the record"))
}

/// Builds listings as the system writes them, for tests.
#[cfg(test)]
pub(crate) mod chain {
    use std::mem::offset_of;

    use super::{HEADER, Metadata, Record};
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    /// Records chained as `NtQueryDirectoryFile` chains them, each starting
    /// 8-byte aligned.
    #[derive(Default)]
    pub struct Chain {
        pub bytes: Vec<u8>,
        last: Option<usize>,
    }

    impl Chain {
        /// Adds a record. A reparse point's tag goes where the system puts
        /// it; `ea_size` is what the field holds for any other entry.
        pub fn push(&mut self, name: &str, metadata: Metadata, ea_size: u32) -> &mut Self {
            let start = self.bytes.len().next_multiple_of(8);
            if let Some(last) = self.last {
                let next = u32::try_from(start - last).expect("a short chain");
                put(
                    &mut self.bytes,
                    last + offset_of!(Record, NextEntryOffset),
                    &next.to_le_bytes(),
                );
            }
            let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            self.bytes.resize(start + HEADER + name.len(), 0);
            let at = |field: usize| start + field;
            let field = if metadata.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                ea_size
            } else {
                metadata.reparse_tag
            };
            let length = u32::try_from(name.len()).expect("a short name");
            put(
                &mut self.bytes,
                at(offset_of!(Record, EaSize)),
                &field.to_le_bytes(),
            );
            put(
                &mut self.bytes,
                at(offset_of!(Record, FileNameLength)),
                &length.to_le_bytes(),
            );
            put(
                &mut self.bytes,
                at(offset_of!(Record, FileAttributes)),
                &metadata.attributes.to_le_bytes(),
            );
            put(
                &mut self.bytes,
                at(offset_of!(Record, EndOfFile)),
                &metadata.size.to_le_bytes(),
            );
            put(
                &mut self.bytes,
                at(offset_of!(Record, LastWriteTime)),
                &metadata.last_write.to_le_bytes(),
            );
            put(&mut self.bytes, start + HEADER, &name);
            self.last = Some(start);
            self
        }
    }

    fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    /// A file's metadata.
    pub fn file(size: u64) -> Metadata {
        Metadata {
            attributes: 0x20,
            reparse_tag: 0,
            size,
            last_write: 133_000_000_000_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use super::chain::{Chain, file};
    use super::{Metadata, Record, records};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_SYMLINK;

    fn parsed(data: &[u8]) -> Vec<(String, Metadata)> {
        let mut entries = Vec::new();
        records(data, &mut |entry| {
            let name = String::from_utf16(&entry.name().collect::<Vec<_>>()).expect("UTF-16");
            entries.push((name, entry.metadata));
            true
        });
        entries
    }

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn a_chain_gives_every_entry_with_its_fields() {
        let directory = Metadata {
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            ..file(0)
        };
        let link = Metadata {
            attributes: FILE_ATTRIBUTE_REPARSE_POINT,
            reparse_tag: IO_REPARSE_TAG_SYMLINK,
            ..file(0)
        };
        let mut chain = Chain::default();
        chain
            .push("file", file(5), 0)
            .push("sub", directory, 0)
            .push("link", link, 0)
            // The field holds the size of extended attributes, not a tag.
            .push("ea", file(1), 99)
            .push("ünïcode", file(2), 0);
        assert_eq!(
            parsed(&chain.bytes),
            [
                ("file".to_owned(), file(5)),
                ("sub".to_owned(), directory),
                ("link".to_owned(), link),
                ("ea".to_owned(), file(1)),
                ("ünïcode".to_owned(), file(2)),
            ]
        );
    }

    #[test]
    fn dot_and_dot_dot_are_left_out() {
        let mut chain = Chain::default();
        chain
            .push(".", file(0), 0)
            .push("..", file(0), 0)
            .push("...", file(0), 0);
        assert_eq!(parsed(&chain.bytes), [("...".to_owned(), file(0))]);
    }

    #[test]
    fn a_false_from_each_stops_the_chain() {
        let mut chain = Chain::default();
        chain.push("a", file(0), 0).push("b", file(0), 0);
        let mut seen = 0;
        let finished = records(&chain.bytes, &mut |_| {
            seen += 1;
            false
        });
        assert!(!finished);
        assert_eq!(seen, 1);
    }

    #[test]
    fn a_chain_cut_anywhere_gives_the_entries_before_the_cut() {
        let mut chain = Chain::default();
        chain
            .push("first", file(1), 0)
            .push("second", file(2), 0)
            .push("third", file(3), 0);
        let whole = parsed(&chain.bytes);
        for cut in 0..=chain.bytes.len() {
            let entries = parsed(&chain.bytes[..cut]);
            assert!(whole.starts_with(&entries), "cut at {cut}");
        }
    }

    #[test]
    fn an_offset_or_a_length_past_the_end_ends_the_chain() {
        let mut chain = Chain::default();
        chain.push("a", file(0), 0).push("b", file(0), 0);

        let mut far = chain.bytes.clone();
        put_u32(&mut far, offset_of!(Record, NextEntryOffset), u32::MAX);
        assert_eq!(parsed(&far), [("a".to_owned(), file(0))]);

        let mut long = chain.bytes.clone();
        put_u32(&mut long, offset_of!(Record, FileNameLength), u32::MAX);
        assert_eq!(parsed(&long), []);
    }

    #[test]
    fn garbage_gives_entries_inside_it_and_no_panic() {
        // Xorshift: the same garbage on every run.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let rounds = if cfg!(miri) { 40 } else { 4000 };
        for _ in 0..rounds {
            let length = usize::try_from(random() % 600).expect("small");
            let mut data: Vec<u8> = (0..length).map(|_| random().to_le_bytes()[0]).collect();
            // Offsets and lengths small enough to lead somewhere, often.
            for at in (0..length.saturating_sub(4)).step_by(8) {
                if random() % 3 == 0 {
                    put_u32(&mut data, at, u32::try_from(random() % 160).expect("small"));
                }
            }
            records(&data, &mut |entry| {
                assert!(entry.name().count() * 2 <= data.len());
                true
            });
        }
    }
}
