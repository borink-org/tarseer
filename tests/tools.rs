//! The stock `zstd` and `jq` commands, run on a manifest file. These are the
//! commands the [`tarseer::zstd`] module shows, and one test holds the module
//! to them.
//!
//! A machine without the tools skips these tests, and says so. Set
//! `TARSEER_REQUIRE_TOOLS` to fail them there, as CI does.

#![cfg(feature = "zstd")]

mod common;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{TempDir, wide_fixture};
use tarseer::file::{ManifestFile, write_file};
use tarseer::zstd::Zstd;
use tarseer::{WalkOptions, walk};

/// Prints the parts and then the index, one JSON line each.
const DECOMPRESS: &str = "zstd -dc tree.zst";
/// Checks every frame's checksum.
const TEST: &str = "zstd -t tree.zst";
/// Counts the frames.
const LIST: &str = "zstd -l tree.zst";
/// Prints each file row of the first part as its name and its size.
const ROWS: &str = "jq -r '.files | [.name, .size] | transpose | .[] | @tsv'";

const BUDGET: u64 = 300;

const ZSTD: Zstd = Zstd {
    level: tarseer::zstd::DEFAULT_LEVEL,
    window_log: tarseer::zstd::DEFAULT_WINDOW_LOG,
};

fn options() -> WalkOptions<'static> {
    WalkOptions {
        budget: BUDGET,
        ..WalkOptions::default()
    }
}

/// Returns `false`, after saying why, if `tool` cannot be run here.
fn have(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        assert!(
            std::env::var_os("TARSEER_REQUIRE_TOOLS").is_none(),
            "`{tool}` is required and is not installed"
        );
        eprintln!("skipped: `{tool}` is not installed");
    }
    found
}

/// Writes a manifest of the wide fixture as `tree.zst`, and returns the
/// directory that holds it with the file's bytes.
fn manifest(tag: &str) -> (TempDir, Vec<u8>) {
    let tree = wide_fixture(tag);
    let mut bytes = Vec::new();
    write_file(tree.path(), &options(), &ZSTD, 2, &mut bytes).unwrap();
    let directory = TempDir::new(&format!("{tag}-out"));
    std::fs::write(directory.path().join("tree.zst"), &bytes).unwrap();
    (directory, bytes)
}

/// Runs `command` through `sh` in `directory`, feeds it `input`, and returns
/// what it printed.
fn run(directory: &Path, command: &str, input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "`{command}` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn the_module_shows_the_commands_these_tests_run() {
    let module = include_str!("../src/zstd.rs");
    for command in [DECOMPRESS, TEST, LIST, ROWS] {
        assert!(module.contains(command), "src/zstd.rs lacks `{command}`");
    }
}

#[test]
fn zstd_prints_the_parts_and_the_index_as_json_lines() {
    if !have("zstd") {
        return;
    }
    let (directory, bytes) = manifest("tools-lines");
    let file = ManifestFile::parse(&ZSTD, &bytes).unwrap();

    let printed = run(directory.path(), DECOMPRESS, b"");
    assert_eq!(printed.last(), Some(&b'\n'), "every line ends in a newline");
    let lines: Vec<&[u8]> = printed[..printed.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect();
    assert!(
        file.index.parts.len() > 1,
        "the fixture makes several parts"
    );
    assert_eq!(lines.len(), file.index.parts.len() + 1);
    for (number, line) in lines[..lines.len() - 1].iter().enumerate() {
        assert_eq!(*line, file.part_json(number).unwrap());
    }
    let index = std::str::from_utf8(lines[lines.len() - 1]).unwrap();
    assert_eq!(index, file.index.to_json().unwrap());
}

#[test]
fn zstd_finds_the_file_sound_and_counts_its_frames() {
    if !have("zstd") {
        return;
    }
    let (directory, bytes) = manifest("tools-list");
    let file = ManifestFile::parse(&ZSTD, &bytes).unwrap();

    run(directory.path(), TEST, b"");

    // The row under the header starts with the number of frames and the
    // number of those that are skippable: here only the seek table.
    let listed = String::from_utf8(run(directory.path(), LIST, b"")).unwrap();
    let row = listed
        .lines()
        .find(|line| {
            line.trim_start()
                .starts_with(|first: char| first.is_ascii_digit())
        })
        .unwrap_or_else(|| panic!("no row of numbers in: {listed}"));
    let mut numbers = row.split_whitespace();
    let frames: usize = numbers.next().unwrap().parse().unwrap();
    let skippable: usize = numbers.next().unwrap().parse().unwrap();
    assert_eq!(frames, file.index.parts.len() + 2);
    assert_eq!(skippable, 1);
}

#[test]
fn zstd_decompresses_each_frame_cut_out_of_the_file() {
    if !have("zstd") {
        return;
    }
    let (directory, bytes) = manifest("tools-cut");
    let file = ManifestFile::parse(&ZSTD, &bytes).unwrap();

    for number in 0..file.index.parts.len() {
        let frame = file.frame(number).unwrap();
        let mut want = file.part_json(number).unwrap();
        want.push(b'\n');
        assert_eq!(run(directory.path(), "zstd -dc", frame), want);
    }
}

#[test]
fn jq_turns_the_columns_of_a_part_into_rows() {
    if !have("zstd") || !have("jq") {
        return;
    }
    let (directory, bytes) = manifest("tools-rows");
    let file = ManifestFile::parse(&ZSTD, &bytes).unwrap();
    let tree = wide_fixture("tools-rows-tree");
    let walked = walk(tree.path(), &options()).unwrap();

    // The first part that holds files, since the first part of all may not.
    let (number, part) = walked
        .parts
        .iter()
        .enumerate()
        .find(|(_, part)| !part.files.is_empty())
        .unwrap();
    let printed = run(directory.path(), ROWS, &file.part_json(number).unwrap());
    let mut want = String::new();
    for row in &part.files {
        writeln!(want, "{}\t{}", part.text(row.name), row.size).unwrap();
    }
    // `jq` ends its lines as the platform does.
    let printed = String::from_utf8(printed).unwrap().replace("\r\n", "\n");
    assert_eq!(printed, want);
}
