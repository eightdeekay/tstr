//! tstr configuration file (`tstr.yaml`) loading.
//!
//! Loading order (see README.md "Configuration"). Summary:
//! - `~/.config/tstr/config.yaml` — user global
//! - `<suite-root>/tstr.yaml` — project local (presence marks suite root)
//! - `--config <path>` — CLI override
//! - Loaded in order; later overrides earlier. Repeatable lists append, scalars replace.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level config. Both sections are optional; an empty file (or no file)
/// yields `Config::default()`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub constants: HashMap<String, serde_yaml::Value>,
    /// How many per-run log files to keep under `<root>/logs/` (oldest pruned
    /// after each run). Defaults to 10; `0` disables pruning (keep everything).
    #[serde(default)]
    pub log_retention: Option<usize>,
}

/// Default number of per-run log files kept under `<root>/logs/`.
pub const DEFAULT_LOG_RETENTION: usize = 10;

/// CLI flag defaults. Any flag tstr accepts may be defaulted here.
/// Only fields needed by Slice 1 are present; future slices add more.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub import: Vec<PathBuf>,
    #[serde(default)]
    pub display: Option<String>,
}

impl Config {
    /// Parse a config from a yaml file on disk.
    pub fn load_from_path(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
        serde_yaml::from_str(&content)
            .map_err(|e| format!("failed to parse {}: {}", path.display(), e))
    }

    /// Layered load: user global → project → cli override. Later overrides earlier.
    /// Missing files are silently skipped (only `--config` errors if its path is bad).
    pub fn load_layered(
        suite_root: Option<&Path>,
        cli_override: Option<&Path>,
    ) -> Result<Self, String> {
        let mut config = Config::default();

        // Seed ALL_CAPS env vars as the lowest-priority constants layer, so
        // `${ORG_TOKEN}` resolves from the environment. Yaml constants merge
        // on top and win on collision. The caps restriction keeps env vars
        // from clobbering camelCase yaml constants (and vice versa).
        config.constants = env_constants();

        if let Some(home) = std::env::var_os("HOME") {
            let user_path = PathBuf::from(home).join(".config/tstr/config.yaml");
            if user_path.is_file() {
                config.merge(Config::load_from_path(&user_path)?);
            }
        }

        if let Some(root) = suite_root {
            let project_path = root.join("tstr.yaml");
            if project_path.is_file() {
                config.merge(Config::load_from_path(&project_path)?);
            }
        }

        if let Some(cli_path) = cli_override {
            // --config errors loudly if its path is bad — user asked for it explicitly.
            config.merge(Config::load_from_path(cli_path)?);
        }

        // Post-merge: resolve `${name}` references inside constant string values
        // against the merged constants table (env + all yaml layers).
        resolve_constant_refs(&mut config.constants)?;

        Ok(config)
    }

    /// Merge `other` into `self`. Scalar fields: `other` wins when present.
    /// List fields: `other` appends to `self`. Constants: per-key, `other` wins.
    fn merge(&mut self, other: Config) {
        self.defaults.import.extend(other.defaults.import);
        if other.defaults.display.is_some() {
            self.defaults.display = other.defaults.display;
        }
        if other.log_retention.is_some() {
            self.log_retention = other.log_retention;
        }
        for (k, v) in other.constants {
            self.constants.insert(k, v);
        }
    }

    /// Per-run log files to keep under `<root>/logs/`. `0` means keep all.
    pub fn log_retention(&self) -> usize {
        self.log_retention.unwrap_or(DEFAULT_LOG_RETENTION)
    }
}

/// Build a constants map from ALL_CAPS process environment variables.
/// Only names matching `[A-Z][A-Z0-9_]*` are included — this keeps env vars
/// (conventionally UPPER_SNAKE) from colliding with camelCase yaml constants.
fn env_constants() -> std::collections::HashMap<String, serde_yaml::Value> {
    env_constants_from(std::env::vars())
}

/// Testable core: filter an iterator of (name, value) pairs to ALL_CAPS names.
fn env_constants_from<I>(vars: I) -> std::collections::HashMap<String, serde_yaml::Value>
where
    I: Iterator<Item = (String, String)>,
{
    vars.filter(|(k, _)| is_env_var_name(k))
        .map(|(k, v)| (k, serde_yaml::Value::String(v)))
        .collect()
}

/// True if `name` looks like a conventional env var: an uppercase letter
/// followed by uppercase letters, digits, or underscores.
fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Resolve `${name}` references inside string values of the constants table
/// by looking up `name` (or `name.sub.key` for nested objects) in the table
/// itself. Iterates until no more substitutions happen; errors on cycle
/// (didn't converge) or unresolved reference (no such constant by load end).
fn resolve_constant_refs(
    constants: &mut std::collections::HashMap<String, serde_yaml::Value>,
) -> Result<(), String> {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    const MAX_ITERS: usize = 100;

    for _ in 0..MAX_ITERS {
        let snapshot = constants.clone();
        let mut changed = false;
        for value in constants.values_mut() {
            walk_substitute(value, &snapshot, &re, &mut changed);
        }
        if !changed {
            // Converged. Verify nothing's left unresolved.
            let mut unresolved = Vec::new();
            for (key, value) in constants.iter() {
                find_unresolved(key, value, &re, &mut unresolved);
            }
            if unresolved.is_empty() {
                return Ok(());
            }
            return Err(format!(
                "unresolved constant reference(s) in tstr.yaml: {}",
                unresolved.join(", "),
            ));
        }
    }
    Err(format!(
        "constant references did not converge after {} iterations \
         (likely a cycle — e.g., `a: ${{b}}` and `b: ${{a}}`)",
        MAX_ITERS,
    ))
}

/// Recursively walk a yaml value, substituting `${name}` references in any
/// string leaves against the given snapshot of the constants table.
fn walk_substitute(
    value: &mut serde_yaml::Value,
    table: &std::collections::HashMap<String, serde_yaml::Value>,
    re: &regex::Regex,
    changed: &mut bool,
) {
    use serde_yaml::Value;
    match value {
        Value::String(s) => {
            if let Some(new_s) = substitute_one(s, table, re) {
                *s = new_s;
                *changed = true;
            }
        }
        Value::Sequence(seq) => {
            for v in seq {
                walk_substitute(v, table, re, changed);
            }
        }
        Value::Mapping(map) => {
            for (_, v) in map {
                walk_substitute(v, table, re, changed);
            }
        }
        _ => {} // numbers, bools, null — no string content to walk
    }
}

/// Substitute all resolvable `${name}` patterns in `s` against the table.
/// Returns `Some(new_string)` if anything was substituted; `None` if no
/// patterns matched anything (so callers can skip the assignment). Unresolved
/// patterns are left in place for the next iteration to handle.
fn substitute_one(
    s: &str,
    table: &std::collections::HashMap<String, serde_yaml::Value>,
    re: &regex::Regex,
) -> Option<String> {
    if !re.is_match(s) {
        return None;
    }
    let mut result = String::with_capacity(s.len());
    let mut cursor = 0;
    let mut any = false;
    for caps in re.captures_iter(s) {
        let m = caps.get(0).unwrap();
        let name = caps.get(1).unwrap().as_str();
        if let Some(replacement) = lookup_dotted(name, table) {
            result.push_str(&s[cursor..m.start()]);
            result.push_str(&replacement);
            cursor = m.end();
            any = true;
        }
        // Else: leave ${name} in place; copy of it will happen below.
    }
    if any {
        result.push_str(&s[cursor..]);
        Some(result)
    } else {
        None
    }
}

/// Look up a (possibly dotted) name in the constants table and stringify
/// the result. Returns `None` if the path doesn't resolve or the leaf isn't
/// a scalar (objects and lists can't be substituted into a string).
fn lookup_dotted(
    path: &str,
    table: &std::collections::HashMap<String, serde_yaml::Value>,
) -> Option<String> {
    use serde_yaml::Value;
    let mut parts = path.split('.');
    let head = parts.next()?;
    let mut current = table.get(head)?.clone();
    for part in parts {
        match current {
            Value::Mapping(map) => {
                let key = Value::String(part.to_string());
                current = map.get(&key)?.clone();
            }
            _ => return None,
        }
    }
    match current {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Collect leftover `${name}` patterns after substitution converged, so we
/// can report exactly which references failed to resolve.
fn find_unresolved(
    path: &str,
    value: &serde_yaml::Value,
    re: &regex::Regex,
    unresolved: &mut Vec<String>,
) {
    use serde_yaml::Value;
    match value {
        Value::String(s) => {
            for caps in re.captures_iter(s) {
                let name = caps.get(1).unwrap().as_str();
                unresolved.push(format!("${{{}}} (at {})", name, path));
            }
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(key_str) = k.as_str() {
                    find_unresolved(&format!("{}.{}", path, key_str), v, re, unresolved);
                }
            }
        }
        Value::Sequence(seq) => {
            for (i, v) in seq.iter().enumerate() {
                find_unresolved(&format!("{}[{}]", path, i), v, re, unresolved);
            }
        }
        _ => {}
    }
}

/// Walk up from `start` looking for `tstr.yaml`. Returns the directory containing
/// it (the suite root), or `None` if not found anywhere up to the filesystem root.
pub fn find_suite_root_by_config(start: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut current = start.as_path();
    loop {
        if current.join("tstr.yaml").is_file() {
            return Some(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_empty_yaml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tstr.yaml");
        fs::write(&path, "").unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert!(cfg.defaults.import.is_empty());
        assert!(cfg.constants.is_empty());
    }

    #[test]
    fn load_full_yaml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tstr.yaml");
        fs::write(&path, r#"
defaults:
  import:
    - /opt/libs
    - ~/.tstr/libs
  display: bars

constants:
  apiVersion: v4
  orgService:
    baseUrl: https://api.example.com
"#).unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.defaults.import.len(), 2);
        assert_eq!(cfg.defaults.display.as_deref(), Some("bars"));
        assert_eq!(cfg.constants.len(), 2);
        assert!(cfg.constants.contains_key("apiVersion"));
        assert!(cfg.constants.contains_key("orgService"));
    }

    #[test]
    fn find_suite_root_finds_yaml_at_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("tstr.yaml"), "").unwrap();
        let sub = root.join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        let found = find_suite_root_by_config(&sub).unwrap();
        assert_eq!(
            fs::canonicalize(&found).unwrap(),
            fs::canonicalize(root).unwrap(),
        );
    }

    #[test]
    fn find_suite_root_none_when_no_yaml() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("a/b");
        fs::create_dir_all(&sub).unwrap();
        // No tstr.yaml anywhere in tmp; walk-up will reach FS root without finding one.
        // Will likely return None unless your /tmp ancestors have a tstr.yaml (unlikely).
        let found = find_suite_root_by_config(&sub);
        assert!(found.is_none() || !found.unwrap().starts_with(tmp.path()));
    }

    // --- constant interpolation ---

    fn cfg_from_yaml(s: &str) -> Result<Config, String> {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tstr.yaml");
        fs::write(&path, s).unwrap();
        // load_layered exercises the post-merge resolver; cfg_from_yaml just
        // wraps load_from_path + manual resolve so tests are self-contained.
        let mut cfg = Config::load_from_path(&path)?;
        resolve_constant_refs(&mut cfg.constants)?;
        Ok(cfg)
    }

    fn const_str(cfg: &Config, key: &str) -> String {
        match cfg.constants.get(key).unwrap() {
            serde_yaml::Value::String(s) => s.clone(),
            other => panic!("expected string at {}, got {:?}", key, other),
        }
    }

    #[test]
    fn interp_simple_substitution() {
        let cfg = cfg_from_yaml("
constants:
  greeting: hello
  msg: \"${greeting} world\"
").unwrap();
        assert_eq!(const_str(&cfg, "msg"), "hello world");
    }

    #[test]
    fn interp_multiple_in_one_string() {
        let cfg = cfg_from_yaml("
constants:
  host: localhost
  port: 8080
  url: \"http://${host}:${port}\"
").unwrap();
        assert_eq!(const_str(&cfg, "url"), "http://localhost:8080");
    }

    #[test]
    fn interp_dotted_lookup() {
        let cfg = cfg_from_yaml("
constants:
  org:
    base: https://api.example.com
  url: \"${org.base}/v4\"
").unwrap();
        assert_eq!(const_str(&cfg, "url"), "https://api.example.com/v4");
    }

    #[test]
    fn interp_chained_references() {
        // a -> b -> c — multiple iterations required to converge.
        let cfg = cfg_from_yaml("
constants:
  c: final
  b: \"${c}\"
  a: \"${b}\"
").unwrap();
        assert_eq!(const_str(&cfg, "a"), "final");
        assert_eq!(const_str(&cfg, "b"), "final");
    }

    #[test]
    fn interp_number_stringification() {
        let cfg = cfg_from_yaml("
constants:
  port: 8080
  host: localhost
  url: \"${host}:${port}\"
").unwrap();
        assert_eq!(const_str(&cfg, "url"), "localhost:8080");
    }

    #[test]
    fn interp_no_patterns_is_noop() {
        let cfg = cfg_from_yaml("
constants:
  greeting: hello
  msg: plain text
").unwrap();
        assert_eq!(const_str(&cfg, "msg"), "plain text");
    }

    #[test]
    fn interp_cycle_errors() {
        let err = cfg_from_yaml("
constants:
  a: \"${b}\"
  b: \"${a}\"
").unwrap_err();
        assert!(
            err.contains("did not converge") || err.contains("cycle"),
            "expected convergence error, got: {}",
            err,
        );
    }

    #[test]
    fn interp_self_reference_errors() {
        let err = cfg_from_yaml("
constants:
  a: \"${a}\"
").unwrap_err();
        assert!(
            err.contains("did not converge") || err.contains("cycle"),
            "expected convergence error, got: {}",
            err,
        );
    }

    #[test]
    fn interp_unresolved_errors() {
        let err = cfg_from_yaml("
constants:
  msg: \"hello ${nothere}\"
").unwrap_err();
        assert!(
            err.contains("unresolved") && err.contains("nothere"),
            "expected unresolved-error mentioning 'nothere', got: {}",
            err,
        );
    }

    #[test]
    fn interp_works_inside_nested_values() {
        let cfg = cfg_from_yaml("
constants:
  ns: dk
  org:
    baseUrl: \"profile.${ns}:8080\"
").unwrap();
        match cfg.constants.get("org").unwrap() {
            serde_yaml::Value::Mapping(m) => {
                let key = serde_yaml::Value::String("baseUrl".to_string());
                match m.get(&key).unwrap() {
                    serde_yaml::Value::String(s) => assert_eq!(s, "profile.dk:8080"),
                    other => panic!("expected string, got {:?}", other),
                }
            }
            other => panic!("expected mapping, got {:?}", other),
        }
    }

    // --- env-var seeding ---

    #[test]
    fn env_filter_keeps_only_all_caps() {
        let vars = vec![
            ("ORG_TOKEN".to_string(), "secret".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("API_V4".to_string(), "yes".to_string()),
            ("camelCase".to_string(), "ignored".to_string()),
            ("lower".to_string(), "ignored".to_string()),
            ("Mixed_Case".to_string(), "ignored".to_string()),
            ("_LEADING".to_string(), "ignored".to_string()),
        ];
        let map = env_constants_from(vars.into_iter());
        assert_eq!(map.len(), 3);
        assert!(map.contains_key("ORG_TOKEN"));
        assert!(map.contains_key("PATH"));
        assert!(map.contains_key("API_V4"));
        assert!(!map.contains_key("camelCase"));
        assert!(!map.contains_key("_LEADING"));
    }

    #[test]
    fn is_env_var_name_rules() {
        assert!(is_env_var_name("ORG_TOKEN"));
        assert!(is_env_var_name("A"));
        assert!(is_env_var_name("API2"));
        assert!(!is_env_var_name(""));
        assert!(!is_env_var_name("lowercase"));
        assert!(!is_env_var_name("camelCase"));
        assert!(!is_env_var_name("_LEADING_UNDERSCORE"));
        assert!(!is_env_var_name("2STARTS_DIGIT"));
    }

    #[test]
    fn env_seeded_constant_resolves_in_yaml() {
        // Simulate: env provides ORG_TOKEN, yaml references it.
        let mut constants = env_constants_from(
            vec![("ORG_TOKEN".to_string(), "abc123".to_string())].into_iter(),
        );
        constants.insert(
            "auth".to_string(),
            serde_yaml::Value::String("bearer ${ORG_TOKEN}".to_string()),
        );
        resolve_constant_refs(&mut constants).unwrap();
        match constants.get("auth").unwrap() {
            serde_yaml::Value::String(s) => assert_eq!(s, "bearer abc123"),
            other => panic!("expected string, got {:?}", other),
        }
    }

    // --- existing merge test ---

    #[test]
    fn merge_lists_append_scalars_override() {
        let mut a = Config {
            defaults: Defaults {
                import: vec![PathBuf::from("/a")],
                display: Some("auto".to_string()),
            },
            constants: HashMap::new(),
            log_retention: None,
        };
        let b = Config {
            defaults: Defaults {
                import: vec![PathBuf::from("/b")],
                display: Some("bars".to_string()),
            },
            constants: HashMap::new(),
            log_retention: Some(25),
        };
        a.merge(b);
        assert_eq!(a.defaults.import, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(a.defaults.display.as_deref(), Some("bars"));
        assert_eq!(a.log_retention(), 25, "scalar override wins on merge");
    }
}
