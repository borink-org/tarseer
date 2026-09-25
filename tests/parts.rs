//! Where parts are cut: against the oracle order, and against a plain
//! recursive statement of the rule, over many budgets.

mod common;

use std::fs;
use std::path::Path;

use common::{TempDir, recurse, wide_fixture};
use tarseer::{EntryKind, WalkOptions, estimate, walk};

// A directory entry as the reference sees it: its estimate, its path, and its
// children.
struct Node {
    path: String,
    kind: EntryKind,
    own: u64,
    children: Vec<Node>,
}

impl Node {
    fn size(&self) -> u64 {
        self.own + self.children.iter().map(Node::size).sum::<u64>()
    }

    fn paths(&self, out: &mut Vec<String>) {
        out.push(self.path.clone());
        for child in &self.children {
            child.paths(out);
        }
    }
}

fn read(directory: &Path, path: &str) -> Vec<Node> {
    let mut children: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    children.sort_by_key(fs::DirEntry::file_name);
    children
        .into_iter()
        .map(|entry| {
            let name = entry.file_name().into_string().unwrap();
            let child = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let file_type = entry.file_type().unwrap();
            if file_type.is_symlink() {
                let target = fs::read_link(entry.path()).unwrap();
                Node {
                    own: estimate(EntryKind::Symlink, &name, target.to_str().unwrap()),
                    path: child,
                    kind: EntryKind::Symlink,
                    children: Vec::new(),
                }
            } else if file_type.is_dir() {
                Node {
                    own: estimate(EntryKind::Directory, &name, ""),
                    children: read(&entry.path(), &child),
                    path: child,
                    kind: EntryKind::Directory,
                }
            } else {
                Node {
                    own: estimate(EntryKind::File, &name, ""),
                    path: child,
                    kind: EntryKind::File,
                    children: Vec::new(),
                }
            }
        })
        .collect()
}

// The rule, stated recursively over a tree already in memory. `carry` holds the
// rows of directories whose row has not left yet.
fn split(children: &[Node], budget: u64, carry: &mut Vec<String>, parts: &mut Vec<Vec<String>>) {
    let mut group = Vec::new();
    let mut group_bytes = 0;
    for child in children {
        let size = child.size();
        if child.kind == EntryKind::Directory && size > budget {
            if !group.is_empty() {
                emit(carry, &mut group, parts);
                group_bytes = 0;
            }
            carry.push(child.path.clone());
            split(&child.children, budget, carry, parts);
            if carry.last() == Some(&child.path) {
                parts.push(std::mem::take(carry));
            }
        } else {
            if !group.is_empty() && group_bytes + size > budget {
                emit(carry, &mut group, parts);
                group_bytes = 0;
            }
            child.paths(&mut group);
            group_bytes += size;
        }
    }
    if !group.is_empty() {
        emit(carry, &mut group, parts);
    }
}

fn emit(carry: &mut Vec<String>, group: &mut Vec<String>, parts: &mut Vec<Vec<String>>) {
    let mut part = std::mem::take(carry);
    part.append(group);
    parts.push(part);
}

const BUDGETS: [u64; 9] = [1, 50, 150, 300, 700, 1_500, 4_000, 20_000, u64::MAX];

#[test]
fn joined_parts_are_the_plain_recursive_walk_at_every_budget() {
    let temp_dir = wide_fixture("joined");
    let mut want = Vec::new();
    recurse(temp_dir.path(), "", &mut want);

    for budget in BUDGETS {
        let options = WalkOptions {
            budget,
            ..WalkOptions::default()
        };
        let walked = walk(temp_dir.path(), &options).unwrap();
        assert_eq!(walked.paths(), want, "budget {budget}");
    }
}

// Checks the walk against the rule at every budget.
fn cut_by_the_rule(root: &Path, budgets: impl IntoIterator<Item = u64>) {
    let tree = read(root, "");
    for budget in budgets {
        let mut want = Vec::new();
        split(&tree, budget, &mut Vec::new(), &mut want);
        {
            let options = WalkOptions {
                budget,
                ..WalkOptions::default()
            };
            let got: Vec<Vec<String>> = walk(root, &options)
                .unwrap()
                .parts
                .iter()
                .map(|part| part.entries().into_iter().map(|(path, _)| path).collect())
                .collect();
            assert_eq!(got, want, "budget {budget}");
        }
    }
}

#[test]
fn parts_are_cut_exactly_where_the_rule_says() {
    let temp_dir = wide_fixture("rule");
    cut_by_the_rule(temp_dir.path(), BUDGETS);
}

// Threads read small subtrees whole, many of them one after another, and hand
// over those they stop inside of. The cuts must not see the difference.
#[test]
fn many_small_subtrees_are_cut_where_the_rule_says() {
    let temp_dir = TempDir::new("rule-small");
    for index in 0..300 {
        let directory = temp_dir.path().join(format!("d{index:03}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..index % 5 {
            fs::write(directory.join(format!("f{file}")), b"").unwrap();
        }
        if index % 7 == 0 {
            let below = directory.join(format!("n{index}/m"));
            fs::create_dir_all(&below).unwrap();
            fs::write(below.join("leaf"), b"").unwrap();
        }
    }
    cut_by_the_rule(
        temp_dir.path(),
        [1, 90, 400, 2_000, 9_000, 60_000, u64::MAX],
    );
}

#[test]
fn a_part_that_holds_more_than_one_unit_stays_within_budget() {
    let temp_dir = wide_fixture("within");
    let budget = 700;
    let options = WalkOptions {
        budget,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options).unwrap();
    assert!(walked.parts.len() > 3, "the fixture is cut at this budget");
    for part in &walked.parts {
        let rows: u64 = part
            .directories
            .iter()
            .map(|row| estimate(EntryKind::Directory, part.text(row.name), ""))
            .chain(
                part.files
                    .iter()
                    .map(|row| estimate(EntryKind::File, part.text(row.name), "")),
            )
            .chain(part.symlinks.iter().map(|row| {
                estimate(
                    EntryKind::Symlink,
                    part.text(row.name),
                    part.text(row.target),
                )
            }))
            .sum();
        // Carried directory rows ride along on top of a group, so allow one
        // row per stem level of slack.
        let slack = (part.stem().len() as u64 + 2) * 60;
        assert!(rows <= budget + slack, "{rows} estimated bytes in one part");
    }
}

#[test]
fn a_stem_names_the_directories_above_the_first_row() {
    let temp_dir = wide_fixture("stem");
    let options = WalkOptions {
        budget: 150,
        ..WalkOptions::default()
    };
    let walked = walk(temp_dir.path(), &options).unwrap();
    for part in &walked.parts {
        let (first, _) = &part.entries()[0];
        let stem: Vec<&str> = part.stem().collect();
        let mut ancestors: Vec<&str> = first.split('/').collect();
        ancestors.pop();
        assert_eq!(stem, ancestors, "part starting at {first}");
    }
}

#[test]
fn the_default_budget_leaves_a_small_tree_whole() {
    let temp_dir = wide_fixture("whole");
    let walked = walk(temp_dir.path(), &WalkOptions::default()).unwrap();
    assert_eq!(walked.parts.len(), 1);
    assert_eq!(walked.parts[0].stem().len(), 0);
}
