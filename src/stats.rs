//! Per-leaf run-duration statistics — `.tstr-stats.json` at the suite root.
//!
//! Every clean leaf run (no failures, no anomalous skips) records its
//! wall-clock duration. The runner sorts sibling subtrees longest-first from
//! these numbers so slow leaves start early and their waits overlap the rest
//! of the suite. The file is machine-local (timings are environment-specific)
//! and self-healing: missing or corrupt files just start fresh, and a leaf's
//! numbers converge again within a few runs of any change.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const STATS_FILE: &str = ".tstr-stats.json";

/// Timing record for one leaf directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeafStat {
    /// Duration of the most recent clean run, in milliseconds.
    pub last_ms: u64,
    /// Exponentially weighted moving average (see `fold`) — the number the
    /// scheduler sorts by. A plain mean never forgets, so a leaf fixed from
    /// 44s to 2s would read "slow" for dozens of runs; the EWMA converges in
    /// a handful.
    pub avg_ms: u64,
    /// Clean runs recorded (informational — the EWMA doesn't use it).
    pub runs: u64,
}

impl LeafStat {
    fn new(ms: u64) -> Self {
        LeafStat { last_ms: ms, avg_ms: ms, runs: 1 }
    }

    /// Fold one new sample into the record: avg ← 0.7·avg + 0.3·sample.
    fn fold(&mut self, ms: u64) {
        self.last_ms = ms;
        self.avg_ms = (self.avg_ms * 7 + ms * 3) / 10;
        self.runs += 1;
    }
}

/// The suite's stats ledger. Loaded once per run, shared across the rayon
/// workers (leaves record concurrently), saved once at the end.
pub struct StatsBook {
    path: PathBuf,
    book: Mutex<HashMap<String, LeafStat>>,
    /// Set on first record — an untouched book (e.g. `--skip`-heavy or failing
    /// run touching no leaf cleanly) never rewrites the file.
    dirty: std::sync::atomic::AtomicBool,
}

impl StatsBook {
    /// Load the book from `<root>/.tstr-stats.json`. A missing or unreadable
    /// file yields an empty book — stats rebuild themselves over the next runs.
    pub fn load(root: &Path) -> StatsBook {
        let path = root.join(STATS_FILE);
        let book = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        StatsBook {
            path,
            book: Mutex::new(book),
            dirty: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Record one clean leaf run. `key` is the leaf dir's path relative to the
    /// suite root (`"."` for a root-level leaf).
    pub fn record(&self, key: &str, elapsed_ms: u64) {
        let mut book = self.book.lock().unwrap();
        match book.get_mut(key) {
            Some(stat) => stat.fold(elapsed_ms),
            None => {
                book.insert(key.to_string(), LeafStat::new(elapsed_ms));
            }
        }
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Expected duration for a leaf (the EWMA), or None if never recorded.
    pub fn expected_ms(&self, key: &str) -> Option<u64> {
        self.book.lock().unwrap().get(key).map(|s| s.avg_ms)
    }

    /// All records, slowest first (avg desc, then path for determinism) —
    /// the `tstr stats` listing.
    pub fn entries_slowest_first(&self) -> Vec<(String, LeafStat)> {
        let mut entries: Vec<(String, LeafStat)> = self.book.lock().unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| b.1.avg_ms.cmp(&a.1.avg_ms).then_with(|| a.0.cmp(&b.0)));
        entries
    }

    /// Write the book back if anything was recorded. Temp-file + rename so an
    /// interrupt can't leave a half-written file behind. Keys are serialized
    /// sorted (BTreeMap) for stable, diffable output.
    pub fn save(&self) -> std::io::Result<()> {
        if !self.dirty.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let sorted: BTreeMap<String, LeafStat> = self.book.lock().unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let json = serde_json::to_string_pretty(&sorted)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// Human-form duration for skip reasons and the stats table:
/// `750ms`, `44.1s`, `2m 5s`.
pub fn fmt_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_record_sets_avg_to_sample() {
        let dir = tempfile::tempdir().unwrap();
        let book = StatsBook::load(dir.path());
        book.record("api/groups", 44_000);
        assert_eq!(book.expected_ms("api/groups"), Some(44_000));
    }

    #[test]
    fn ewma_converges_toward_new_speed() {
        let dir = tempfile::tempdir().unwrap();
        let book = StatsBook::load(dir.path());
        book.record("a", 44_000);
        // Leaf gets fixed: now runs in 2s. EWMA should drop below 10s within
        // a handful of runs — a plain mean would still read ~23s after one.
        for _ in 0..5 {
            book.record("a", 2_000);
        }
        let avg = book.expected_ms("a").unwrap();
        assert!(avg < 10_000, "EWMA should have converged, got {}ms", avg);
        assert!(avg >= 2_000, "EWMA can't undershoot the samples, got {}ms", avg);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let book = StatsBook::load(dir.path());
        book.record("a/b", 1_500);
        book.record("c", 300);
        book.save().unwrap();

        let reloaded = StatsBook::load(dir.path());
        assert_eq!(reloaded.expected_ms("a/b"), Some(1_500));
        assert_eq!(reloaded.expected_ms("c"), Some(300));
        assert_eq!(reloaded.expected_ms("nope"), None);
    }

    #[test]
    fn untouched_book_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let book = StatsBook::load(dir.path());
        book.save().unwrap();
        assert!(!dir.path().join(STATS_FILE).exists());
    }

    #[test]
    fn corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATS_FILE), "{not json").unwrap();
        let book = StatsBook::load(dir.path());
        assert_eq!(book.expected_ms("a"), None);
        // And a save after recording replaces the corrupt file cleanly.
        book.record("a", 100);
        book.save().unwrap();
        let reloaded = StatsBook::load(dir.path());
        assert_eq!(reloaded.expected_ms("a"), Some(100));
    }
}
