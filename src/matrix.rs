//! Matrix fan-out planning.
//!
//! A `matrix name = [ "Label": { ... }, ... ];` statement in a directory's
//! `.const.tstr` fans the rest of that directory — setup, child dirs, tests,
//! cleanup — out once per entry, each run seeded with that entry's variables.
//! Several matrices in one directory fan out over their cartesian product; a
//! matrix in a nested directory fans out again inside each enclosing entry.
//!
//! `matrix` is a top-level statement of a const file only, so every label is
//! known from the AST before anything runs. That lets the display size its
//! rows, `--matrix` selectors and `--set` collisions be checked up front, and
//! a blocked directory still report one (skipped) run per entry.

use std::collections::HashSet;
use std::path::Path;

use crate::ast::{Expr, FileType, Statement};
use crate::discovery::{Suite, TestEntry};
use crate::eval::MatrixDef;
use crate::value::ValueMap;

/// Joins entry labels of nested or multiplied matrices: `Site A × prod`.
pub const LABEL_SEP: &str = " × ";

/// A matrix as declared in source — labels plus, where the entry value is an
/// object literal, its keys (None when the value is computed).
#[derive(Debug, Clone)]
pub struct StaticMatrix {
    pub name: String,
    /// Root-relative path of the declaring const file, for messages.
    pub file: String,
    pub labels: Vec<String>,
    pub literal_keys: Vec<Option<Vec<String>>>,
}

/// One run of a fanned-out directory: its combined label and the variables
/// it injects into scope.
#[derive(Debug, Clone)]
pub struct Combo {
    pub label: String,
    pub vars: ValueMap,
}

/// The matrices declared by `dir`'s const files, in execution order (const
/// files lex-sorted, statements in file order).
pub fn dir_matrices(dir: &Suite, root: &Path) -> Vec<StaticMatrix> {
    let mut consts: Vec<&TestEntry> = dir.entries.values()
        .filter(|e| e.file.file_type == FileType::Const)
        .collect();
    consts.sort_by_key(|e| e.path.file_name().map(|n| n.to_os_string()).unwrap_or_default());

    let mut out = Vec::new();
    for entry in consts {
        for stmt in &entry.file.body {
            if let Statement::Matrix { name, entries } = stmt {
                out.push(StaticMatrix {
                    name: name.clone(),
                    file: rel(&entry.path, root),
                    labels: entries.iter().map(|e| e.label.clone()).collect(),
                    literal_keys: entries.iter().map(|e| match &e.value {
                        Expr::JsonObject(fields) => Some(fields.iter().map(|(k, _)| k.clone()).collect()),
                        _ => None,
                    }).collect(),
                });
            }
        }
    }
    out
}

/// Indices of the entries a matrix keeps under the `--matrix` selectors: if
/// any selector names one of its labels, only the named ones; otherwise all.
/// So `--matrix "Site A"` narrows the sites matrix and leaves an envs matrix
/// alone.
pub fn kept(labels: &[String], select: &[String]) -> Vec<usize> {
    let named: Vec<usize> = (0..labels.len())
        .filter(|&i| select.iter().any(|s| s == &labels[i]))
        .collect();
    if named.is_empty() { (0..labels.len()).collect() } else { named }
}

/// Every combination of the kept entries of `matrices`, as index tuples
/// (one index per matrix), first matrix varying slowest.
fn product(matrices: &[StaticMatrix], select: &[String]) -> Vec<Vec<usize>> {
    let mut acc: Vec<Vec<usize>> = vec![Vec::new()];
    for m in matrices {
        let keep = kept(&m.labels, select);
        acc = acc.iter()
            .flat_map(|prefix| keep.iter().map(move |&i| {
                let mut next = prefix.clone();
                next.push(i);
                next
            }))
            .collect();
    }
    acc
}

fn tuple_label(matrices: &[StaticMatrix], tuple: &[usize]) -> String {
    tuple.iter().enumerate()
        .map(|(m, &i)| matrices[m].labels[i].as_str())
        .collect::<Vec<_>>()
        .join(LABEL_SEP)
}

/// Labels of every run a directory fans out into — empty when it declares no
/// matrix. Used to size the display before anything runs.
pub fn combo_labels(matrices: &[StaticMatrix], select: &[String]) -> Vec<String> {
    if matrices.is_empty() {
        return Vec::new();
    }
    product(matrices, select).iter().map(|t| tuple_label(matrices, t)).collect()
}

/// Build the runs for a directory from its evaluated matrices. `evaluated`
/// must line up with `declared` (same names, same entry counts) — a const
/// file that `return`s before its matrix, say, breaks that, and is an error.
/// A variable set by both an entry and `--set`/`--url`, or by two matrices in
/// the same combination, is an error too.
pub fn combos(
    declared: &[StaticMatrix],
    evaluated: &[MatrixDef],
    select: &[String],
    cli_keys: &HashSet<String>,
) -> Result<Vec<Combo>, String> {
    if declared.len() != evaluated.len()
        || declared.iter().zip(evaluated).any(|(d, e)| d.name != e.name || d.labels.len() != e.entries.len())
    {
        let missing: Vec<&str> = declared.iter()
            .filter(|d| !evaluated.iter().any(|e| e.name == d.name))
            .map(|d| d.name.as_str())
            .collect();
        return Err(format!(
            "matrix {} was not evaluated (does its const file return early?)",
            quoted_list(&missing),
        ));
    }

    let mut out = Vec::new();
    for tuple in product(declared, select) {
        let mut vars = ValueMap::new();
        let mut owner: Vec<(&str, &str)> = Vec::new(); // (var, matrix) for overlap messages
        for (m, &i) in tuple.iter().enumerate() {
            let entry = &evaluated[m].entries[i];
            for (k, v) in &entry.vars {
                if cli_keys.contains(k) {
                    return Err(cli_conflict(&declared[m].name, &entry.label, k));
                }
                if let Some((_, other)) = owner.iter().find(|(var, _)| var == k) {
                    return Err(format!(
                        "variable '{}' is set by both matrix '{}' and matrix '{}'",
                        k, other, declared[m].name,
                    ));
                }
                owner.push((k.as_str(), declared[m].name.as_str()));
                vars.insert(k.clone(), v.clone());
            }
        }
        out.push(Combo { label: tuple_label(declared, &tuple), vars });
    }
    Ok(out)
}

/// `outer × inner` — a nested directory's label inside its enclosing run.
pub fn join(outer: Option<&str>, inner: &str) -> String {
    match outer {
        Some(o) => format!("{}{}{}", o, LABEL_SEP, inner),
        None => inner.to_string(),
    }
}

/// Startup checks over the whole discovered suite, so a malformed matrix or a
/// bad flag fails before any request goes out. Returns every problem found.
pub fn validate(suite: &Suite, root: &Path, select: &[String], cli_keys: &HashSet<String>) -> Vec<String> {
    let mut errors = Vec::new();
    let mut all_labels: HashSet<String> = HashSet::new();
    validate_dir(suite, root, cli_keys, &mut all_labels, &mut errors);
    for s in select {
        if !all_labels.contains(s) {
            errors.push(format!("--matrix '{}' matches no matrix entry in this run", s));
        }
    }
    errors
}

fn validate_dir(
    dir: &Suite,
    root: &Path,
    cli_keys: &HashSet<String>,
    all_labels: &mut HashSet<String>,
    errors: &mut Vec<String>,
) {
    // `matrix` anywhere but the top level of a const file.
    for entry in dir.entries.values() {
        let top_ok = entry.file.file_type == FileType::Const;
        for stmt in &entry.file.body {
            match stmt {
                Statement::Matrix { name, .. } if !top_ok => errors.push(format!(
                    "{}: matrix '{}' is only allowed in a .const.tstr file",
                    rel(&entry.path, root), name,
                )),
                other => nested_matrices(other, &rel(&entry.path, root), errors),
            }
        }
    }

    let matrices = dir_matrices(dir, root);
    let mut names: HashSet<&str> = HashSet::new();
    for m in &matrices {
        if !names.insert(m.name.as_str()) {
            errors.push(format!("{}: matrix '{}' is declared twice in this directory", m.file, m.name));
        }
        if m.labels.is_empty() {
            errors.push(format!("{}: matrix '{}' has no entries", m.file, m.name));
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for (label, keys) in m.labels.iter().zip(&m.literal_keys) {
            if !seen.insert(label.as_str()) {
                errors.push(format!("{}: matrix '{}' has two entries labelled '{}'", m.file, m.name, label));
            }
            for k in keys.iter().flatten() {
                if cli_keys.contains(k) {
                    errors.push(format!("{}: {}", m.file, cli_conflict(&m.name, label, k)));
                }
            }
            all_labels.insert(label.clone());
        }
    }
    // Two matrices of one directory may not set the same variable — their
    // product would have to pick one silently.
    for (a, ma) in matrices.iter().enumerate() {
        for mb in &matrices[a + 1..] {
            let ka: HashSet<&String> = ma.literal_keys.iter().flatten().flatten().collect();
            let mut clash: Vec<&String> = mb.literal_keys.iter().flatten().flatten()
                .filter(|k| ka.contains(k))
                .collect();
            clash.sort();
            clash.dedup();
            for k in clash {
                errors.push(format!(
                    "{}: variable '{}' is set by both matrix '{}' and matrix '{}'",
                    mb.file, k, ma.name, mb.name,
                ));
            }
        }
    }

    let mut children: Vec<&Suite> = dir.children.values().collect();
    children.sort_by(|a, b| a.path.cmp(&b.path));
    for child in children {
        validate_dir(child, root, cli_keys, all_labels, errors);
    }
}

/// Flag a `matrix` buried inside an `if` or `retry` body.
fn nested_matrices(stmt: &Statement, file: &str, errors: &mut Vec<String>) {
    let bodies: Vec<&Vec<Statement>> = match stmt {
        Statement::If { then_body, else_body, .. } => vec![then_body, else_body],
        Statement::Retry { body, .. } => vec![body],
        _ => return,
    };
    for body in bodies {
        for inner in body {
            if let Statement::Matrix { name, .. } = inner {
                errors.push(format!(
                    "{}: matrix '{}' must be a top-level statement, not inside a block",
                    file, name,
                ));
            }
            nested_matrices(inner, file, errors);
        }
    }
}

fn cli_conflict(matrix: &str, label: &str, var: &str) -> String {
    format!(
        "matrix '{}' entry '{}' sets '{}', which is also given on the command line (--set/--url); drop one",
        matrix, label, var,
    )
}

fn quoted_list(names: &[&str]) -> String {
    names.iter().map(|n| format!("'{}'", n)).collect::<Vec<_>>().join(", ")
}

fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::EvaluatedMatrixEntry;
    use crate::value::Value;

    fn sm(name: &str, labels: &[&str]) -> StaticMatrix {
        StaticMatrix {
            name: name.to_string(),
            file: "m.const.tstr".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            literal_keys: labels.iter().map(|_| None).collect(),
        }
    }

    fn def(name: &str, entries: &[(&str, &str, i64)]) -> MatrixDef {
        MatrixDef {
            name: name.to_string(),
            entries: entries.iter().map(|(label, k, v)| {
                let mut vars = ValueMap::new();
                vars.insert(k.to_string(), Value::Number(*v as f64));
                EvaluatedMatrixEntry { label: label.to_string(), vars }
            }).collect(),
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn product_labels_first_matrix_slowest() {
        let ms = [sm("sites", &["A", "B"]), sm("envs", &["dev", "prod"])];
        assert_eq!(combo_labels(&ms, &[]), strings(&["A × dev", "A × prod", "B × dev", "B × prod"]));
    }

    #[test]
    fn selectors_narrow_only_the_matrix_they_name() {
        let ms = [sm("sites", &["A", "B", "C"]), sm("envs", &["dev", "prod"])];
        let select = strings(&["A", "C"]);
        assert_eq!(combo_labels(&ms, &select), strings(&["A × dev", "A × prod", "C × dev", "C × prod"]));
    }

    #[test]
    fn no_matrices_means_no_fan_out() {
        assert!(combo_labels(&[], &[]).is_empty());
    }

    #[test]
    fn combos_merge_vars_across_matrices() {
        let declared = [sm("sites", &["A", "B"]), sm("envs", &["dev"])];
        let evaluated = [def("sites", &[("A", "site", 1), ("B", "site", 2)]), def("envs", &[("dev", "env", 9)])];
        let got = combos(&declared, &evaluated, &[], &HashSet::new()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].label, "B × dev");
        assert_eq!(got[1].vars.get("site"), Some(&Value::Number(2.0)));
        assert_eq!(got[1].vars.get("env"), Some(&Value::Number(9.0)));
    }

    #[test]
    fn combos_reject_cli_collision() {
        let declared = [sm("sites", &["A"])];
        let evaluated = [def("sites", &[("A", "urlPrefix", 1)])];
        let cli: HashSet<String> = ["urlPrefix".to_string()].into_iter().collect();
        let err = combos(&declared, &evaluated, &[], &cli).unwrap_err();
        assert!(err.contains("'urlPrefix'"), "{}", err);
    }

    #[test]
    fn combos_reject_overlap_between_matrices() {
        let declared = [sm("a", &["x"]), sm("b", &["y"])];
        let evaluated = [def("a", &[("x", "v", 1)]), def("b", &[("y", "v", 2)])];
        let err = combos(&declared, &evaluated, &[], &HashSet::new()).unwrap_err();
        assert!(err.contains("'v'"), "{}", err);
    }

    #[test]
    fn combos_reject_unevaluated_matrix() {
        let declared = [sm("sites", &["A"])];
        let err = combos(&declared, &[], &[], &HashSet::new()).unwrap_err();
        assert!(err.contains("'sites'"), "{}", err);
    }

    #[test]
    fn join_nests_labels() {
        assert_eq!(join(None, "A"), "A");
        assert_eq!(join(Some("A"), "dev"), "A × dev");
    }
}
