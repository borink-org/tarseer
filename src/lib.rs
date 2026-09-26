//! Walks a directory tree and records every entry in it, in parts that each
//! fit a memory budget.
//!
//! [`walk_parts`] hands each [`TreePart`] to a sink as soon as the part is
//! complete, and [`walk()`] collects them. The [`walk`](mod@walk) module
//! describes the walk and where parts are cut. The walk opens no file and
//! reads no contents.
//!
//! The [`manifest`] module holds what a walk produces: the parts and an
//! index of them. [`write_frames`] encodes each part and the index into a
//! frame of bytes with a [`Codec`] of your choice. The [`mod@file`] module writes
//! the frames as one file and reads it back, in a [`Format`](file::Format)
//! of your choice.
#![cfg_attr(
    feature = "zstd",
    doc = "\nThe `zstd` feature, on by default, adds the [`mod@zstd`] module: the zstd codec and\nfile format."
)]
#![cfg_attr(
    not(feature = "zstd"),
    doc = "\nThe `zstd` feature, which is off in this build, adds the zstd codec and file format."
)]
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
//! failed on. [`Cancelled`] and [`TreePartFull`] keep their own types inside a
//! [`WalkError`]: `report.contains::<Cancelled>()`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod file;
pub mod frames;
pub mod json;
pub mod manifest;
pub mod tape;
pub mod walk;
#[cfg(feature = "zstd")]
pub mod zstd;

pub use crate::frames::{
    Codec, Encoder, Frame, FrameKind, Plain, ReadError, WriteError, decode_index, decode_line,
    write_frames,
};
pub use crate::json::JsonError;
pub use crate::manifest::{
    DirectoryRow, EntryKind, FileRow, Index, PartEntry, SymlinkRow, Timestamp, TreePart,
    TreePartFull, walk_order,
};
pub use crate::walk::{
    Cancelled, Candidate, DEFAULT_BUDGET, Filter, Listing, OnError, Progress, SkipReason, Skips,
    Walk, WalkError, WalkOptions, estimate, walk, walk_parts,
};
