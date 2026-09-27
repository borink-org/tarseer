// How the calls here stay sound.
//
// The system checks every handle and pointer that a process gives it. A bad
// handle gets an error, and a bad pointer an error or a fault in this process. What a call can harm is this process's memory, and
// only through what the call is given. Each `unsafe` block below keeps these
// rules for what it gives, and its `SAFETY` comment says how:
//
// 1. Handles. A handle is open for as long as the call runs. Every handle is
//    borrowed from an `OwnedHandle`, which closes it only when dropped.
// 2. Bounds. A pointer is valid for as many bytes as the call is told, and
//    as aligned as the call needs.
// 3. Access. The system writes only through pointers made from `&mut`
//    borrows or raw borrows of mutable places, and only reads through the
//    others. No Rust reference to that memory is used while the call runs.
// 4. Lifetime. The system is done with every pointer when the call returns.
//    Every handle here is open for synchronous I/O, and on one, a call waits
//    for its I/O. On a handle open for asynchronous I/O, a call can return
//    `STATUS_PENDING` or `ERROR_IO_PENDING`. The system then writes into its
//    buffer and its status block later, and the status block is on the
//    stack. `completed` and `succeeded` abort the process if a call ever
//    returns either. Unwinding would free memory that the system still writes.
// 5. Output. What a call writes is read only as far as the call says it
//    wrote. A struct that the system fills is all integers, so any bytes are
//    a valid one. Listings and reparse points are parsed as bytes, in safe
//    code. Every buffer starts zeroed, so none is read uninitialized.
// 6. Ownership. A handle a call returns is owned by one `OwnedHandle`, made
//    only when the call succeeded.
//
// The unit tests run the code here against stand-ins for the calls (see
// `sys`), also under Miri. Miri checks rules 2, 3 and 5 on what is given.

use std::io;
use std::ops::Range;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NETWORK_OPEN_INFORMATION, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileFullDirectoryInformation,
    FileNetworkOpenInformation, NTCREATEFILE_CREATE_OPTIONS,
};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, STATUS_NO_MORE_FILES,
    STATUS_NO_SUCH_FILE, STATUS_PENDING, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
    MAXIMUM_REPARSE_DATA_BUFFER_SIZE, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::core::BOOL;

use crate::records::{Entry, Metadata, records};
use crate::sys::{
    DeviceIoControl, GetFileInformationByHandleEx, GetFullPathNameW, NtCreateFile,
    NtQueryDirectoryFile, NtQueryInformationFile, RtlNtStatusToDosError,
};

// 64 KiB: a few hundred entries a call.
const BUFFER_BYTES: usize = 64 << 10;

// Room for the longest target a reparse point holds, and a NUL after it. The
// target starts after a header of at least 16 bytes.
const TARGET_UNITS: usize = (MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize - 16) / 2 + 1;

/// An open directory.
///
/// Its handle is always open for synchronous I/O. On a handle open for
/// asynchronous I/O, a call could write into its buffer after it returned.
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

impl Directory {
    /// Opens the directory at `path`, following a symlink or junction there.
    ///
    /// # Errors
    ///
    /// Returns the open's error if it fails. Returns an error of kind
    /// [`io::ErrorKind::NotADirectory`] if `path` is not a directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        // `std` opens for synchronous I/O unless asked otherwise (rule 4).
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
    /// Returns the open's error if it fails, which it does if `name` is not a
    /// directory.
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
    /// It takes `&mut self` because the open handle keeps the listing's
    /// place. Two listings of one handle at once would each miss the entries
    /// that the other read.
    ///
    /// An entry's metadata is the copy NTFS keeps in the entry of the name
    /// listed. Writing through one name refreshes that name's entry only. The
    /// entries of a hard-linked file's other names keep the old size and
    /// times until you open the file through them. A directory's own entry is
    /// updated late too, and [`Directory::metadata`] reads the directory's
    /// own.
    ///
    /// # Errors
    ///
    /// Returns a read's error if it fails. `each` has then seen the entries
    /// that were read before it.
    pub fn list(
        &mut self,
        buffer: &mut Buffer,
        mut each: impl FnMut(&Entry<'_>) -> bool,
    ) -> io::Result<()> {
        let handle = self.raw();
        let buffer = buffer.aligned();
        let mut restart = true;
        loop {
            let mut status_block = IO_STATUS_BLOCK::default();
            // SAFETY:
            // 1. The handle is borrowed from `self`.
            // 2. The buffer is as long as the call is told, 8-byte aligned as
            //    the records need, and the status block is its own type.
            // 3. Both come from `&mut` borrows, used by nothing else until the
            //    call returns. The other pointers are null, which the call
            //    takes as none: no event, no routine, no name to match.
            // 4. The handle is open for synchronous I/O; `completed` checks.
            let status = completed(unsafe {
                NtQueryDirectoryFile(
                    handle,
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
            });
            restart = false;
            if status == STATUS_NO_MORE_FILES || status == STATUS_NO_SUCH_FILE {
                return Ok(());
            }
            if status < 0 {
                return Err(nt_error(status));
            }
            // 5. Only what the call says it wrote.
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
    /// Returns a call's error if it fails.
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata_of(&self.handle)
    }

    /// Reads the metadata of `name` inside this directory, from the file
    /// itself, not following a link. `name` is in UTF-16 units, as for
    /// [`Directory::open_dir`].
    ///
    /// # Errors
    ///
    /// Returns a call's error if it fails.
    pub fn metadata_of(&self, name: &[u16]) -> io::Result<Metadata> {
        let handle = self.open_relative(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0)?;
        metadata_of(&handle)
    }

    /// Reads the target of the symlink or junction `name` inside this
    /// directory, as `std::fs::read_link` gives it, and calls `with` with it.
    ///
    /// The target is in UTF-16 units that need not be valid UTF-16. It is in
    /// a buffer on the stack, so this method allocates nothing. Copy the
    /// target in `with` to keep it, for example with `<[u16]>::to_vec`.
    ///
    /// # Errors
    ///
    /// Returns a call's error if it fails. Returns an error if `name` is
    /// another kind of reparse point, or not one at all.
    pub fn read_link<R>(&self, name: &[u16], with: impl FnOnce(&[u16]) -> R) -> io::Result<R> {
        let handle = self.open_relative(name, FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0)?;
        let mut target = [0u16; TARGET_UNITS];
        let range = reparse_target(&handle, &mut target)?;
        Ok(with(&target[range]))
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
            // The call only reads it (rule 3), though the type says `*mut`.
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
        // SAFETY:
        // 1. The directory the name is opened in is borrowed from `self`.
        // 2. The attributes, the name and its units are live values of their
        //    types, and the name's length is the units' length in bytes.
        // 3. The call writes only the handle and the status block, raw
        //    borrows of local variables; it reads the rest. The other
        //    pointers are null, which the call takes as none.
        // 4. It opens for synchronous I/O, and so does every call on the
        //    handle it returns; `completed` checks this one.
        let status = completed(unsafe {
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
        });
        if status < 0 {
            return Err(nt_error(status));
        }
        // SAFETY: 6. The call succeeded, so `handle` is open, and nothing
        // else owns it.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

// The length of a buffer or a struct, as the calls take it: a `u32`. The
// largest here is the listing's buffer, of 64 KiB.
fn length(bytes: usize) -> u32 {
    u32::try_from(bytes).expect("a buffer under 4 GiB")
}

// Rule 4 for the native calls: aborts if a call has left its I/O running.
fn completed(status: NTSTATUS) -> NTSTATUS {
    if status == STATUS_PENDING {
        eprintln!(
            "tarseer-nt: a call returned STATUS_PENDING on a handle open for synchronous I/O"
        );
        std::process::abort();
    }
    status
}

// Rule 4 for the Win32 calls: aborts if a call has left its I/O running.
fn succeeded(done: BOOL) -> io::Result<()> {
    if done != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_IO_PENDING.cast_signed()) {
        eprintln!(
            "tarseer-nt: a call returned ERROR_IO_PENDING on a handle open for synchronous I/O"
        );
        std::process::abort();
    }
    Err(error)
}

// The metadata of the file `handle` is open on.
fn metadata_of(handle: &OwnedHandle) -> io::Result<Metadata> {
    let mut information = FILE_NETWORK_OPEN_INFORMATION::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY:
    // 1. The handle is borrowed.
    // 2. The information and the status block are each their own type, of
    //    the length the call is told.
    // 3. Both are raw borrows of local variables that nothing else uses
    //    until the call returns.
    // 4. The handle is open for synchronous I/O; `completed` checks.
    let status = completed(unsafe {
        NtQueryInformationFile(
            handle.as_raw_handle(),
            &raw mut status_block,
            (&raw mut information).cast(),
            length(size_of::<FILE_NETWORK_OPEN_INFORMATION>()),
            FileNetworkOpenInformation,
        )
    });
    if status < 0 {
        return Err(nt_error(status));
    }
    // 5. The struct is all integers.
    let attributes = information.FileAttributes;
    let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        0
    } else {
        attributes_of(handle)?.1
    };
    Ok(Metadata {
        attributes,
        reparse_tag,
        size: information.EndOfFile.cast_unsigned(),
        last_write: information.LastWriteTime,
    })
}

// The attributes and the reparse tag of the file `handle` is open on.
fn attributes_of(handle: &OwnedHandle) -> io::Result<(u32, u32)> {
    let mut information = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY:
    // 1. The handle is borrowed.
    // 2. The information is its own type, of the length the call is told.
    // 3. It is a raw borrow of a local variable that nothing else uses until
    //    the call returns.
    // 4. The handle is open for synchronous I/O; `succeeded` checks.
    succeeded(unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut information).cast(),
            length(size_of::<FILE_ATTRIBUTE_TAG_INFO>()),
        )
    })?;
    // 5. The struct is all integers.
    Ok((information.FileAttributes, information.ReparseTag))
}

// Reads the target of the symlink or junction `handle` is open on into
// `target`, as `std::fs::read_link` gives it. Returns where it is there.
fn reparse_target(
    handle: &OwnedHandle,
    target: &mut [u16; TARGET_UNITS],
) -> io::Result<Range<usize>> {
    let mut buffer = [0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut returned = 0;
    // SAFETY:
    // 1. The handle is borrowed.
    // 2. The buffer is as long as the call is told, and needs no alignment:
    //    it is read as bytes. The count is a `u32`.
    // 3. The call writes the buffer, from a `&mut` borrow, and the count, a
    //    raw borrow of a local variable. Nothing else uses either until it
    //    returns. It takes no input, and the other pointers are null, which
    //    the call takes as none.
    // 4. The handle is open for synchronous I/O, and no `OVERLAPPED` is
    //    given; `succeeded` checks.
    succeeded(unsafe {
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
    })?;
    // 5. Only what the call says it wrote, as bytes.
    let data = buffer.get(..returned as usize).unwrap_or(&buffer);
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
    let units = bytes.len() / 2;
    if units >= TARGET_UNITS {
        return Err(io::Error::other("a reparse point longer than its buffer"));
    }
    for (unit, &pair) in target.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *unit = u16::from_le_bytes(pair);
    }
    // An absolute target starts with `\??\`, which `std` turns into `\\?\`
    // and then into a plain path where that means the same.
    let backslash = u16::from(b'\\');
    let question = u16::from(b'?');
    if !relative && target[..units].starts_with(&[backslash, question, question, backslash]) {
        target[1] = backslash;
        return user_path(target, units);
    }
    Ok(0..units)
}

// `\\?\C:\...` as `C:\...`, and `\\?\UNC\...` as `\\...`, when Windows reads
// the shorter path as the same one; otherwise the path unchanged. This is
// `std`'s `from_wide_to_user_path`, which `std::fs::read_link` uses.
//
// Without the `\\?\` prefix, Windows changes some paths when it reads them.
// It resolves `.` and `..`, drops trailing dots and spaces, and reads names
// such as `CON` as devices. `GetFullPathNameW` applies the same rules, so a
// path that it returns unchanged means the same without the prefix.
fn user_path(path: &mut [u16; TARGET_UNITS], units: usize) -> io::Result<Range<usize>> {
    const LEGACY_MAX_PATH: usize = 260;
    // `std` counts the NUL it ends the path with.
    if units + 1 > LEGACY_MAX_PATH {
        return Ok(0..units);
    }
    let unit = |c: u8| u16::from(c);
    let is_drive = units >= 7 && path[4] != 0 && path[5] == unit(b':') && path[6] == unit(b'\\');
    let is_unc = units >= 8 && path[4..8] == [unit(b'U'), unit(b'N'), unit(b'C'), unit(b'\\')];
    let from = if is_drive {
        4
    } else if is_unc {
        path[6] = unit(b'\\');
        6
    } else {
        return Ok(0..units);
    };
    // The call takes a path that ends with a NUL, and `path` has room for it.
    path[units] = 0;
    let mut full = [0u16; LEGACY_MAX_PATH + 1];
    // SAFETY: GetFullPathNameW touches no handle, and does no I/O (rules 1,
    // 4 and 6 do not apply).
    // 2. `path` ends with a NUL, and `full` is as many units long as the
    //    call is told.
    // 3. The call reads `path`, and writes `full`, from a `&mut` borrow
    //    that nothing else uses until it returns. The file part's pointer is
    //    null, which the call takes as none.
    let written = unsafe {
        GetFullPathNameW(
            path[from..].as_ptr(),
            length(full.len()),
            full.as_mut_ptr(),
            ptr::null_mut(),
        )
    } as usize;
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    // 5. Only what the call says it wrote: a length under the buffer's is
    // the path's, without its NUL.
    if written < full.len() && full[..written] == path[from..units] {
        return Ok(from..units);
    }
    if is_unc {
        path[6] = unit(b'C');
    }
    Ok(0..units)
}

fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a conversion of a status code, which takes no pointer or
    // handle.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code.cast_signed())
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows_sys::Wdk::Storage::FileSystem::FILE_NETWORK_OPEN_INFORMATION;
    use windows_sys::Win32::Foundation::STATUS_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    };
    use windows_sys::Win32::System::SystemServices::{
        IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
    };

    use super::{Buffer, Directory};
    use crate::records::chain::{Chain, file};
    use crate::sys::stand_in::{Script, asked, handle, script};

    fn directory() -> Directory {
        // SAFETY: `handle` gives an open handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle()) };
        Directory { handle }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn names(directory: &mut Directory) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        directory
            .list(&mut Buffer::default(), |entry| {
                names.push(String::from_utf16(&entry.name().collect::<Vec<_>>()).expect("UTF-16"));
                true
            })
            .map(|()| names)
    }

    #[test]
    fn a_listing_reads_until_there_are_no_more_entries() {
        let mut first = Chain::default();
        first.push(".", file(0), 0).push("a", file(1), 0);
        let mut second = Chain::default();
        second.push("b", file(2), 0);
        script(Script {
            listing: [Ok(first.bytes), Ok(second.bytes)].into(),
            ..Script::default()
        });

        assert_eq!(names(&mut directory()).expect("list"), ["a", "b"]);
        assert_eq!(
            asked(|script| script.restarts.clone()),
            [true, false, false]
        );
    }

    #[test]
    fn a_failed_read_fails_the_listing_after_the_entries_before_it() {
        let mut first = Chain::default();
        first.push("a", file(1), 0);
        script(Script {
            listing: [Ok(first.bytes), Err(STATUS_ACCESS_DENIED)].into(),
            ..Script::default()
        });

        let mut seen = Vec::new();
        let read = directory().list(&mut Buffer::default(), |entry| {
            seen.push(entry.metadata);
            true
        });
        assert!(read.is_err());
        assert_eq!(seen, [file(1)]);
    }

    #[test]
    fn a_listing_that_is_stopped_reads_no_further() {
        let mut first = Chain::default();
        first.push("a", file(1), 0).push("b", file(1), 0);
        script(Script {
            listing: [Ok(first.bytes)].into(),
            ..Script::default()
        });

        directory()
            .list(&mut Buffer::default(), |_| false)
            .expect("list");
        assert_eq!(asked(|script| script.restarts.len()), 1);
    }

    #[test]
    fn a_directory_opens_by_its_name_relative_to_its_parent() {
        script(Script::default());
        let mut opened = directory().open_dir(&wide("sub")).expect("open");
        assert_eq!(asked(|script| script.opened.clone()), [(wide("sub"), true)]);
        assert_eq!(names(&mut opened).expect("list"), Vec::<String>::new());

        script(Script {
            create: STATUS_ACCESS_DENIED,
            ..Script::default()
        });
        assert!(directory().open_dir(&wide("sub")).is_err());
    }

    #[test]
    fn a_directory_reads_its_own_metadata() {
        script(Script {
            network_open: FILE_NETWORK_OPEN_INFORMATION {
                FileAttributes: FILE_ATTRIBUTE_DIRECTORY,
                EndOfFile: 4096,
                LastWriteTime: 7,
                ..FILE_NETWORK_OPEN_INFORMATION::default()
            },
            ..Script::default()
        });
        let metadata = directory().metadata().expect("metadata");
        assert_eq!(
            (metadata.attributes, metadata.reparse_tag),
            (FILE_ATTRIBUTE_DIRECTORY, 0)
        );
        assert_eq!((metadata.size, metadata.last_write), (4096, 7));
        // Only a reparse point has a tag to read.
        assert_eq!(asked(|script| script.attribute_tag_reads), 0);

        let attributes = FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT;
        script(Script {
            network_open: FILE_NETWORK_OPEN_INFORMATION {
                FileAttributes: attributes,
                ..FILE_NETWORK_OPEN_INFORMATION::default()
            },
            attribute_tag: FILE_ATTRIBUTE_TAG_INFO {
                FileAttributes: attributes,
                ReparseTag: IO_REPARSE_TAG_MOUNT_POINT,
            },
            ..Script::default()
        });
        let metadata = directory().metadata().expect("metadata");
        assert_eq!(metadata.reparse_tag, IO_REPARSE_TAG_MOUNT_POINT);
    }

    #[test]
    fn a_name_reads_its_own_metadata_opened_as_itself() {
        script(Script {
            network_open: FILE_NETWORK_OPEN_INFORMATION {
                EndOfFile: 5,
                LastWriteTime: 9,
                ..FILE_NETWORK_OPEN_INFORMATION::default()
            },
            ..Script::default()
        });
        let metadata = directory().metadata_of(&wide("file")).expect("metadata");
        assert_eq!((metadata.size, metadata.last_write), (5, 9));
        assert_eq!(
            asked(|script| script.opened.clone()),
            [(wide("file"), false)]
        );
    }

    // A REPARSE_DATA_BUFFER with one name as both the substitute and the
    // print name.
    fn reparse(tag: u32, target: &str, relative: bool) -> Vec<u8> {
        let name: Vec<u8> = target.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = u16::try_from(name.len()).expect("a short target");
        let mut data = Vec::new();
        data.extend(tag.to_le_bytes());
        data.extend([0; 4]);
        // The substitute name's offset and length, and the print name's.
        data.extend(
            [0u16, length, 0, length]
                .iter()
                .flat_map(|field| field.to_le_bytes()),
        );
        if tag == IO_REPARSE_TAG_SYMLINK {
            data.extend(u32::from(relative).to_le_bytes());
        }
        data.extend(name);
        data
    }

    fn read_link(reparse: Vec<u8>) -> std::io::Result<String> {
        script(Script {
            reparse,
            ..Script::default()
        });
        let target = directory().read_link(&wide("link"), String::from_utf16)?;
        assert_eq!(
            asked(|script| script.opened.clone()),
            [(wide("link"), false)]
        );
        Ok(target.expect("UTF-16"))
    }

    #[test]
    fn a_link_reads_as_std_reads_it() {
        let symlink = |target, relative| reparse(IO_REPARSE_TAG_SYMLINK, target, relative);
        assert_eq!(
            read_link(symlink(r"\??\C:\target", false)).expect("read"),
            r"C:\target"
        );
        assert_eq!(
            read_link(symlink(r"..\target", true)).expect("read"),
            r"..\target"
        );
        assert_eq!(
            read_link(symlink(r"\??\UNC\server\share", false)).expect("read"),
            r"\\server\share"
        );
        let junction = reparse(IO_REPARSE_TAG_MOUNT_POINT, r"\??\D:\target", false);
        assert_eq!(read_link(junction).expect("read"), r"D:\target");
    }

    #[test]
    fn a_short_or_unknown_reparse_point_is_refused() {
        let whole = reparse(IO_REPARSE_TAG_SYMLINK, r"\??\C:\target", false);
        for cut in 0..whole.len() {
            assert!(read_link(whole[..cut].to_vec()).is_err(), "cut at {cut}");
        }
        assert!(read_link(reparse(0x8000_0017, "x", false)).is_err());
    }
}
