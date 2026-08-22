# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk,
the index over it, its JSON form, and blake3 digests of the file contents.

```
tarseer [--threads N] [--hash] [--json] <dir>
cargo run --example dump_index -- <dir> [--hash]   # the same, indented
```

Compression, the container format and extraction come next; the index is what
they all plan from.
