//! Safe calls into the Windows native API for reading a directory tree.
//!
//! `Directory::list` reads a directory with `NtQueryDirectoryFile`, which
//! gives every entry's size, times and attributes with its name.
//! `Directory::open_dir` opens a directory with `NtCreateFile` relative to
//! its open parent, as `openat` does on Unix. `std` opens every directory by
//! its full path, which Windows resolves from the root each time, and a
//! directory's own metadata takes it another open.
//!
//! This crate holds the `unsafe` code that tarseer's Windows reader needs: the
//! calls into the system, and taking ownership of the handles they return.
//! The rules those calls keep are at the top of `src/directory.rs`. What the
//! system writes is parsed as bytes, in safe code.
//!
//! On other platforms the crate is empty.

#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(windows)]
mod directory;
#[cfg(windows)]
mod records;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use directory::{Buffer, Directory};
#[cfg(windows)]
pub use records::{Entry, Metadata};
