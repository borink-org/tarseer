//! Walk a directory tree and index what is there.
//!
//! ```no_run
//! let tree = tarseer::walk(std::path::Path::new("."))?;
//! let index = tarseer::index_tree(&tree)?;
//! println!("{}", index.summary());
//! # Ok::<(), tarseer::BoxError>(())
//! ```
//!
//! # Shape
//!
//! [`walk()`] produces a [`SourceTree`]: three columns of fixed-size rows over
//! one string tape, in the order a plain recursive sorted walk would visit
//! them. [`index_tree`] turns that into an [`Index`], which stores ancestry
//! once and every row sorted by path.
//!
//! Both halves are built to be planned from rather than iterated: sizes and
//! kinds sit in their own columns, so deciding *what work to do* never touches
//! the strings. That is what later stages pack and extract in parallel from.
//! Nothing here opens a file or reads its contents.

#![forbid(unsafe_code)]

pub mod arena;
pub mod error;
pub mod index;
pub mod tape;
pub mod tree;
pub mod walk;

pub use crate::error::{BoxError, Result};
pub use crate::index::{Index, Kind, PathScratch, index_tree};
pub use crate::tree::{Skips, SourceTree};
pub use crate::walk::walk;
