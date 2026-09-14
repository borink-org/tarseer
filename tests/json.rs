//! The JSON form: that it is the same tree, column for column, and that a
//! reader learns from it everything the listing shows.

mod common;

use common::fixture;
use tarseer::walk;

#[test]
fn every_group_has_one_value_per_row_in_every_column() {
    let temp_dir = fixture("json");
    let tree = walk(temp_dir.path()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&tree.to_json().unwrap()).unwrap();

    for (group, cols, want) in [
        (
            "files",
            &["path", "size", "mode", "mtime"][..],
            tree.files.len(),
        ),
        ("dirs", &["path", "mode", "mtime"][..], tree.dirs.len()),
        ("links", &["path", "target", "mtime"][..], tree.links.len()),
    ] {
        for col in cols {
            let got = doc[group][col]
                .as_array()
                .expect("a column is an array")
                .len();
            assert_eq!(got, want, "{group}.{col}");
        }
    }
    assert_eq!(doc["count"].as_u64(), Some(tree.len() as u64));
}

#[test]
fn the_json_names_every_path_the_listing_does() {
    let temp_dir = fixture("json-paths");
    let tree = walk(temp_dir.path()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&tree.to_json().unwrap()).unwrap();

    let mut from_json: Vec<String> = ["files", "dirs", "links"]
        .iter()
        .flat_map(|group| doc[*group]["path"].as_array().unwrap())
        .map(|entry| entry.as_str().unwrap().to_owned())
        .collect();
    from_json.sort();

    assert_eq!(from_json, tree.paths());
}

#[test]
fn the_columns_carry_the_sizes_and_targets_that_were_written() {
    let temp_dir = fixture("json-values");
    let tree = walk(temp_dir.path()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&tree.to_json().unwrap()).unwrap();

    // Paths and sizes are separate arrays, so reading one means indexing the
    // other — exactly what a consumer of this document has to do.
    let paths = doc["files"]["path"].as_array().unwrap();
    let sizes = doc["files"]["size"].as_array().unwrap();
    let at = |name: &str| -> u64 {
        let index = paths
            .iter()
            .position(|entry| entry.as_str() == Some(name))
            .unwrap();
        sizes[index].as_u64().unwrap()
    };
    assert_eq!(at("top.txt"), 3);
    assert_eq!(at("zed/z.bin"), 10);

    if cfg!(unix) {
        assert_eq!(doc["links"]["path"][0].as_str(), Some("alpha/link"));
        assert_eq!(doc["links"]["target"][0].as_str(), Some("../top.txt"));
    }
}

#[test]
fn an_empty_tree_is_still_a_whole_document() {
    let temp_dir = common::TempDir::new("json-empty");
    let tree = walk(temp_dir.path()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&tree.to_json().unwrap()).unwrap();

    assert_eq!(doc["count"].as_u64(), Some(0));
    assert_eq!(doc["files"]["size"].as_array().unwrap().len(), 0);
    assert_eq!(doc["skips"]["unreadable"].as_u64(), Some(0));
}
