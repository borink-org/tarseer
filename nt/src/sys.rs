// The calls into the system that `directory` makes.
//
// The crate's unit tests, which Miri runs, make none of them. Each call is a
// stand-in that does everything its documented contract lets the system do
// with its arguments. It reads every byte of every input, and writes every
// byte of every output. It also checks the rules of `directory` that it can
// see, such as synchronous I/O. Miri then reports an argument that is not
// valid for all of that. Such a pointer is out of bounds, misaligned,
// dangling, or made from a shared borrow and written through. The stand-ins
// answer from a `Script` that the test sets.
//
// The stand-ins are in `sys/stand_in.rs`, part of the crate when it is built
// for its unit tests: the crate's own code calls them. Tests in `tests/` build
// against the crate as it ships, and make the real calls.

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
pub(crate) mod stand_in;
#[cfg(test)]
pub(crate) use stand_in::*;
