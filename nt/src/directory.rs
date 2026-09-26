use std::io;
use std::mem::offset_of;
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
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_NO_MORE_FILES,
    STATUS_NO_SUCH_FILE, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
    GetFileInformationByHandleEx, GetFullPathNameW, MAXIMUM_REPARSE_DATA_BUFFER_SIZE, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

// 64 KiB: a few hundred entries a call.
const BUFFER_BYTES: usize = 64 << 10;

/// An open directory.
///
/// Its handle is always open for synchronous I/O, which the calls on it rely
/// on: a call on a handle open for asynchronous I/O could write into its
/// buffer after it returned.
pub struct Directory {
    handle: OwnedHandle,
}

/// Where [`Directory::list`] reads a listing. One buffer serves every
/// directory in turn.
pub struct Buffer {
    // A few bytes more than it uses, to start the records 8-byte aligned.
    bytes: Vec<u8>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self {
            bytes: vec![0; BUFFER_BYTES + 7],
        }
    }
}

impl Buffer {
    fn aligned(&mut self) -> &mut [u8] {
        let skip = self.bytes.as_ptr().addr().wrapping_neg() % 8;
        &mut self.bytes[skip..skip + BUFFER_BYTES]
    }
}

/// What the system says of a file, a directory or a link.
#[derive(Clone, Copy, Debug)]
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
    /// [`Directory::list`].
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

impl Directory {
    /// Opens the directory at `path`, following a symlink or junction there.
    ///
    /// # Errors
    ///
    /// If the open fails, or [`io::ErrorKind::NotADirectory`] if `path` is
    /// not a directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        // `std` opens for synchronous I/O unless asked otherwise.
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let handle = OwnedHandle::from(file);
        let (attributes, _) = attributes_of(&handle)?;
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "not a directory",
            ));
        }
        Ok(Self { handle })
    }

    /// Opens the directory `name` inside this one, never following a link: a
    /// name that is a symlink or junction is opened as the link. `name` is in
    /// UTF-16 units, and matches without regard to case, as Win32 opens do.
    ///
    /// # Errors
    ///
    /// If the open fails, or if `name` is not a directory.
    pub fn open_dir(&self, name: &[u16]) -> io::Result<Self> {
        let handle = self.open_relative(
            name,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        )?;
        Ok(Self { handle })
    }

    /// Reads the directory from its start, and calls `each` with every entry
    /// but `.` and `..`, until `each` returns `false`.
    ///
    /// An entry's metadata is the copy NTFS keeps in the entry of the name
    /// listed. Writing through one name refreshes that name's entry only: the
    /// entries of a hard-linked file's other names keep the old size and
    /// times until it is opened through them. A directory's own entry is
    /// updated late too, and [`Directory::metadata`] reads the directory's
    /// own.
    ///
    /// # Errors
    ///
    /// If a read fails. `each` has then seen the entries read before it.
    pub fn list(
        &self,
        buffer: &mut Buffer,
        mut each: impl FnMut(&Entry<'_>) -> bool,
    ) -> io::Result<()> {
        let buffer = buffer.aligned();
        let mut restart = true;
        loop {
            let mut status_block = IO_STATUS_BLOCK::default();
            // SAFETY: the handle is an open directory, open for synchronous
            // I/O, and the buffer is as long as it says and 8-byte aligned.
            let status = unsafe {
                NtQueryDirectoryFile(
                    self.raw(),
                    ptr::null_mut(),
                    None,
                    ptr::null(),
                    &raw mut status_block,
                    buffer.as_mut_ptr().cast(),
                    length(buffer.len()),
                    FileFullDirectoryInformation,
                    false,
                    ptr::null(),
                    restart,
                )
            };
            restart = false;
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                return Ok(());
            }
            if status < 0 {
                return Err(nt_error(status));
            }
            let written = status_block.Information.min(buffer.len());
            if !records(&buffer[..written], &mut each) {
                return Ok(());
            }
        }
    }

    /// Reads the metadata of this directory, from the directory itself.
    ///
    /// # Errors
    ///
    /// If a call fails.
    pub fn metadata(&self) -> io::Result<Metadata> {
        let mut information = FILE_NETWORK_OPEN_INFORMATION::default();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the handle is open, and the struct is as long as it says.
        let status = unsafe {
            NtQueryInformationFile(
                self.raw(),
                &raw mut status_block,
                (&raw mut information).cast(),
                length(size_of::<FILE_NETWORK_OPEN_INFORMATION>()),
                FileNetworkOpenInformation,
            )
        };
        if status < 0 {
            return Err(nt_error(status));
        }
        let attributes = information.FileAttributes;
        let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            0
        } else {
            attributes_of(&self.handle)?.1
        };
        Ok(Metadata {
            attributes,
            reparse_tag,
            size: information.EndOfFile.cast_unsigned(),
            last_write: information.LastWriteTime,
        })
    }

    /// Reads the target of the symlink or junction `name` inside this
    /// directory, as `std::fs::read_link` gives it.
    ///
    /// # Errors
    ///
    /// If a call fails, or if `name` is another kind of reparse point, or none.
    pub fn read_link(&self, name: &[u16]) -> io::Result<Vec<u16>> {
        let handle = self.open_relative(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0)?;
        reparse_target(&handle)
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    // Opens `name` in this directory for synchronous I/O, never following a
    // link.
    fn open_relative(
        &self,
        name: &[u16],
        access: FILE_ACCESS_RIGHTS,
        options: NTCREATEFILE_CREATE_OPTIONS,
    ) -> io::Result<OwnedHandle> {
        let name_length = u16::try_from(size_of_val(name))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidFilename, "the name is too long"))?;
        let object_name = UNICODE_STRING {
            Length: name_length,
            MaximumLength: name_length,
            Buffer: name.as_ptr().cast_mut(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: length(size_of::<OBJECT_ATTRIBUTES>()),
            RootDirectory: self.raw(),
            ObjectName: &raw const object_name,
            // As Win32 opens, and so `std`.
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };
        let mut handle: HANDLE = ptr::null_mut();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer is to a live value of the type it is declared
        // as, and the name outlives the call, which only reads it.
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

// Calls `each` with the records of one read, a chain of
// `FILE_FULL_DIR_INFORMATION`s, each giving the offset of the next. Returns
// `false` if `each` did.
fn records(data: &[u8], each: &mut impl FnMut(&Entry<'_>) -> bool) -> bool {
    type Record = FILE_FULL_DIR_INFORMATION;
    let header = offset_of!(Record, FileName);
    let mut at = 0;
    while let Some(fixed) = data.get(at..at + header) {
        let length = u32_at(fixed, offset_of!(Record, FileNameLength)) as usize;
        let Some(name) = data.get(at + header..at + header + length) else {
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
        at += next;
    }
    true
}

// The length of a buffer or a struct, as the calls take it.
fn length(bytes: usize) -> u32 {
    u32::try_from(bytes).expect("a buffer under 4 GiB")
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(*bytes[at..].first_chunk().expect("a field of the record"))
}

fn i64_at(bytes: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(*bytes[at..].first_chunk().expect("a field of the record"))
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
            length(size_of::<FILE_ATTRIBUTE_TAG_INFO>()),
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
    // SAFETY: the handle is open for synchronous I/O, and the buffer is as
    // long as it says.
    let done = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            length(buffer.len()),
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
            length(full.len()),
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
