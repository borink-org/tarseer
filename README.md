# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk,
the index over it, and its JSON form.

```
tarseer [--threads N] [--json] <dir>
cargo run --example dump_index -- <dir>     # the same document, indented
```

Nothing here opens a file or reads its contents yet.
