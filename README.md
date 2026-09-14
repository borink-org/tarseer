# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk,
cut into parts, and each part's JSON form.

```
tarseer <dir> [--budget BYTES]
```

Parts go to stdout as JSON, one per line; the counts go to stderr, so piping the
one does not swallow the other.

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

## Errors

Fallible calls return an [`error-stack`](https://docs.rs/error-stack) `Report`
over `WalkError` or `JsonError`. Inside a walk report, `Cancelled` and
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
