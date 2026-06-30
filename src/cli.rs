use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::Config;
use crate::discovery;
use crate::filter;
use crate::output::{BarStyle, OutputMode, Printer};
use crate::runner;
use crate::scheduler::FileIndex;
use crate::value::Value;

#[derive(Parser)]
#[command(name = "tstr", about = "HTTP API test runner", version)]
pub struct Cli {
    /// Explicit config file path. Overrides any user-global or project tstr.yaml
    /// for fields it specifies. Other fields still merge from those sources.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DisplayMode {
    /// 1:1 glyphs when the row fits, bucketed bar otherwise.
    Auto,
    /// Always use the bucketed colored-block bar.
    Bars,
}

impl DisplayMode {
    fn to_bar_style(self) -> BarStyle {
        match self {
            DisplayMode::Auto => BarStyle::Auto,
            DisplayMode::Bars => BarStyle::Bars,
        }
    }
}

/// How `--repeat N` runs the suite.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RepeatMode {
    /// Run the suite N times, one pass after another (safe default).
    Sequential,
    /// Run N independent passes at once — requires a suite that tolerates
    /// concurrent copies of itself (no colliding fixed-name resources).
    Concurrent,
}

impl RepeatMode {
    /// Parse the `repeat_mode:` config string. Unrecognized values yield `None`
    /// (the caller warns and falls back).
    fn from_config(s: &str) -> Option<RepeatMode> {
        match s.trim().to_lowercase().as_str() {
            "sequential" | "seq" => Some(RepeatMode::Sequential),
            "concurrent" | "parallel" => Some(RepeatMode::Concurrent),
            _ => None,
        }
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run tests
    Run {
        /// Directory to run — the suite root or a subdirectory (default: current directory)
        #[arg(default_value = ".")]
        target: String,

        /// Set default base URL (shorthand for --set 'urlPrefix=<url>')
        #[arg(long = "url")]
        url: Option<String>,

        /// Set/override a variable (repeatable)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,

        /// Stop all execution on first failure
        #[arg(long)]
        stop_on_error: bool,

        /// Run the entire suite N times (see --repeat-mode for how)
        #[arg(long, default_value = "1", value_name = "N")]
        repeat: usize,

        /// How --repeat runs: `sequential` (one pass after another) or
        /// `concurrent` (N overlapping passes). Overrides the suite's
        /// `defaults.repeat_mode`; unset falls back to it, then to sequential.
        #[arg(long, value_enum, value_name = "MODE")]
        repeat_mode: Option<RepeatMode>,

        /// HTTP request timeout in seconds (per-request). 0 disables the timeout.
        #[arg(long, default_value = "60", value_name = "SECONDS")]
        timeout: u64,

        /// Verbose output (show logs, timing, scope changes)
        #[arg(short, long)]
        verbose: bool,

        /// Quiet output (only summary and failures)
        #[arg(short, long)]
        quiet: bool,

        /// Slot-display rendering: `auto` switches between per-test
        /// glyphs and a bucketed bar based on row width; `bars` forces
        /// the colored bucketed bar for every row.
        #[arg(long, value_enum, default_value_t = DisplayMode::Auto)]
        display: DisplayMode,

        /// Max concurrent worker threads. Defaults to CPU count. HTTP work
        /// is I/O-bound (each blocking request parks a worker), so a value
        /// well above CPU count often increases throughput. 0 = CPU count.
        #[arg(short = 'j', long, default_value = "0", value_name = "N")]
        jobs: usize,
    },

    /// List tests matching a pattern
    List {
        /// Directory path or test pattern (default: current directory)
        #[arg(default_value = ".")]
        target: String,

        /// Comma-separated role filter: test, setup, cleanup, const, fetch,
        /// exporter, or `all`. Default: everything except exporters.
        #[arg(long = "type", value_name = "ROLES", default_value = "test,setup,cleanup,const,fetch")]
        ty: String,

        /// Use the old flat listing (one path per line), for shell piping.
        #[arg(long)]
        flat: bool,

        /// List only files turned off via a `disabled:` metadata marker,
        /// in a table showing each one's reason. Ignores --type/--flat.
        #[arg(long)]
        disabled: bool,
    },

    /// Remove all run logs (the `logs/` dir and `tstr-last-run.log`) under the suite root
    Clean {
        /// Directory inside the suite (default: current directory)
        #[arg(default_value = ".")]
        target: String,
    },
}

pub fn run(cli: Cli) {
    let config_override = cli.config.clone();
    match cli.command {
        Commands::Run { target, url, set, stop_on_error, repeat, repeat_mode, timeout, verbose, quiet, display, jobs } => {
            crate::http::set_timeout(timeout);
            // Size the global rayon pool before any parallel work. Default
            // (jobs == 0) leaves rayon's CPU-count default in place.
            if jobs > 0 {
                let _ = rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global();
            }
            run_command(&target, url, set, stop_on_error, repeat, repeat_mode, verbose, quiet, display, config_override);
        }
        Commands::List { target, ty, flat, disabled } => {
            list_command(&target, &ty, flat, disabled);
        }
        Commands::Clean { target } => {
            clean_command(&target);
        }
    }
}

/// `tstr clean` — remove the `logs/` directory and the `tstr-last-run.log`
/// symlink under the suite root. The auto-prune on each run usually makes this
/// unnecessary, but it's here to reclaim space or reset the run counter.
fn clean_command(target: &str) {
    let path = Path::new(target);
    let start = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = discovery::find_root(&start);

    let removed = crate::output::clean_run_logs(&root);
    if removed == 0 {
        println!("No run logs to clean under {}", root.display());
    } else {
        println!("Cleaned {} run log(s) under {}", removed, root.display());
    }
}

/// Resolve a target string into a root path, optional filter pattern, and optional
/// target directory (for scoped discovery — skip parsing files outside the target).
/// If target is a directory, it's used as the starting point.
/// If target is a pattern (contains * or doesn't exist as a dir), use cwd and treat as pattern.
fn resolve_target(target: &str) -> (PathBuf, Option<String>, Option<PathBuf>) {
    let path = Path::new(target);

    if path.is_dir() {
        // It's a directory — use it as the target
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let root = discovery::find_root(&abs);
        if abs == root {
            (root, None, None)
        } else {
            let rel = abs.strip_prefix(&root)
                .unwrap_or(&abs)
                .to_string_lossy()
                .to_string();
            let pattern = format!("{}/*", rel);
            (root, Some(pattern), Some(abs))
        }
    } else {
        // Not a directory — treat as a pattern, find root from cwd
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = discovery::find_root(&cwd);
        (root, Some(target.to_string()), None)
    }
}

/// Resolve a `run` target into (suite_root, optional scoped subdir).
///
/// `run` takes a **directory** — the suite root, or a subdirectory to scope the
/// run to. A target that isn't an existing directory is a hard error: there is
/// no name/glob filtering and no "run the whole suite anyway" fallback (those
/// would silently walk an unrelated tree). The default target `.` is the cwd.
fn resolve_run_target(target: &str) -> Result<(PathBuf, Option<PathBuf>), String> {
    let path = Path::new(target);
    if !path.exists() {
        return Err(format!("no such directory: '{}'", target));
    }
    if !path.is_dir() {
        return Err(format!(
            "'{}' is not a directory — tstr run takes a directory, not a single file",
            target
        ));
    }
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = discovery::find_root(&abs);
    let target_dir = if abs == root { None } else { Some(abs) };
    Ok((root, target_dir))
}

fn run_command(
    target: &str,
    url: Option<String>,
    set_vars: Vec<String>,
    stop_on_error: bool,
    repeat: usize,
    repeat_mode: Option<RepeatMode>,
    verbose: bool,
    quiet: bool,
    display: DisplayMode,
    config_override: Option<PathBuf>,
) {
    if repeat == 0 {
        eprintln!("error: --repeat must be >= 1");
        process::exit(1);
    }

    let mut overrides: HashMap<String, Value> = HashMap::new();
    if let Some(base_url) = url {
        overrides.insert("urlPrefix".to_string(), Value::String(base_url));
    }
    for s in &set_vars {
        match s.split_once('=') {
            Some((key, value)) => {
                overrides.insert(key.to_string(), Value::String(value.to_string()));
            }
            None => {
                eprintln!("error: --set value must be KEY=VALUE, got: {}", s);
                process::exit(1);
            }
        }
    }

    // Resolve target into root + optional scoped subdir. A non-directory target
    // fails fast (no pattern filtering, no whole-suite fallback).
    let (root, target_dir) = match resolve_run_target(target) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            process::exit(1);
        }
    };

    // Load layered config: ~/.config/tstr/config.yaml → <root>/tstr.yaml → --config
    let config = match Config::load_layered(Some(&root), config_override.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            process::exit(1);
        }
    };

    if verbose {
        eprintln!("Suite root: {}", root.display());
        if let Some(ref d) = target_dir {
            eprintln!("Scoped to: {}", d.display());
        }
        if !config.constants.is_empty() || !config.defaults.import.is_empty() {
            eprintln!("Config: {} constants, {} import dirs",
                config.constants.len(), config.defaults.import.len());
        }
        eprintln!();
    }

    // Determine output mode early
    let is_tty = atty::is(atty::Stream::Stdout);

    // Discover from root (scoped to target directory if specified)
    let (suite, warnings) = discovery::discover_lenient_scoped(&root, target_dir.as_deref());
    if !warnings.is_empty() {
        if verbose {
            eprintln!("Parse warnings ({}):", warnings.len());
            for w in &warnings {
                eprintln!("  {}", w);
            }
            eprintln!();
        } else if !quiet && !is_tty {
            eprintln!("\x1b[33m{} file(s) skipped (parse errors) — use -v to see details\x1b[0m\n", warnings.len());
        }
    }

    // Structural rule: tests live only in leaf directories.
    let leaf_violations = discovery::check_leaf_only_tests(&suite, &root);
    if !leaf_violations.is_empty() {
        eprintln!("error: .test.tstr / .fetch.tstr files are only allowed in leaf directories");
        eprintln!("       (a directory with subdirectories is scaffolding — const/setup/cleanup/lib only).");
        eprintln!("       Move these into a leaf directory:");
        for v in &leaf_violations {
            eprintln!("         {}", v);
        }
        process::exit(1);
    }

    // Structural rule: setup/cleanup scaffold the directories below them, so
    // they belong in a non-leaf dir. A leaf has nothing to scaffold — reject.
    let leaf_setup_cleanup = discovery::check_leaf_setup_cleanup(&suite, &root);
    if !leaf_setup_cleanup.is_empty() {
        eprintln!("error: .setup.tstr / .cleanup.tstr files are not allowed in leaf directories");
        eprintln!("       (setup/cleanup scaffold the directories below them — they belong in a");
        eprintln!("       non-leaf parent. Move them up, or rename to .test if they're tests.)");
        eprintln!("       Offending files:");
        for v in &leaf_setup_cleanup {
            eprintln!("         {}", v);
        }
        process::exit(1);
    }

    // Resolve how --repeat runs: CLI flag wins, else the suite's config, else
    // sequential. An unrecognized config value warns and falls back.
    let effective_repeat_mode = repeat_mode.or_else(|| {
        config.defaults.repeat_mode.as_deref().and_then(|s| {
            let parsed = RepeatMode::from_config(s);
            if parsed.is_none() {
                eprintln!("warning: unknown repeat_mode '{}' in config \
                    (use sequential|concurrent); using sequential", s);
            }
            parsed
        })
    }).unwrap_or(RepeatMode::Sequential);
    let concurrent_repeat = repeat > 1 && effective_repeat_mode == RepeatMode::Concurrent;

    let mode = if quiet {
        OutputMode::Quiet
    } else if verbose {
        OutputMode::Verbose
    } else if concurrent_repeat {
        // N overlapping passes can't stream per-test (lines would interleave),
        // but in a terminal they DO render as one wide bar per dir, sized to
        // `tests × repeat`. Off a terminal there's no live display → summary-only.
        if is_tty { OutputMode::Interactive } else { OutputMode::Quiet }
    } else if is_tty {
        OutputMode::Interactive
    } else if repeat > 1 {
        // Non-interactive sequential --repeat: don't stream per-test FAIL/PASS for
        // every iteration (would be N times the noise). Quiet by default; -v overrides.
        OutputMode::Quiet
    } else {
        OutputMode::Normal
    };
    // Concurrent repeat forces the bucketed bar — one row per dir spanning all
    // its (tests × repeat) cells; per-test glyphs wouldn't fit or read sensibly.
    let bar_style = if concurrent_repeat { BarStyle::Bars } else { display.to_bar_style() };
    let printer = Arc::new(Printer::new(mode, bar_style));
    printer.init_failure_log(&root, config.log_retention());
    if !warnings.is_empty() {
        printer.log_parse_errors(&warnings);
    }

    let _ = stop_on_error; // not yet wired through structural runner

    // Precompute the constants namespace from yaml. Wrapped in Arc so per-file
    // scopes share one map without deep-cloning per file.
    let constants_map: HashMap<String, Value> = config.constants.iter()
        .map(|(k, v)| (k.clone(), Value::from_yaml(v)))
        .collect();
    let constants = Arc::new(constants_map);

    // Keep a Suite reference for the structural runner before FileIndex consumes it.
    let suite_for_structural = suite.clone();
    let index = FileIndex::build(suite, root.clone());

    let opts = runner::RunOptions {
        stop_on_error,
        halt_flag: None,
        display_root: target_dir.clone(),
        config,
        constants,
    };

    let run_start = std::time::Instant::now();
    let totals = if repeat == 1 {
        runner::run_structural(&suite_for_structural, &index, &overrides, &opts, &printer)
    } else if concurrent_repeat {
        runner::run_repeated_concurrent(repeat, &suite_for_structural, &index, &overrides, &opts, &printer)
    } else {
        runner::run_repeated_sequential(repeat, &suite_for_structural, &index, &overrides, &opts, &printer)
    };
    printer.set_wall_clock(run_start.elapsed());

    // Append the variable summary block(s) to the log
    printer.flush_summary();

    // Summary
    if repeat > 1 {
        let tests_per_run = totals.total() / repeat;
        eprintln!("({} iterations x {} tests)", repeat, tests_per_run);
    }
    printer.summary(totals.total(), totals.passed, totals.failed, totals.skipped, warnings.len());

    if let Some((path, count)) = printer.failure_log_info() {
        eprintln!("{} failure(s) logged to {}", count, path);
    } else if let Some(path) = printer.log_path() {
        eprintln!("Run log: {}", path);
    }

    if totals.failed > 0 {
        process::exit(1);
    }
}

fn list_command(target: &str, ty: &str, flat: bool, disabled: bool) {
    let (root, pattern, target_dir) = resolve_target(target);

    let (suite, warnings) = discovery::discover_lenient_scoped(&root, target_dir.as_deref());
    if !warnings.is_empty() {
        for w in &warnings {
            eprintln!("warning: {}", w);
        }
        eprintln!();
    }

    if disabled {
        list_disabled(&suite, &root, pattern.as_deref());
        return;
    }

    let roles = parse_role_filter(ty);

    if flat {
        list_flat(&suite, &root, pattern.as_deref(), &roles);
    } else {
        list_grouped(&suite, &root, pattern.as_deref(), &roles);
    }
}

/// Parse the --type CSV into a set of FileTypes. "all" expands to every variant.
fn parse_role_filter(ty: &str) -> std::collections::HashSet<crate::ast::FileType> {
    use crate::ast::FileType;
    let mut roles = std::collections::HashSet::new();
    for tok in ty.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match tok.to_lowercase().as_str() {
            "all" => {
                roles.extend([FileType::Test, FileType::Setup, FileType::Cleanup,
                    FileType::Const, FileType::Fetch, FileType::Exporter, FileType::Lib]);
            }
            "test" => { roles.insert(FileType::Test); }
            "setup" => { roles.insert(FileType::Setup); }
            "cleanup" => { roles.insert(FileType::Cleanup); }
            "const" => { roles.insert(FileType::Const); }
            "fetch" => { roles.insert(FileType::Fetch); }
            "exporter" => { roles.insert(FileType::Exporter); }
            "lib" => { roles.insert(FileType::Lib); }
            other => {
                eprintln!("warning: unknown --type value '{}' (ignored)", other);
            }
        }
    }
    roles
}

fn role_label(ft: &crate::ast::FileType) -> &'static str {
    use crate::ast::FileType;
    match ft {
        FileType::Test => "test",
        FileType::Setup => "setup",
        FileType::Cleanup => "cleanup",
        FileType::Const => "const",
        FileType::Fetch => "fetch",
        FileType::Exporter => "exporter",
        FileType::Lib => "lib",
    }
}

/// Flat listing — one path per line, no headers. Pipeable.
fn list_flat(
    suite: &discovery::Suite,
    root: &Path,
    pattern: Option<&str>,
    roles: &std::collections::HashSet<crate::ast::FileType>,
) {
    let mut rows = Vec::new();
    collect_entries(suite, root, &mut rows);
    rows.retain(|(_, _, ft, _, _)| roles.contains(ft));
    if let Some(p) = pattern {
        rows.retain(|(rel_path, _, _, _, _)| filter::matches_pattern(rel_path, p));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    if rows.is_empty() {
        match pattern {
            Some(p) => println!("No tests match: {}", p),
            None => println!("No tests found."),
        }
    } else {
        for (rel, _, _, _, _) in &rows {
            println!("  {}", rel);
        }
        println!("\n{} entr{}", rows.len(), if rows.len() == 1 { "y" } else { "ies" });
    }
}

/// Per-directory tables: each dir gets a header and a name|role|params|returns table.
fn list_grouped(
    suite: &discovery::Suite,
    root: &Path,
    pattern: Option<&str>,
    roles: &std::collections::HashSet<crate::ast::FileType>,
) {
    let mut rows = Vec::new();
    collect_entries(suite, root, &mut rows);
    rows.retain(|(_, _, ft, _, _)| roles.contains(ft));
    if let Some(p) = pattern {
        rows.retain(|(rel_path, _, _, _, _)| filter::matches_pattern(rel_path, p));
    }

    if rows.is_empty() {
        match pattern {
            Some(p) => println!("No tests match: {}", p),
            None => println!("No tests found."),
        }
        return;
    }

    // Group by directory (the leading path component before the file stem).
    let mut by_dir: std::collections::BTreeMap<String, Vec<(String, &'static str, String, String)>>
        = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for (rel, name, ft, params, returns) in rows {
        let dir = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        by_dir.entry(dir)
            .or_insert_with(Vec::new)
            .push((name, role_label(&ft), params, returns));
        total += 1;
    }

    let dir_count = by_dir.len();
    for (dir, mut entries) in by_dir {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let header = if dir.is_empty() { ".".to_string() } else { dir };
        println!("{}", header);
        print_table(&entries);
        println!();
    }
    println!("{} entr{} across {} director{}",
        total,
        if total == 1 { "y" } else { "ies" },
        dir_count,
        if dir_count == 1 { "y" } else { "ies" },
    );
}

/// `tstr list --disabled` — enumerate every file carrying a `disabled:` metadata
/// marker as a Test|Reason table. Reads the marker statically from the parsed
/// AST (via File::disabled_reason); nothing is executed.
fn list_disabled(suite: &discovery::Suite, root: &Path, pattern: Option<&str>) {
    let mut rows: Vec<(String, String)> = Vec::new();
    collect_disabled(suite, root, &mut rows);
    if let Some(p) = pattern {
        rows.retain(|(rel, _)| filter::matches_pattern(rel, p));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    if rows.is_empty() {
        println!("No disabled tests.");
        return;
    }

    let h_test = "test";
    let h_reason = "reason";
    let w_test = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(h_test.len());
    let w_reason = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(h_reason.len());

    println!("| {:wt$} | {:wr$} |", h_test, h_reason, wt = w_test, wr = w_reason);
    println!("|{:-<wt$}|{:-<wr$}|", "", "", wt = w_test + 2, wr = w_reason + 2);
    for (test, reason) in &rows {
        println!("| {:wt$} | {:wr$} |", test, reason, wt = w_test, wr = w_reason);
    }
    println!("\n{} disabled test{}", rows.len(), if rows.len() == 1 { "" } else { "s" });
}

/// Walk the suite tree collecting (relative_path_with_stem, reason) for every
/// file with a `disabled:` metadata marker.
fn collect_disabled(suite: &discovery::Suite, root: &Path, out: &mut Vec<(String, String)>) {
    let rel_dir = suite.path.strip_prefix(root).unwrap_or(&suite.path);
    for (stem, entry) in &suite.entries {
        if let Some(reason) = entry.file.disabled_reason() {
            let rel = if rel_dir.as_os_str().is_empty() {
                stem.clone()
            } else {
                format!("{}/{}", rel_dir.display(), stem)
            };
            out.push((rel, reason.to_string()));
        }
    }
    for child in suite.children.values() {
        collect_disabled(child, root, out);
    }
}

/// Render one directory's rows as an aligned name|role|params|returns table.
fn print_table(rows: &[(String, &'static str, String, String)]) {
    let h_name = "name";
    let h_role = "role";
    let h_params = "params";
    let h_returns = "returns";

    let w_name = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(h_name.len());
    let w_role = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(h_role.len());
    let w_params = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(h_params.len());
    let w_returns = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(h_returns.len());

    println!("| {:wn$} | {:wr$} | {:wp$} | {:wo$} |",
        h_name, h_role, h_params, h_returns,
        wn = w_name, wr = w_role, wp = w_params, wo = w_returns);
    println!("|{:-<wn$}|{:-<wr$}|{:-<wp$}|{:-<wo$}|",
        "", "", "", "",
        wn = w_name + 2, wr = w_role + 2, wp = w_params + 2, wo = w_returns + 2);
    for (name, role, params, returns) in rows {
        println!("| {:wn$} | {:wr$} | {:wp$} | {:wo$} |",
            name, role, params, returns,
            wn = w_name, wr = w_role, wp = w_params, wo = w_returns);
    }
}

/// Walk the suite tree producing one row per file entry.
/// Row tuple: (relative_path_with_stem, stem, file_type, params_csv, returns_csv)
fn collect_entries(
    suite: &discovery::Suite,
    root: &Path,
    out: &mut Vec<(String, String, crate::ast::FileType, String, String)>,
) {
    let rel_dir = suite.path.strip_prefix(root).unwrap_or(&suite.path);
    for (stem, entry) in &suite.entries {
        let rel = if rel_dir.as_os_str().is_empty() {
            stem.clone()
        } else {
            format!("{}/{}", rel_dir.display(), stem)
        };
        let params = format_list(&entry.file.inputs);
        let returns = format_list(&entry.file.outputs);
        out.push((rel, stem.clone(), entry.file.file_type.clone(), params, returns));
    }
    for child in suite.children.values() {
        collect_entries(child, root, out);
    }
}

fn format_list(items: &[String]) -> String {
    if items.is_empty() { "—".to_string() } else { items.join(", ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_run_target_rejects_missing_path() {
        // A non-existent target fails fast — no pattern fallback, no tree walk.
        let err = resolve_run_target("definitely-not-a-dir-xyz").unwrap_err();
        assert!(err.contains("no such directory"), "got: {}", err);
    }

    #[test]
    fn resolve_run_target_rejects_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.test.tstr");
        std::fs::write(&file, "{ x = 1; }\n").unwrap();
        let err = resolve_run_target(file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("not a directory"), "got: {}", err);
    }

    #[test]
    fn resolve_run_target_accepts_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tstr.yaml"), "constants: {}\n").unwrap();
        let (root, scoped) = resolve_run_target(dir.path().to_str().unwrap()).unwrap();
        // The dir is its own suite root (has tstr.yaml), so nothing is scoped.
        assert_eq!(root, std::fs::canonicalize(dir.path()).unwrap());
        assert!(scoped.is_none());
    }
}
