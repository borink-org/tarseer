//! Content digests for the files a walk found.
//!
//! blake3, over the tree's file column, on the pool. The walk deliberately
//! reads no file contents; this is the one place that does, and it is separate
//! so that indexing a tree stays as cheap as walking it.
//!
//! # Why a whole-file read rather than a stream
//!
//! Files are hashed one per task, and blake3 is fast enough that on a warm
//! cache the read dominates. Two-thirds of the files in a source tree fit in a
//! single page, so a task that reads the file whole and hashes it in memory
//! beats one that streams a buffer through the hasher; the exception is the
//! rare large file, which is read in `CHUNK`-sized pieces so an ISO in the
//! tree does not put its whole self in RAM.
//!
//! A file that changed between the walk and the read is an **error**, not a
//! shrug: the size in the tree is what later stages reserve space for, and a
//! digest certifying different bytes than the ones that get stored is worse
//! than no digest at all.

use std::io::Read;
use std::path::Path;

use rayon::prelude::*;

use crate::bail;
use crate::error::{Context, Result};
use crate::tree::SourceTree;

/// A content digest as the hasher produces it: 32 raw bytes.
pub type Digest = [u8; 32];

/// Bytes per read for a file too big to hold whole.
const CHUNK: usize = 1 << 20;

/// Above this, read in `CHUNK`s instead of all at once.
const WHOLE_MAX: u64 = 8 << 20;

/// The name of the algorithm, as the index records it.
pub const ALGO: &str = "blake3";

/// Hash every file in `tree`, in `tree.files` order.
///
/// `root` is the directory the tree was walked from; rows carry relative
/// paths, so this is what makes them openable again.
///
/// # Errors
/// If a file cannot be opened or read, or if its size no longer matches what
/// the walk recorded.
pub fn hash_tree(root: &Path, tree: &SourceTree) -> Result<Vec<Digest>> {
    tree.files
        .par_iter()
        .map(|f| {
            let rel = tree.text(f.rel);
            hash_file(&root.join(rel), f.size)
                .map_err(|e| -> crate::error::BoxError { format!("hash {rel}: {e}").into() })
        })
        .collect()
}

/// Hash one file, checking it is still the size the walk saw.
///
/// # Errors
/// If the file cannot be opened or read, or has changed size.
pub fn hash_file(path: &Path, expect: u64) -> Result<Digest> {
    let mut f = std::fs::File::open(path).ctx(|| format!("open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();

    let read = if expect <= WHOLE_MAX {
        let mut buf = Vec::with_capacity(usize::try_from(expect).unwrap_or(0));
        let n = f
            .read_to_end(&mut buf)
            .ctx(|| format!("read {}", path.display()))?;
        hasher.update(&buf);
        u64::try_from(n).unwrap_or(u64::MAX)
    } else {
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            let n = f
                .read(&mut buf)
                .ctx(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += u64::try_from(n).unwrap_or(0);
        }
        total
    };

    if read != expect {
        bail!(
            "{} was {expect} bytes at walk time and {read} when read",
            path.display()
        );
    }
    Ok(*hasher.finalize().as_bytes())
}

/// The hex form of a [`Digest`], on the stack.
#[derive(Clone, Copy)]
pub struct HexDigest([u8; 64]);

impl HexDigest {
    /// Hex-encode `d`, lowercase, as the index stores it.
    #[must_use]
    pub fn of(d: &Digest) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 64];
        for (i, &b) in d.iter().enumerate() {
            out[i * 2] = HEX[(b >> 4) as usize];
            out[i * 2 + 1] = HEX[(b & 0xf) as usize];
        }
        Self(out)
    }

    /// The 64 hex characters.
    ///
    /// # Panics
    /// Cannot: every byte written above is an ASCII hex digit.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("hex is ASCII")
    }
}
