use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::{File, FileType};
use crate::parser;

/// A parsed test file with its metadata.
#[derive(Debug, Clone)]
pub struct TestEntry {
    /// Full path to the .tstr file
    pub path: PathBuf,
    /// Display name derived from filename: "create-group.test.tstr" → "Create Group"
    pub name: String,
    /// Parsed AST
    pub file: File,
}

/// A directory in the test suite hierarchy.
#[derive(Debug, Clone)]
pub struct Suite {
    /// Directory path
    pub path: PathBuf,
    /// Test files in this directory, keyed by stem (filename without extensions)
    pub entries: HashMap<String, TestEntry>,
    /// Child suites (subdirectories), keyed by directory name
    pub children: HashMap<String, Suite>,
}

impl Suite {
    /// Returns true if this suite has no child suites (it's a leaf).
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Collect all entries of a given type in this suite.
    pub fn entries_of_type(&self, file_type: &FileType) -> Vec<&TestEntry> {
        self.entries.values()
            .filter(|e| e.file.file_type == *file_type)
            .collect()
    }
}

/// Derive a display name from a filename stem.
/// "create-group" → "Create Group", "fetch_site_config" → "Fetch Site Config"
fn display_name(stem: &str) -> String {
    stem.split(|c| c == '-' || c == '_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{}{}", upper, chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the stem from a .tstr filename (everything before the type/extension).
/// "create-group.test.tstr" → "create-group"
/// "simple.tstr" → "simple"
fn file_stem(filename: &str) -> &str {
    let without_tstr = filename.strip_suffix(".tstr").unwrap_or(filename);
    // Strip the type extension if present
    if let Some(dot_pos) = without_tstr.rfind('.') {
        let ext = &without_tstr[dot_pos + 1..];
        match ext {
            "test" | "fetch" | "setup" | "cleanup" | "const" | "exporter" | "lib" => {
                &without_tstr[..dot_pos]
            }
            _ => without_tstr,
        }
    } else {
        without_tstr
    }
}

/// Find the suite root by walking up from `start` until we find the highest
/// directory containing `.tstr` files. Returns the root path.
/// If no ancestor has `.tstr` files, returns `start` itself.
pub fn find_root(start: &Path) -> PathBuf {
    // tstr.yaml is the authoritative root marker — if found, it wins.
    // Falls back to the legacy "highest ancestor with .tstr files" heuristic
    // for projects that haven't (yet) created a tstr.yaml.
    if let Some(root) = crate::config::find_suite_root_by_config(start) {
        return root;
    }

    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut highest_with_tstr = None;

    let mut current = start.as_path();
    loop {
        if dir_has_tstr_files(current) {
            highest_with_tstr = Some(current.to_path_buf());
        }

        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }

    highest_with_tstr.unwrap_or_else(|| start.clone())
}

/// Check if a directory directly contains any .tstr files (not recursive).
fn dir_has_tstr_files(dir: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".tstr") {
                    return true;
                }
            }
        }
    }
    false
}

/// Discover and parse all .tstr files in a directory tree.
/// Returns a Suite representing the root directory, or errors if any files fail to parse.
pub fn discover(root: &Path) -> Result<Suite, Vec<String>> {
    let mut errors = Vec::new();
    let suite = discover_dir(root, &mut errors, None);

    if errors.is_empty() {
        Ok(suite)
    } else {
        Err(errors)
    }
}

/// Like discover, but always returns the suite even if some files fail to parse.
/// Parse errors are returned separately as warnings.
/// Discover a test suite. If `target` is provided, only parse files that are
/// within the target subtree or are const files in ancestor directories
/// (needed for scope). Sibling directories outside the target are skipped.
pub fn discover_lenient(root: &Path) -> (Suite, Vec<String>) {
    let mut errors = Vec::new();
    let suite = discover_dir(root, &mut errors, None);
    (suite, errors)
}

pub fn discover_lenient_scoped(root: &Path, target: Option<&Path>) -> (Suite, Vec<String>) {
    let mut errors = Vec::new();
    let suite = discover_dir(root, &mut errors, target);
    (suite, errors)
}

/// Enforce the structural rule that `.test.tstr` / `.fetch.tstr` files live
/// only in **leaf** directories (those with no child directories). A directory
/// that has subdirectories is scaffolding — const/setup/cleanup/lib only.
/// Returns the suite-root-relative paths of any offending files (empty = OK).
pub fn check_leaf_only_tests(suite: &Suite, root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    collect_leaf_violations(suite, root, &mut violations);
    violations.sort();
    violations
}

fn collect_leaf_violations(suite: &Suite, root: &Path, out: &mut Vec<String>) {
    if !suite.is_leaf() {
        for entry in suite.entries.values() {
            if matches!(entry.file.file_type, FileType::Test | FileType::Fetch) {
                let rel = entry.path.strip_prefix(root).unwrap_or(&entry.path);
                out.push(rel.to_string_lossy().to_string());
            }
        }
    }
    for child in suite.children.values() {
        collect_leaf_violations(child, root, out);
    }
}

fn discover_dir(dir: &Path, errors: &mut Vec<String>, target: Option<&Path>) -> Suite {
    let mut entries = HashMap::new();
    let mut children = HashMap::new();

    // Determine this directory's relationship to the target:
    // - in_scope: this dir is the target or inside it → parse everything
    // - ancestor: this dir is above the target → parse only scope-contributing
    //   files (const + setup + lib), skip the ancestor's own tests/cleanups
    // - outside: sibling branch → skip entirely (shouldn't be called)
    let (in_scope, is_ancestor) = match target {
        None => (true, false),
        Some(t) => {
            if dir.starts_with(t) || dir == t {
                (true, false)
            } else if t.starts_with(dir) {
                (false, true)
            } else {
                (false, false)
            }
        }
    };

    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            errors.push(format!("cannot read directory {}: {}", dir.display(), e));
            return Suite { path: dir.to_path_buf(), entries, children };
        }
    };

    let mut dir_entries: Vec<_> = read_dir
        .filter_map(|e| e.ok())
        .collect();
    dir_entries.sort_by_key(|e| e.file_name());

    for entry in dir_entries {
        let path = entry.path();

        if path.is_dir() {
            // Skip directories that are completely outside the target scope
            if let Some(t) = target {
                let dominated = t.starts_with(&path) || path.starts_with(t);
                if !dominated {
                    continue;
                }
            }
            let dir_name = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let child_suite = discover_dir(&path, errors, target);
            children.insert(dir_name, child_suite);
        } else if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
            if filename.ends_with(".tstr") {
                // In ancestor dirs, keep everything that contributes to the
                // target's scope — const + setup (ambient cascade) and lib
                // (call resolution) — but skip the ancestor's own runnable
                // tests/cleanups. The runner cascades these into the target's
                // scope without executing ancestor tests.
                if is_ancestor {
                    let contributes_scope = filename.contains(".const.")
                        || filename.contains(".setup.")
                        || filename.contains(".lib.");
                    if !contributes_scope {
                        continue;
                    }
                }
                // Outside target entirely — skip (shouldn't normally reach here)
                if !in_scope && !is_ancestor {
                    continue;
                }

                match parse_test_file(&path, filename) {
                    Ok(test_entry) => {
                        let stem = file_stem(filename).to_string();
                        entries.insert(stem, test_entry);
                    }
                    Err(e) => {
                        errors.push(format!("{}: {}", path.display(), e));
                    }
                }
            }
        }
    }

    Suite { path: dir.to_path_buf(), entries, children }
}

fn parse_test_file(path: &Path, filename: &str) -> Result<TestEntry, String> {
    let source = fs::read_to_string(path)
        .map_err(|e| format!("cannot read file: {}", e))?;

    let file = parser::parse_file(&source, filename)?;
    let stem = file_stem(filename);
    let name = display_name(stem);

    Ok(TestEntry {
        path: path.to_path_buf(),
        name,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_suite() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Root const
        fs::write(
            root.join("shared-values.const.tstr"),
            "baseUrl = \"http://localhost:8080\";\n<-- baseUrl\n",
        ).unwrap();

        // crud/ subdirectory (leaf)
        fs::create_dir(root.join("crud")).unwrap();
        fs::write(
            root.join("crud/create-group.test.tstr"),
            "req -->\nr = req.post(\"http://localhost/v4/groups\") ? 2xx | \"Failed\";\ngroupId = r.id;\n<-- groupId\n",
        ).unwrap();
        fs::write(
            root.join("crud/delete-group.test.tstr"),
            "req, groupId -->\nr = req.delete(\"http://localhost/v4/groups\") ? 2xx | \"Failed\";\n",
        ).unwrap();

        // members/ subdirectory (leaf)
        fs::create_dir(root.join("members")).unwrap();
        fs::write(
            root.join("members/add-member.test.tstr"),
            "req, groupId -->\nr = req.post(\"http://localhost/v4/members\") ? 2xx | \"Failed\";\n",
        ).unwrap();

        dir
    }

    #[test]
    fn test_display_name() {
        assert_eq!(display_name("create-group"), "Create Group");
        assert_eq!(display_name("fetch_site_config"), "Fetch Site Config");
        assert_eq!(display_name("simple"), "Simple");
    }

    #[test]
    fn test_file_stem() {
        assert_eq!(file_stem("create-group.test.tstr"), "create-group");
        assert_eq!(file_stem("shared-values.const.tstr"), "shared-values");
        assert_eq!(file_stem("simple.tstr"), "simple");
        assert_eq!(file_stem("site-config.fetch.tstr"), "site-config");
    }

    #[test]
    fn test_discover_structure() {
        let dir = create_test_suite();
        let suite = discover(dir.path()).unwrap();

        // Root has one const file
        assert_eq!(suite.entries.len(), 1);
        assert!(suite.entries.contains_key("shared-values"));

        // Two child suites
        assert_eq!(suite.children.len(), 2);
        assert!(suite.children.contains_key("crud"));
        assert!(suite.children.contains_key("members"));

        // crud has 2 test files
        let crud = &suite.children["crud"];
        assert_eq!(crud.entries.len(), 2);
        assert!(crud.entries.contains_key("create-group"));
        assert!(crud.entries.contains_key("delete-group"));
        assert!(crud.is_leaf());

        // members has 1 test file
        let members = &suite.children["members"];
        assert_eq!(members.entries.len(), 1);
        assert!(members.entries.contains_key("add-member"));
        assert!(members.is_leaf());
    }

    #[test]
    fn test_discover_file_types() {
        let dir = create_test_suite();
        let suite = discover(dir.path()).unwrap();

        let consts = suite.entries_of_type(&FileType::Const);
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].name, "Shared Values");

        let crud = &suite.children["crud"];
        let tests = crud.entries_of_type(&FileType::Test);
        assert_eq!(tests.len(), 2);
    }

    #[test]
    fn leaf_only_tests_passes_valid_suite() {
        // The fixture has tests only in leaf dirs (crud/, members/).
        let dir = create_test_suite();
        let suite = discover(dir.path()).unwrap();
        assert!(check_leaf_only_tests(&suite, dir.path()).is_empty());
    }

    #[test]
    fn leaf_only_tests_flags_test_in_non_leaf() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // A non-leaf dir (has a child) that ALSO holds a test file.
        fs::write(root.join("oops.test.tstr"), "true || \"x\";\n").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/ok.test.tstr"), "true || \"x\";\n").unwrap();

        let suite = discover(root).unwrap();
        let violations = check_leaf_only_tests(&suite, root);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("oops.test.tstr"),
            "expected the non-leaf test to be flagged, got: {:?}", violations);
    }

    #[test]
    fn test_discover_parsed_content() {
        let dir = create_test_suite();
        let suite = discover(dir.path()).unwrap();

        let crud = &suite.children["crud"];
        let create = &crud.entries["create-group"];
        assert_eq!(create.file.inputs, vec!["req"]);
        assert_eq!(create.file.outputs, vec!["groupId"]);
        assert_eq!(create.file.body.len(), 2); // HTTP call + assignment
    }

    #[test]
    fn test_discover_parse_error() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("bad.test.tstr"),
            "this is not valid tstr syntax",
        ).unwrap();

        let result = discover(dir.path());
        assert!(result.is_err());
    }
}
