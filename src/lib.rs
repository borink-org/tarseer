//! Walk a directory tree and record what is there.
//!
//! ```no_run
//! let tree = tarseer::walk(std::path::Path::new("."))?;
//! let index = tarseer::index_tree(&tree)?;
//! println!("{}", index.summary());
//! # Ok::<(), tarseer::BoxError>(())
//! ```
//!
//! [`walk()`] produces a [`SourceTree`]: three columns of fixed-size `Copy`
//! rows over one string tape, in the order a plain recursive sorted walk would
//! visit them. The walk itself runs on a thread pool, one task per directory,
//! over arenas that never move what they have already written, reading each
//! directory with `getdents64` on Linux and `read_dir` elsewhere.
//!
//! [`index_tree`] turns that into an [`Index`]: every entry in path order,
//! stored as columns, with ancestry interned once in a directory table so a
//! path costs only its own last component.
//!
//! [`hash_tree`] is the one thing here that reads file contents, and it is
//! separate for that reason: indexing a tree stays as cheap as walking it
//! unless digests are asked for.
//!
//! Both halves are built to be planned from rather than iterated — sizes and
//! kinds sit in their own columns, so deciding *what work to do* never touches
//! the strings. Nothing here opens a file or reads its contents.

#![forbid(unsafe_code)]

pub mod arena;
pub mod error;
pub mod hash;
pub mod index;
#[cfg(target_os = "linux")]
mod rustix_scan;
pub mod tape;
pub mod tree;
pub mod walk;

pub use crate::error::{BoxError, Result};
pub use crate::hash::{Digest, hash_tree};
pub use crate::index::{Index, Kind, PathScratch, index_tree, index_tree_with};
pub use crate::tree::{DirRow, FileRow, LinkRow, Skips, SourceTree};
pub use crate::walk::walk;
