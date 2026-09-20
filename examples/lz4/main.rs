//! Walks a directory and writes its manifest as an LZ4 file, then reads it
//! back. `format.rs` beside this file is the whole format.
//!
//! ```text
//! cargo run --example lz4 -- <dir> <out>
//! ```

mod format;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use format::Lz4;
use tarseer::WalkOptions;
use tarseer::file::{ManifestFile, write_file};

fn main() -> ExitCode {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(directory), Some(out)) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: lz4 <dir> <out>");
        return ExitCode::FAILURE;
    };

    let mut bytes = Vec::new();
    let written = match write_file(&directory, &WalkOptions::default(), &Lz4, 2, &mut bytes) {
        Ok(written) => written,
        Err(report) => {
            eprintln!("lz4: {report:?}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = fs::write(&out, &bytes) {
        eprintln!("lz4: writing {}: {error}", out.display());
        return ExitCode::FAILURE;
    }

    let file = match ManifestFile::parse(&Lz4, &bytes) {
        Ok(file) => file,
        Err(report) => {
            eprintln!("lz4: {report:?}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "{} parts, {} entries; {} bytes of JSON in {} bytes",
        file.index.parts.len(),
        file.index.entries(),
        written.raw_len,
        written.len,
    );
    ExitCode::SUCCESS
}
