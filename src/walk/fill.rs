// Reading the metadata that a walk with `Metadata::Kinds` left out.

use std::io;
use std::path::Path;

use super::reader::{Directory, Stat};
#[cfg(unix)]
use crate::manifest::FileRow;
use crate::manifest::TreePart;

/// Reads the size, modification time and permission bits of every row of
/// `part`, from a walk of `root` with [`Metadata::Kinds`](super::Metadata),
/// and returns how many rows could not be read. Those keep size 0, no
/// modification time and mode 0: the entry may be gone since the walk.
///
/// Each directory that holds a row is opened once, relative to the one above
/// it, and each row is read relative to its directory, so this costs what
/// reading the metadata in the walk would have. Parts do not depend on each
/// other: read them on any thread, while their files are already being
/// copied.
///
/// # Errors
/// If `root` cannot be opened.
pub fn read_metadata(root: &Path, part: &mut TreePart) -> io::Result<usize> {
    let mut chain = Chain {
        open: vec![Directory::open_root(root)?],
        nodes: Vec::new(),
        wanted: Vec::new(),
    };
    let mut failed = 0;
    for index in 0..part.directories.len() {
        let row = part.directories[index];
        match chain.stat(part, row.parent, row.name) {
            Ok(stat) => {
                let row = &mut part.directories[index];
                row.mtime = stat.mtime;
                row.mode = stat.mode;
            }
            Err(_) => failed += 1,
        }
    }
    for index in 0..part.files.len() {
        let row = part.files[index];
        match chain.stat(part, row.parent, row.name) {
            Ok(stat) => {
                let row = &mut part.files[index];
                row.size = stat.size;
                row.mtime = stat.mtime;
                row.mode = stat.mode;
            }
            Err(_) => failed += 1,
        }
    }
    for index in 0..part.symlinks.len() {
        let row = part.symlinks[index];
        match chain.stat(part, row.parent, row.name) {
            Ok(stat) => part.symlinks[index].mtime = stat.mtime,
            Err(_) => failed += 1,
        }
    }
    Ok(failed)
}

/// Reads the metadata of a regular file the caller has open, `file`, and
/// records it in `row` as a walk with [`Metadata::Full`](super::Metadata)
/// would have.
///
/// For a consumer that opens every file anyway, such as a copy, this is one
/// call on the open file and no lookup of its name, where [`read_metadata`]
/// looks every row up again. The metadata is then that of the file as it was
/// opened, not as it was listed. Directories and symlinks are not opened this
/// way; [`read_metadata`] reads those.
///
/// # Errors
/// If the metadata cannot be read, or `file` is not a regular file, for one
/// that was replaced since the walk. `row` is then unchanged.
#[cfg(unix)]
pub fn read_file_metadata(file: impl std::os::fd::AsFd, row: &mut FileRow) -> io::Result<()> {
    let stat = super::reader::stat_fd(file.as_fd())?;
    if stat.kind != super::reader::Kind::File {
        return Err(io::Error::other("the open file is not a regular file"));
    }
    row.size = stat.size;
    row.mtime = stat.mtime;
    row.mode = stat.mode;
    Ok(())
}

// The directories from the root down to the one last read in, all open.
// Rows of one table come in walk order, so the next row's directory is
// mostly this one or one close to it.
struct Chain {
    // `open[0]` is the root, and `open[i + 1]` is node `nodes[i]`.
    open: Vec<Directory>,
    nodes: Vec<u32>,
    // The nodes from the root down to the one wanted, reused.
    wanted: Vec<u32>,
}

impl Chain {
    fn stat(&mut self, part: &TreePart, parent: u32, name: u32) -> io::Result<Stat> {
        if self.nodes.last().copied().unwrap_or(0) != parent {
            self.enter(part, parent)?;
        }
        self.open
            .last()
            .expect("the root is always open")
            .stat_name(part.text(name))
    }

    fn enter(&mut self, part: &TreePart, node: u32) -> io::Result<()> {
        self.wanted.clear();
        let mut at = node;
        while at != 0 {
            self.wanted.push(at);
            at = part.node(at).0;
        }
        self.wanted.reverse();
        let common = self
            .nodes
            .iter()
            .zip(&self.wanted)
            .take_while(|(open, wanted)| open == wanted)
            .count();
        self.nodes.truncate(common);
        self.open.truncate(common + 1);
        for &below in &self.wanted[common..] {
            let above = self.open.last().expect("the root is always open");
            let directory = above.open_name(part.node(below).1)?;
            self.open.push(directory);
            self.nodes.push(below);
        }
        Ok(())
    }
}
