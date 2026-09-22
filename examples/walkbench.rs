//! Measures the public walk, optionally including JSON serialization.
//!
//! Build with `cargo build --release --example walkbench --no-default-features`.
//! Run `walkbench <walk|json> <tree> <budget-bytes> <threads> [walk|completion]`.

use std::error::Error;
use std::hint::black_box;
use std::path::Path;

use tarseer::{PartOrder, WalkOptions, walk_parts};

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let [mode, tree, budget, threads, rest @ ..] = arguments.as_slice() else {
        return Err(
            "usage: walkbench <walk|json> <tree> <budget-bytes> <threads> [walk|completion]".into(),
        );
    };
    let serialize = match mode.to_str() {
        Some("walk") => false,
        Some("json") => true,
        _ => return Err("mode must be walk or json".into()),
    };
    let order = match rest {
        [] => PartOrder::Walk,
        [value] if value == "walk" => PartOrder::Walk,
        [value] if value == "completion" => PartOrder::Completion,
        _ => return Err("order must be walk or completion".into()),
    };
    let options = WalkOptions {
        budget: budget.to_str().ok_or("budget must be a number")?.parse()?,
        threads: threads
            .to_str()
            .ok_or("threads must be a number")?
            .parse()?,
        order,
        ..WalkOptions::default()
    };
    let mut entries = 0;
    let mut parts = 0;
    let mut json_bytes = 0;
    let mut line = Vec::new();
    walk_parts(Path::new(tree), &options, &mut |part| {
        entries += part.len();
        parts += 1;
        if serialize {
            line.clear();
            part.write_json(&mut line)
                .map_err(|error| error.change_context(tarseer::WalkError))?;
            json_bytes += line.len();
            black_box(&line);
        } else {
            black_box(&part);
        }
        Ok(())
    })?;
    eprintln!("{entries} entries, {parts} parts, {json_bytes} JSON bytes");
    Ok(())
}
