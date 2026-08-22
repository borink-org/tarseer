//! Walk a directory and print its index as indented JSON.
//!
//! ```text
//! cargo run --example dump_index -- some/dir
//! ```
//!
//! The same document `tarseer --json` prints, laid out to be read: one array
//! per column, all of the same length, plus the scalars that describe the
//! whole tree.

fn main() -> Result<(), tarseer::BoxError> {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: dump_index <dir>");
        return Ok(());
    };
    let dir = std::path::PathBuf::from(dir);

    let tree = tarseer::walk(&dir)?;
    let index = tarseer::index_tree(&tree)?;
    println!("{}", index.to_json_pretty()?);

    eprintln!("{}", index.summary());
    if tree.skips.any() {
        eprintln!("skipped {} entries", tree.skips.total());
    }
    Ok(())
}
