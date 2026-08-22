//! A string arena that never moves what it has already written.
//!
//! The walk hands out `&str` into this while it is still appending, so growth
//! must not copy. Blocks start at `BLOCK_MIN` and double to `BLOCK_MAX`; a
//! string never straddles two. A [`Span`] packs the block index into the high
//! bits of a `u32` and the offset into the low ones.
//!
//! A *fixed* 64 KiB block is a trap: one near-empty block per scan task, over
//! thousands of tasks, took peak live memory from 58 to 384 MiB on a 180,000
//! entry tree. Doubling from small means a task that scans one short directory
//! keeps one short block.

use crate::bail;
use crate::error::{Context, Result};

const BLOCK_SHIFT: u32 = 16;
const BLOCK_MAX: usize = 1 << BLOCK_SHIFT;
const BLOCK_MASK: usize = BLOCK_MAX - 1;
const BLOCK_MIN: usize = 256;

/// Where one string sits in the [`Arena`] that wrote it. Carries no evidence
/// of which arena that was, so the type owning both is what keeps them
/// together.
#[derive(Clone, Copy)]
pub struct Span {
    start: u32,
    end: u32,
}

impl Span {
    /// Length in bytes of the string this span covers.
    #[must_use]
    pub const fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Default)]
pub struct Arena {
    blocks: Vec<String>,
}

impl Arena {
    /// Write `parts` end to end as one string and hand back its span.
    ///
    /// # Errors
    /// If the joined string is longer than a block, or the arena has more
    /// blocks than a span can address.
    pub fn push(&mut self, parts: &[&str]) -> Result<Span> {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len > BLOCK_MAX {
            bail!("string of {len} bytes does not fit an arena block of {BLOCK_MAX}");
        }
        if self
            .blocks
            .last()
            .is_none_or(|b| b.len() + len > b.capacity())
        {
            let cap = self
                .blocks
                .last()
                .map_or(BLOCK_MIN, |b| (b.capacity() * 2).min(BLOCK_MAX))
                .max(len);
            self.blocks.push(String::with_capacity(cap));
        }
        let block = u32::try_from(self.blocks.len() - 1)
            .ok()
            .filter(|b| *b <= u32::MAX >> BLOCK_SHIFT)
            .ctx(|| "arena exceeds the blocks a u32 span can address".into())?;
        let last = self
            .blocks
            .last_mut()
            .unwrap_or_else(|| unreachable!("just pushed"));
        let start = (block << BLOCK_SHIFT) | u32::try_from(last.len()).unwrap_or_default();
        for p in parts {
            last.push_str(p);
        }
        Ok(Span {
            start,
            end: start + u32::try_from(len).unwrap_or_default(),
        })
    }

    /// Resolve a span written into *this* arena.
    ///
    /// Slices by length rather than by masking `end`, because a string that
    /// ends exactly on a block boundary has an `end` whose masked offset is
    /// zero in the *next* block.
    ///
    /// # Panics
    /// If `span` came from a different arena.
    #[must_use]
    pub fn get(&self, span: Span) -> &str {
        let block = &self.blocks[(span.start >> BLOCK_SHIFT) as usize];
        let off = span.start as usize & BLOCK_MASK;
        &block[off..off + span.len()]
    }

    /// Bytes interned — block slack excluded, because the caller is sizing a
    /// tape these strings are about to be copied into.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.blocks.iter().map(String::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_survive_the_arena_growing() {
        let mut a = Arena::default();
        let spans: Vec<Span> = (0..2000)
            .map(|i| a.push(&[&format!("entry-{i}")]).unwrap())
            .collect();
        for (i, s) in spans.iter().enumerate() {
            assert_eq!(a.get(*s), format!("entry-{i}"));
        }
    }

    #[test]
    fn a_string_ending_on_a_block_boundary_reads_back() {
        let mut a = Arena::default();
        // Fill the first block exactly, then read the string that closed it.
        let filler = "x".repeat(BLOCK_MIN);
        let s = a.push(&[&filler]).unwrap();
        a.push(&["next"]).unwrap();
        assert_eq!(a.get(s), filler);
    }

    #[test]
    fn a_string_larger_than_a_block_is_refused() {
        let mut a = Arena::default();
        assert!(a.push(&[&"x".repeat(BLOCK_MAX + 1)]).is_err());
    }
}
