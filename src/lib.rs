//! Walks a directory tree and records every entry in it, in parts that each
//! fit a memory budget.
//!
//! This crate is the source half of a `.tar.zst` archiver. It walks a tree,
//! cuts the walk into [`Part`]s, writes a part as JSON, and writes every part
//! of a walk into one compressed, indexed manifest. It opens no file and
//! reads no contents.
//!
//! # How a walk works
//!
//! 1. Fill a [`WalkOptions`]. [`WalkOptions::default()`] records everything
//!    under the root, fails on the first entry it cannot read, and cuts parts
//!    at [`DEFAULT_BUDGET`], 4 MiB of estimated JSON.
//! 2. Call [`walk_parts`] with the root and a sink. The walk hands the sink
//!    each [`Part`] as soon as the part is complete, in walk order, and holds
//!    nothing of it afterwards. [`walk()`] does the same and collects the
//!    parts into a [`Walk`].
//! 3. Read each part's rows ([`Part::dirs`], [`Part::files`],
//!    [`Part::links`]), or write it with [`Part::to_json`].
//!
//! A part is a contiguous run of the walk, with the directories above its
//! first row as a stem. You can read it without any other part. The
//! [`walk`](mod@walk) module says where the cuts fall and what the walk holds
//! in memory.
//!
//! # Examples
//!
//! Count the rows under this crate's `src` directory, one part at a time:
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
//! # Writing and reading a manifest
//!
//! [`write_manifest`] walks a root and writes every part, compressed, into
//! one manifest with an index. [`Manifest::parse`] opens a manifest held in
//! memory, and [`Manifest::part_json`] decompresses one part of it.
//!
//! ```
//! use std::path::Path;
//! use tarseer::{Manifest, WalkOptions, WriteOptions, write_manifest};
//!
//! let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
//! let mut bytes = Vec::new();
//! let written = write_manifest(
//!     &root,
//!     &WalkOptions::default(),
//!     &WriteOptions::default(),
//!     &mut bytes,
//! )
//! .unwrap();
//!
//! let manifest = Manifest::parse(&bytes).unwrap();
//! assert_eq!(manifest.index, written.index);
//! let json = manifest.part_json(0).unwrap();
//! assert!(json.starts_with(b"{\"stem\":[]"));
//! ```
//!
//! The [`manifest`] module describes the layout and what each compression
//! thread holds.
//!
//! # Errors
//!
//! Every fallible call returns an [`error_stack::Report`] over one of
//! [`WalkError`], [`JsonError`], [`WriteError`] and [`ReadError`]. The report
//! carries what you cannot work out for yourself, such as the entry the walk
//! failed on. It does not repeat what you passed in, such as the root.
//!
//! Two causes keep their own types inside a [`WalkError`]: [`Cancelled`] and
//! [`PartFull`]. Test for them with `report.contains::<Cancelled>()`.

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
pub use crate::part::{DirRow, EntryKind, FileRow, LinkRow, Part, PartFull, Timestamp, walk_order};
pub use crate::walk::{
    Cancelled, Candidate, DEFAULT_BUDGET, Filter, Listing, OnError, Progress, SkipReason, Skips,
    Walk, WalkError, WalkOptions, estimate, walk, walk_parts,
};
