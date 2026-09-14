# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk
and its JSON form.

```
tarseer <dir>
```

The tree goes to stdout as JSON; the counts go to stderr, so piping the one
does not swallow the other.

Nothing here opens a file or reads its contents yet.

## Errors

Fallible calls return an [`error-stack`](https://docs.rs/error-stack) `Report`
over one of three contexts — `WalkError`, `TreeFull`, `JsonError` — one per
unit of fallibility rather than one per call site. A report names the entry the
walk tripped over, since that is the part a caller cannot work out for itself.

On the command line a failure prints the whole report: every context it passed
through and what was attached at each. Set `RUST_BACKTRACE=1` to get a
backtrace with it.

## Attribution

None of the ideas here are original.

- [stringtape](https://crates.io/crates/stringtape) — the string-tape layout
  `src/tape.rs` uses. The idea long predates it: interned string tables,
  rope-style text buffers, arenas addressed by offset.

More will land here as the walk, index and container do.
