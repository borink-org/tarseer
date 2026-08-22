//! The JSON form: that it is the same document, column for column, and that a
//! reader can rebuild every path from it without help.

mod common;

use common::fixture;
use tarseer::{index_tree, walk};

fn paths(index: &tarseer::Index) -> Vec<String> {
    let mut out = Vec::new();
    index.for_each_path(|_, p| out.push(p.to_owned()));
    out
}

#[test]
fn the_json_carries_one_value_per_row_per_column() {
    let t = fixture("json");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&index.to_json().unwrap()).unwrap();

    let n = index.len();
    for col in ["kind", "size", "mode", "mtime", "dir", "name", "link"] {
        assert_eq!(
            v[col].as_array().expect("a column is an array").len(),
            n,
            "column {col}"
        );
    }
    assert_eq!(v["count"].as_u64(), Some(n as u64));
    assert_eq!(
        v["dir_parent"].as_array().unwrap().len(),
        v["dir_name"].as_array().unwrap().len()
    );
}

#[test]
fn the_json_names_every_path_the_listing_does() {
    let t = fixture("json-paths");
    let index = index_tree(&walk(t.path()).unwrap()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&index.to_json().unwrap()).unwrap();

    let nums = |col: &str| -> Vec<u64> {
        v[col]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap())
            .collect()
    };
    let strs = |col: &str| -> Vec<String> {
        v[col]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_owned())
            .collect()
    };

    // Rebuild every path from the columns the way a reader would, walking the
    // directory table to the root and writing back down.
    let (dir_parent, dir_name) = (nums("dir_parent"), strs("dir_name"));
    let root = u64::from(u32::MAX);
    let mut rebuilt = Vec::new();
    for (i, name) in strs("name").into_iter().enumerate() {
        let mut parts = Vec::new();
        let mut at = nums("dir")[i];
        while at != root {
            let at_i = usize::try_from(at).unwrap();
            parts.push(dir_name[at_i].clone());
            at = dir_parent[at_i];
        }
        parts.reverse();
        parts.push(name);
        parts.retain(|s| !s.is_empty());
        rebuilt.push(parts.join("/"));
    }
    assert_eq!(rebuilt, paths(&index));
}
