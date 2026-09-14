//! The `tarseer` command.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Arg, Command, value_parser};
use error_stack::{Report, ResultExt as _};

use tarseer::{DEFAULT_BUDGET, Skips, WalkError, WalkOptions, walk_parts};

fn main() -> ExitCode {
    let matches = Command::new("tarseer")
        .about("Walk a directory tree and print its parts as JSON, one per line")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("dir")
                .required(true)
                .value_parser(value_parser!(PathBuf))
                .help("Directory to walk"),
        )
        .arg(
            Arg::new("budget")
                .long("budget")
                .value_parser(value_parser!(u64))
                .help("Estimated JSON bytes per part [default: 4 MiB]"),
        )
        .get_matches();

    let dir: &PathBuf = matches.get_one("dir").expect("required");
    let budget = matches
        .get_one::<u64>("budget")
        .copied()
        .unwrap_or(DEFAULT_BUDGET);
    let options = WalkOptions {
        budget,
        ..WalkOptions::default()
    };

    let mut totals = Totals::default();
    let walked = walk_parts(dir, &options, &mut |part| {
        let json = part
            .to_json()
            .attach_with(|| format!("part {}", totals.parts))
            .change_context(WalkError)?;
        println!("{json}");
        totals.parts += 1;
        totals.files += part.files.len();
        totals.dirs += part.dirs.len();
        totals.links += part.links.len();
        totals.bytes += part.total_bytes();
        Ok(())
    });
    match walked {
        Ok(skips) => {
            summarize(&totals, skips);
            ExitCode::SUCCESS
        }
        Err(report) => fail(&report),
    }
}

#[derive(Default)]
struct Totals {
    parts: usize,
    files: usize,
    dirs: usize,
    links: usize,
    bytes: u64,
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

/// The counts, on stderr — the parts on stdout are nobody else's to share.
fn summarize(totals: &Totals, skips: Skips) {
    eprintln!(
        "{} parts, {} entries: {} files, {} dirs, {} symlinks, {} bytes",
        totals.parts,
        totals.files + totals.dirs + totals.links,
        totals.files,
        totals.dirs,
        totals.links,
        totals.bytes
    );
    if skips.any() {
        eprintln!(
            "skipped {}: {} special, {} non-UTF-8, {} unreadable, {} failed",
            skips.total(),
            skips.special,
            skips.non_utf8,
            skips.unreadable,
            skips.failed
        );
    }
}
