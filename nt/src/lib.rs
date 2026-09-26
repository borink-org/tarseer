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
//! calls into the system, and taking ownership of the handles they return. The
//! records the system writes are read as bytes, without `unsafe`.
//!
//! On other platforms the crate is empty.

#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(windows)]
mod directory;

#[cfg(windows)]
pub use directory::{Buffer, Directory, Entry, Metadata};
