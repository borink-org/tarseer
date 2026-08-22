# tarseer

Parallel `.tar.zst` archiver + custom index with optimized file walking,
parallelized tar creation and compression and random-access support.

## Status

Early. What exists today is the first vertical slice: walk a tree, index what
is there, print it.

```
tarseer [--threads N] <dir>
```

* **`walk`** — a parallel breadth-first source walk, one task per directory,
  over arenas that never move what they have already written.
* **`tree`** — three columns of fixed-size rows over one string tape.
* **`index`** — every entry in path order, stored as columns, with ancestry
  interned once in a directory table.

Nothing here opens a file or reads its contents yet. Compression, the container
format and extraction come next; the index is the thing they all plan from.
