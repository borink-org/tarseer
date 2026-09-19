//! Many strings in one allocation, each addressed by its index.
//!
//! [`StrTape`] holds every string in one text buffer and one `u32` offset per
//! string. This crate uses it for the names in a [`Part`](crate::Part).
//!
//! The layout is the one the `stringtape` crate uses (see the README). That
//! crate reads its strings back through `from_utf8_unchecked`, and this crate
//! forbids `unsafe` code, so the layout is written out here instead.

use std::fmt;

/// The tape holds 4 GiB of text and cannot take more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeFull;

impl fmt::Display for TapeFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("string tape exceeded its 4 GiB limit")
    }
}

impl std::error::Error for TapeFull {}

/// An append-only sequence of strings, addressed by index.
///
/// The text lives in one buffer, and each string is a `u32` offset into it.
/// A tape is two allocations however many strings it holds. The text is
/// limited to 4 GiB so that an offset fits a `u32`.
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
    /// Creates an empty tape with room for `entries` strings holding `bytes`
    /// of text in total.
    #[must_use]
    pub fn with_capacity(bytes: usize, entries: usize) -> Self {
        let mut offsets = Vec::with_capacity(entries + 1);
        offsets.push(0);
        Self {
            arena: String::with_capacity(bytes),
            offsets,
        }
    }

    /// Appends `text` as the next string.
    ///
    /// # Errors
    /// [`TapeFull`] if appending `text` would take the tape past 4 GiB. The
    /// tape is unchanged after that error.
    pub fn push(&mut self, text: &str) -> Result<(), TapeFull> {
        let end = u32::try_from(self.arena.len() + text.len()).map_err(|_| TapeFull)?;
        self.arena.push_str(text);
        self.offsets.push(end);
        Ok(())
    }

    /// Returns the string at `index`, or `None` if the tape holds fewer than
    /// `index + 1` strings.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        let end = *self.offsets.get(index.checked_add(1)?)? as usize;
        let start = self.offsets[index] as usize;
        Some(&self.arena[start..end])
    }

    /// Returns the number of strings.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Returns `true` if the tape holds no strings.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the bytes of text held, summed over every string.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.arena.len()
    }

    /// Returns every string, in the order it was pushed.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.offsets
            .windows(2)
            .map(|window| &self.arena[window[0] as usize..window[1] as usize])
    }
}
