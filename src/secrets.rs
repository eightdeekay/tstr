//! Secret redaction registry.
//!
//! `!secret <path>` constants in `tstr.yaml` load their value from a file and
//! register the plaintext here. Display paths call [`redact`] before printing,
//! so a secret that lands in a variable table, an export listing, or an error
//! never reaches the terminal.
//!
//! ## Why a content registry and not a `Value::Secret` variant
//!
//! A tainted-value variant loses the taint at the first `format!`. The pg
//! connection string is built by interpolating `${dbPassword}` into a URL at
//! config-load time, so by the time anything could print it, the password is an
//! anonymous substring of a larger `String`. Matching on content survives
//! interpolation, concatenation, and cloning for free.
//!
//! The tradeoff is that redaction is substring-based, so short values would
//! censor unrelated output. [`MIN_SECRET_LEN`] is the guard.

use std::sync::{OnceLock, RwLock};

/// Values shorter than this are never registered. A 3-character password would
/// otherwise blank out every incidental occurrence of those characters in the
/// report.
pub const MIN_SECRET_LEN: usize = 6;

/// What a redacted secret renders as. ASCII on purpose: the report's truncation
/// paths slice by byte index, and a multibyte mask could split a char boundary.
const MASK: &str = "[redacted]";

fn registry() -> &'static RwLock<Vec<String>> {
    static REGISTRY: OnceLock<RwLock<Vec<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a plaintext secret for redaction. Called once per `!secret`
/// constant during config load. Values under [`MIN_SECRET_LEN`] are ignored —
/// see the module docs.
pub fn register(secret: &str) {
    if secret.len() < MIN_SECRET_LEN {
        return;
    }
    let mut reg = registry().write().unwrap();
    if !reg.iter().any(|s| s == secret) {
        reg.push(secret.to_string());
    }
}

/// Replace every registered secret in `text` with the mask. Cheap no-op when
/// nothing is registered, which is the common case.
pub fn redact(text: &str) -> String {
    let reg = registry().read().unwrap();
    if reg.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for secret in reg.iter() {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), MASK);
        }
    }
    out
}

/// Drop all registered secrets. Tests only — the registry is process-global and
/// would otherwise leak across test cases.
#[cfg(test)]
pub fn clear() {
    registry().write().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The registry is process-global, so these tests cannot run concurrently.
    static GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn redacts_a_registered_secret() {
        let _g = GUARD.lock().unwrap();
        clear();
        register("hunter2pass");
        assert_eq!(redact("pw=hunter2pass;"), "pw=[redacted];");
        clear();
    }

    #[test]
    fn redacts_secret_embedded_in_a_connection_string() {
        let _g = GUARD.lock().unwrap();
        clear();
        register("s3kr1tvalue");
        let conn = "postgres://doadmin:s3kr1tvalue@db.example.com:25060/defaultdb";
        assert_eq!(
            redact(conn),
            "postgres://doadmin:[redacted]@db.example.com:25060/defaultdb",
        );
        clear();
    }

    #[test]
    fn ignores_values_below_the_minimum_length() {
        let _g = GUARD.lock().unwrap();
        clear();
        register("abc");
        assert_eq!(redact("abc"), "abc");
        clear();
    }

    #[test]
    fn passes_text_through_when_nothing_is_registered() {
        let _g = GUARD.lock().unwrap();
        clear();
        assert_eq!(redact("nothing to hide"), "nothing to hide");
    }

    #[test]
    fn registering_the_same_secret_twice_is_idempotent() {
        let _g = GUARD.lock().unwrap();
        clear();
        register("repeatedsecret");
        register("repeatedsecret");
        assert_eq!(redact("repeatedsecret"), "[redacted]");
        clear();
    }
}
