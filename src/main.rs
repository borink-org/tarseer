//! The `tarseer` command.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, Command};

use tarseer::{Result, walk};

fn main() -> ExitCode {
    let m = Command::new("tarseer")
        .about("Walk a directory tree and list what is there")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("dir")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Directory to walk"),
        )
        .get_matches();

    let dir: &PathBuf = m.get_one("dir").expect("required");
    match run(dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tarseer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(dir: &Path) -> Result<()> {
    let tree = walk(dir)?;
    for p in tree.paths() {
        println!("{p}");
    }
    println!(
        "{} entries: {} files, {} dirs, {} symlinks, {} bytes",
        tree.len(),
        tree.files.len(),
        tree.dirs.len(),
        tree.links.len(),
        tree.total_bytes(),
    );
    if tree.skips.any() {
        eprintln!(
            "skipped {}: {} special, {} non-UTF-8, {} unreadable",
            tree.skips.total(),
            tree.skips.special,
            tree.skips.non_utf8,
            tree.skips.unreadable
        );
    }
    Ok(())
}
