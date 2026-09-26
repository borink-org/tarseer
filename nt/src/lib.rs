//! Reads a directory tree on Windows through the native API.
//!
//! # Reading a tree
//!
//! 1. Open the root with [`Directory::open`].
//! 2. Call [`Directory::list`] with a [`Buffer`]. It calls you with each
//!    [`Entry`]: its name, and the [`Metadata`] that the listing holds.
//! 3. Open a subdirectory with [`Directory::open_dir`], by its name.
//! 4. Read a directory's own metadata with [`Directory::metadata`], and the
//!    target of a link with [`Directory::read_link`].
//!
//! # Choosing it over `std::fs`
//!
//! `std::fs` opens every directory by its full path, which Windows resolves
//! one component at a time. It opens a directory again, by path, to read its
//! metadata. Win32 has no call that opens a file relative to an open
//! directory, and `NtCreateFile` has. This crate opens each directory once,
//! relative to its open parent. It then reads the listing and the metadata
//! from that handle.
//!
//! Benchmarks of tarseer on Windows Server 2025, with NTFS on a local NVMe
//! disk, showed it faster beyond noise with this crate. With warm caches, the
//! gain was largest on one directory of 100,000 files and on a tree 400 levels
//! deep. With cold caches, it was largest on trees of many small files. On
//! 100,000 directories of three files each, with cold caches, the disk set
//! the time with or without this crate.
//!
//! # Auditing the unsafe code
//!
//! The calls into the system are this crate's only `unsafe` code. The rules
//! they keep are at the top of `src/directory.rs`, and each `unsafe` block
//! says how it keeps them. This crate parses what the system writes as
//! bytes, in safe code. Its unit tests run the calls against stand-ins, also
//! under Miri.
//!
//! On other platforms this crate is empty.

#![cfg_attr(not(windows), allow(rustdoc::broken_intra_doc_links))]
#![deny(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

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
