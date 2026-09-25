// The reader for Windows: `NtQueryDirectoryFile` into one buffer, which gives
// every entry's size, times and attributes with its name, and `NtCreateFile`
// relative to the open directory, as the linux reader uses `openat`. `std`
// opens every directory by its full path, which Windows resolves from the
// root each time, and a directory's own metadata takes it another open.
//
// A file's metadata from the listing is the copy NTFS keeps in the entry of
// the name listed. Writing through one name refreshes that name's entry
// only: the entries of a hard-linked file's other names keep the old size
// and times until it is opened through them.
// `NtQueryInformationByName` (`FileStatInformation`) would read the file's
// own, with its link count, at one call per file, as `statx` on Linux.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_FULL_DIR_INFORMATION, FILE_NETWORK_OPEN_INFORMATION, FILE_OPEN,
    FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    FileFullDirectoryInformation, FileNetworkOpenInformation, NTCREATEFILE_CREATE_OPTIONS,
    NtCreateFile, NtQueryDirectoryFile, NtQueryInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_TOO_MANY_OPEN_FILES, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
    STATUS_NO_MORE_FILES, STATUS_NO_SUCH_FILE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, GetFileInformationByHandleEx, GetFullPathNameW,
    MAXIMUM_REPARSE_DATA_BUFFER_SIZE, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

use super::{Kind, Listed, Listing, Spare, Stat};
use crate::manifest::Timestamp;

// A reparse point whose tag says it names another file: a symlink or a
// junction. `std` counts exactly these as symlinks; the others (deduplicated
// or cloud files, ...) are files and directories like any other.
const NAME_SURROGATE: u32 = 0x2000_0000;

// 100-nanosecond intervals from 1601, the Windows epoch, to 1970.
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// Where `NtQueryDirectoryFile` writes. One buffer serves every directory,
/// because a listing is copied out of it before the next directory is read.
pub struct Scratch {
    // `u64`s, for the alignment of the records written into it.
    buffer: Vec<u64>,
    name: String,
    /// See [`Spare`].
    pub spare: Spare,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            // 64 KiB: a few hundred entries a call.
            buffer: vec![0; (64 << 10) / 8],
            name: String::new(),
            spare: Spare::default(),
        }
    }
}

/// What the listing said of each entry, by slot: its metadata costs no call.
#[derive(Default)]
pub struct Held {
    entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    attributes: u32,
    tag: u32,
    size: u64,
    written: i64,
}

/// An open directory.
pub struct Directory {
    handle: OwnedHandle,
}

impl Directory {
    /// Whether the walk reads a directory's metadata from the directory itself
    /// and not from its parent's listing. The listing holds a copy that the
    /// filesystem updates late, so a directory that was just written into
    /// shows an old mtime there; the open directory costs one call.
    pub const STATS_ITSELF: bool = true;

    /// Whether a value of this type keeps a handle open.
    pub const HOLDS_A_HANDLE: bool = true;

    /// Opens the root of a walk. A root that is a symlink is followed.
    pub fn open_root(root: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root)?;
        let handle = OwnedHandle::from(file);
        let (attributes, _) = attributes_of(&handle)?;
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "the root is not a directory",
            ));
        }
        Ok(Self { handle })
    }

    /// Opens the directory `listed` names inside this one.
    pub fn open_child(&self, listed: &Listed, listing: &Listing) -> io::Result<Self> {
        self.open_name(name_of(listed, listing))
    }

    /// Opens the directory `name` inside this one, not following a link.
    pub fn open_name(&self, name: &str) -> io::Result<Self> {
        let handle = self.open_relative(
            name,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        )?;
        Ok(Self { handle })
    }

    /// Reads the metadata of `name` inside this directory, without following
    /// a symlink.
    pub fn stat_name(&self, name: &str) -> io::Result<Stat> {
        let handle = self.open_relative(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0)?;
        stat_of(&handle)
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
        let buffer = &mut scratch.buffer;
        let bytes = u32::try_from(buffer.len() * 8).expect("a small buffer");
        let mut restart = true;
        loop {
            let mut status_block = IO_STATUS_BLOCK::default();
            // SAFETY: the handle is an open directory, opened for synchronous
            // I/O, and the buffer is as long as it says and 8-byte aligned.
            let status = unsafe {
                NtQueryDirectoryFile(
                    self.raw(),
                    ptr::null_mut(),
                    None,
                    ptr::null(),
                    &raw mut status_block,
                    buffer.as_mut_ptr().cast(),
                    bytes,
                    FileFullDirectoryInformation,
                    false,
                    ptr::null(),
                    restart,
                )
            };
            restart = false;
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                break;
            }
            if status < 0 {
                listing.failures.push((nt_error(status), None, "listing"));
                break;
            }
            let written = status_block.Information.min(buffer.len() * 8);
            if !read_records(buffer, written, &mut scratch.name, &mut listing) {
                break;
            }
        }
        listing.sort();
        Ok(listing)
    }

    /// Reads the metadata of `listed`, as its listing gave it.
    // The signature is the other readers', which make a call here.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub fn stat(&self, listed: &Listed, listing: &Listing) -> io::Result<Stat> {
        let entry = listing.held.entries[listed.slot as usize];
        Ok(converted(
            entry.attributes,
            entry.tag,
            entry.size,
            entry.written,
        ))
    }

    /// Reads the metadata of this directory.
    pub fn stat_self(&self) -> io::Result<Stat> {
        stat_of(&self.handle)
    }

    /// Reads the target of the symlink `listed`. `None` if it is not UTF-8.
    pub fn read_link(&self, listed: &Listed, listing: &Listing) -> io::Result<Option<String>> {
        let handle = self.open_relative(
            name_of(listed, listing),
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            0,
        )?;
        let target = reparse_target(&handle)?;
        Ok(String::from_utf16(&target).ok())
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    // Opens `name` in this directory, never following a link: a name that
    // became a link since it was listed is opened as the link.
    fn open_relative(
        &self,
        name: &str,
        access: FILE_ACCESS_RIGHTS,
        options: NTCREATEFILE_CREATE_OPTIONS,
    ) -> io::Result<OwnedHandle> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let length = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidFilename, "the name is too long"))?;
        let object_name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: wide.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("a small struct"),
            RootDirectory: self.raw(),
            ObjectName: &raw const object_name,
            // As Win32 opens, and so as `std` did.
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };
        let mut handle: HANDLE = ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer is to a live value of the type it is declared
        // as, and the name's buffer outlives the call.
        let status = unsafe {
            NtCreateFile(
                &raw mut handle,
                access,
                &raw const attributes,
                &raw mut status_block,
                ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                options
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | FILE_OPEN_REPARSE_POINT
                    | FILE_OPEN_FOR_BACKUP_INTENT,
                ptr::null(),
                0,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        // SAFETY: the call succeeded, so `handle` is open and now owned here.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

// Copies the records of one call into `listing`. Returns `false` if the
// listing can hold no more. The buffer is of `u64`s, so a record, which
// starts on a multiple of 8, is as aligned as its type needs.
#[allow(clippy::cast_ptr_alignment)]
fn read_records(buffer: &[u64], written: usize, name: &mut String, listing: &mut Listing) -> bool {
    let base = buffer.as_ptr().cast::<u8>();
    let header = std::mem::offset_of!(FILE_FULL_DIR_INFORMATION, FileName);
    let mut at = 0;
    while at + header <= written {
        // SAFETY: the record starts inside what the call wrote, 8-byte
        // aligned, and its fixed part fits there.
        let record = unsafe { &*base.add(at).cast::<FILE_FULL_DIR_INFORMATION>() };
        let units = (record.FileNameLength / 2) as usize;
        if at + header + units * 2 > written {
            break;
        }
        // SAFETY: the name's units were checked to lie inside what was
        // written, and a `u16` needs no more alignment than the record has.
        let wide =
            unsafe { std::slice::from_raw_parts(base.add(at + header).cast::<u16>(), units) };
        let next = record.NextEntryOffset as usize;
        if wide != [u16::from(b'.')] && wide != [u16::from(b'.'); 2] {
            name.clear();
            let mut lossy = false;
            for unit in char::decode_utf16(wide.iter().copied()) {
                name.push(unit.unwrap_or_else(|_| {
                    lossy = true;
                    char::REPLACEMENT_CHARACTER
                }));
            }
            let attributes = record.FileAttributes;
            let tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                0
            } else {
                // With a reparse point, this field holds its tag.
                record.EaSize
            };
            let kind = if lossy {
                Kind::NonUtf8
            } else {
                kind_of(attributes, tag)
            };
            let slot = listing.held.entries.len();
            let Some(listed) = Listed::new(listing.names_mut(), name.as_bytes(), kind, slot) else {
                let error = io::Error::other("the listing is larger than 4 GiB");
                listing.failures.push((error, None, "listing"));
                return false;
            };
            listing.entries.push(listed);
            listing.held.entries.push(Entry {
                attributes,
                tag,
                size: record.EndOfFile.cast_unsigned(),
                written: record.LastWriteTime,
            });
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    true
}

fn name_of<'l>(listed: &Listed, listing: &'l Listing) -> &'l str {
    listing
        .name_text(listed)
        .expect("only entries with UTF-8 names are opened")
}

fn kind_of(attributes: u32, tag: u32) -> Kind {
    let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0 {
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
fn converted(attributes: u32, tag: u32, size: u64, written: i64) -> Stat {
    let kind = kind_of(attributes, tag);
    let readonly = attributes & FILE_ATTRIBUTE_READONLY != 0;
    Stat {
        kind,
        size,
        mtime: Some(timestamp(written)),
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

// One call for everything a row records, where `GetFileInformationByHandle`
// makes several, one of them for the volume.
fn stat_of(handle: &OwnedHandle) -> io::Result<Stat> {
    let mut information = FILE_NETWORK_OPEN_INFORMATION::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is open, and the struct is as long as it says.
    let status = unsafe {
        NtQueryInformationFile(
            handle.as_raw_handle(),
            &raw mut status_block,
            (&raw mut information).cast(),
            u32::try_from(size_of::<FILE_NETWORK_OPEN_INFORMATION>()).expect("a small struct"),
            FileNetworkOpenInformation,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    let attributes = information.FileAttributes;
    let tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        0
    } else {
        attributes_of(handle)?.1
    };
    Ok(converted(
        attributes,
        tag,
        information.EndOfFile.cast_unsigned(),
        information.LastWriteTime,
    ))
}

// The attributes and the reparse tag of the file `handle` is open on.
fn attributes_of(handle: &OwnedHandle) -> io::Result<(u32, u32)> {
    let mut information = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle is open, and the struct is as long as it says.
    let done = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut information).cast(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).expect("a small struct"),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((information.FileAttributes, information.ReparseTag))
}

// The target of the symlink or junction `handle` is open on, as
// `std::fs::read_link` gives it.
fn reparse_target(handle: &OwnedHandle) -> io::Result<Vec<u16>> {
    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut returned = 0;
    // SAFETY: the handle is open, and the buffer is as long as it says.
    let done = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).expect("16 KiB"),
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    let data = &buffer[..returned as usize];
    let u16_at = |at: usize| -> io::Result<u16> {
        data.get(at..at + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .ok_or_else(|| io::Error::other("a reparse point shorter than its header"))
    };
    let tag = u32::from(u16_at(0)?) | (u32::from(u16_at(2)?) << 16);
    // REPARSE_DATA_BUFFER: the tag, two lengths, then the names' offsets
    // and lengths, and for a symlink its flags, before the names.
    let (names_at, relative) = match tag {
        IO_REPARSE_TAG_SYMLINK => (20, u16_at(16)? & 1 != 0),
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        _ => return Err(io::Error::other("an unsupported reparse point")),
    };
    let start = names_at + usize::from(u16_at(8)?);
    let bytes = data
        .get(start..start + usize::from(u16_at(10)?))
        .ok_or_else(|| io::Error::other("a reparse point shorter than its target"))?;
    let mut target: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    // An absolute target starts with `\??\`, which `std` turns into `\\?\`
    // and then into a plain path where that means the same.
    let backslash = u16::from(b'\\');
    let question = u16::from(b'?');
    if !relative && target.starts_with(&[backslash, question, question, backslash]) {
        target[1] = backslash;
        return user_path(target);
    }
    Ok(target)
}

// `\\?\C:\...` as `C:\...`, and `\\?\UNC\...` as `\\...`, when Windows reads
// the shorter path as the same one; otherwise the path unchanged. What `std`
// does for `read_link`.
fn user_path(mut path: Vec<u16>) -> io::Result<Vec<u16>> {
    const LEGACY_MAX_PATH: usize = 260;
    // `std` counts the NUL it ends the path with.
    if path.len() + 1 > LEGACY_MAX_PATH {
        return Ok(path);
    }
    let unit = |c: u8| u16::from(c);
    let is_drive =
        path.len() >= 7 && path[4] != 0 && path[5] == unit(b':') && path[6] == unit(b'\\');
    let is_unc = path.len() >= 8 && path[4..8] == [unit(b'U'), unit(b'N'), unit(b'C'), unit(b'\\')];
    let from = if is_drive {
        4
    } else if is_unc {
        path[6] = unit(b'\\');
        6
    } else {
        return Ok(path);
    };
    let short: Vec<u16> = path[from..].iter().copied().chain([0]).collect();
    let mut full = vec![0u16; LEGACY_MAX_PATH + 1];
    // SAFETY: `short` ends with a NUL, and `full` is as long as it says.
    let length = unsafe {
        GetFullPathNameW(
            short.as_ptr(),
            u32::try_from(full.len()).expect("a short buffer"),
            full.as_mut_ptr(),
            ptr::null_mut(),
        )
    } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length < full.len() && full[..length] == short[..short.len() - 1] {
        return Ok(short[..short.len() - 1].to_vec());
    }
    if is_unc {
        path[6] = unit(b'C');
    }
    Ok(path)
}

fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a pure conversion of a status code.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code.cast_signed())
}
