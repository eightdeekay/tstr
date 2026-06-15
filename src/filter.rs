/// Pattern matching for --only and list commands.
///
/// Rules:
/// - `*` matches anything (including `/`)
/// - If no `*` present, treated as `*pattern*` (substring match)
/// - Match is against the full relative path (e.g., `sso/provider/crud/tests/01-create-provider`)

/// Check if a path matches a pattern.
pub fn matches_pattern(path: &str, pattern: &str) -> bool {
    // No wildcards → substring match
    if !pattern.contains('*') {
        return path.contains(pattern);
    }

    glob_match(path, pattern)
}

/// Simple glob matching where `*` matches any sequence of characters (including `/`).
fn glob_match(text: &str, pattern: &str) -> bool {
    let mut ti = 0; // text index
    let mut pi = 0; // pattern index
    let mut star_pi = None; // position of last * in pattern
    let mut star_ti = 0;    // text position when last * was matched

    let text = text.as_bytes();
    let pattern = pattern.as_bytes();

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'*') {
            // Star: record position and try matching zero characters
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if pi < pattern.len() && (pattern[pi] == text[ti] || pattern[pi] == b'?') {
            // Exact match or ? wildcard
            ti += 1;
            pi += 1;
        } else if let Some(sp) = star_pi {
            // Mismatch, but we have a star to backtrack to
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    // Consume remaining stars in pattern
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

/// Collect all test paths in a suite tree (relative to root).
pub fn collect_test_paths(
    suite: &crate::discovery::Suite,
    root: &std::path::Path,
) -> Vec<String> {
    let mut paths = Vec::new();
    collect_paths_recursive(suite, root, &mut paths);
    paths.sort();
    paths
}

fn collect_paths_recursive(
    suite: &crate::discovery::Suite,
    root: &std::path::Path,
    paths: &mut Vec<String>,
) {
    for (stem, entry) in &suite.entries {
        if entry.file.file_type == crate::ast::FileType::Const
            || entry.file.file_type == crate::ast::FileType::Lib
        {
            continue;
        }
        let relative = suite.path.strip_prefix(root).unwrap_or(&suite.path);
        let full_path = if relative.as_os_str().is_empty() {
            stem.clone()
        } else {
            format!("{}/{}", relative.display(), stem)
        };
        paths.push(full_path);
    }

    for child in suite.children.values() {
        collect_paths_recursive(child, root, paths);
    }
}

/// Filter a suite to only include tests matching the pattern.
/// Returns the set of matching test stems (full relative paths).
pub fn filter_tests(
    suite: &crate::discovery::Suite,
    root: &std::path::Path,
    pattern: &str,
) -> std::collections::HashSet<String> {
    let all_paths = collect_test_paths(suite, root);
    all_paths.into_iter()
        .filter(|p| matches_pattern(p, pattern))
        .collect()
}

/// Collect test directories with their test counts.
/// Returns (dir_relative_path, test_count) sorted by path.
/// Optionally filtered by a pattern (only counts matching tests).
pub fn collect_test_dirs(
    suite: &crate::discovery::Suite,
    root: &std::path::Path,
    filter_pattern: Option<&str>,
) -> Vec<(String, usize)> {
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    collect_dirs_recursive(suite, root, &mut dirs, filter_pattern);
    let mut result: Vec<_> = dirs.into_iter().filter(|(_, count)| *count > 0).collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn collect_dirs_recursive(
    suite: &crate::discovery::Suite,
    root: &std::path::Path,
    dirs: &mut std::collections::HashMap<String, usize>,
    filter_pattern: Option<&str>,
) {
    let relative = suite.path.strip_prefix(root).unwrap_or(&suite.path);
    let dir_path = relative.to_string_lossy().to_string();

    let mut count = 0;
    for (stem, entry) in &suite.entries {
        if entry.file.file_type == crate::ast::FileType::Const
            || entry.file.file_type == crate::ast::FileType::Lib
        {
            continue;
        }
        let full_path = if dir_path.is_empty() {
            stem.clone()
        } else {
            format!("{}/{}", dir_path, stem)
        };
        if let Some(pattern) = filter_pattern {
            if !matches_pattern(&full_path, pattern) {
                continue;
            }
        }
        count += 1;
    }

    if count > 0 {
        dirs.insert(if dir_path.is_empty() { ".".to_string() } else { dir_path }, count);
    }

    for child in suite.children.values() {
        collect_dirs_recursive(child, root, dirs, filter_pattern);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substring_match() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "provider"));
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "create"));
        assert!(!matches_pattern("sso/provider/crud/tests/01-create-provider", "group"));
    }

    #[test]
    fn test_wildcard_prefix() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "sso/*"));
        assert!(!matches_pattern("profile/group/crud/tests/01-create", "sso/*"));
    }

    #[test]
    fn test_wildcard_suffix() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "*provider"));
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "*create*"));
    }

    #[test]
    fn test_wildcard_middle() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "sso/*/crud/*"));
        assert!(matches_pattern("profile/group/crud/tests/01-create", "*/crud/*"));
    }

    #[test]
    fn test_exact_match() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "sso/provider/crud/tests/01-create-provider"));
    }

    #[test]
    fn test_star_matches_slash() {
        assert!(matches_pattern("sso/provider/crud/tests/01-create-provider", "sso*provider"));
    }

    #[test]
    fn test_pattern_with_no_star_is_substring() {
        assert!(matches_pattern("profile/group/crud/tests/01-create-group", "group"));
        assert!(matches_pattern("profile/group/crud/tests/01-create-group", "crud"));
        assert!(matches_pattern("profile/group/crud/tests/01-create-group", "01-create"));
    }
}
