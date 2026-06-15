//! Suite-wide file index.
//!
//! Flat index of every parsed file in the suite. Used by the structural
//! runner to look up library functions visible from a given directory.
//! No DAG scheduling, no `_in`/`_out` resolution — that's all gone.

use std::path::{Path, PathBuf};

use crate::ast::{File, FileType};
use crate::discovery::{Suite, TestEntry};

pub type FileId = usize;

/// One file in the suite-wide index.
#[derive(Debug)]
pub struct FileNode {
    pub id: FileId,
    pub path: PathBuf,
    pub dir: PathBuf,
    pub stem: String,
    pub display_name: String,
    pub file_type: FileType,
    pub file: File,
}

impl FileNode {
    /// Path relative to the suite root, e.g. "accounts/tests/01-list-expand.test.tstr"
    pub fn rel_path(&self, root: &Path) -> String {
        self.path.strip_prefix(root)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .to_string()
    }

    /// Test path relative to root without the `.test.tstr` extension —
    /// used for filter matching, e.g. "accounts/tests/01-list-expand".
    pub fn match_path(&self, root: &Path) -> String {
        let rel = self.path.strip_prefix(root).unwrap_or(&self.path);
        let parent = rel.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        if parent.is_empty() {
            self.stem.clone()
        } else {
            format!("{}/{}", parent, self.stem)
        }
    }
}

#[derive(Debug)]
pub struct FileIndex {
    pub root: PathBuf,
    pub files: Vec<FileNode>,
}

impl FileIndex {
    /// Flatten a Suite tree into an indexed file list.
    pub fn build(suite: Suite, root: PathBuf) -> Self {
        let mut files = Vec::new();
        flatten_suite(suite, &mut files);
        FileIndex { root, files }
    }

    /// Build the set of libraries visible from `from_dir`, per the resolution
    /// rule: walk from `from_dir` up to root; at each level check the dir
    /// directly and any `lib/` subtree at that level. Closest scope wins.
    /// Returns name → File AST for use in evaluation.
    pub fn visible_libs(&self, from_dir: &Path) -> std::collections::HashMap<String, std::sync::Arc<File>> {
        let mut out: std::collections::HashMap<String, std::sync::Arc<File>> = std::collections::HashMap::new();
        let mut current: &Path = from_dir;
        loop {
            let lib_root = current.join("lib");
            for f in &self.files {
                if f.file_type != FileType::Lib { continue; }
                let in_this_level = f.dir == current;
                let in_lib_subtree = f.dir.starts_with(&lib_root);
                if (in_this_level || in_lib_subtree) && !out.contains_key(&f.stem) {
                    out.insert(f.stem.clone(), std::sync::Arc::new(f.file.clone()));
                }
            }
            if current == self.root { break; }
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        out
    }
}

fn flatten_suite(suite: Suite, out: &mut Vec<FileNode>) {
    let Suite { path: dir, entries, children } = suite;
    for (stem, entry) in entries {
        let TestEntry { path, name, file } = entry;
        let id = out.len();
        out.push(FileNode {
            id,
            path,
            dir: dir.clone(),
            stem,
            display_name: name,
            file_type: file.file_type.clone(),
            file,
        });
    }
    for (_, child) in children {
        flatten_suite(child, out);
    }
}

