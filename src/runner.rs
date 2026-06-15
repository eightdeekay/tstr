//! Structural runner — phase → directory → lex-order execution model.
//!
//! Walks the suite tree directly: no DAG, no pull-matching. Files run in
//! phase + dir + lex order; sibling dirs sequential for MVP; setup files
//! broadcast their `return` / _out into ambient scope; tests assert.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::ast::FileType;
use crate::config::Config;
use crate::discovery::{Suite, TestEntry};
use crate::eval::{self, FileResult};
use crate::output::Printer;
use crate::scheduler::FileIndex;
use crate::value::Value;

/// Options controlling test execution behavior.
pub struct RunOptions {
    pub stop_on_error: bool,
    /// Shared halt flag — when provided, allows multiple concurrent runs to
    /// signal each other to stop (e.g., --repeat with --stop-on-error).
    pub halt_flag: Option<Arc<AtomicBool>>,
    /// Anchor for the slot display: each slot represents one immediate
    /// child of `display_root`. With `display_root == suite root`, slots
    /// are TLDs (broad summary); with a deeper target, slots zoom into
    /// that target's subdirs. None falls back to the suite root.
    pub display_root: Option<std::path::PathBuf>,
    /// Loaded configuration (user + project + --config layers merged).
    /// Default-constructed when no yaml is present.
    pub config: Config,
    /// Precomputed constants namespace, derived from `config.constants`.
    /// Shared via Arc so per-file scopes can attach cheaply.
    pub constants: Arc<HashMap<String, Value>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            stop_on_error: false,
            halt_flag: None,
            display_root: None,
            config: Config::default(),
            constants: Arc::new(HashMap::new()),
        }
    }
}

/// Accumulated counters from a run.
pub struct RunTotals {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub halted: bool,
}

impl RunTotals {
    pub fn new() -> Self {
        RunTotals { passed: 0, failed: 0, skipped: 0, halted: false }
    }

    fn record(&mut self, result: &FileResult) {
        if result.is_const {
            return;
        }
        if result.skipped {
            self.skipped += 1;
        } else if result.failures.is_empty() {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.skipped
    }

    pub fn merge(&mut self, other: RunTotals) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
        if other.halted { self.halted = true; }
    }
}



/// Top-level entry for the structural runner.
pub fn run_structural(
    suite: &Suite,
    index: &FileIndex,
    cli_overrides: &HashMap<String, Value>,
    opts: &RunOptions,
    printer: &Arc<Printer>,
) -> RunTotals {
    let mut totals = RunTotals::new();

    // Build the initial ambient scope from CLI overrides only.
    // Constants and libs are attached per-file (each file gets its own
    // freshly-constructed scope so cascading is explicit).
    let initial_ambient: HashMap<String, Value> = cli_overrides.clone();

    let display_root = opts.display_root.clone()
        .unwrap_or_else(|| index.root.clone());

    // Set up the interactive slot display: one slot per immediate child of
    // display_root, sized by its non-const file count. No-op outside
    // Interactive mode (register_directories guards on mode).
    let dir_totals = compute_slot_totals(suite, &display_root);
    let summaries: Vec<(String, usize)> = dir_totals.iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    printer.register_directories(summaries);

    totals.merge(run_dir_structural(
        suite,
        &initial_ambient,
        None,
        index,
        opts,
        printer,
        &display_root,
    ));

    // Race-proof final redraw so the last file's outcome is on screen.
    printer.finalize_slots();

    totals
}

/// Count non-const files per display slot (immediate child of `display_root`,
/// or "(root)" for files directly under it). Sizes the slot progress bars.
fn compute_slot_totals(
    suite: &Suite,
    display_root: &std::path::Path,
) -> HashMap<String, usize> {
    let mut totals = HashMap::new();
    collect_slot_totals(suite, display_root, &mut totals);
    totals
}

fn collect_slot_totals(
    dir: &Suite,
    display_root: &std::path::Path,
    totals: &mut HashMap<String, usize>,
) {
    for entry in dir.entries.values() {
        if entry.file.file_type == FileType::Const {
            continue;
        }
        let key = slot_key(&entry.path, display_root);
        *totals.entry(key).or_insert(0) += 1;
    }
    for child in dir.children.values() {
        collect_slot_totals(child, display_root, totals);
    }
}

/// Slot key for a file: the first path component below `display_root`, or
/// "(root)" if the file sits directly in `display_root`. Matches output.rs's
/// `tld_of` so register_directories and record_test agree on slot names.
fn slot_key(path: &std::path::Path, display_root: &std::path::Path) -> String {
    let rel = path.strip_prefix(display_root).unwrap_or(path);
    match rel.components().next() {
        Some(c) if rel.components().count() > 1 => c.as_os_str().to_string_lossy().to_string(),
        _ => "(root)".to_string(),
    }
}

/// Recursive walk: each dir builds its scope (parent + this dir's
/// const + setup), then children run **in parallel**, then this dir's
/// tests + cleanups. Returns this subtree's accumulated totals.
///
/// `parent_ambient` is the ambient scope inherited from the parent dir
/// (after parent's const + setup ran). This dir's const + setup append
/// to a clone of it.
///
/// Parallelism: sibling child directories run concurrently via rayon's
/// work-stealing pool (bounded to CPU count, RAYON_NUM_THREADS-overridable).
/// Within a directory, files stay sequential — const/setup must cascade
/// in order, and tests/cleanups run lex-ordered per the structural model.
/// The shared `dir_ambient` is frozen (read-only) before children fan out,
/// so there's no contention on it.
/// `blocked_in` carries a reason when an ancestor's const/setup didn't
/// complete cleanly. When set, this dir's const/setup/test/cleanup files are
/// all skipped (not run) — their inputs were never established, so running
/// them would just emit a pile of cascading failures.
fn run_dir_structural(
    dir: &Suite,
    parent_ambient: &HashMap<String, Value>,
    blocked_in: Option<String>,
    index: &FileIndex,
    opts: &RunOptions,
    printer: &Arc<Printer>,
    display_root: &std::path::Path,
) -> RunTotals {
    use rayon::prelude::*;

    let mut totals = RunTotals::new();

    // Sort entries into phase buckets, each by filename for lex order.
    let mut consts: Vec<&TestEntry> = dir.entries.values()
        .filter(|e| e.file.file_type == FileType::Const)
        .collect();
    let mut setups: Vec<&TestEntry> = dir.entries.values()
        .filter(|e| e.file.file_type == FileType::Setup)
        .collect();
    let mut tests: Vec<&TestEntry> = dir.entries.values()
        .filter(|e| matches!(e.file.file_type, FileType::Test | FileType::Fetch))
        .collect();
    let mut cleanups: Vec<&TestEntry> = dir.entries.values()
        .filter(|e| e.file.file_type == FileType::Cleanup)
        .collect();

    let lex_key = |e: &&TestEntry| e.path.file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    consts.sort_by_key(lex_key);
    setups.sort_by_key(lex_key);
    tests.sort_by_key(lex_key);
    cleanups.sort_by_key(lex_key);

    // Phase 1+2 for this dir: run const + setup, accumulating their
    // outputs into the ambient scope. Sequential — they cascade. If one
    // doesn't complete cleanly, `blocked` is set: its scope was never
    // published, so every dependent file (here and in descendants) is
    // skipped instead of run into a cascade of null-reference failures.
    let mut dir_ambient = parent_ambient.clone();
    let mut blocked: Option<String> = blocked_in;
    for entry in consts.iter().chain(setups.iter()) {
        let result = run_or_skip(entry, &dir_ambient, blocked.as_deref(), index, opts);
        if blocked.is_none() && (!result.failures.is_empty() || result.skipped) {
            blocked = Some(block_reason(&result, entry));
        }
        merge_exports_into(&mut dir_ambient, &result.exports);
        report_file(entry, &result, index, display_root, printer, &mut totals);
    }

    // Freeze the scope; children read it immutably and run concurrently.
    let dir_ambient = dir_ambient;
    let blocked = blocked;
    let children: Vec<&Suite> = dir.children.values().collect();
    let child_totals = children.par_iter()
        .map(|child| run_dir_structural(child, &dir_ambient, blocked.clone(), index, opts, printer, display_root))
        .reduce(RunTotals::new, |mut a, b| { a.merge(b); a });
    totals.merge(child_totals);

    // Phase 3 — tests in this dir (sequential, lex order).
    for entry in tests {
        let result = run_or_skip(entry, &dir_ambient, blocked.as_deref(), index, opts);
        report_file(entry, &result, index, display_root, printer, &mut totals);
    }

    // Phase 4 — cleanups in this dir (sequential, lex order).
    for entry in cleanups {
        let result = run_or_skip(entry, &dir_ambient, blocked.as_deref(), index, opts);
        report_file(entry, &result, index, display_root, printer, &mut totals);
    }

    totals
}

/// Run a file — unless its inputs aren't available, in which case skip it
/// (recording why) rather than executing into a pile of null-reference
/// failures. Two skip triggers, most-specific first:
///   1. A declared input parameter (`name -->`) resolves to null/absent →
///      name it. This is the actionable message for the test author.
///   2. An upstream const/setup didn't complete (`blocked`) → cite that.
fn run_or_skip(
    entry: &TestEntry,
    ambient: &HashMap<String, Value>,
    blocked: Option<&str>,
    index: &FileIndex,
    opts: &RunOptions,
) -> FileResult {
    let missing = missing_inputs(&entry.file, ambient);
    if !missing.is_empty() {
        return skipped_result(entry, &unavailable_reason(&missing));
    }
    if let Some(reason) = blocked {
        return skipped_result(entry, reason);
    }
    exec_structural_file(entry, ambient, index, opts)
}

/// Declared input parameters (`name -->`) that are absent or null in ambient
/// scope — i.e. the inputs a prior setup was supposed to publish but didn't.
fn missing_inputs(file: &crate::ast::File, ambient: &HashMap<String, Value>) -> Vec<String> {
    let mut missing = Vec::new();
    for name in &file.inputs {
        match ambient.get(name.as_str()) {
            None => missing.push(name.clone()),
            Some(Value::Null) => missing.push(name.clone()),
            Some(_) => {} // available
        }
    }
    missing
}

/// "input parameter 'x' not available" / "input parameters 'x', 'y' not available".
fn unavailable_reason(names: &[String]) -> String {
    if names.len() == 1 {
        format!("input parameter '{}' not available", names[0])
    } else {
        let list = names.iter()
            .map(|n| format!("'{}'", n))
            .collect::<Vec<_>>()
            .join(", ");
        format!("input parameters {} not available", list)
    }
}

/// Why downstream files should be skipped after a const/setup didn't complete.
/// Names the setup's declared outputs (the parameters now unavailable) so even
/// dependents that don't declare those inputs get an actionable message.
fn block_reason(result: &FileResult, entry: &TestEntry) -> String {
    let what = if !result.failures.is_empty() { "failed" } else { "did not run" };
    if entry.file.outputs.is_empty() {
        format!("prior setup '{}' {}", entry.name, what)
    } else {
        format!("{} (setup '{}' {})",
            unavailable_reason(&entry.file.outputs), entry.name, what)
    }
}

/// A FileResult for a file we deliberately skipped (never executed). Counts as
/// a skip — never a pass or fail — and carries the reason for the log/UI.
fn skipped_result(entry: &TestEntry, reason: &str) -> FileResult {
    FileResult {
        name: entry.name.clone(),
        skipped: true,
        disabled: false,
        skip_reason: Some(reason.to_string()),
        inputs: Vec::new(),
        failures: Vec::new(),
        endpoint: None,
        exports: HashMap::new(),
        logs: Vec::new(),
        elapsed: std::time::Duration::ZERO,
        is_const: entry.file.file_type == FileType::Const,
        matrices: Vec::new(),
    }
}

/// Report one file's result to every consumer: the run log + streaming output
/// (file_result), the interactive slot display (record_test), and the totals.
fn report_file(
    entry: &TestEntry,
    result: &eval::FileResult,
    index: &FileIndex,
    display_root: &std::path::Path,
    printer: &Arc<Printer>,
    totals: &mut RunTotals,
) {
    printer.file_result(
        result,
        depth_of_path(&entry.path, &index.root),
        Some(&rel_path_of(&entry.path, &index.root)),
    );
    printer.record_test(&slot_key(&entry.path, display_root), 0, result);
    totals.record(result);
}

/// Execute one file under structural rules. The file sees:
/// - ambient vars merged from its inherited scope
/// - the project constants namespace
/// - the libs visible from its dir
/// Sets `_in` for backward compat with legacy files that still use it.
fn exec_structural_file(
    entry: &TestEntry,
    ambient: &HashMap<String, Value>,
    index: &FileIndex,
    opts: &RunOptions,
) -> eval::FileResult {
    let file_dir = entry.path.parent().unwrap_or(&index.root).to_path_buf();
    let visible_libs = Arc::new(index.visible_libs(&file_dir));

    let mut file_scope = eval::Scope::new()
        .with_constants(Arc::clone(&opts.constants))
        .with_libs(visible_libs);

    // Seed ambient vars into the scope (bare-name access).
    for (k, v) in ambient {
        file_scope.set(k.clone(), v.clone());
    }
    // Legacy compat: also build a `_in` object so files using `_in.X`
    // syntax keep working during migration.
    let in_obj: HashMap<String, Value> = ambient.clone();
    file_scope.set("_in".to_string(), Value::Object(in_obj));
    file_scope.set("_out".to_string(), Value::Object(HashMap::new()));

    match eval::exec_file(&entry.file, &entry.name, &mut file_scope) {
        Ok(result) => result,
        Err(e) => eval::FileResult {
            name: entry.name.clone(),
            skipped: false,
            disabled: false,
            skip_reason: None,
            inputs: Vec::new(),
            failures: vec![eval::AssertionFailure::new(format!("runtime error: {}", e))],
            endpoint: file_scope.last_endpoint(),
            exports: HashMap::new(),
            logs: file_scope.take_logs(),
            elapsed: std::time::Duration::ZERO,
            is_const: entry.file.file_type == FileType::Const,
            matrices: Vec::new(),
        },
    }
}

fn merge_exports_into(ambient: &mut HashMap<String, Value>, exports: &HashMap<String, Value>) {
    for (k, v) in exports {
        ambient.insert(k.clone(), v.clone());
    }
}

fn rel_path_of(path: &std::path::Path, root: &std::path::Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string()
}

fn depth_of_path(path: &std::path::Path, root: &std::path::Path) -> usize {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.parent().map(|p| p.components().count()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> crate::ast::File {
        crate::parser::parse_file(src, "t.test.tstr").unwrap()
    }

    fn entry(name: &str, src: &str) -> TestEntry {
        TestEntry {
            path: std::path::PathBuf::from(format!("{}.test.tstr", name)),
            name: name.to_string(),
            file: parse(src),
        }
    }

    #[test]
    fn missing_inputs_flags_absent_and_null() {
        let file = parse("orgId, token -->\nx = 1;");
        let mut ambient: HashMap<String, Value> = HashMap::new();
        ambient.insert("token".to_string(), Value::Null); // present but null counts as missing
        // orgId absent + token null → both reported
        let mut got = missing_inputs(&file, &ambient);
        got.sort();
        assert_eq!(got, vec!["orgId".to_string(), "token".to_string()]);
    }

    #[test]
    fn missing_inputs_empty_when_available() {
        let file = parse("orgId -->\nx = 1;");
        let mut ambient: HashMap<String, Value> = HashMap::new();
        ambient.insert("orgId".to_string(), Value::String("abc".to_string()));
        assert!(missing_inputs(&file, &ambient).is_empty());
    }

    #[test]
    fn missing_inputs_empty_when_nothing_declared() {
        // A file with no input header can't be skipped by this check — the
        // upstream-failure backstop covers ambient-only dependents instead.
        let file = parse("x = 1;");
        assert!(missing_inputs(&file, &HashMap::new()).is_empty());
    }

    #[test]
    fn unavailable_reason_singular_and_plural() {
        assert_eq!(unavailable_reason(&["orgId".to_string()]),
            "input parameter 'orgId' not available");
        assert_eq!(unavailable_reason(&["a".to_string(), "b".to_string()]),
            "input parameters 'a', 'b' not available");
    }

    #[test]
    fn block_reason_failed_setup_names_its_outputs() {
        let e = entry("00 Login", "x = 1;\n<-- orgId");
        let mut result = skipped_result(&e, "placeholder");
        result.failures = vec![crate::eval::AssertionFailure::new("boom")];
        assert_eq!(block_reason(&result, &e),
            "input parameter 'orgId' not available (setup '00 Login' failed)");
    }

    #[test]
    fn block_reason_setup_without_outputs_is_generic() {
        // No declared outputs to name, and it was skipped/disabled rather than
        // failed → "did not run".
        let e = entry("00 Login", "x = 1;");
        let result = skipped_result(&e, "placeholder"); // skipped=true, failures empty
        assert_eq!(block_reason(&result, &e), "prior setup '00 Login' did not run");
    }
}
