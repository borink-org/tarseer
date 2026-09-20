//! Walks a directory tree and records every entry in it, in parts that each
//! fit a memory budget.
//!
//! [`walk_parts`] hands each [`Part`] to a sink as soon as the part is
//! complete, and [`walk()`] collects them. The [`walk`](mod@walk) module
//! describes the walk and where parts are cut. [`write_manifest`] writes a
//! walk as a compressed manifest, and the [`manifest`] module describes that.
//! The walk opens no file and reads no contents.
//!
//! # Examples
//!
//! ```
//! use std::path::Path;
//! use tarseer::{WalkOptions, walk_parts};
//!
//! let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
//! let mut rows = 0;
//! let skips = walk_parts(&root, &WalkOptions::default(), &mut |part| {
//!     rows += part.len();
//!     Ok(())
//! })
//! .unwrap();
//! assert!(rows > 0);
//! assert!(!skips.any());
//! ```
//!
//! # Errors
//!
//! Every fallible call returns an [`error_stack::Report`] over one of
//! [`WalkError`], [`JsonError`], [`WriteError`] and [`ReadError`]. The report
//! names what you cannot work out for yourself, such as the entry the walk
//! failed on. [`Cancelled`] and [`PartFull`] keep their own types inside a
//! [`WalkError`]: `report.contains::<Cancelled>()`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod json;
pub mod manifest;
pub mod part;
pub mod tape;
pub mod walk;

pub use crate::json::JsonError;
pub use crate::manifest::{
    Index, Manifest, PartEntry, ReadError, WriteError, WriteOptions, Written, write_manifest,
};
pub use crate::part::{
    DirectoryRow, EntryKind, FileRow, Part, PartFull, SymlinkRow, Timestamp, walk_order,
};
pub use crate::walk::{
    Cancelled, Candidate, DEFAULT_BUDGET, Filter, Listing, OnError, Progress, SkipReason, Skips,
    Walk, WalkError, WalkOptions, estimate, walk, walk_parts,
};
