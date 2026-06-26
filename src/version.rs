//! Minimal version comparison for the `requires:` metadata gate.
//!
//! tstr versions are plain `MAJOR.MINOR.PATCH` numerics (no pre-release tags),
//! so we hand-roll a small comparator rather than pull in the `semver` crate —
//! one fewer dependency to keep current. A requirement is an optional comparison
//! operator plus a dotted version (`>= 0.5.3`, `> 0.4`, `0.5.0`); a bare version
//! means `>=`, since `requires:` connotes a *minimum* version.

use std::cmp::Ordering;

/// The version of the running tstr binary, captured from Cargo at compile time.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A parsed `requires:` constraint: a comparison operator and a dotted version.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Requirement {
    op: Op,
    version: Vec<u64>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Op {
    Ge,
    Gt,
    Eq,
    Le,
    Lt,
}

/// Parse a `requires:` value into a `Requirement`. Returns a friendly message on
/// malformed input — the parser calls this at parse time so an authoring typo
/// (`requires: soonish`) surfaces early with source-line context, not at runtime.
pub fn parse_requirement(s: &str) -> Result<Requirement, String> {
    let s = s.trim();
    // Pull off a leading operator if present; a bare version defaults to `>=`.
    // Two-char operators must be checked before their single-char prefixes.
    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (Op::Ge, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (Op::Le, r)
    } else if let Some(r) = s.strip_prefix("==") {
        (Op::Eq, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Op::Lt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Op::Eq, r)
    } else {
        (Op::Ge, s)
    };

    let version_text = rest.trim();
    if version_text.is_empty() {
        return Err(format!("`requires:` is missing a version (got '{}')", s));
    }
    let version = parse_components(version_text).ok_or_else(|| {
        format!(
            "`requires:` version must be dotted numbers like `0.5.3` (got '{}')",
            version_text
        )
    })?;
    Ok(Requirement { op, version })
}

/// Parse `"0.5.3"` → `[0, 5, 3]`. `None` if any component isn't a non-negative
/// integer (or there are no components).
fn parse_components(s: &str) -> Option<Vec<u64>> {
    let parts: Result<Vec<u64>, _> = s.split('.').map(|p| p.trim().parse::<u64>()).collect();
    match parts {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Compare two component lists, zero-padding the shorter so `0.5` == `0.5.0`.
fn cmp_components(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

impl Requirement {
    /// Whether `have` (a dotted version string, e.g. the running binary's)
    /// satisfies this requirement. An unparseable `have` is treated as unmet.
    pub fn is_satisfied_by(&self, have: &str) -> bool {
        let have = match parse_components(have) {
            Some(v) => v,
            None => return false,
        };
        let ord = cmp_components(&have, &self.version);
        match self.op {
            Op::Ge => ord != Ordering::Less,
            Op::Gt => ord == Ordering::Greater,
            Op::Eq => ord == Ordering::Equal,
            Op::Le => ord != Ordering::Greater,
            Op::Lt => ord == Ordering::Less,
        }
    }

    /// Whether the running tstr binary satisfies this requirement.
    pub fn is_satisfied_by_current(&self) -> bool {
        self.is_satisfied_by(current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(s: &str) -> Requirement {
        parse_requirement(s).unwrap()
    }

    #[test]
    fn ge_is_the_default_operator() {
        assert!(req("0.4.6").is_satisfied_by("0.4.6"));
        assert!(req("0.4.6").is_satisfied_by("0.5.0"));
        assert!(!req("0.5.0").is_satisfied_by("0.4.6"));
    }

    #[test]
    fn explicit_operators() {
        assert!(req(">= 0.5.3").is_satisfied_by("0.5.3"));
        assert!(req(">= 0.5.3").is_satisfied_by("1.0.0"));
        assert!(!req(">= 0.5.3").is_satisfied_by("0.5.2"));
        assert!(req("> 0.4").is_satisfied_by("0.4.1"));
        assert!(!req("> 0.4").is_satisfied_by("0.4.0"));
        assert!(req("<= 0.5.0").is_satisfied_by("0.4.9"));
        assert!(req("= 0.4.6").is_satisfied_by("0.4.6"));
        assert!(!req("= 0.4.6").is_satisfied_by("0.4.7"));
    }

    #[test]
    fn shorter_version_is_zero_padded() {
        // `0.5` means `0.5.0`, so `0.5.1` satisfies `>= 0.5`.
        assert!(req(">= 0.5").is_satisfied_by("0.5.1"));
        assert!(req(">= 0.5").is_satisfied_by("0.5.0"));
        assert!(!req(">= 0.5").is_satisfied_by("0.4.9"));
    }

    #[test]
    fn malformed_requirements_error() {
        assert!(parse_requirement("soonish").is_err());
        assert!(parse_requirement(">=").is_err());
        assert!(parse_requirement(">= 1.x").is_err());
        assert!(parse_requirement("").is_err());
    }
}
