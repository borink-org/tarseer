//! The `tarseer` command.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Arg, Command};
use error_stack::Report;

use tarseer::{SourceTree, walk};

fn main() -> ExitCode {
    let matches = Command::new("tarseer")
        .about("Walk a directory tree and print what is there as JSON")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("dir")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Directory to walk"),
        )
        .get_matches();

    let dir: &PathBuf = matches.get_one("dir").expect("required");

    // Each step is matched where it happens rather than funnelled through one
    // `run() -> Result<()>`. Two fallible calls with two different contexts
    // would need a third, invented one to share a signature, and a top line
    // reading "the command failed" tells a reader less than the walk's own
    // context already does.
    let tree = match walk(dir) {
        Ok(tree) => tree,
        Err(report) => return fail(&report),
    };
    let json = match tree.to_json_pretty() {
        Ok(json) => json,
        Err(report) => return fail(&report),
    };
    println!("{json}");
    summarize(&tree);
    ExitCode::SUCCESS
}

/// Print a report and exit unsuccessfully.
///
/// `{:?}` rather than `{}` on purpose: a `Report`'s `Display` is its top
/// context alone, and its `Debug` is the whole thing — every context it passed
/// through, what was attached at each, and a backtrace when `RUST_BACKTRACE`
/// asks for one. On a command that has just failed, the whole thing is the
/// point.
fn fail<C: 'static>(report: &Report<C>) -> ExitCode {
    eprintln!("tarseer: {report:?}");
    ExitCode::FAILURE
}

/// The counts, on stderr — the document on stdout is nobody else's to share.
fn summarize(tree: &SourceTree) {
    eprintln!(
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
}
