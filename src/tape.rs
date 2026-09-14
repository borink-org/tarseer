// TODO(docs): scaffold. Public docs in this file are notes, not prose.

//! A string tape: many strings in one allocation, addressed by index.
//!
//! Tape: one text arena, one `u32` offset per string, append-only. Not the
//! `stringtape` crate — that reaches its `&str` through
//! `from_utf8_unchecked`, and this crate is `forbid(unsafe_code)`.
//! Attribution in the README.

use std::fmt;

/// The tape's 4 GiB text limit was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeFull;

impl fmt::Display for TapeFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("string tape exceeded its 4 GiB limit")
    }
}

impl std::error::Error for TapeFull {}

/// Append-only, indexable strings in one arena.
///
/// - one `String` per row: one live allocation each, ~25 MB of headers and
///   size-class rounding at 200k paths
/// - arena + one `u32` offset: two allocations, no rounding
/// - text capped at 4 GiB, so an offset fits a `u32`
#[derive(Debug, Clone)]
pub struct StrTape {
    arena: String,
    /// `offsets[index]..offsets[index + 1]` is string `index`. Always starts
    /// with a 0, so it is one longer than the number of strings.
    offsets: Vec<u32>,
}

impl Default for StrTape {
    fn default() -> Self {
        Self {
            arena: String::new(),
            offsets: vec![0],
        }
    }
}

impl StrTape {
    /// Room for `entries` strings over `bytes` of text.
    #[must_use]
    pub fn with_capacity(bytes: usize, entries: usize) -> Self {
        let mut offsets = Vec::with_capacity(entries + 1);
        offsets.push(0);
        Self {
            arena: String::with_capacity(bytes),
            offsets,
        }
    }

    /// Append `text`.
    ///
    /// # Errors
    /// [`TapeFull`] past 4 GiB. Nothing appended, so the tape stays
    /// consistent.
    pub fn push(&mut self, text: &str) -> Result<(), TapeFull> {
        let end = u32::try_from(self.arena.len() + text.len()).map_err(|_| TapeFull)?;
        self.arena.push_str(text);
        self.offsets.push(end);
        Ok(())
    }

    /// The string at `index`, or `None` past the end.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        let end = *self.offsets.get(index.checked_add(1)?)? as usize;
        let start = self.offsets[index] as usize;
        Some(&self.arena[start..end])
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of text held.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.arena.len()
    }

    /// Every string, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.offsets
            .windows(2)
            .map(|window| &self.arena[window[0] as usize..window[1] as usize])
    }
}
