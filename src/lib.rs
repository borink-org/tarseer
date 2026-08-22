//! Walk a directory tree and record what is there.
//!
//! ```no_run
//! let tree = tarseer::walk(std::path::Path::new(".")).unwrap();
//! for p in tree.paths() {
//!     println!("{p}");
//! }
//! ```
//!
//! [`walk()`] produces a [`SourceTree`]: three columns of fixed-size `Copy`
//! rows over one string tape, in the order a plain recursive sorted walk would
//! visit them. The walk itself runs on a thread pool, one task per directory,
//! over arenas that never move what they have already written.
//!
//! The tree is built to be planned from rather than iterated — sizes and kinds
//! sit in their own columns, so deciding *what work to do* never touches the
//! strings. Nothing here opens a file or reads its contents.

#![forbid(unsafe_code)]

pub mod arena;
pub mod error;
pub mod tape;
pub mod tree;
pub mod walk;

pub use crate::error::{BoxError, Result};
pub use crate::tree::{DirRow, FileRow, LinkRow, Skips, SourceTree};
pub use crate::walk::walk;
