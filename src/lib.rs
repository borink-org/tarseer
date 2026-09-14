// TODO(docs): everything public in this crate is scaffold — the `//!` and
// `///` blocks are notes with the substance in them, not finished prose. Each
// needs a human pass before this is published. `//` comments like this one are
// for us and are not held to that.

//! Walk a directory tree and record what is there.
//!
//! ```no_run
//! let tree = tarseer::walk(std::path::Path::new(".")).unwrap();
//! for path in tree.paths() {
//!     println!("{path}");
//! }
//! ```
//!
//! - [`walk()`] → [`SourceTree`]: three columns of fixed-size `Copy` rows over
//!   one string tape
//! - order: what a plain recursive sorted walk would visit
//! - planned from, not iterated — sizes and kinds in their own columns, so
//!   deciding *what work to do* never touches the strings
//! - opens no file, reads no contents
//! - [`SourceTree::to_json`]: same shape, one array per column; the whole of
//!   what the command prints
//!
//! # Errors
//!
//! - [`error_stack::Report`] over one of [`WalkError`], [`TreeFull`],
//!   [`JsonError`]
//! - one context per unit of fallibility, not per call site
//! - attached: what a caller cannot reconstruct (the entry tripped over)
//! - not attached: what it already holds (the root it passed in)

#![forbid(unsafe_code)]

pub mod json;
pub mod tape;
pub mod tree;
pub mod walk;

pub use crate::json::JsonError;
pub use crate::tree::{DirRow, FileRow, LinkRow, Skips, SourceTree, TreeFull};
pub use crate::walk::{WalkError, walk};
