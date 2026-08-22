//! A string tape: many strings in one allocation, addressed by index.
//!
//! A walk of 200,000 paths held as a `String` per row is 200,000 live
//! allocations and ~25 MB of headers and size-class rounding. One arena plus a
//! `u32` offset per string is two allocations and no rounding, and every
//! consumer here reads strings in index order anyway. Text is capped at 4 GiB
//! so an offset fits a `u32`.

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

/// An append-only, indexable collection of strings in one arena.
#[derive(Debug, Clone)]
pub struct StrTape {
    arena: String,
    /// `offs[i]..offs[i + 1]` is string `i`; always starts with a 0, so its
    /// length is one more than the number of strings.
    offs: Vec<u32>,
}

impl Default for StrTape {
    fn default() -> Self {
        Self {
            arena: String::new(),
            offs: vec![0],
        }
    }
}

impl StrTape {
    /// Room for `entries` strings over `bytes` of text.
    #[must_use]
    pub fn with_capacity(bytes: usize, entries: usize) -> Self {
        let mut offs = Vec::with_capacity(entries + 1);
        offs.push(0);
        Self {
            arena: String::with_capacity(bytes),
            offs,
        }
    }

    /// Append `s`.
    ///
    /// # Errors
    /// [`TapeFull`] if the arena would pass 4 GiB. Nothing is appended in that
    /// case, so the tape stays consistent.
    pub fn push(&mut self, s: &str) -> Result<(), TapeFull> {
        let end = u32::try_from(self.arena.len() + s.len()).map_err(|_| TapeFull)?;
        self.arena.push_str(s);
        self.offs.push(end);
        Ok(())
    }

    /// The string at `i`, or `None` past the end.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&str> {
        let end = *self.offs.get(i.checked_add(1)?)? as usize;
        let start = self.offs[i] as usize;
        Some(&self.arena[start..end])
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.offs.len().saturating_sub(1)
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of text held — not the capacity.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.arena.len()
    }

    /// Bytes the arena can hold before it grows.
    ///
    /// Exposed so a caller that sized the tape up front can assert it sized it
    /// right: "reserved exactly" and "reserved nearly, then doubled once" are
    /// invisible in the result and are the whole point of reserving.
    #[must_use]
    pub const fn text_capacity(&self) -> usize {
        self.arena.capacity()
    }

    /// Every string, in index order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.offs
            .windows(2)
            .map(|w| &self.arena[w[0] as usize..w[1] as usize])
    }
}
