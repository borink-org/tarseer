//! Threads that read directories ahead of the walk. They decide how long a
//! walk takes, and nothing about what it returns.

mod common;

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use common::{TempDir, wide_fixture};
use error_stack::Report;
use tarseer::{Cancelled, Candidate, EntryKind, Filter, WalkError, WalkOptions, walk, walk_parts};

const THREADS: [usize; 4] = [1, 2, 3, 8];

fn options(budget: u64, threads: usize) -> WalkOptions<'static> {
    WalkOptions {
        budget,
        threads,
        ..WalkOptions::default()
    }
}

/// The parts of a walk, as the JSON a manifest holds.
fn parts(root: &Path, options: &WalkOptions<'_>) -> Vec<String> {
    walk(root, options)
        .unwrap()
        .parts
        .iter()
        .map(|part| part.to_json().unwrap())
        .collect()
}

/// A directory too large for one scan, with subdirectories among its files, so
/// that a later scan of it finds directories as well. Some of them are empty.
fn large_fixture(tag: &str) -> TempDir {
    let temp_dir = wide_fixture(tag);
    let large = temp_dir.path().join("large");
    fs::create_dir(&large).unwrap();
    for index in 0..2600 {
        if index % 400 == 0 {
            let directory = large.join(format!("e{index:04}.d"));
            fs::create_dir(&directory).unwrap();
            // Every other one stays empty: a subtree with no rows.
            if index % 800 != 0 {
                fs::write(directory.join("inner"), b"i").unwrap();
            }
        } else {
            fs::write(large.join(format!("e{index:04}")), b"").unwrap();
        }
    }
    temp_dir
}

#[test]
fn the_parts_are_the_same_with_any_number_of_threads() {
    let temp_dir = large_fixture("threads-same");
    for budget in [300, 20_000, tarseer::DEFAULT_BUDGET] {
        let want = parts(temp_dir.path(), &options(budget, 0));
        assert!(want.iter().map(String::len).sum::<usize>() > 100_000);
        for threads in THREADS {
            let got = parts(temp_dir.path(), &options(budget, threads));
            assert_eq!(got, want, "budget {budget}, {threads} threads");
        }
    }
}

#[test]
fn continuations_across_small_subtrees_preserve_the_parts() {
    let fixture = TempDir::new("threads-continuations");
    for index in (0..160).rev() {
        let directory = fixture.path().join(format!("d{index:03}"));
        fs::create_dir_all(directory.join("empty")).unwrap();
        for file in 0..index % 7 {
            fs::write(directory.join(format!("f{file}")), b"row").unwrap();
        }
        if index % 11 == 0 {
            fs::create_dir_all(directory.join("nested/child")).unwrap();
            fs::write(directory.join("nested/child/file"), b"deep").unwrap();
        }
    }
    for budget in [1, 83, 300, 64 << 10, tarseer::DEFAULT_BUDGET] {
        let want = parts(fixture.path(), &options(budget, 0));
        for threads in [1, 2, 8] {
            assert_eq!(parts(fixture.path(), &options(budget, threads)), want);
        }
    }
}

// Threads take a subtree that fits a part as one piece, where a walk without
// them visits every row. Many budgets put the cuts in many places, among them
// right after an empty directory, which is a subtree with no rows.
#[test]
fn the_parts_are_the_same_wherever_the_budget_puts_the_cuts() {
    let temp_dir = large_fixture("threads-budgets");
    for budget in (150..6_000).step_by(83) {
        let want = parts(temp_dir.path(), &options(budget, 0));
        let got = parts(temp_dir.path(), &options(budget, 3));
        assert_eq!(got, want, "budget {budget}");
    }
}

#[test]
fn a_filter_refuses_the_same_entries_from_any_thread() {
    struct NoInner;
    impl Filter for NoInner {
        fn keep(&self, candidate: &Candidate<'_>) -> bool {
            // A sibling decides, so the listing has to be the right one.
            !(candidate.kind == EntryKind::Directory
                && candidate.name == "inner"
                && candidate.listing.contains("f3"))
        }
    }

    let temp_dir = large_fixture("threads-filter");
    let filtered = |threads| WalkOptions {
        filter: Some(&NoInner),
        ..options(2_000, threads)
    };
    let want = parts(temp_dir.path(), &filtered(0));
    assert_ne!(want, parts(temp_dir.path(), &options(2_000, 0)));
    for threads in THREADS {
        assert_eq!(parts(temp_dir.path(), &filtered(threads)), want);
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_the_same_with_threads() {
    use std::os::unix::fs::PermissionsExt;

    use common::fixture;
    use tarseer::OnError;

    let temp_dir = fixture("threads-locked");
    let locked = temp_dir.path().join("zed");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let failed = walk(temp_dir.path(), &options(300, 4));
    let skipped = walk(
        temp_dir.path(),
        &WalkOptions {
            on_error: OnError::Skip,
            ..options(300, 4)
        },
    );
    let inline = walk(
        temp_dir.path(),
        &WalkOptions {
            on_error: OnError::Skip,
            ..options(300, 0)
        },
    );
    // Restore before asserting, so a failure still lets the fixture clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let report = failed.expect_err("an unopenable directory fails the walk");
    assert!(format!("{report:?}").contains("listing zed"), "{report:?}");
    let (skipped, inline) = (skipped.unwrap(), inline.unwrap());
    assert_eq!(skipped.skips.unreadable, 1);
    assert_eq!(skipped.paths(), inline.paths());
}

#[test]
fn a_raised_cancel_flag_stops_a_walk_with_threads() {
    let temp_dir = large_fixture("threads-cancel");
    let cancel = AtomicBool::new(true);
    let cancelled = WalkOptions {
        cancel: Some(&cancel),
        ..options(300, 4)
    };
    let report = walk(temp_dir.path(), &cancelled).expect_err("cancelled");
    assert!(report.contains::<Cancelled>());
}

#[test]
fn an_error_from_the_sink_ends_a_walk_with_threads() {
    let temp_dir = large_fixture("threads-sink");
    let mut seen = 0;
    let result = walk_parts(temp_dir.path(), &options(300, 4), &mut |_| {
        seen += 1;
        if seen == 3 {
            return Err(Report::new(WalkError).attach("the sink is full"));
        }
        Ok(())
    });
    let report = result.expect_err("the sink's error");
    assert!(format!("{report:?}").contains("the sink is full"));
    assert_eq!(seen, 3);
}

#[test]
#[should_panic(expected = "a thread of the walk panicked")]
fn a_panic_on_a_thread_ends_the_walk_and_does_not_hang_it() {
    struct Panics;
    impl Filter for Panics {
        fn keep(&self, candidate: &Candidate<'_>) -> bool {
            assert_ne!(candidate.name, "e2001", "the filter gives up");
            true
        }
    }

    let temp_dir = large_fixture("threads-panic");
    let panicking = WalkOptions {
        filter: Some(&Panics),
        ..options(300, 4)
    };
    let _ = walk(temp_dir.path(), &panicking);
}
