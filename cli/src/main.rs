//! The `tarseer` command.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command, value_parser};
use error_stack::{Report, ResultExt as _};

use tarseer::file::{Written, write_file};
use tarseer::frames::DEFAULT_THREADS;
use tarseer::zstd::Zstd;
use tarseer::{DEFAULT_BUDGET, Metadata, Skips, WalkError, WalkOptions, WriteError, walk_parts};

// mimalloc for the binary only; the library sets no allocator.
#[global_allocator]
static GLOBAL: cyo_mimalloc::MiMalloc = cyo_mimalloc::MiMalloc;

fn main() -> ExitCode {
    // Where memory is not overcommitted (Windows), mimalloc commits each page
    // whole, 4 MiB for blocks over 84 KiB, and growing buffers take one per
    // size class per thread; committing on demand charges what is touched.
    // Before the first walk thread, and only if the environment does not say.
    if std::env::var_os("MIMALLOC_PAGE_COMMIT_ON_DEMAND").is_none() {
        cyo_mimalloc::MiMalloc::option_set(
            cyo_mimalloc::mi_option_t::mi_option_page_commit_on_demand,
            2,
        );
    }
    let matches = Command::new("tarseer")
        .about("Walk a directory tree into parts: JSON lines on stdout, or the same lines as a zstd file")
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
                .help("Write the lines, and an index as the last one, to this zstd file"),
        )
        .arg(
            Arg::new("level")
                .long("level")
                .value_parser(value_parser!(i32))
                .help("zstd level for every part and the index [default: 3]"),
        )
        .arg(
            Arg::new("window-log")
                .long("window-log")
                .value_parser(value_parser!(u32))
                .help("Match window for compression, as a power of two; 0 lets zstd choose [default: 19]"),
        )
        .arg(
            Arg::new("threads")
                .long("threads")
                .value_parser(value_parser!(usize))
                .help("Compression threads [default: 2]"),
        )
        .arg(
            Arg::new("walk-threads")
                .long("walk-threads")
                .value_parser(value_parser!(usize))
                .help("Threads that read directories; 0 walks on the calling thread alone [default: one per processor]"),
        )
        .arg(
            Arg::new("max-walk-threads")
                .long("max-walk-threads")
                .value_parser(value_parser!(usize))
                .help("Most walk threads while the walk waits on storage [default: 4 × --walk-threads]"),
        )
        .arg(
            Arg::new("metadata")
                .long("metadata")
                .value_parser(["full", "kinds"])
                .help("What the walk reads of each entry: size, mtime and mode, or only its name and kind [default: full]"),
        )
        .get_matches();

    let directory: &PathBuf = matches
        .get_one("dir")
        .expect("clap requires the directory argument");
    let options = WalkOptions {
        budget: matches
            .get_one::<u64>("budget")
            .copied()
            .unwrap_or(DEFAULT_BUDGET),
        threads: matches
            .get_one::<usize>("walk-threads")
            .copied()
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
            }),
        max_threads: matches
            .get_one::<usize>("max-walk-threads")
            .copied()
            .unwrap_or(0),
        metadata: match matches.get_one::<String>("metadata").map(String::as_str) {
            Some("kinds") => Metadata::Kinds,
            _ => Metadata::Full,
        },
        ..WalkOptions::default()
    };
    match matches.get_one::<PathBuf>("out") {
        Some(out) => write(directory, &options, out, &matches),
        None => print_parts(directory, &options),
    }
}

fn write(
    directory: &Path,
    options: &WalkOptions<'_>,
    out: &Path,
    matches: &ArgMatches,
) -> ExitCode {
    let defaults = Zstd::default();
    let codec = Zstd {
        level: matches
            .get_one::<i32>("level")
            .copied()
            .unwrap_or(defaults.level),
        window_log: matches
            .get_one::<u32>("window-log")
            .copied()
            .unwrap_or(defaults.window_log),
    };
    let threads = matches
        .get_one::<usize>("threads")
        .copied()
        .unwrap_or(DEFAULT_THREADS);
    let file = match File::create(out)
        .attach_with(|| format!("creating {}", out.display()))
        .change_context(WriteError)
    {
        Ok(file) => file,
        Err(report) => return fail(&report),
    };
    match write_file(
        directory,
        options,
        &codec,
        threads,
        &mut BufWriter::new(file),
    ) {
        Ok(written) => {
            summarize_manifest(&written);
            ExitCode::SUCCESS
        }
        Err(report) => fail(&report),
    }
}

fn print_parts(directory: &Path, options: &WalkOptions<'_>) -> ExitCode {
    let mut totals = Totals::default();
    let walked = walk_parts(directory, options, &mut |part| {
        let json = part
            .to_json()
            .attach_with(|| format!("part {}", totals.parts))
            .change_context(WalkError)?;
        println!("{json}");
        totals.parts += 1;
        totals.files += part.files.len() as u64;
        totals.directories += part.directories.len() as u64;
        totals.symlinks += part.symlinks.len() as u64;
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
    directories: u64,
    symlinks: u64,
}

// Prints the report and returns the failure exit code. `{:?}`, because a
// `Report`'s `Display` is its top context alone and its `Debug` is every
// context, every attachment, and a backtrace when `RUST_BACKTRACE` is set.
fn fail<C: 'static>(report: &Report<C>) -> ExitCode {
    eprintln!("tarseer: {report:?}");
    ExitCode::FAILURE
}

// The counts go to stderr so that stdout holds only the parts.
fn summarize(totals: &Totals, skips: Skips) {
    eprintln!(
        "{} parts, {} entries: {} files, {} directories, {} symlinks",
        totals.parts,
        totals.files + totals.directories + totals.symlinks,
        totals.files,
        totals.directories,
        totals.symlinks,
    );
    summarize_skips(skips);
}

fn summarize_manifest(written: &Written) {
    let index = &written.index;
    let sum = |field: fn(&tarseer::PartEntry) -> u64| index.parts.iter().map(field).sum::<u64>();
    #[allow(clippy::cast_precision_loss)]
    let ratio = written.raw_len as f64 / written.len.max(1) as f64;
    eprintln!(
        "{} parts, {} entries: {} files, {} directories, {} symlinks; {} bytes of JSON in {} bytes ({ratio:.1}x)",
        index.parts.len(),
        index.entries(),
        sum(|part| part.files),
        sum(|part| part.directories),
        sum(|part| part.symlinks),
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
