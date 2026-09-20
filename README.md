# tarseer

Walks a directory tree into memory-bounded parts and writes them as a
compressed, indexed manifest. It opens no file and reads no contents.

## Walking a tree

```
tarseer <dir> [--budget BYTES]
```

Writes the walk of `<dir>` to stdout as JSON, one part per line, and the
counts to stderr. A part is a contiguous run of the walk that you can read
without any other part; `--budget` sets how many estimated JSON bytes a part
may hold before the walk cuts it (4 MiB by default).

## Writing a manifest

```
tarseer <dir> --out FILE [--budget BYTES] [--level N] [--window-log N] [--threads N]
```

Writes the walk of `<dir>` to `FILE` as a compressed manifest. The manifest
holds every part as its own zstd frame, then an index of the parts, then a
footer.

- `--level` is the zstd level (9).
- `--window-log` is the match window as a power of two (19). 0 lets zstd
  choose. The window sets how much memory each compression thread holds.
- `--threads` is the number of compression threads (2). The bytes written do
  not depend on it.

Measured 2026-09-19 on `/nix/store` (1.86 million entries, 102 parts) with the
defaults: 73.8 MB of JSON became an 11.7 MB manifest in 2.8 s. Peak memory was
120 MB with the allocator at its defaults, and 43 MB with
`MIMALLOC_PURGE_DELAY=0 MIMALLOC_ARENA_EAGER_COMMIT=0` in the environment.
`Cargo.toml` records what each setting does.

## As a library

```rust
use std::path::Path;
use tarseer::{WalkOptions, walk_parts};

let root = Path::new("/some/tree");
walk_parts(root, &WalkOptions::default(), &mut |part| {
    println!("{}", part.to_json()?);
    Ok(())
})?;
```

`walk_parts` hands each part to the closure as soon as it is complete and holds
nothing of it afterwards. `walk` collects the parts instead. `write_manifest`
walks and writes a manifest, and `Manifest::parse` reads one back. The crate
documentation has the full procedure and the layout.

## Parts

A walk is depth-first over name-sorted entries, and it is cut into **parts**:
contiguous runs of that order, each readable on its own. A part carries its
stem (the directories above its first row) and three tables of rows, one
column per attribute: directories, files and symlinks. A symlink row records
whether it is a Windows directory link, since extraction there must choose a
kind before the target exists. A file with more than one name is recorded at
each name as a plain file.

Parts are cut by the tree's structure against a budget of estimated JSON bytes:

- a directory whose whole subtree fits the budget is never split;
- a directory whose subtree does not fit groups its children in order, and each
  group takes as many children as fit;
- a child whose own subtree does not fit closes the group before it and is cut
  by the same rules;
- a directory's own row goes into the first part of its contents.

Where a cut falls depends only on the tree, the filter and the budget. The walk
holds only the rows not yet in a sealed part, so its memory follows the budget
and not the size of the tree.

The walk takes optional hooks: a filter consulted before an entry is read,
progress callbacks, and a cancel flag. A policy says what an entry the walk
cannot read costs, a directory it cannot open included: the walk fails, or
skips and counts it.

## The manifest

```
[part 0][part 1]…[part n-1][index][footer]
```

Every piece is a zstd skippable frame, so a plain zstd decoder skips the whole
manifest. A manifest appended to an ordinary zstd stream leaves that stream
decoding to the same bytes.

- **A part** is a tag and one ordinary zstd frame of the part's JSON. Parts
  decompress independently and in any order.
- **The index** is a tag, the format version and one zstd frame of columnar
  JSON. For each part it holds the offset, the frame length, the JSON length,
  the row counts and the path of the first row. A directory is one contiguous
  run of parts, so a search over the first paths finds the parts it spans.
- **The footer** is fixed-size and last. It holds the manifest's length, the
  index frame's length, the format version and `TARSEER\x1a`.

Offsets count from the manifest's first byte, so bytes placed before the
manifest do not change it. Every frame carries a checksum of its content, and a
damaged frame is refused.

## Errors

Fallible calls return an [`error-stack`](https://docs.rs/error-stack) `Report`
over `WalkError`, `JsonError`, `WriteError` or `ReadError`. Inside a walk
report, `Cancelled` and `PartFull` keep their own types:
`report.contains::<Cancelled>()`. A report names the entry the walk failed on.

On the command line a failure prints the whole report: every context it passed
through and what was attached at each. Set `RUST_BACKTRACE=1` to get a
backtrace with it.

## Attribution

- [stringtape](https://crates.io/crates/stringtape): the string-tape layout
  that `src/tape.rs` uses. The idea long predates it: interned string tables,
  rope-style text buffers, arenas addressed by offset.
