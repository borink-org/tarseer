//! The `tarseer` command.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command, value_parser};
use error_stack::{Report, ResultExt as _};

use tarseer::{
    DEFAULT_BUDGET, Skips, WalkError, WalkOptions, WriteError, WriteOptions, Written, walk_parts,
    write_manifest,
};

// mimalloc for the binary. Its memory-return settings are worth knowing: see the
// note on the dependency in Cargo.toml.
#[global_allocator]
static GLOBAL: cyo_mimalloc::MiMalloc = cyo_mimalloc::MiMalloc;

fn main() -> ExitCode {
    let matches = Command::new("tarseer")
        .about("Walk a directory tree into parts: JSON lines on stdout, or a compressed manifest")
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
        .arg(
            Arg::new("out")
                .long("out")
                .short('o')
                .value_parser(value_parser!(PathBuf))
                .help("Write the compressed manifest here instead of JSON lines to stdout"),
        )
        .arg(
            Arg::new("level")
                .long("level")
                .value_parser(value_parser!(i32))
                .help("zstd level for every part [default: 9]"),
        )
        .arg(
            Arg::new("window-log")
                .long("window-log")
                .value_parser(value_parser!(u32))
                .help("Match window per part, as a power of two; 0 for zstd's own [default: 19]"),
        )
        .arg(
            Arg::new("threads")
                .long("threads")
                .value_parser(value_parser!(usize))
                .help("Compression threads [default: 2]"),
        )
        .get_matches();

    let dir: &PathBuf = matches.get_one("dir").expect("required");
    let options = WalkOptions {
        budget: matches
            .get_one::<u64>("budget")
            .copied()
            .unwrap_or(DEFAULT_BUDGET),
        ..WalkOptions::default()
    };
    match matches.get_one::<PathBuf>("out") {
        Some(out) => write(dir, &options, out, &matches),
        None => print_parts(dir, &options),
    }
}

fn write(dir: &Path, options: &WalkOptions<'_>, out: &Path, matches: &ArgMatches) -> ExitCode {
    let defaults = WriteOptions::default();
    let write_options = WriteOptions {
        level: matches
            .get_one::<i32>("level")
            .copied()
            .unwrap_or(defaults.level),
        window_log: matches
            .get_one::<u32>("window-log")
            .copied()
            .unwrap_or(defaults.window_log),
        threads: matches
            .get_one::<usize>("threads")
            .copied()
            .unwrap_or(defaults.threads),
    };
    let file = match File::create(out)
        .attach_with(|| format!("creating {}", out.display()))
        .change_context(WriteError)
    {
        Ok(file) => file,
        Err(report) => return fail(&report),
    };
    match write_manifest(dir, options, &write_options, &mut BufWriter::new(file)) {
        Ok(written) => {
            summarize_manifest(&written);
            ExitCode::SUCCESS
        }
        Err(report) => fail(&report),
    }
}

fn print_parts(dir: &Path, options: &WalkOptions<'_>) -> ExitCode {
    let mut totals = Totals::default();
    let walked = walk_parts(dir, options, &mut |part| {
        let json = part
            .to_json()
            .attach_with(|| format!("part {}", totals.parts))
            .change_context(WalkError)?;
        println!("{json}");
        totals.parts += 1;
        totals.files += part.files.len() as u64;
        totals.dirs += part.dirs.len() as u64;
        totals.links += part.links.len() as u64;
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
    files: u64,
    dirs: u64,
    links: u64,
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
        "{} parts, {} entries: {} files, {} dirs, {} symlinks",
        totals.parts,
        totals.files + totals.dirs + totals.links,
        totals.files,
        totals.dirs,
        totals.links,
    );
    summarize_skips(skips);
}

fn summarize_manifest(written: &Written) {
    let index = &written.index;
    let sum = |field: fn(&tarseer::PartEntry) -> u64| index.parts.iter().map(field).sum::<u64>();
    #[allow(clippy::cast_precision_loss)]
    let ratio = written.raw_len as f64 / written.len.max(1) as f64;
    eprintln!(
        "{} parts, {} entries: {} files, {} dirs, {} symlinks; {} bytes of JSON in {} bytes ({ratio:.1}x)",
        index.parts.len(),
        index.entries(),
        sum(|part| part.files),
        sum(|part| part.dirs),
        sum(|part| part.links),
        written.raw_len,
        written.len,
    );
    summarize_skips(index.skips);
}

fn summarize_skips(skips: Skips) {
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
