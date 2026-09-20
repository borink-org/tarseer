# tarseer

Walks a directory tree into memory-bounded parts and writes them, with an
index, as JSON Lines in a zstd file. It opens no file and reads no contents.

Early: the format and the API change without notice.

```
tarseer <dir>              the walk as JSON, one part per line
tarseer <dir> --out FILE   the same lines and an index, as one zstd file
```

`tarseer --help` lists the options. The crate documentation describes the
walk, where parts are cut, and the file. `zstd -dc FILE` prints it.

## Attribution

- [stringtape](https://crates.io/crates/stringtape): the string-tape layout
  that `src/tape.rs` uses. The idea long predates it: interned string tables,
  rope-style text buffers, arenas addressed by offset.
