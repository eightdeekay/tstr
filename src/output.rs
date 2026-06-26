use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::eval::FileResult;

// ANSI color codes
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, PartialEq)]
pub enum OutputMode {
    Interactive,
    Normal,
    Verbose,
    Quiet,
}

/// Slot-row rendering style. `Auto` is the original heuristic (1:1 glyphs
/// when the count fits, bucketed bar otherwise). `Bars` forces the bucketed
/// renderer for every row, even short ones, with bar width capped at
/// `min(count, max_bar_width)` so a 5-test dir doesn't render as 5 blocks
/// floating in 85 chars of empty bar.
#[derive(Clone, Copy, PartialEq)]
pub enum BarStyle {
    Auto,
    Bars,
}

/// Thread-safe printer for streaming test results.
pub struct Printer {
    out: Mutex<Box<dyn Write + Send>>,
    pub mode: OutputMode,
    pub bar_style: BarStyle,
    pending_headers: Mutex<Vec<(usize, String)>>,
    interactive: Mutex<InteractiveState>,
    matrix: Mutex<MatrixDisplayState>,
    matrix_stop: Arc<AtomicBool>,
    failure_log: Mutex<Option<Box<dyn Write + Send>>>,
    failure_log_path: Mutex<Option<String>>,
    failure_count: Mutex<usize>,
    pending_summaries: Mutex<Vec<(String, Vec<(String, Option<String>, crate::value::Value)>)>>,
    /// Per-top-level-directory stats for the end-of-run table
    tld_stats: Mutex<HashMap<String, TldStats>>,
    /// Wall-clock duration of the whole run, set by the runner before
    /// `summary`. Distinct from the per-suite "Time" column, which sums
    /// each file's own elapsed (work-time) and so reads the same whether
    /// the run was parallel or serial.
    wall_clock: Mutex<Option<std::time::Duration>>,
}

#[derive(Default, Clone)]
struct TldStats {
    passed: usize,
    failed: usize,
    skipped: usize,
    elapsed: std::time::Duration,
}

/// Fixed-slot interactive display.
struct InteractiveState {
    /// Fixed display slots — one per visible directory.
    slots: Vec<Slot>,
    /// Map: dir_path → slot index (for visible directories only).
    /// Overflow dirs are absent here; their results contribute to totals.
    dir_to_slot: HashMap<String, usize>,
    /// Column width for path alignment
    col_width: usize,
    /// Max tests across all directories (for indicator alignment)
    max_tests: usize,
    /// Bar width in characters for the bracketed progress region. All
    /// rows render exactly this many chars in 1:1 (count <= bar_width)
    /// or bucketed (count > bar_width) mode.
    bar_width: usize,
    /// Total number of display lines (header + slots + overflow + footer)
    display_lines: usize,
    /// Number of slots
    num_slots: usize,
    /// 0 or 1 — whether an "(and X more)" overflow row sits between the
    /// last slot and the footer. Affects the cursor-up math.
    overflow_lines: usize,
    /// Running totals
    total_passed: usize,
    total_failed: usize,
    total_skipped: usize,
    /// Remaining counts
    remaining_dirs: usize,
    remaining_tests: usize,
    /// Total counts (for header)
    total_dirs: usize,
    total_tests: usize,
    /// Number of dirs that didn't fit on screen (rendered into the
    /// overflow row but otherwise tracked through totals only).
    hidden_dirs: usize,
    /// Lines reserved for the errors panel below the footer (excludes
    /// the separator). 0 means panel is disabled — used when the
    /// terminal is too short to give it any room.
    panel_size: usize,
    /// Width to truncate panel lines to (term_width - 1 for safety).
    panel_width: usize,
    /// Rolling buffer of recent failure messages, capped at panel_size.
    /// Each entry is the formatted "dir/name: message" line ready to draw.
    error_log: Vec<String>,
    initialized: bool,
}

struct Slot {
    /// Currently assigned directory path, or None if idle
    dir_path: Option<String>,
    /// Test count for current directory
    count: usize,
    /// Current indicators
    indicators: Vec<Indicator>,
    /// Number of tests completed so far in this dir; used to assign the
    /// next indicator position and to detect when the dir is done.
    completed: usize,
}


/// Matrix-specific interactive display state.
struct MatrixDisplayState {
    rows: Vec<MatrixRow>,
    /// Column width for label alignment
    label_width: usize,
    /// Number of test groups (directories) per combination
    num_groups: usize,
    /// Group names for column reference
    group_names: Vec<String>,
    /// Number of iteration columns (≥1). 1 = no --repeat; N = --repeat N.
    num_iters: usize,
    /// Visual column width of each iter cell (including brackets + padding).
    iter_col_width: usize,
    /// Total display lines (header + rows)
    display_lines: usize,
    /// Spinner frame counter
    spinner_frame: usize,
    initialized: bool,
}

struct MatrixRow {
    label: String,
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    /// Per-iteration, per-group progress: `iters[iter_idx][group_idx]`
    iters: Vec<Vec<GroupProgress>>,
}

#[derive(Clone)]
enum GroupProgress {
    Pending,
    InProgress { completed: usize, total: usize, has_failure: bool },
    Done { all_passed: bool },
}

const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl MatrixDisplayState {
    fn new() -> Self {
        MatrixDisplayState {
            rows: Vec::new(),
            label_width: 0,
            num_groups: 0,
            group_names: Vec::new(),
            num_iters: 1,
            iter_col_width: 0,
            display_lines: 0,
            spinner_frame: 0,
            initialized: false,
        }
    }
}

#[derive(Clone)]
struct SlotDrawInfo {
    slot_idx: usize,
    num_slots: usize,
    col_width: usize,
    bar_width: usize,
    overflow_lines: usize,
    /// Lines that sit below the footer (separator + error rows). Adds
    /// to lines_from_bottom calculations so cursor-up doesn't land in
    /// the wrong place when an errors panel is enabled.
    lines_below_footer: usize,
    bar_style: BarStyle,
    path: Option<String>,
    count: usize,
    indicators: Vec<Indicator>,
}

#[derive(Clone)]
struct PanelSnapshot {
    panel_size: usize,
    entries: Vec<String>,
}

#[derive(Clone, Copy)]
struct StatusInfo {
    completed: usize,
    total: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    lines_below_footer: usize,
}

#[derive(Clone, Copy)]
enum Indicator {
    Pending,
    Pass,
    Fail,
    Skip,
    /// Intentionally turned off via `disabled:`. Rendered distinctly
    /// from a conditional `Skip` so postponed-fix tests stay visible.
    Disabled,
}

impl InteractiveState {
    fn new() -> Self {
        InteractiveState {
            slots: Vec::new(),
            dir_to_slot: HashMap::new(),
            col_width: 0,
            max_tests: 0,
            bar_width: 0,
            display_lines: 0,
            num_slots: 0,
            overflow_lines: 0,
            total_passed: 0,
            total_failed: 0,
            total_skipped: 0,
            remaining_dirs: 0,
            remaining_tests: 0,
            total_dirs: 0,
            total_tests: 0,
            hidden_dirs: 0,
            panel_size: 0,
            panel_width: 0,
            error_log: Vec::new(),
            initialized: false,
        }
    }
}

impl Printer {
    pub fn new(mode: OutputMode, bar_style: BarStyle) -> Self {
        Printer {
            out: Mutex::new(Box::new(io::stdout())),
            mode,
            bar_style,
            pending_headers: Mutex::new(Vec::new()),
            interactive: Mutex::new(InteractiveState::new()),
            matrix: Mutex::new(MatrixDisplayState::new()),
            matrix_stop: Arc::new(AtomicBool::new(false)),
            failure_log: Mutex::new(None),
            failure_log_path: Mutex::new(None),
            failure_count: Mutex::new(0),
            pending_summaries: Mutex::new(Vec::new()),
            tld_stats: Mutex::new(HashMap::new()),
            wall_clock: Mutex::new(None),
        }
    }

    /// Record the whole-run wall-clock duration (set by the runner before
    /// `summary`). Surfaced as a separate line so parallel speedup is visible.
    pub fn set_wall_clock(&self, d: std::time::Duration) {
        *self.wall_clock.lock().unwrap() = Some(d);
    }

    /// Whether a fixed-position live display currently owns the screen.
    /// When true, the streaming `file_result` / `error` paths must stay
    /// silent or their writes will corrupt the cursor-driven redraw.
    fn live_display_active(&self) -> bool {
        self.interactive.lock().map(|s| s.initialized).unwrap_or(false)
            || self.matrix.lock().map(|s| s.initialized).unwrap_or(false)
    }

    pub fn init_failure_log(&self, _root: &std::path::Path) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let log_path = cwd.join("tstr-last-run.log");
        if let Ok(file) = std::fs::File::create(&log_path) {
            *self.failure_log.lock().unwrap() = Some(Box::new(file));
            *self.failure_log_path.lock().unwrap() = Some(log_path.to_string_lossy().to_string());
        }
    }

    /// Write the test entry to the run log, including the variable table.
    /// Called for every test (PASS/FAIL/SKIP) regardless of verbosity.
    pub fn log_test(&self, result: &FileResult, source_file: Option<&str>) {
        if result.is_const {
            return;
        }
        let mut log = self.failure_log.lock().unwrap();
        if let Some(ref mut f) = *log {
            let label = if result.disabled {
                "DISABLED"
            } else if result.incompatible {
                "INCOMPATIBLE"
            } else if result.skipped {
                "SKIP"
            } else if result.failures.is_empty() {
                "PASS"
            } else {
                "FAIL"
            };
            let suffix = match source_file {
                Some(p) => format!("  ({})", p),
                None => String::new(),
            };
            let _ = writeln!(f, "{}  {}{}", label, result.name, suffix);

            if result.skipped {
                if let Some(ref reason) = result.skip_reason {
                    let _ = writeln!(f, "      reason: {}", reason);
                }
            }

            if let Some(ref ep) = result.endpoint {
                let _ = writeln!(f, "      {}", ep);
            }

            if !result.failures.is_empty() {
                for failure in &result.failures {
                    let _ = writeln!(f, "      {}", failure.message);
                }
                *self.failure_count.lock().unwrap() += 1;
            }

            write_var_table(f, &result.inputs, &result.exports);

            if !result.logs.is_empty() {
                let _ = writeln!(f, "      logs:");
                for log_msg in &result.logs {
                    let _ = writeln!(f, "        {}", log_msg);
                }
            }
            let _ = writeln!(f);
        }
    }

    /// Buffer a scope snapshot for the end-of-run summary.
    pub fn log_summary(&self, label: &str, vars: &[(String, Option<String>, crate::value::Value)]) {
        if vars.is_empty() {
            return;
        }
        self.pending_summaries.lock().unwrap()
            .push((label.to_string(), vars.to_vec()));
    }

    /// Flush all pending summary blocks to the log (called once at end of run).
    pub fn flush_summary(&self) {
        let mut summaries = self.pending_summaries.lock().unwrap();
        if summaries.is_empty() {
            return;
        }
        let mut log = self.failure_log.lock().unwrap();
        if let Some(ref mut f) = *log {
            for (label, vars) in summaries.drain(..) {
                let _ = writeln!(f, "=== Variables [{}] ===", label);
                write_summary_table(f, &vars);
                let _ = writeln!(f);
            }
        }
    }

    pub fn log_parse_errors(&self, warnings: &[String]) {
        let mut log = self.failure_log.lock().unwrap();
        if let Some(ref mut f) = *log {
            for w in warnings {
                let _ = writeln!(f, "PARSE  {}", w);
            }
            if !warnings.is_empty() {
                let _ = writeln!(f);
            }
        }
    }

    pub fn failure_log_info(&self) -> Option<(String, usize)> {
        let path = self.failure_log_path.lock().unwrap().clone();
        let count = *self.failure_count.lock().unwrap();
        if count > 0 { path.map(|p| (p, count)) } else { None }
    }

    /// Path to the run log, regardless of pass/fail count.
    pub fn log_path(&self) -> Option<String> {
        self.failure_log_path.lock().unwrap().clone()
    }

    /// Clean up the log file if there were no failures.
    pub fn cleanup_log_on_success(&self) {
        let count = *self.failure_count.lock().unwrap();
        if count == 0 {
            if let Some(ref path) = *self.failure_log_path.lock().unwrap() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Register directories for interactive mode. Eagerly pre-assigns one
    /// slot per directory (sorted), capping at the terminal-height fit.
    /// Anything that doesn't fit is folded into a single "(and X more
    /// dirs not shown)" overflow row whose tests still feed totals/footer
    /// and the run log. If even one slot can't fit (or no TTY size is
    /// available and the suite is too tall), initialization is skipped
    /// and the streaming `file_result` path takes over.
    pub fn register_directories(&self, mut dirs: Vec<(String, usize)>) {
        if self.mode != OutputMode::Interactive {
            return;
        }
        if dirs.is_empty() {
            return;
        }

        // Stable layout — visible rows always come from the same set,
        // and dirs that overflow into the "and X more" row are deterministic.
        dirs.sort_by(|a, b| a.0.cmp(&b.0));

        // Reserved layout overhead, top-to-bottom:
        //   visible_count slots
        //   overflow_lines (0 or 1)
        //   1 status line (was header+footer; now merged)
        //   1 separator (when panel shown)
        //   panel_size error rows (when panel shown)
        //   2-line buffer below — the redraw never walks into the
        //   final summary or the user's prompt area.
        const STRUCT_LINES: usize = 4; // status + summary + 2-line buffer
        const PANEL_LINES_DEFAULT: usize = 5;
        const PANEL_SEP_LINES: usize = 1;
        const PANEL_MIN_VIEW_BUDGET: usize = 8; // need this many free lines before we even consider a panel
        let (term_w, term_h) = terminal_size::terminal_size()
            .map(|(w, h)| (w.0 as usize, h.0 as usize))
            .unwrap_or((80, 24));

        // Panel only fits if the terminal has room for it AND at least
        // one slot row alongside it.
        let panel_size = if term_h >= STRUCT_LINES + PANEL_SEP_LINES + PANEL_LINES_DEFAULT + 1 {
            PANEL_LINES_DEFAULT
        } else if term_h >= PANEL_MIN_VIEW_BUDGET + PANEL_SEP_LINES + 2 {
            // Shrink to fit
            term_h.saturating_sub(STRUCT_LINES + PANEL_SEP_LINES + 1).min(PANEL_LINES_DEFAULT)
        } else {
            0
        };
        let panel_total = if panel_size > 0 { PANEL_SEP_LINES + panel_size } else { 0 };

        let max_visible_rows = term_h.saturating_sub(STRUCT_LINES + panel_total);
        if max_visible_rows == 0 {
            return;
        }

        let total_dirs = dirs.len();
        let total_tests: usize = dirs.iter().map(|(_, c)| *c).sum();

        // Reserve one row for the overflow message if we can't show every dir.
        let needs_overflow = total_dirs > max_visible_rows;
        let visible_count = if needs_overflow {
            // overflow row eats one row from the budget
            total_dirs.min(max_visible_rows.saturating_sub(1))
        } else {
            total_dirs
        };
        if visible_count == 0 {
            return;
        }
        let hidden = total_dirs - visible_count;

        let visible_dirs = &dirs[..visible_count];
        let col_width = visible_dirs.iter().map(|(p, _)| p.len()).max().unwrap_or(0) + 2;
        let max_tests = visible_dirs.iter().map(|(_, c)| *c).max().unwrap_or(0);

        // Bar width: term_width minus name column, brackets, margins, and
        // room for a trailing "  N/T" counter sized to the largest dir.
        let counter_width = format!("  {0}/{0}", max_tests).len();
        let chrome = col_width + 2 /* "[" + "]" */ + 2 /* leading margin */;
        let bar_budget = term_w.saturating_sub(chrome).saturating_sub(counter_width);
        // Auto mode caps at max_tests so dirs with few tests don't render a
        // tiny glyph row floating in 80 chars of empty space. Bars mode
        // stretches to the full budget so the bar visibly fills the row.
        let bar_width = match self.bar_style {
            BarStyle::Bars => bar_budget.max(1),
            BarStyle::Auto => bar_budget.min(max_tests).max(1),
        };

        let mut state = self.interactive.lock().unwrap();
        if state.initialized {
            return;
        }
        let mut out = self.out.lock().unwrap();

        state.col_width = col_width;
        state.max_tests = max_tests;
        state.bar_width = bar_width;
        state.num_slots = visible_count;
        state.overflow_lines = if needs_overflow { 1 } else { 0 };
        state.total_dirs = total_dirs;
        state.total_tests = total_tests;
        state.remaining_dirs = total_dirs;
        state.remaining_tests = total_tests;
        state.hidden_dirs = hidden;
        state.panel_size = panel_size;
        state.panel_width = term_w.saturating_sub(1).max(20);
        state.display_lines = visible_count + state.overflow_lines + 1 + panel_total;

        // Pre-assign slots for visible dirs.
        for (i, (dir_path, count)) in visible_dirs.iter().enumerate() {
            state.slots.push(Slot {
                dir_path: Some(dir_path.clone()),
                count: *count,
                indicators: vec![Indicator::Pending; *count],
                completed: 0,
            });
            state.dir_to_slot.insert(dir_path.clone(), i);
        }

        // Initial slot rows (all-pending bars).
        for i in 0..visible_count {
            let info = self.gather_slot_draw_info(&state, i);
            write_slot_row(&mut out, &info);
            let _ = writeln!(out);
        }

        // Overflow row (static — totals already capture progress).
        if needs_overflow {
            let _ = writeln!(out, "{DIM}  (and {} more dir(s) not shown — see tstr-last-run.log){RESET}",
                hidden);
        }

        // Combined status line (was header + footer).
        let _ = writeln!(out, "{DIM}Tests: 0/{}  Passed: 0  Failed: 0  Skipped: 0{RESET}",
            state.total_tests);

        // Errors panel (separator + N reserved lines, all initially blank).
        if panel_size > 0 {
            let sep: String = std::iter::repeat('─').take(state.panel_width.min(60)).collect();
            let _ = writeln!(out, "{DIM}{}{RESET}", sep);
            for _ in 0..panel_size {
                let _ = writeln!(out);
            }
        }
        let _ = out.flush();

        state.initialized = true;
    }

    /// One-shot per-test update for the slot display. Updates the
    /// pre-assigned slot for `dir_path` if visible, otherwise just
    /// updates totals (overflow dirs aren't drawn but their results
    /// still feed the footer and the errors panel). The `_dir_total`
    /// argument is retained for API compatibility — slot totals are
    /// fixed at registration.
    pub fn record_test(&self, dir_path: &str, _dir_total: usize, result: &FileResult) {
        if self.mode != OutputMode::Interactive {
            return;
        }
        if result.is_const {
            return;
        }

        let (slot_info, status_info, panel_snapshot) = {
            let mut state = self.interactive.lock().unwrap();
            if !state.initialized {
                return;
            }

            // Totals always count, regardless of slot visibility.
            if result.skipped {
                state.total_skipped += 1;
            } else if result.failures.is_empty() {
                state.total_passed += 1;
            } else {
                state.total_failed += 1;
            }
            state.remaining_tests = state.remaining_tests.saturating_sub(1);

            // Append to the rolling errors panel for any failed test.
            let mut panel_changed = false;
            if !result.failures.is_empty() && state.panel_size > 0 {
                if let Some(f) = result.failures.first() {
                    let line = format_panel_entry(dir_path, &result.name, &f.message, state.panel_width);
                    state.error_log.push(line);
                    let cap = state.panel_size;
                    while state.error_log.len() > cap {
                        state.error_log.remove(0);
                    }
                    panel_changed = true;
                }
            }

            let slot_info = if let Some(slot_idx) = state.dir_to_slot.get(dir_path).copied() {
                let indicator = if result.disabled {
                    Indicator::Disabled
                } else if result.skipped {
                    Indicator::Skip
                } else if result.failures.is_empty() {
                    Indicator::Pass
                } else {
                    Indicator::Fail
                };

                let slot = &mut state.slots[slot_idx];
                let idx = slot.completed.min(slot.indicators.len().saturating_sub(1));
                if idx < slot.indicators.len() {
                    slot.indicators[idx] = indicator;
                }
                slot.completed += 1;
                let just_finished = slot.completed >= slot.count;

                if just_finished {
                    state.remaining_dirs = state.remaining_dirs.saturating_sub(1);
                }

                Some(self.gather_slot_draw_info(&state, slot_idx))
            } else {
                None
            };

            let status_info = self.gather_status_info(&state);
            let panel_snapshot = if panel_changed { Some(self.gather_panel_snapshot(&state)) } else { None };
            (slot_info, status_info, panel_snapshot)
        };

        if let Some(info) = slot_info {
            self.draw_slot(&info);
        }
        self.draw_status(status_info);
        if let Some(snap) = panel_snapshot {
            self.draw_errors(&snap);
        }
    }

    /// Lines below the footer: 0 if no panel, else 1 (separator) + panel_size.
    fn lines_below_footer(state: &InteractiveState) -> usize {
        if state.panel_size > 0 { 1 + state.panel_size } else { 0 }
    }

    /// Gather draw info from state without holding the lock during I/O.
    fn gather_slot_draw_info(&self, state: &InteractiveState, slot_idx: usize) -> SlotDrawInfo {
        let slot = &state.slots[slot_idx];
        SlotDrawInfo {
            slot_idx,
            num_slots: state.num_slots,
            col_width: state.col_width,
            bar_width: state.bar_width,
            overflow_lines: state.overflow_lines,
            lines_below_footer: Self::lines_below_footer(state),
            bar_style: self.bar_style,
            path: slot.dir_path.clone(),
            count: slot.count,
            indicators: slot.indicators.clone(),
        }
    }

    fn gather_status_info(&self, state: &InteractiveState) -> StatusInfo {
        StatusInfo {
            completed: state.total_tests.saturating_sub(state.remaining_tests),
            total: state.total_tests,
            passed: state.total_passed,
            failed: state.total_failed,
            skipped: state.total_skipped,
            lines_below_footer: Self::lines_below_footer(state),
        }
    }

    fn gather_panel_snapshot(&self, state: &InteractiveState) -> PanelSnapshot {
        PanelSnapshot {
            panel_size: state.panel_size,
            entries: state.error_log.clone(),
        }
    }

    /// Draw a slot line (no state lock needed).
    fn draw_slot(&self, info: &SlotDrawInfo) {
        let mut out = self.out.lock().unwrap();
        // Lines below this row: remaining slots + overflow row + footer + panel.
        let lines_from_bottom = (info.num_slots - info.slot_idx) + info.overflow_lines + 1 + info.lines_below_footer;

        let _ = write!(out, "\x1b[{}A\r\x1b[2K", lines_from_bottom);
        write_slot_row(&mut out, info);
        let _ = write!(out, "\n");
        if lines_from_bottom > 1 {
            let _ = write!(out, "\x1b[{}B", lines_from_bottom - 1);
        }
        let _ = out.flush();
    }

    /// Combined status line: progress + per-status totals.
    fn draw_status(&self, info: StatusInfo) {
        let mut out = self.out.lock().unwrap();
        let lines_from_bottom = 1 + info.lines_below_footer;
        let _ = write!(out, "\x1b[{}A\r\x1b[2K", lines_from_bottom);
        let pass_color = if info.passed > 0 { GREEN } else { "" };
        let fail_color = if info.failed > 0 { RED } else { "" };
        let skip_color = if info.skipped > 0 { YELLOW } else { "" };
        let _ = write!(out,
            "{DIM}Tests: {}/{}{RESET}  {}Passed: {}{RESET}  {}Failed: {}{RESET}  {}Skipped: {}{RESET}",
            info.completed, info.total,
            pass_color, info.passed,
            fail_color, info.failed,
            skip_color, info.skipped);
        let _ = write!(out, "\n");
        if lines_from_bottom > 1 {
            let _ = write!(out, "\x1b[{}B", lines_from_bottom - 1);
        }
        let _ = out.flush();
    }

    /// Decrement the slot display's expected totals by one for a test
    /// that the runner has decided will never call `record_test` (e.g.,
    /// a cleanup file whose `_in.X` provider failed — silently dropped
    /// from execution by design). Without this, the slot's bar reserves
    /// an indicator slot that stays Pending forever.
    pub fn skip_silent(&self, dir_path: &str) {
        if self.mode != OutputMode::Interactive {
            return;
        }
        let (slot_info, status_info) = {
            let mut state = self.interactive.lock().unwrap();
            if !state.initialized {
                return;
            }
            state.total_tests = state.total_tests.saturating_sub(1);
            state.remaining_tests = state.remaining_tests.saturating_sub(1);

            let slot_idx_opt = state.dir_to_slot.get(dir_path).copied();
            if let Some(slot_idx) = slot_idx_opt {
                let slot = &mut state.slots[slot_idx];
                if slot.count > 0 {
                    slot.count -= 1;
                    slot.indicators.pop();
                }
                // If this drop puts us at the threshold, account for the
                // dir being effectively complete.
                if slot.completed >= slot.count && slot.completed > 0 {
                    // Already counted via record_test's just_finished branch
                    // when the last real test completed; nothing to do here.
                }
            }
            let slot_info = slot_idx_opt.map(|i| self.gather_slot_draw_info(&state, i));
            let status_info = self.gather_status_info(&state);
            (slot_info, status_info)
        };
        if let Some(info) = slot_info {
            self.draw_slot(&info);
        }
        self.draw_status(status_info);
    }

    /// One-shot full redraw from current state. Call after parallel
    /// execution finishes (e.g., end of `run_plan_iter`) to guarantee the
    /// final visual matches state, regardless of any draw-order races
    /// between concurrent `record_test` calls during the run.
    pub fn finalize_slots(&self) {
        if self.mode != OutputMode::Interactive {
            return;
        }
        let (slot_infos, status_info, panel_snapshot) = {
            let state = self.interactive.lock().unwrap();
            if !state.initialized {
                return;
            }
            let slots: Vec<SlotDrawInfo> = (0..state.num_slots)
                .map(|i| self.gather_slot_draw_info(&state, i))
                .collect();
            let status = self.gather_status_info(&state);
            let panel = self.gather_panel_snapshot(&state);
            (slots, status, panel)
        };
        for info in &slot_infos {
            self.draw_slot(info);
        }
        self.draw_status(status_info);
        self.draw_errors(&panel_snapshot);
    }

    /// Redraw the entire errors panel. Called whenever the rolling buffer
    /// changes; cheap because the panel is at most a handful of lines.
    fn draw_errors(&self, snap: &PanelSnapshot) {
        if snap.panel_size == 0 {
            return;
        }
        let mut out = self.out.lock().unwrap();
        // Cursor sits below the entire display. The first error line is
        // panel_size lines above us (separator is one above that).
        let _ = write!(out, "\x1b[{}A\r", snap.panel_size);
        for i in 0..snap.panel_size {
            let _ = write!(out, "\x1b[2K");
            if let Some(line) = snap.entries.get(i) {
                let _ = write!(out, "{}", line);
            }
            let _ = writeln!(out);
        }
        // Final \n landed cursor back where it started. No need to move.
        let _ = out.flush();
    }

    // --- Streaming mode methods ---

    pub fn queue_suite_header(&self, name: &str, depth: usize) {
        if self.mode == OutputMode::Quiet || self.mode == OutputMode::Interactive {
            return;
        }
        self.pending_headers.lock().unwrap().push((depth, name.to_string()));
    }

    pub fn dequeue_suite_header(&self) {
        if self.mode == OutputMode::Interactive {
            return;
        }
        self.pending_headers.lock().unwrap().pop();
    }

    fn flush_headers(&self, out: &mut Box<dyn Write + Send>) {
        let mut headers = self.pending_headers.lock().unwrap();
        for (depth, name) in headers.drain(..) {
            let indent = "  ".repeat(depth);
            let _ = writeln!(out, "{}{}:", indent, name);
        }
    }

    pub fn file_result(&self, result: &FileResult, depth: usize, source_file: Option<&str>, scaffold: bool) {
        // Log every test (PASS/FAIL/SKIP) to the run log regardless of mode.
        self.log_test(result, source_file);

        let failed = !result.skipped && !result.failures.is_empty();

        // Per-suite summary stats. Consts are loads, not tests. Scaffolding
        // (non-leaf setup/cleanup) is infrastructure — it only earns a table
        // row when it FAILS (so the table's Fail count matches the exit code);
        // passing/skipped scaffolding stays invisible.
        if !result.is_const && (!scaffold || failed) {
            let tld = source_file.map(tld_of).unwrap_or_else(|| "(root)".to_string());
            let mut stats = self.tld_stats.lock().unwrap();
            let entry = stats.entry(tld).or_default();
            entry.elapsed += result.elapsed;
            if result.skipped {
                entry.skipped += 1;
            } else if result.failures.is_empty() {
                entry.passed += 1;
            } else {
                entry.failed += 1;
            }
        }

        // Suppress streaming only when a fixed live display owns the
        // screen — otherwise (Interactive but slot init was skipped due
        // to terminal-height fallback) fall through to streaming.
        if self.mode == OutputMode::Interactive && self.live_display_active() {
            return;
        }

        // Consts (loads) and passing/skipped scaffolding stream only under -v;
        // a failed scaffold always streams so the failure is never swallowed.
        if result.is_const && self.mode != OutputMode::Verbose {
            return;
        }
        if scaffold && !failed && self.mode != OutputMode::Verbose {
            return;
        }

        let mut out = self.out.lock().unwrap();
        self.flush_headers(&mut out);
        let indent = "  ".repeat(depth);

        if result.disabled {
            if self.mode != OutputMode::Quiet {
                let reason = result.skip_reason.as_deref().unwrap_or("");
                let _ = writeln!(out, "{}{CYAN}  DISABLED{RESET}  {}  {DIM}{}{RESET}",
                    indent, result.name, reason);
            }
        } else if result.incompatible {
            if self.mode != OutputMode::Quiet {
                let reason = result.skip_reason.as_deref().unwrap_or("");
                let _ = writeln!(out, "{}{MAGENTA}  INCOMPATIBLE{RESET}  {}  {DIM}{}{RESET}",
                    indent, result.name, reason);
            }
        } else if result.skipped {
            if self.mode != OutputMode::Quiet {
                match result.skip_reason.as_deref() {
                    Some(reason) if !reason.is_empty() => {
                        let _ = writeln!(out, "{}{YELLOW}  SKIP{RESET}  {}  {DIM}{}{RESET}",
                            indent, result.name, reason);
                    }
                    _ => {
                        let _ = writeln!(out, "{}{YELLOW}  SKIP{RESET}  {}", indent, result.name);
                    }
                }
            }
        } else if result.failures.is_empty() {
            if self.mode != OutputMode::Quiet {
                let timing = if self.mode == OutputMode::Verbose || result.elapsed.as_millis() > 500 {
                    format!(" {DIM}({}){RESET}", format_duration(result.elapsed))
                } else {
                    String::new()
                };
                let label = if result.is_const { "LOAD" } else { "PASS" };
                let _ = writeln!(out, "{}{GREEN}  {}{RESET}  {}{}", indent, label, result.name, timing);
            }

            if self.mode == OutputMode::Verbose && !result.exports.is_empty() {
                for (k, v) in &result.exports {
                    let display = v.to_display_string();
                    let truncated = if display.len() > 60 {
                        format!("{}...", &display[..57])
                    } else {
                        display
                    };
                    let _ = writeln!(out, "{}        {DIM}+ {} = {}{RESET}", indent, k, truncated);
                }
            }
        } else {
            let path_hint = match source_file {
                Some(p) => format!("  {DIM}({}){RESET}", p),
                None => String::new(),
            };
            let _ = writeln!(out, "{}{RED}  FAIL{RESET}  {}{}", indent, result.name, path_hint);
            if let Some(ref ep) = result.endpoint {
                let _ = writeln!(out, "{}        {DIM}{}{RESET}", indent, ep);
            }
            for f in &result.failures {
                let _ = writeln!(out, "{}        {RED}{}{RESET}", indent, f.message);
            }
        }

        let show_logs = match self.mode {
            OutputMode::Verbose => true,
            OutputMode::Normal | OutputMode::Interactive => !result.failures.is_empty(),
            OutputMode::Quiet => false,
        };
        if show_logs && !result.logs.is_empty() {
            for log in &result.logs {
                let _ = writeln!(out, "{}        {CYAN}log: {}{RESET}", indent, log);
            }
        }
    }

    pub fn error(&self, msg: &str, depth: usize) {
        if self.mode == OutputMode::Interactive && self.live_display_active() {
            return;
        }
        let mut out = self.out.lock().unwrap();
        self.flush_headers(&mut out);
        let indent = "  ".repeat(depth);
        let _ = writeln!(out, "{}{RED}  ERROR {}{RESET}", indent, msg);
    }

    pub fn halted(&self, depth: usize) {
        let mut out = self.out.lock().unwrap();
        let indent = "  ".repeat(depth);
        let _ = writeln!(out, "{}{RED}  ** Halted on error **{RESET}", indent);
    }

    // --- Matrix display methods ---

    /// Initialize matrix display mode. Called by the runner when matrices are discovered.
    /// `entries` is a list of (label, test_count) for each combination.
    /// `groups` is a list of directory names (test groups).
    /// Enter matrix display mode. Returns `true` if this call initialized the
    /// display (and the caller owns the spinner lifecycle), `false` if the
    /// display was already initialized (e.g., pre-setup by cli.rs for --repeat).
    pub fn enter_matrix_mode(&self, entries: Vec<(String, usize)>, groups: Vec<String>) -> bool {
        if self.mode != OutputMode::Interactive {
            return false;
        }

        let mut state = self.matrix.lock().unwrap();
        if state.initialized {
            return false;
        }
        let mut out = self.out.lock().unwrap();

        let num_groups = groups.len();
        let num_iters = state.num_iters.max(1);
        state.label_width = entries.iter().map(|(l, _)| l.len()).max().unwrap_or(0).max(6) + 2;
        state.num_groups = num_groups;
        state.group_names = groups;

        for (label, total) in &entries {
            state.rows.push(MatrixRow {
                label: label.clone(),
                total: *total * num_iters,
                passed: 0,
                failed: 0,
                skipped: 0,
                iters: vec![vec![GroupProgress::Pending; num_groups]; num_iters],
            });
        }

        // header + rows + footer
        state.display_lines = entries.len() + 2;

        // Column width for each iter cell — max of the cell's visible width
        // (`[...groups...]`) and its header label width.
        let cell_visible = num_groups + 2; // brackets + one char per group
        let label_len = if num_iters > 1 {
            format!("Iter {}", num_iters).len()
        } else {
            "Progress".len()
        };
        let col_width = cell_visible.max(label_len);
        state.iter_col_width = col_width;

        // Print header — each cell left-aligned into col_width, joined by one space.
        let header_cells: String = if num_iters > 1 {
            (0..num_iters)
                .map(|i| format!("{:<cw$}", format!("Iter {}", i + 1), cw = col_width))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            format!("{:<cw$}", "Progress", cw = col_width)
        };
        let _ = writeln!(out, "{DIM}{:<lw$}  Tests  Pass  Fail  Skip  {}{RESET}",
            "Matrix", header_cells, lw = state.label_width);

        // Print initial rows
        for row in &state.rows {
            self.write_matrix_row(&mut out, row, &state);
        }

        // Footer
        let _ = writeln!(out, "");
        let _ = out.flush();

        state.initialized = true;
        true
    }

    /// Set the number of iteration columns before matrix mode is entered.
    /// Call this from cli.rs when `--repeat N` is used.
    pub fn set_repeat_iters(&self, n: usize) {
        let mut state = self.matrix.lock().unwrap();
        state.num_iters = n.max(1);
    }

    fn write_matrix_row(&self, out: &mut Box<dyn Write + Send>, row: &MatrixRow, state: &MatrixDisplayState) {
        let label_color = if row.failed > 0 { RED } else if row.passed == row.total && row.total > 0 { GREEN } else { "" };
        let reset = if !label_color.is_empty() { RESET } else { "" };

        let _ = write!(out, "{}{:<lw$}{}  {:>5}  {:>4}  {:>4}  {:>4}  ",
            label_color, row.label, reset,
            row.total, row.passed, row.failed, row.skipped,
            lw = state.label_width);

        // Render one bracketed cell per iteration column, each padded to
        // iter_col_width so it aligns with its header label above.
        let cell_visible = state.num_groups + 2;
        let pad = state.iter_col_width.saturating_sub(cell_visible);
        for (i, iter_groups) in row.iters.iter().enumerate() {
            if i > 0 { let _ = write!(out, " "); }
            let _ = write!(out, "[");
            for g in iter_groups {
                match g {
                    GroupProgress::Pending => { let _ = write!(out, "{DIM}·{RESET}"); }
                    GroupProgress::InProgress { has_failure, .. } => {
                        let ch = SPINNER_CHARS[state.spinner_frame % SPINNER_CHARS.len()];
                        if *has_failure {
                            let _ = write!(out, "{RED}{}{RESET}", ch);
                        } else {
                            let _ = write!(out, "{CYAN}{}{RESET}", ch);
                        }
                    }
                    GroupProgress::Done { all_passed: true } => { let _ = write!(out, "{GREEN}✓{RESET}"); }
                    GroupProgress::Done { all_passed: false } => { let _ = write!(out, "{RED}✗{RESET}"); }
                }
            }
            let _ = write!(out, "]");
            if pad > 0 { let _ = write!(out, "{}", " ".repeat(pad)); }
        }
        let _ = writeln!(out);
    }

    /// Start the spinner refresh thread. Returns a join handle.
    pub fn start_matrix_spinner(self: &Arc<Self>) -> std::thread::JoinHandle<()> {
        let printer = Arc::clone(self);
        std::thread::spawn(move || {
            while !printer.matrix_stop.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(120));
                if printer.matrix_stop.load(Ordering::Relaxed) {
                    break;
                }

                let state = printer.matrix.lock().unwrap();
                if !state.initialized {
                    continue;
                }
                // Check if any group is still in progress
                let has_active = state.rows.iter().any(|r|
                    r.iters.iter().any(|it|
                        it.iter().any(|g| matches!(g, GroupProgress::InProgress { .. }))
                    )
                );
                if !has_active {
                    continue;
                }
                drop(state);

                printer.redraw_matrix();
            }
        })
    }

    /// Stop the spinner thread.
    pub fn stop_matrix_spinner(&self) {
        self.matrix_stop.store(true, Ordering::Relaxed);
    }

    /// Force a final redraw of the matrix display. Call this after
    /// `stop_matrix_spinner` to flush any pending row updates that the
    /// spinner thread may not have picked up.
    pub fn finalize_matrix(&self) {
        if self.mode != OutputMode::Interactive { return; }
        self.redraw_matrix();
    }

    /// Redraw all matrix rows (called by spinner thread).
    fn redraw_matrix(&self) {
        let mut state = self.matrix.lock().unwrap();
        if !state.initialized { return; }

        state.spinner_frame += 1;
        let mut out = self.out.lock().unwrap();

        // Move up to first row (skip footer)
        let rows = state.rows.len();
        let up = rows + 1; // rows + footer
        let _ = write!(out, "\x1b[{}A\r", up);

        for row in &state.rows {
            let _ = write!(out, "\x1b[2K");
            self.write_matrix_row(&mut out, row, &state);
        }

        // Redraw footer
        let _ = write!(out, "\x1b[2K");
        let _ = writeln!(out, "");
        let _ = out.flush();
    }

    /// Update matrix display when a test group starts for a combination.
    pub fn matrix_group_start(&self, combo_label: &str, iter_index: usize, group_index: usize, test_count: usize) {
        if self.mode != OutputMode::Interactive { return; }

        let mut state = self.matrix.lock().unwrap();
        if !state.initialized { return; }

        if let Some(row) = state.rows.iter_mut().find(|r| r.label == combo_label) {
            if let Some(groups) = row.iters.get_mut(iter_index) {
                if group_index < groups.len() {
                    groups[group_index] = GroupProgress::InProgress {
                        completed: 0,
                        total: test_count,
                        has_failure: false,
                    };
                }
            }
        }
    }

    /// Look up the group index for a given group key (rel_path of the test file).
    pub fn matrix_group_index(&self, key: &str) -> Option<usize> {
        let state = self.matrix.lock().unwrap();
        state.group_names.iter().position(|n| n == key)
    }

    /// Update matrix display when a test completes within a group.
    /// Handles Pending → Done directly when each "group" is a single test
    /// (the new plan-driven runner doesn't pre-call matrix_group_start).
    pub fn matrix_test_complete(&self, combo_label: &str, iter_index: usize, group_index: usize, passed: bool) {
        if self.mode != OutputMode::Interactive { return; }

        let mut state = self.matrix.lock().unwrap();
        if !state.initialized { return; }

        if let Some(row) = state.rows.iter_mut().find(|r| r.label == combo_label) {
            if passed {
                row.passed += 1;
            } else {
                row.failed += 1;
            }

            if let Some(groups) = row.iters.get_mut(iter_index) {
                if group_index < groups.len() {
                    match &mut groups[group_index] {
                        GroupProgress::Pending => {
                            groups[group_index] = GroupProgress::Done { all_passed: passed };
                        }
                        GroupProgress::InProgress { completed, total, has_failure } => {
                            *completed += 1;
                            if !passed { *has_failure = true; }
                            let has_fail = *has_failure;
                            if *completed >= *total {
                                groups[group_index] = GroupProgress::Done { all_passed: !has_fail };
                            }
                        }
                        GroupProgress::Done { .. } => {}
                    }
                }
            }
        }
    }

    /// Update matrix display when a test is skipped.
    pub fn matrix_test_skip(&self, combo_label: &str, iter_index: usize, group_index: usize) {
        if self.mode != OutputMode::Interactive { return; }

        let mut state = self.matrix.lock().unwrap();
        if !state.initialized { return; }

        if let Some(row) = state.rows.iter_mut().find(|r| r.label == combo_label) {
            row.skipped += 1;

            if let Some(groups) = row.iters.get_mut(iter_index) {
                if group_index < groups.len() {
                    match &mut groups[group_index] {
                        GroupProgress::Pending => {
                            groups[group_index] = GroupProgress::Done { all_passed: true };
                        }
                        GroupProgress::InProgress { completed, total, has_failure } => {
                            *completed += 1;
                            let is_done = *completed >= *total;
                            let all_ok = !*has_failure;
                            if is_done {
                                groups[group_index] = GroupProgress::Done { all_passed: all_ok };
                            }
                        }
                        GroupProgress::Done { .. } => {}
                    }
                }
            }
        }
    }

    /// Mark a group as done for a combination (when it had no tests to run).
    pub fn matrix_group_done(&self, combo_label: &str, iter_index: usize, group_index: usize) {
        if self.mode != OutputMode::Interactive { return; }

        let mut state = self.matrix.lock().unwrap();
        if !state.initialized { return; }

        if let Some(row) = state.rows.iter_mut().find(|r| r.label == combo_label) {
            if let Some(groups) = row.iters.get_mut(iter_index) {
                if group_index < groups.len() {
                    if matches!(groups[group_index], GroupProgress::Pending) {
                        groups[group_index] = GroupProgress::Done { all_passed: true };
                    }
                }
            }
        }
    }

    pub fn summary(&self, _total: usize, _passed: usize, _failed: usize, _skipped: usize, parse_errors: usize) {
        let mut out = self.out.lock().unwrap();

        if self.mode == OutputMode::Interactive {
            // Move past display area
            let _ = writeln!(out);
        }

        let stats = self.tld_stats.lock().unwrap();
        let mut rows: Vec<(String, TldStats)> = stats.iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut total = TldStats::default();
        for (_, s) in &rows {
            total.passed += s.passed;
            total.failed += s.failed;
            total.skipped += s.skipped;
            total.elapsed += s.elapsed;
        }

        // Column widths
        let name_w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0)
            .max("Suite".len())
            .max("TOTAL".len());
        let pass_w = rows.iter().map(|(_, s)| digits(s.passed)).max().unwrap_or(1)
            .max(digits(total.passed)).max("Pass".len());
        let fail_w = rows.iter().map(|(_, s)| digits(s.failed)).max().unwrap_or(1)
            .max(digits(total.failed)).max("Fail".len());
        let skip_w = rows.iter().map(|(_, s)| digits(s.skipped)).max().unwrap_or(1)
            .max(digits(total.skipped)).max("Skip".len());
        let total_col = total.passed + total.failed + total.skipped;
        let tot_w = rows.iter().map(|(_, s)| digits(s.passed + s.failed + s.skipped)).max().unwrap_or(1)
            .max(digits(total_col)).max("Total".len());
        let time_strs: Vec<String> = rows.iter().map(|(_, s)| format_seconds(s.elapsed)).collect();
        let total_time = format_seconds(total.elapsed);
        let time_w = time_strs.iter().map(|s| s.len()).max().unwrap_or(0)
            .max(total_time.len()).max("Time".len());

        let _ = writeln!(out);
        let _ = writeln!(out, "{:<nw$}  {:>pw$}  {:>fw$}  {:>sw$}  {:>tw$}  {:>mw$}",
            "Suite", "Pass", "Fail", "Skip", "Total", "Time",
            nw = name_w, pw = pass_w, fw = fail_w, sw = skip_w, tw = tot_w, mw = time_w);
        let sep = format!("{}  {}  {}  {}  {}  {}",
            "-".repeat(name_w), "-".repeat(pass_w), "-".repeat(fail_w),
            "-".repeat(skip_w), "-".repeat(tot_w), "-".repeat(time_w));
        let _ = writeln!(out, "{}", sep);

        for ((name, s), tstr) in rows.iter().zip(time_strs.iter()) {
            let row_total = s.passed + s.failed + s.skipped;
            let pass_str = colored_count(s.passed, GREEN);
            let fail_str = colored_count(s.failed, RED);
            let skip_str = colored_count(s.skipped, YELLOW);
            let _ = writeln!(out, "{:<nw$}  {}  {}  {}  {:>tw$}  {:>mw$}",
                name,
                right_pad_colored(&pass_str, s.passed, pass_w),
                right_pad_colored(&fail_str, s.failed, fail_w),
                right_pad_colored(&skip_str, s.skipped, skip_w),
                row_total, tstr,
                nw = name_w, tw = tot_w, mw = time_w);
        }

        let _ = writeln!(out, "{}", sep);
        let total_pass = colored_count(total.passed, GREEN);
        let total_fail = colored_count(total.failed, RED);
        let total_skip = colored_count(total.skipped, YELLOW);
        let _ = writeln!(out, "{:<nw$}  {}  {}  {}  {:>tw$}  {:>mw$}",
            "TOTAL",
            right_pad_colored(&total_pass, total.passed, pass_w),
            right_pad_colored(&total_fail, total.failed, fail_w),
            right_pad_colored(&total_skip, total.skipped, skip_w),
            total_col, total_time,
            nw = name_w, tw = tot_w, mw = time_w);

        // Wall-clock line: the "Time" column above is summed work-time (so it
        // reads the same parallel or serial); this shows actual elapsed and,
        // when meaningfully shorter, the parallel speedup.
        if let Some(wall) = *self.wall_clock.lock().unwrap() {
            let work = total.elapsed.as_secs_f64();
            let wall_s = wall.as_secs_f64();
            let speedup = if wall_s > 0.0 { work / wall_s } else { 1.0 };
            if speedup >= 1.5 {
                let _ = writeln!(out, "{DIM}wall-clock: {} ({:.1}x parallel speedup over {} of work){RESET}",
                    format_seconds(wall), speedup, format_seconds(total.elapsed));
            } else {
                let _ = writeln!(out, "{DIM}wall-clock: {}{RESET}", format_seconds(wall));
            }
        }

        if parse_errors > 0 {
            let _ = writeln!(out, "\n{YELLOW}{} parse error(s){RESET}", parse_errors);
        }
    }
}

/// Render a single slot row's content (without cursor positioning or trailing
/// newline). Used by both the live redraw path (`draw_slot`) and the initial
/// render in `register_directories`.
///
/// 1:1 mode (count <= bar_width): per-test glyphs `✓ ✗ - ·` plus dim `.`
/// trailing fill so brackets align across rows of varying counts.
///
/// Bucketed mode (count > bar_width): each bar character represents
/// ceil(count / bar_width) tests; color reflects that bucket's most severe
/// outcome (any fail → red, else any skip → yellow, else green). Buckets
/// with no completed tests render as a dim shaded block. After the bar, a
/// trailing "  N/T" counter shows aggregate progress.
fn write_slot_row(out: &mut Box<dyn Write + Send>, info: &SlotDrawInfo) {
    let path = match info.path.as_deref() {
        Some(p) => p,
        None => {
            let _ = write!(out, "{DIM}  (idle){RESET}");
            return;
        }
    };

    let _ = write!(out, "{:<width$} [", path, width = info.col_width);

    let use_glyphs = matches!(info.bar_style, BarStyle::Auto) && info.count <= info.bar_width;
    if use_glyphs {
        // 1:1 — per-test glyphs. No trailing alignment pad: a complete
        // dir's bar should read as full, not 70%-full with a dim tail.
        for ind in &info.indicators {
            match ind {
                Indicator::Pending => { let _ = write!(out, "{DIM}·{RESET}"); }
                Indicator::Pass => { let _ = write!(out, "{GREEN}✓{RESET}"); }
                Indicator::Fail => { let _ = write!(out, "{RED}✗{RESET}"); }
                Indicator::Skip => { let _ = write!(out, "{YELLOW}-{RESET}"); }
                Indicator::Disabled => { let _ = write!(out, "{CYAN}▢{RESET}"); }
            }
        }
    } else {
        // Block-bar mode — always renders at the full `bar_width`.
        // Two sub-modes share the same per-char span math:
        //   span = [floor(ch*count/bar_w), floor((ch+1)*count/bar_w))
        //
        // count >= bar_w  → bucketed: span has >=1 tests, bucket_color
        //   paints the span the color of its most severe outcome.
        // count <  bar_w  → stretched: each test occupies multiple
        //   consecutive chars (span has 0 or 1 tests). All chars within
        //   one test's span share that test's outcome color, so a 7-test
        //   row paints across the full width with ~13 chars per test.
        let bar_w = info.bar_width.max(1);
        let count = info.count;
        if count >= bar_w {
            for ch in 0..bar_w {
                let start = (ch * count) / bar_w;
                let end = if ch + 1 == bar_w {
                    count
                } else {
                    ((ch + 1) * count) / bar_w
                };
                if start >= end {
                    let _ = write!(out, "{DIM}░{RESET}");
                    continue;
                }
                let bucket = &info.indicators[start..end];
                let mut pass = 0usize;
                let mut fail = 0usize;
                let mut skip = 0usize;
                let mut pending = 0usize;
                for ind in bucket {
                    match ind {
                        Indicator::Pass => pass += 1,
                        Indicator::Fail => fail += 1,
                        // Disabled folds into the skip bucket for hue blending.
                        Indicator::Skip | Indicator::Disabled => skip += 1,
                        Indicator::Pending => pending += 1,
                    }
                }
                if pending == bucket.len() {
                    let _ = write!(out, "{DIM}░{RESET}");
                } else {
                    let color = bucket_color(pass, fail, skip);
                    let _ = write!(out, "{}█{RESET}", color);
                }
            }
        } else {
            // Stretched: char ch shows test_idx = floor(ch*count/bar_w).
            for ch in 0..bar_w {
                let test_idx = (ch * count) / bar_w;
                let ind = info.indicators.get(test_idx).copied().unwrap_or(Indicator::Pending);
                match ind {
                    Indicator::Pending => { let _ = write!(out, "{DIM}░{RESET}"); }
                    Indicator::Pass => { let _ = write!(out, "{GREEN}█{RESET}"); }
                    Indicator::Fail => { let _ = write!(out, "{RED}█{RESET}"); }
                    Indicator::Skip => { let _ = write!(out, "{YELLOW}█{RESET}"); }
                    Indicator::Disabled => { let _ = write!(out, "{CYAN}█{RESET}"); }
                }
            }
        }
    }

    // Always-on counter shows aggregate progress regardless of mode.
    let completed: usize = info.indicators.iter()
        .filter(|i| !matches!(i, Indicator::Pending))
        .count();
    let _ = write!(out, "] {DIM}{}/{}{RESET}", completed, info.count);
    // Failures are routed to the dedicated errors panel below the
    // footer — keeping them off the row prevents the line-wrap that
    // corrupts the cursor-driven redraw.
}

/// Format one entry for the errors panel, sized to fit `max_width`
/// visible characters. Layout is `<dir>/<test>: <message>`, with the
/// test/path part dim and the error message in red.
fn format_panel_entry(dir_path: &str, test_name: &str, msg: &str, max_width: usize) -> String {
    // When the slot IS the test (leaf "one row per test" view), the slot key
    // equals the test name — don't repeat it as a `<name>/<name>` prefix.
    let raw = if dir_path.is_empty() || dir_path == test_name {
        format!("{}: {}", test_name, msg)
    } else {
        format!("{}/{}: {}", dir_path, test_name, msg)
    };
    let truncated = truncate_chars(&raw, max_width);
    if let Some(pos) = truncated.find(": ") {
        let (label, rest) = truncated.split_at(pos + 2);
        format!("{DIM}{}{RESET}{RED}{}{RESET}", label, rest)
    } else {
        format!("{RED}{}{RESET}", truncated)
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut count = 0;
    for c in s.chars() {
        if count + 1 >= max {
            break;
        }
        out.push(c);
        count += 1;
    }
    out.push('…');
    out
}

/// ANSI color for a bucket: the color of its most severe outcome. Any
/// failure → red; else any skip (disabled folds into skip) → yellow; else
/// all-pass → green. This matches the at-a-glance reading of the per-test
/// glyph bars instead of blending outcomes into an in-between hue. An
/// all-pending bucket renders dim, though the caller guards that case.
fn bucket_color(pass: usize, fail: usize, skip: usize) -> &'static str {
    if fail > 0 {
        RED
    } else if skip > 0 {
        YELLOW
    } else if pass > 0 {
        GREEN
    } else {
        DIM
    }
}

fn digits(n: usize) -> usize {
    if n == 0 { 1 } else { (n as f64).log10() as usize + 1 }
}

fn format_seconds(d: std::time::Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

fn colored_count(n: usize, color: &str) -> String {
    if n > 0 { format!("{}{}{}", color, n, RESET) } else { format!("{}", n) }
}

/// Right-align a colored string in a column of `width`, ignoring ANSI color codes
/// in the width calculation.
fn right_pad_colored(s: &str, n: usize, width: usize) -> String {
    let visible = digits(n);
    let pad = width.saturating_sub(visible);
    format!("{}{}", " ".repeat(pad), s)
}

/// Extract the top-level directory from a relative source path.
/// "accounts/tests/01-list-expand.test.tstr" → "accounts"
/// "01-list-expand.test.tstr" → "(root)"
fn tld_of(source_file: &str) -> String {
    match source_file.find('/') {
        Some(idx) => source_file[..idx].to_string(),
        None => "(root)".to_string(),
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

const VALUE_MAX: usize = 60;

fn truncate_value(v: &crate::value::Value) -> String {
    let s = v.to_display_string();
    if s.len() > VALUE_MAX {
        format!("{}...", &s[..VALUE_MAX - 3])
    } else {
        s
    }
}

/// Write a per-test variable table: 3 columns (source, name, value), with sections
/// for `_in` and `_out`. Indented under the test entry.
fn write_var_table(
    f: &mut Box<dyn Write + Send>,
    inputs: &[(String, Option<String>, crate::value::Value)],
    outputs: &HashMap<String, crate::value::Value>,
) {
    if inputs.is_empty() && outputs.is_empty() {
        return;
    }
    // Compute column widths across both sections
    let in_src_w = inputs.iter()
        .map(|(_, src, _)| src.as_deref().unwrap_or("?").len())
        .max().unwrap_or(0);
    let out_src_w = if outputs.is_empty() { 0 } else { "self".len() };
    let src_w = in_src_w.max(out_src_w).max("source".len());

    let in_name_w = inputs.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
    let out_name_w = outputs.keys().map(|k| k.len()).max().unwrap_or(0);
    let name_w = in_name_w.max(out_name_w).max("variable".len());

    let _ = writeln!(f, "      {:<sw$}  {:<nw$}  value",
        "source", "variable", sw = src_w, nw = name_w);
    let _ = writeln!(f, "      {}  {}  {}",
        "-".repeat(src_w), "-".repeat(name_w), "-".repeat(5));

    if !inputs.is_empty() {
        let _ = writeln!(f, "      [_in]");
        for (name, src, value) in inputs {
            let src_str = src.as_deref().unwrap_or("?");
            let _ = writeln!(f, "      {:<sw$}  {:<nw$}  {}",
                src_str, name, truncate_value(value), sw = src_w, nw = name_w);
        }
    }

    if !outputs.is_empty() {
        let _ = writeln!(f, "      [_out]");
        let mut keys: Vec<&String> = outputs.keys().collect();
        keys.sort();
        for k in keys {
            let _ = writeln!(f, "      {:<sw$}  {:<nw$}  {}",
                "self", k, truncate_value(&outputs[k]), sw = src_w, nw = name_w);
        }
    }
}

/// Write the end-of-run variable summary table.
fn write_summary_table(
    f: &mut Box<dyn Write + Send>,
    vars: &[(String, Option<String>, crate::value::Value)],
) {
    let src_w = vars.iter()
        .map(|(_, src, _)| src.as_deref().unwrap_or("?").len())
        .max().unwrap_or(0)
        .max("source".len());
    let name_w = vars.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0).max("variable".len());

    let _ = writeln!(f, "  {:<sw$}  {:<nw$}  value",
        "source", "variable", sw = src_w, nw = name_w);
    let _ = writeln!(f, "  {}  {}  {}",
        "-".repeat(src_w), "-".repeat(name_w), "-".repeat(5));
    for (name, src, value) in vars {
        let src_str = src.as_deref().unwrap_or("?");
        let _ = writeln!(f, "  {:<sw$}  {:<nw$}  {}",
            src_str, name, truncate_value(value), sw = src_w, nw = name_w);
    }
}
