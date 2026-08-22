//! The `tarseer` command.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, Command};

use tarseer::{Kind, Result, index_tree, walk};

fn main() -> ExitCode {
    let m = Command::new("tarseer")
        .about("Walk a directory tree and index what is there")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("dir")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Directory to walk"),
        )
        .arg(
            Arg::new("threads")
                .long("threads")
                .value_parser(clap::value_parser!(usize))
                .help("Scan threads; defaults to the number of cores"),
        )
        .get_matches();

    let dir: &PathBuf = m.get_one("dir").expect("required");
    let threads = m.get_one::<usize>("threads").copied();

    match run(dir, threads) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tarseer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(dir: &Path, threads: Option<usize>) -> Result<()> {
    if let Some(n) = threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .map_err(|e| -> tarseer::BoxError { format!("thread pool: {e}").into() })?;
    }

    let tree = walk(dir)?;
    let skips = tree.skips;
    let index = index_tree(&tree)?;

    index.for_each_path(|i, p| match index.kind(i) {
        Some(Kind::Symlink) => println!("l {:>12} {p} -> {}", "", index.link(i)),
        Some(Kind::Dir) => println!("d {:>12} {p}", ""),
        _ => println!("f {:>12} {p}", index.size[i]),
    });
    println!("{}", index.summary());

    if skips.any() {
        eprintln!(
            "skipped {}: {} special, {} non-UTF-8, {} unreadable",
            skips.total(),
            skips.special,
            skips.non_utf8,
            skips.unreadable
        );
    }
    Ok(())
}
