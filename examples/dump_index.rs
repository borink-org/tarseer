//! Walk a directory and print its index as indented JSON.
//!
//! ```text
//! cargo run --example dump_index -- some/dir
//! cargo run --example dump_index -- some/dir --hash
//! ```
//!
//! The same document `tarseer --json` prints, laid out to be read: one array
//! per column, all of the same length, plus the scalars that describe the
//! whole tree.

fn main() -> Result<(), tarseer::BoxError> {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: dump_index <dir> [--hash]");
        return Ok(());
    };
    let dir = std::path::PathBuf::from(dir);
    let hash = std::env::args().any(|a| a == "--hash");

    let tree = tarseer::walk(&dir)?;
    let sums = if hash {
        tarseer::hash_tree(&dir, &tree)?
    } else {
        Vec::new()
    };
    let index = tarseer::index_tree_with(&tree, &sums)?;
    println!("{}", index.to_json_pretty()?);

    eprintln!("{}", index.summary());
    if tree.skips.any() {
        eprintln!("skipped {} entries", tree.skips.total());
    }
    Ok(())
}
