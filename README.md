# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk,
cut into parts, each part's JSON form, and the compressed manifest that holds
them.

```
tarseer <dir> [--budget BYTES]
tarseer <dir> --out FILE [--budget BYTES] [--level N] [--threads N]
```

Without `--out`, parts go to stdout as JSON, one per line. With it, they are
written as a compressed manifest. Either way the counts go to stderr, so piping
the one does not swallow the other.

Nothing here opens a file or reads its contents yet.

## Parts

A walk is depth-first over name-sorted entries, and it is handed out as
**parts**: contiguous runs of that order, each readable on its own. A part
carries its stem (the directories above its first row), a directory table local
to it, and one column per attribute.

Parts are cut by the tree's structure against a budget of estimated JSON bytes
(4 MiB by default):

- a subtree within the budget is never split;
- an over-budget directory groups its children in order, each group as large as
  fits;
- an over-budget child closes the group before it and is split the same way;
- a directory's own row goes with the first part of its contents.

Where a cut falls depends only on the tree, never on what came before it or on
how the walk was scheduled, and only the rows not yet sealed into a part are
held — so memory follows the budget, not the size of the tree.

The walk takes optional hooks, all dynamically dispatched: a filter consulted
before anything is stated, progress callbacks, a cancel flag, and a policy for
entries it fails to read (fail the walk, or skip and count them).

## The manifest

```
[part 0][part 1]…[part n-1][index][footer]
```

Every piece is a zstd skippable frame, so a plain zstd decoder skips the whole
manifest. Once a payload sits in front of it, what that decoder yields is
exactly the payload.

- **A part** is a tag and one ordinary zstd frame of the part's JSON. Parts
  decompress independently and in any order.
- **The index** is a tag, the format version and one zstd frame of columnar
  JSON: for each part its offset, frame length, JSON length, row counts and the
  path of its first row, so the parts a folder spans can be found by search.
- **The footer** is fixed-size and last. It holds the manifest's length, the
  index frame's length, the format version and `TARSEER\x1a`.

Offsets count from the manifest's first byte, so the same bytes can follow a
payload unchanged. Parts are compressed on worker threads while the walk goes
on, and the output does not depend on how many.

On `/nix/store` (1.7 million entries, 91 parts) the default level 9 turns 67 MB
of JSON into 10.7 MB, in 3.0 s with a 59 MB peak including the walk.

## Errors

Fallible calls return an [`error-stack`](https://docs.rs/error-stack) `Report`
over `WalkError`, `JsonError`, `WriteError` or `ReadError`. Inside a walk report, `Cancelled` and
`PartFull` stay distinguishable with `report.contains::<_>()`. A report names
the entry the walk tripped over, since that is the part a caller cannot work out
for itself.

On the command line a failure prints the whole report: every context it passed
through and what was attached at each. Set `RUST_BACKTRACE=1` to get a
backtrace with it.

## Attribution

None of the ideas here are original.

- [stringtape](https://crates.io/crates/stringtape) — the string-tape layout
  `src/tape.rs` uses. The idea long predates it: interned string tables,
  rope-style text buffers, arenas addressed by offset.

More will land here as the walk, index and container do.
