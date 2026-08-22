# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. Being built up a slice at a time; what exists today is the source walk
and the index over it.

```
tarseer [--threads N] <dir>
```

Nothing here opens a file or reads its contents yet.
