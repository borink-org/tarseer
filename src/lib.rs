// TODO(docs): everything public in this crate is scaffold — the `//!` and
// `///` blocks are notes with the substance in them, not finished prose. Each
// needs a human pass before this is published. `//` comments like this one are
// for us and are not held to that.

//! Walk a directory tree and record what is there, in independent parts.
//!
//! ```no_run
//! use tarseer::{WalkOptions, walk};
//!
//! let walk = walk(std::path::Path::new("."), &WalkOptions::default()).unwrap();
//! for path in walk.paths() {
//!     println!("{path}");
//! }
//! ```
//!
//! - [`walk_parts()`] → a stream of [`Part`]s in walk order, each a contiguous
//!   run of it with a stem, readable on its own; [`walk()`] collects them
//! - cut by tree structure against a budget of estimated JSON bytes, so what is
//!   held at once stays bounded however large the tree
//! - hooks, all optional and dynamic: a [`Filter`], [`Progress`], a cancel
//!   flag, and an [`OnError`] policy
//! - opens no file, reads no contents
//! - [`Part::to_json`]: one document per part, one array per column
//! - [`write_manifest()`]: the walk's parts as independent zstd frames with an
//!   index and a footer, all in skippable frames; [`Manifest`] reads it back
//! - every frame carries a checksum, and its match window is capped, which is
//!   what bounds a compression thread:
//!   [`DEFAULT_WINDOW_LOG`](manifest::DEFAULT_WINDOW_LOG)
//!
//! # Errors
//!
//! - [`error_stack::Report`] over one of [`WalkError`], [`JsonError`],
//!   [`WriteError`], [`ReadError`]; causes
//!   [`Cancelled`] and [`PartFull`] stay distinguishable inside a walk report
//! - attached: what a caller cannot reconstruct (the entry tripped over)
//! - not attached: what it already holds (the root it passed in)

#![forbid(unsafe_code)]

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
