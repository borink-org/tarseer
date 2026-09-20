# tarseer

Walks a directory tree into memory-bounded parts and writes them as a
compressed, indexed manifest. It opens no file and reads no contents.

Early: the format and the API change without notice.

```
tarseer <dir>              the walk as JSON, one part per line
tarseer <dir> --out FILE   the walk as a compressed manifest
```

`tarseer --help` lists the options. The crate documentation describes the
walk, where parts are cut, and the manifest.

## Attribution

- [stringtape](https://crates.io/crates/stringtape): the string-tape layout
  that `src/tape.rs` uses. The idea long predates it: interned string tables,
  rope-style text buffers, arenas addressed by offset.
