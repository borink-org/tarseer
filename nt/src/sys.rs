// The calls into the system that `directory` makes.
//
// The crate's own unit tests, which Miri runs, make none: each call is a
// stand-in that does everything its documented contract lets the system do
// with its arguments. It reads every byte of every input it is given and
// writes every byte of every output, and checks the rules of `directory` that
// it can see, such as synchronous I/O. Miri then reports an argument that is
// not valid for all of that: a pointer out of bounds, misaligned, made from a
// shared borrow and written through, or dangling. The stand-ins answer from a
// `Script` the test sets. Tests in `tests/` make the real calls.

#[cfg(not(test))]
pub(crate) use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, NtQueryDirectoryFile, NtQueryInformationFile,
};
#[cfg(not(test))]
pub(crate) use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
#[cfg(not(test))]
pub(crate) use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandleEx, GetFullPathNameW,
};
#[cfg(not(test))]
pub(crate) use windows_sys::Win32::System::IO::DeviceIoControl;

#[cfg(test)]
pub(crate) use stand_in::*;

#[cfg(test)]
#[allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc)]
pub(crate) mod stand_in {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::os::windows::io::IntoRawHandle;
    use std::ptr;

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_INFORMATION_CLASS, FILE_NETWORK_OPEN_INFORMATION,
        FILE_SYNCHRONOUS_IO_NONALERT, FileFullDirectoryInformation, FileNetworkOpenInformation,
        NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_MORE_DATA, HANDLE, NTSTATUS, STATUS_NO_MORE_FILES, SetLastError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_INFO_BY_HANDLE_CLASS, FILE_SHARE_MODE, FileAttributeTagInfo,
    };
    use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, OVERLAPPED, PIO_APC_ROUTINE};
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    use windows_sys::core::{BOOL, PCWSTR, PWSTR};

    /// What the stand-ins answer, and what they were asked.
    #[derive(Default)]
    pub struct Script {
        /// What each read of a listing writes, in turn, and then
        /// `STATUS_NO_MORE_FILES`. `Err` fails that read.
        pub listing: VecDeque<Result<Vec<u8>, NTSTATUS>>,
        /// Whether each read of a listing asked to restart it.
        pub restarts: Vec<bool>,
        /// What `NtCreateFile` returns; it opens only if this is 0.
        pub create: NTSTATUS,
        /// The names `NtCreateFile` was asked to open, and whether each was to
        /// be a directory.
        pub opened: Vec<(Vec<u16>, bool)>,
        pub network_open: FILE_NETWORK_OPEN_INFORMATION,
        pub attribute_tag: FILE_ATTRIBUTE_TAG_INFO,
        /// How many times `FileAttributeTagInfo` was asked for.
        pub attribute_tag_reads: usize,
        /// What `FSCTL_GET_REPARSE_POINT` writes.
        pub reparse: Vec<u8>,
    }

    thread_local! {
        static SCRIPT: RefCell<Script> = RefCell::default();
    }

    /// Sets what the stand-ins answer on this thread.
    pub fn script(script: Script) {
        SCRIPT.with_borrow_mut(|current| *current = script);
    }

    /// Reads what the stand-ins were asked on this thread.
    pub fn asked<T>(read: impl FnOnce(&Script) -> T) -> T {
        SCRIPT.with_borrow(read)
    }

    /// A handle that `OwnedHandle` can close: a thread's, which Miri knows.
    pub fn handle() -> HANDLE {
        std::thread::spawn(|| {}).into_raw_handle()
    }

    // Reads every byte of `length` at `from`, as the system may.
    unsafe fn read_all(from: *const c_void, length: usize) -> Vec<u8> {
        // SAFETY: the stand-ins' callers promise `from` is readable for
        // `length` bytes; Miri checks it.
        unsafe { std::slice::from_raw_parts(from.cast::<u8>(), length) }.to_vec()
    }

    // Writes every byte of `length` at `to`, as the system may, and then
    // `value` at its start.
    unsafe fn write_all(to: *mut c_void, length: usize, value: &[u8]) {
        assert!(value.len() <= length, "the stand-in's answer fits");
        // SAFETY: the stand-ins' callers promise `to` is writable for
        // `length` bytes; Miri checks it.
        unsafe {
            ptr::write_bytes(to.cast::<u8>(), 0xa5, length);
            ptr::copy_nonoverlapping(value.as_ptr(), to.cast::<u8>(), value.len());
        }
    }

    fn bytes_of<T>(value: &T) -> Vec<u8> {
        // SAFETY: `T` is one of the system's structs, all integers, and
        // `value` is a live reference to one.
        unsafe { read_all(ptr::from_ref(value).cast(), size_of::<T>()) }
    }

    unsafe fn complete(
        status_block: *mut IO_STATUS_BLOCK,
        status: NTSTATUS,
        written: usize,
    ) -> NTSTATUS {
        let mut block = IO_STATUS_BLOCK::default();
        block.Anonymous.Status = status;
        block.Information = written;
        // SAFETY: the callers promise the status block is writable; Miri
        // checks it.
        unsafe { status_block.write(block) };
        status
    }

    pub unsafe extern "system" fn NtQueryDirectoryFile(
        _handle: HANDLE,
        event: HANDLE,
        apc_routine: PIO_APC_ROUTINE,
        _apc_context: *const c_void,
        status_block: *mut IO_STATUS_BLOCK,
        information: *mut c_void,
        length: u32,
        class: FILE_INFORMATION_CLASS,
        single: bool,
        name: *const UNICODE_STRING,
        restart: bool,
    ) -> NTSTATUS {
        assert!(
            event.is_null() && apc_routine.is_none(),
            "no completion but the call's return"
        );
        assert!(name.is_null() && !single);
        assert_eq!(class, FileFullDirectoryInformation);
        assert!(
            information.cast::<u64>().is_aligned(),
            "the buffer is 8-byte aligned"
        );
        let next = SCRIPT.with_borrow_mut(|script| {
            script.restarts.push(restart);
            script.listing.pop_front()
        });
        let (answer, status) = match next {
            None => (Vec::new(), STATUS_NO_MORE_FILES),
            Some(Ok(records)) => (records, 0),
            Some(Err(status)) => (Vec::new(), status),
        };
        // SAFETY: the callers promise the buffer is writable for `length`
        // bytes and the status block is writable; Miri checks both.
        unsafe {
            write_all(information, length as usize, &answer);
            complete(status_block, status, answer.len())
        }
    }

    pub unsafe extern "system" fn NtCreateFile(
        handle: *mut HANDLE,
        _access: FILE_ACCESS_RIGHTS,
        attributes: *const OBJECT_ATTRIBUTES,
        status_block: *mut IO_STATUS_BLOCK,
        allocation_size: *const i64,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _share: FILE_SHARE_MODE,
        _disposition: NTCREATEFILE_CREATE_DISPOSITION,
        options: NTCREATEFILE_CREATE_OPTIONS,
        extended_attributes: *const c_void,
        extended_attributes_length: u32,
    ) -> NTSTATUS {
        assert_ne!(options & FILE_SYNCHRONOUS_IO_NONALERT, 0, "synchronous I/O");
        assert!(allocation_size.is_null() && extended_attributes.is_null());
        assert_eq!(extended_attributes_length, 0);
        // SAFETY: the callers promise every pointer in the attributes is
        // readable for what it says; Miri checks them.
        let name = unsafe {
            let attributes = attributes.read();
            let name = attributes.ObjectName.read();
            assert_eq!(name.Length % 2, 0);
            assert!(name.Length <= name.MaximumLength);
            let bytes = read_all(name.Buffer.cast(), usize::from(name.Length));
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect()
        };
        let directory = options & FILE_DIRECTORY_FILE != 0;
        let status = SCRIPT.with_borrow_mut(|script| {
            script.opened.push((name, directory));
            script.create
        });
        if status == 0 {
            // SAFETY: the callers promise `handle` is writable.
            unsafe { handle.write(self::handle()) };
        }
        // SAFETY: as above, for the status block.
        unsafe { complete(status_block, status, 0) }
    }

    pub unsafe extern "system" fn NtQueryInformationFile(
        _handle: HANDLE,
        status_block: *mut IO_STATUS_BLOCK,
        information: *mut c_void,
        length: u32,
        class: FILE_INFORMATION_CLASS,
    ) -> NTSTATUS {
        assert_eq!(class, FileNetworkOpenInformation);
        assert!(
            information
                .cast::<FILE_NETWORK_OPEN_INFORMATION>()
                .is_aligned()
        );
        let answer = SCRIPT.with_borrow(|script| bytes_of(&script.network_open));
        // SAFETY: the callers promise the buffer is writable for `length`
        // bytes and the status block is writable; Miri checks both.
        unsafe {
            write_all(information, length as usize, &answer);
            complete(status_block, 0, answer.len())
        }
    }

    pub unsafe extern "system" fn GetFileInformationByHandleEx(
        _handle: HANDLE,
        class: FILE_INFO_BY_HANDLE_CLASS,
        information: *mut c_void,
        length: u32,
    ) -> BOOL {
        assert_eq!(class, FileAttributeTagInfo);
        assert!(information.cast::<FILE_ATTRIBUTE_TAG_INFO>().is_aligned());
        let answer = SCRIPT.with_borrow_mut(|script| {
            script.attribute_tag_reads += 1;
            bytes_of(&script.attribute_tag)
        });
        // SAFETY: the callers promise the buffer is writable for `length`
        // bytes; Miri checks it.
        unsafe { write_all(information, length as usize, &answer) };
        1
    }

    pub unsafe extern "system" fn DeviceIoControl(
        _handle: HANDLE,
        code: u32,
        input: *const c_void,
        input_length: u32,
        output: *mut c_void,
        output_length: u32,
        returned: *mut u32,
        overlapped: *mut OVERLAPPED,
    ) -> BOOL {
        assert_eq!(code, FSCTL_GET_REPARSE_POINT);
        assert!(overlapped.is_null(), "no completion but the call's return");
        let answer = SCRIPT.with_borrow(|script| script.reparse.clone());
        // SAFETY: the callers promise the input is readable, the output
        // writable for their lengths, and `returned` writable; Miri checks
        // them.
        unsafe {
            if !input.is_null() {
                read_all(input, input_length as usize);
            }
            if answer.len() > output_length as usize {
                SetLastError(ERROR_MORE_DATA);
                return 0;
            }
            write_all(output, output_length as usize, &answer);
            returned.write(u32::try_from(answer.len()).expect("a short answer"));
        }
        1
    }

    // Gives the path back as it was given: a path that is already full.
    pub unsafe extern "system" fn GetFullPathNameW(
        name: PCWSTR,
        length: u32,
        full: PWSTR,
        file_part: *mut PWSTR,
    ) -> u32 {
        let mut units = Vec::new();
        // SAFETY: the callers promise `name` is readable up to its NUL;
        // Miri checks it.
        unsafe {
            while *name.add(units.len()) != 0 {
                units.push(*name.add(units.len()));
            }
        }
        let needed = u32::try_from(units.len() + 1).expect("a short path");
        if needed > length {
            return needed;
        }
        units.push(0);
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        // SAFETY: the callers promise `full` is writable for `length` units,
        // and `file_part` writable if it is not null; Miri checks them.
        unsafe {
            write_all(full.cast(), length as usize * 2, &bytes);
            if !file_part.is_null() {
                file_part.write(ptr::null_mut());
            }
        }
        needed - 1
    }

    pub unsafe extern "system" fn RtlNtStatusToDosError(status: NTSTATUS) -> u32 {
        // Enough of the system's table for the tests.
        match status.cast_unsigned() {
            0xC000_0034 => 2,
            _ => 317,
        }
    }
}
