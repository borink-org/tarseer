//! The JSON form: that it is the same part, column for column.

mod common;

use common::{fixture, wide_fixture};
use tarseer::{WalkOptions, walk};

fn parse(part: &tarseer::Part) -> serde_json::Value {
    serde_json::from_str(&part.to_json().unwrap()).unwrap()
}

#[test]
fn every_group_has_one_value_per_row_in_every_column() {
    let temp_dir = fixture("json");
    let walked = walk(temp_dir.path(), &WalkOptions::default()).unwrap();
    let part = &walked.parts[0];
    let document = parse(part);

    for (group, columns, want) in [
        (
            "files",
            &["parent", "name", "size", "mode", "mtime", "mtime_nanos"][..],
            part.files.len(),
        ),
        (
            "directories",
            &["parent", "name", "mode", "mtime", "mtime_nanos"][..],
            part.directories.len(),
        ),
        (
            "symlinks",
            &[
                "parent",
                "name",
                "target",
                "mtime",
                "mtime_nanos",
                "directory",
            ][..],
            part.symlinks.len(),
        ),
    ] {
        for column in columns {
            let got = document[group][column]
                .as_array()
                .expect("a column is an array")
                .len();
            assert_eq!(got, want, "{group}.{column}");
        }
    }
}

#[test]
fn the_json_rebuilds_every_path_of_its_part() {
    let temp_dir = wide_fixture("json-paths");
    let options = WalkOptions {
        budget: 300,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options).unwrap();
    assert!(walked.parts.len() > 1);

    for part in &walked.parts {
        let document = parse(part);
        let stem: Vec<String> = document["stem"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect();
        let directory_parent = document["directories"]["parent"].as_array().unwrap();
        let directory_name = document["directories"]["name"].as_array().unwrap();

        // Node 0 is the root, then the stem, then each directory row.
        let node_path = |node: u64| -> String {
            let mut components = Vec::new();
            let mut node = usize::try_from(node).unwrap();
            while node != 0 {
                if node <= stem.len() {
                    components.push(stem[node - 1].clone());
                    node -= 1;
                } else {
                    let row = node - stem.len() - 1;
                    components.push(directory_name[row].as_str().unwrap().to_owned());
                    node = usize::try_from(directory_parent[row].as_u64().unwrap()).unwrap();
                }
            }
            components.reverse();
            components.join("/")
        };
        let join = |parent: &serde_json::Value, name: &serde_json::Value| {
            let directory = node_path(parent.as_u64().unwrap());
            let name = name.as_str().unwrap();
            if directory.is_empty() {
                name.to_owned()
            } else {
                format!("{directory}/{name}")
            }
        };

        let mut from_json = Vec::new();
        for group in ["directories", "files", "symlinks"] {
            let parents = document[group]["parent"].as_array().unwrap();
            let names = document[group]["name"].as_array().unwrap();
            for (parent, name) in parents.iter().zip(names) {
                from_json.push(join(parent, name));
            }
        }
        from_json.sort_by(|left, right| tarseer::walk_order(left, right));

        let want: Vec<String> = part.entries().into_iter().map(|(path, _)| path).collect();
        assert_eq!(from_json, want);
    }
}

#[test]
fn the_columns_carry_the_sizes_and_targets_that_were_written() {
    let temp_dir = fixture("json-values");
    let walked = walk(temp_dir.path(), &WalkOptions::default()).unwrap();
    let document = parse(&walked.parts[0]);

    let names = document["files"]["name"].as_array().unwrap();
    let sizes = document["files"]["size"].as_array().unwrap();
    let at = |name: &str| -> u64 {
        let index = names
            .iter()
            .position(|entry| entry.as_str() == Some(name))
            .unwrap();
        sizes[index].as_u64().unwrap()
    };
    assert_eq!(at("top.txt"), 3);
    assert_eq!(at("z.bin"), 10);

    if cfg!(unix) {
        assert_eq!(document["symlinks"]["name"][0].as_str(), Some("link"));
        assert_eq!(
            document["symlinks"]["target"][0].as_str(),
            Some("../top.txt")
        );
        assert!(
            document["symlinks"]["directory"][0].is_null(),
            "a Unix symlink has no kind"
        );
    }
}
