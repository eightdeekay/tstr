//! Valkey support — compiled only under `--features valkey`.
//!
//! ## Model: plain-data handles, one short-lived connection per op
//!
//! Same shape as `src/postgres.rs` / `src/kafka.rs`: `$.valkey(config)` returns
//! an ordinary `Value::Object` tagged `__kind: "vk.connection"` that carries
//! the config fields. There is no registry of live connections — the object
//! *is* the handle, so it clones, compares, renders, and `export`s like any
//! other value, and assigning a field (`v.db = 2`) reconfigures the next op.
//!
//! Each op opens a fresh connection, runs, and drops it. Unlike Postgres/Kafka
//! this uses the `redis` crate's **blocking** client (Valkey speaks the Redis
//! protocol), so no async runtime is spun up per op; socket read/write
//! timeouts provide the hang backstop instead.
//!
//! ## Surface
//!
//! - `$.valkey(config)` → a connection handle. `config` is a
//!   `redis://` / `rediss://` / `valkey://` / `valkeys://` URL string, or an
//!   object with `url`/`host`/`port`/`user`/`password`/`db`/`protocol`/`tls`/
//!   `tlsInsecure`/`tlsRootCert`. Discrete fields override the URL's parts. An
//!   unrecognized field is an error.
//! - `handle.cmd(name, ...args)` → runs any command; the reply is mapped to a
//!   DSL value (see [`reply_to_value`]). `name` may hold several words
//!   (`"CONFIG GET"`).
//! - `handle.getJson(key)` → `GET` + JSON parse (`null` when the key is absent).
//! - `handle.scan(pattern)` → every key matching `pattern`, via a full `SCAN`
//!   iteration (non-blocking on the server, unlike `KEYS`).
//!
//! RESP3 is the default protocol (Valkey has supported it since its first
//! release), so hash/map replies come back as objects rather than flat arrays.

use std::sync::Once;
use std::time::Duration;

use redis::{
    Client, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, ProtocolVersion, TlsCertificates,
};

use crate::eval::{EvalError, Scope};
use crate::value::{Value, ValueMap};

/// Backstop for establishing a connection (TCP + TLS + HELLO/AUTH).
const CONNECT_TIMEOUT_SECS: u64 = 10;
/// Backstop for any single read or write on an open connection. A blocking
/// command (`BLPOP k 60`) with a longer server-side timeout than this fails.
const OP_TIMEOUT_SECS: u64 = 30;
/// `COUNT` hint per `SCAN` round trip.
const SCAN_COUNT: u32 = 500;

/// `__kind` tag values identifying tstr objects that stand in for Valkey
/// resources.
pub mod kind {
    /// `$.valkey(config)` → a connection handle (config fields + tag).
    pub const CONNECTION: &str = "vk.connection";
}

/// Field name carrying the `__kind` tag on our tagged objects.
pub const KIND_FIELD: &str = "__kind";

/// Config keys accepted on a `$.valkey({...})` object.
const KNOWN_FIELDS: [&str; 10] = [
    "url",
    "host",
    "port",
    "user",
    "password",
    "db",
    "protocol",
    "tls",
    "tlsInsecure",
    "tlsRootCert",
];

/// If `value` is a tagged Valkey object, return its `__kind`; else `None`.
/// Scoped to the `vk.` namespace — Kafka and Postgres share the `__kind` field,
/// so each subsystem must claim only its own.
pub fn kind_of(value: &Value) -> Option<String> {
    match value.get_field(KIND_FIELD) {
        Value::String(s) if s.starts_with("vk.") => Some(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Connection config
// ---------------------------------------------------------------------------

/// Connection settings read off a handle. `url` (if any) is the base; each
/// discrete field that is set overrides the matching part of it.
#[derive(Debug)]
struct VkConf {
    url: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    db: Option<i64>,
    protocol: Option<ProtocolVersion>,
    tls: Option<bool>,
    insecure: bool,
    root_cert: Option<String>,
}

impl VkConf {
    /// Read a `VkConf` off a connection handle. Read at every op, so a field
    /// assigned mid-test (`v.db = 3`) takes effect on the next call.
    fn from_handle(h: &Value) -> Result<VkConf, EvalError> {
        let opt_str = |field: &str| match h.get_field(field) {
            Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        };
        let port = match h.get_field("port") {
            Value::Null => None,
            Value::Number(n) if (1.0..=65535.0).contains(&n) && n.fract() == 0.0 => Some(n as u16),
            Value::String(s) => Some(s.parse::<u16>().map_err(|_| {
                EvalError::new(format!("$.valkey: `port` must be 1-65535, got \"{s}\""))
            })?),
            other => {
                return Err(EvalError::new(format!(
                    "$.valkey: `port` must be 1-65535, got {other}"
                )))
            }
        };
        let db = match h.get_field("db") {
            Value::Null => None,
            Value::Number(n) if n >= 0.0 && n.fract() == 0.0 => Some(n as i64),
            Value::String(s) => Some(s.parse::<i64>().ok().filter(|d| *d >= 0).ok_or_else(|| {
                EvalError::new(format!("$.valkey: `db` must be a non-negative integer, got \"{s}\""))
            })?),
            other => {
                return Err(EvalError::new(format!(
                    "$.valkey: `db` must be a non-negative integer, got {other}"
                )))
            }
        };
        let protocol = match h.get_field("protocol") {
            Value::Null => None,
            v => Some(parse_protocol(&v)?),
        };
        let tls = match h.get_field("tls") {
            Value::Null => None,
            Value::Bool(b) => Some(b),
            other => {
                return Err(EvalError::new(format!(
                    "$.valkey: `tls` must be true or false, got {other}"
                )))
            }
        };
        Ok(VkConf {
            url: opt_str("url"),
            host: opt_str("host"),
            port,
            user: opt_str("user"),
            password: opt_str("password"),
            db,
            protocol,
            tls,
            insecure: h.get_field("tlsInsecure").is_truthy(),
            root_cert: opt_str("tlsRootCert"),
        })
    }

    /// Resolve to a `redis::ConnectionInfo`: parse the URL (or start from
    /// `localhost:6379`), then layer the discrete fields on top.
    fn connection_info(&self) -> Result<ConnectionInfo, EvalError> {
        let base = match &self.url {
            Some(u) => u.as_str().into_connection_info().map_err(|e| {
                EvalError::new(format!("$.valkey: invalid connection URL: {e}"))
            })?,
            None => ConnectionAddr::Tcp("localhost".to_string(), 6379)
                .into_connection_info()
                .map_err(|e| EvalError::new(format!("$.valkey: {e}")))?,
        };

        // --- address: host / port / tls / tlsInsecure ---
        let addr_overridden = self.host.is_some()
            || self.port.is_some()
            || self.tls.is_some()
            || self.insecure;
        let info = match (base.addr().clone(), addr_overridden) {
            (ConnectionAddr::Unix(_), false) => base,
            (ConnectionAddr::Unix(_), true) => {
                return Err(EvalError::new(
                    "$.valkey: host/port/tls can't be combined with a unix-socket URL",
                ))
            }
            (addr, _) => {
                let (url_host, url_port, url_tls, url_insecure) = match addr {
                    ConnectionAddr::Tcp(h, p) => (h, p, false, false),
                    ConnectionAddr::TcpTls {
                        host, port, insecure, ..
                    } => (host, port, true, insecure),
                    _ => unreachable!("unix handled above"),
                };
                let tls = self.tls.unwrap_or(url_tls);
                let insecure = self.insecure || url_insecure;
                if !tls && (insecure || self.root_cert.is_some()) {
                    return Err(EvalError::new(
                        "$.valkey: `tlsInsecure`/`tlsRootCert` need TLS — set `tls: true` \
                         or use a rediss:// / valkeys:// URL",
                    ));
                }
                if insecure && self.root_cert.is_some() {
                    return Err(EvalError::new(
                        "$.valkey: `tlsInsecure` and `tlsRootCert` are mutually exclusive — \
                         tlsInsecure skips the verification tlsRootCert exists to perform",
                    ));
                }
                let host = self.host.clone().unwrap_or(url_host);
                let port = self.port.unwrap_or(url_port);
                let addr = if tls {
                    ConnectionAddr::TcpTls {
                        host,
                        port,
                        insecure,
                        tls_params: None,
                    }
                } else {
                    ConnectionAddr::Tcp(host, port)
                };
                base.set_addr(addr)
            }
        };

        // --- handshake: user / password / db / protocol ---
        let mut rs = info.redis_settings().clone();
        if let Some(u) = &self.user {
            rs = rs.set_username(u);
        }
        if let Some(p) = &self.password {
            rs = rs.set_password(p);
        }
        if let Some(d) = self.db {
            rs = rs.set_db(d);
        }
        // An explicit `protocol` field wins; else a URL that names one
        // (`?protocol=2`) keeps it; else RESP3.
        let url_names_protocol = self.url.as_deref().is_some_and(|u| u.contains("protocol="));
        match self.protocol {
            Some(p) => rs = rs.set_protocol(p),
            None if url_names_protocol => {}
            None => rs = rs.set_protocol(ProtocolVersion::RESP3),
        }
        Ok(info.set_redis_settings(rs))
    }

    /// Build a `redis::Client` — with a custom CA bundle when `tlsRootCert` is
    /// set, otherwise trusting OS-native + webpki roots.
    fn client(&self) -> Result<Client, EvalError> {
        let info = self.connection_info()?;
        let built = match &self.root_cert {
            Some(path) => Client::build_with_tls(
                info,
                TlsCertificates {
                    client_tls: None,
                    root_cert: Some(load_root_cert(path)?),
                },
            ),
            None => Client::open(info),
        };
        built.map_err(|e| EvalError::new(format!("$.valkey: {e}")))
    }
}

/// `protocol: 2 | 3 | "resp2" | "resp3"`.
fn parse_protocol(v: &Value) -> Result<ProtocolVersion, EvalError> {
    let s = v.to_display_string().to_ascii_lowercase();
    match s.as_str() {
        "2" | "resp2" => Ok(ProtocolVersion::RESP2),
        "3" | "resp3" => Ok(ProtocolVersion::RESP3),
        _ => Err(EvalError::new(format!(
            "$.valkey: `protocol` must be 2 or 3, got {v}"
        ))),
    }
}

/// Read a PEM CA bundle, checking up front that it holds at least one usable
/// certificate. A bad file otherwise surfaces as an inscrutable handshake
/// failure with nothing pointing back at the config.
fn load_root_cert(path: &str) -> Result<Vec<u8>, EvalError> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;

    let resolved = crate::config::expand_tilde(path);
    let bytes = std::fs::read(&resolved).map_err(|e| {
        EvalError::new(format!(
            "$.valkey: cannot read tlsRootCert {}: {e}",
            resolved.display()
        ))
    })?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            EvalError::new(format!(
                "$.valkey: invalid PEM in tlsRootCert {}: {e}",
                resolved.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(EvalError::new(format!(
            "$.valkey: tlsRootCert {} contains no CERTIFICATE block",
            resolved.display()
        )));
    }
    Ok(bytes)
}

/// The `redis` crate builds its rustls config from the process-default crypto
/// provider. Only `ring` is compiled in, so rustls would pick it on its own;
/// installing it explicitly just makes that independent of feature unification.
fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Open a connection for one op, run `f` on it, drop it.
fn with_conn<T>(
    handle: &Value,
    what: &str,
    f: impl FnOnce(&mut redis::Connection) -> redis::RedisResult<T>,
) -> Result<T, EvalError> {
    ensure_crypto_provider();
    let client = VkConf::from_handle(handle)?.client()?;
    let mut con = client
        .get_connection_with_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .map_err(|e| EvalError::new(format!("valkey connect failed: {e}")))?;
    let op = Some(Duration::from_secs(OP_TIMEOUT_SECS));
    con.set_read_timeout(op)
        .and_then(|_| con.set_write_timeout(op))
        .map_err(|e| EvalError::new(format!("valkey connect failed: {e}")))?;
    f(&mut con).map_err(|e| EvalError::new(format!("valkey {what} failed: {e}")))
}

// ---------------------------------------------------------------------------
// Reply / argument shaping
// ---------------------------------------------------------------------------

/// Map a server reply to a DSL value.
///
/// | reply | value |
/// |---|---|
/// | nil | `null` |
/// | integer / double | number |
/// | bulk / simple / verbatim string, `OK` | string (non-UTF-8 bytes are replaced) |
/// | array / set / push | array |
/// | map (RESP3) | object (keys stringified, server order kept) |
/// | boolean (RESP3) | bool |
/// | big number | string (exact digits) |
///
/// An error nested in an aggregate (e.g. inside an `EXEC` reply) fails the op.
fn reply_to_value(v: redis::Value) -> Result<Value, EvalError> {
    use redis::Value as R;
    Ok(match v {
        R::Nil => Value::Null,
        R::Int(i) => Value::Number(i as f64),
        R::Double(d) => Value::Number(d),
        R::Boolean(b) => Value::Bool(b),
        R::Okay => Value::String("OK".to_string()),
        R::SimpleString(s) => Value::String(s),
        R::BulkString(b) => Value::String(bytes_to_string(b)),
        R::VerbatimString { text, .. } => Value::String(text),
        R::BigNumber(b) => Value::String(bytes_to_string(b)),
        R::Array(items) | R::Set(items) | R::Push { data: items, .. } => Value::Array(
            items
                .into_iter()
                .map(reply_to_value)
                .collect::<Result<_, _>>()?,
        ),
        R::Map(pairs) => {
            let mut m = ValueMap::with_capacity(pairs.len());
            for (k, v) in pairs {
                m.insert(reply_to_value(k)?.to_display_string(), reply_to_value(v)?);
            }
            Value::Object(m)
        }
        R::Attribute { data, .. } => reply_to_value(*data)?,
        R::ServerError(e) => return Err(EvalError::new(format!("valkey server error: {e}"))),
        // `redis::Value` is #[non_exhaustive]; a variant added by a future
        // crate version renders as its debug form rather than failing.
        other => Value::String(format!("{other:?}")),
    })
}

fn bytes_to_string(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Encode one command argument. Scalars go as their display text (`5`, not
/// `5.0`); objects/arrays as JSON so `cmd("SET", k, {…})` stores a JSON blob.
/// `null` is rejected — Valkey has no null argument, and silently sending
/// `"null"` would store a lie.
fn arg_text(v: &Value, idx: usize) -> Result<String, EvalError> {
    match v {
        Value::Null => Err(EvalError::new(format!(
            "valkey cmd: argument {} is null — Valkey has no null argument",
            idx + 1
        ))),
        Value::Object(_) | Value::Array(_) => Ok(crate::http::value_to_json_string(v)),
        other => Ok(other.to_display_string()),
    }
}

/// Build a `redis::Cmd` from a (possibly multi-word) command name + args.
fn build_cmd(name: &str, args: &[Value]) -> Result<redis::Cmd, EvalError> {
    let mut words = name.split_whitespace();
    let first = words
        .next()
        .ok_or_else(|| EvalError::new("valkey cmd: the command name is empty"))?;
    let mut c = redis::cmd(first);
    for w in words {
        c.arg(w);
    }
    for (i, a) in args.iter().enumerate() {
        c.arg(arg_text(a, i)?);
    }
    Ok(c)
}

/// Run-log endpoint label: the command and its first argument (normally the
/// key). Values are deliberately left out — they may be secrets (`AUTH`,
/// `SET session:… <token>`).
fn endpoint_label(name: &str, args: &[Value]) -> String {
    let cmd = name.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
    let label = match (cmd.as_str(), args.first()) {
        ("AUTH" | "HELLO", _) | (_, None) => format!("VALKEY {cmd}"),
        (_, Some(k)) => format!("VALKEY {cmd} {}", k.to_display_string()),
    };
    match label.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &label[..cut]),
        None => label,
    }
}

// ---------------------------------------------------------------------------
// Handle constructor
// ---------------------------------------------------------------------------

/// `$.valkey(config)` → a connection handle. No connection is opened here.
pub fn connection_from_config(config: &Value) -> Result<Value, EvalError> {
    let mut fields = ValueMap::new();
    fields.insert(
        KIND_FIELD.to_string(),
        Value::String(kind::CONNECTION.to_string()),
    );
    match config {
        Value::String(s) => {
            fields.insert("url".to_string(), Value::String(s.clone()));
        }
        Value::Object(m) => {
            // An unknown key is an error, not a no-op — see the matching note
            // in postgres::connection_from_config.
            let mut unknown: Vec<&str> = m
                .keys()
                .map(|k| k.as_str())
                .filter(|k| !KNOWN_FIELDS.contains(k))
                .collect();
            if !unknown.is_empty() {
                unknown.sort_unstable();
                return Err(EvalError::new(format!(
                    "$.valkey: unknown config field(s): {}. Known fields: {}",
                    unknown.join(", "),
                    KNOWN_FIELDS.join(", "),
                )));
            }
            for key in KNOWN_FIELDS {
                if let Some(v) = m.get(key) {
                    fields.insert(key.to_string(), v.clone());
                }
            }
        }
        other => {
            return Err(EvalError::new(format!(
                "$.valkey(config) expects a connection URL string or a config object, got {}",
                other.type_name()
            )))
        }
    }
    let handle = Value::Object(fields);
    // Validate eagerly so a bad port/protocol/TLS combo fails at the
    // `$.valkey(...)` line, not at the first command.
    VkConf::from_handle(&handle)?.connection_info()?;
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

/// Route a method call on a Valkey-tagged object. `args` are already evaluated
/// by the caller (`eval::eval_method_call`).
pub fn dispatch_method(
    kind: &str,
    obj: &Value,
    method: &str,
    args: &[Value],
    scope: &Scope,
) -> Result<Value, EvalError> {
    match (kind, method) {
        (kind::CONNECTION, "cmd") => cmd(obj, args, scope),
        (kind::CONNECTION, "getJson") => get_json(obj, args, scope),
        (kind::CONNECTION, "scan") => scan(obj, args, scope),
        (kind::CONNECTION, m) => Err(EvalError::new(format!(
            "unknown Valkey handle method '.{m}()' (expected cmd, getJson or scan)"
        ))),
        (k, m) => Err(EvalError::new(format!(
            "'.{m}()' is not valid on a Valkey {k} value"
        ))),
    }
}

/// `handle.cmd(name, ...args)` → the mapped reply.
fn cmd(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    let name = match args.first() {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(EvalError::new(format!(
                "valkey cmd: the first argument must be the command name, got {}",
                other.type_name()
            )))
        }
        None => {
            return Err(EvalError::new(
                "valkey handle.cmd(name, ...args) needs at least the command name",
            ))
        }
    };
    let rest = &args[1..];
    let c = build_cmd(&name, rest)?;
    scope.set_endpoint(endpoint_label(&name, rest));
    let reply = with_conn(handle, &name.to_uppercase(), |con| {
        c.query::<redis::Value>(con)
    })?;
    reply_to_value(reply)
}

/// `handle.getJson(key)` → `GET key`, parsed as JSON. `null` when the key does
/// not exist; an error when the stored value isn't JSON.
fn get_json(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    let key = match args {
        [k] => arg_text(k, 0)?,
        _ => return Err(EvalError::new("valkey handle.getJson(key) takes 1 argument")),
    };
    scope.set_endpoint(format!("VALKEY GET {key}"));
    let reply = with_conn(handle, "GET", |con| {
        redis::cmd("GET").arg(&key).query::<redis::Value>(con)
    })?;
    let text = match reply_to_value(reply)? {
        Value::Null => return Ok(Value::Null),
        Value::String(s) => s,
        other => {
            return Err(EvalError::new(format!(
                "valkey getJson: GET {key} returned a {}, not a string",
                other.type_name()
            )))
        }
    };
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        EvalError::new(format!("valkey getJson: value at '{key}' is not valid JSON: {e}"))
    })?;
    Ok(crate::http::json_to_value(&json))
}

/// `handle.scan(pattern)` → every matching key (deduplicated, in the order
/// `SCAN` returned them). Iterates the cursor to completion in one connection.
fn scan(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    let pattern = match args {
        [p] => arg_text(p, 0)?,
        _ => return Err(EvalError::new("valkey handle.scan(pattern) takes 1 argument")),
    };
    scope.set_endpoint(format!("VALKEY SCAN {pattern}"));
    let keys = with_conn(handle, "SCAN", |con| {
        let mut seen = std::collections::HashSet::new();
        let mut keys = Vec::new();
        let mut cursor = "0".to_string();
        loop {
            let (next, batch): (String, Vec<String>) = redis::cmd("SCAN")
                .arg(&cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(SCAN_COUNT)
                .query(con)?;
            // SCAN may return a key more than once across iterations.
            for k in batch {
                if seen.insert(k.clone()) {
                    keys.push(Value::String(k));
                }
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        Ok(keys)
    })?;
    Ok(Value::Array(keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::Value as R;

    fn sc() -> Scope {
        Scope::new()
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn info_for(config: Value) -> Result<ConnectionInfo, EvalError> {
        let h = connection_from_config(&config)?;
        VkConf::from_handle(&h)?.connection_info()
    }

    // ---- config ----

    #[test]
    fn kind_of_tags() {
        let h = connection_from_config(&s("valkey://localhost")).unwrap();
        assert_eq!(kind_of(&h).as_deref(), Some(kind::CONNECTION));
        assert_eq!(kind_of(&obj(&[("__kind", s("pg.connection"))])), None);
        assert_eq!(kind_of(&Value::Number(1.0)), None);
    }

    #[test]
    fn url_schemes_parse() {
        for u in ["redis://h:7000", "valkey://h:7000"] {
            let i = info_for(s(u)).unwrap();
            assert!(matches!(i.addr(), ConnectionAddr::Tcp(h, 7000) if h == "h"), "{u}");
        }
        for u in ["rediss://h:7000", "valkeys://h:7000"] {
            let i = info_for(s(u)).unwrap();
            assert!(matches!(i.addr(), ConnectionAddr::TcpTls { port: 7000, insecure: false, .. }), "{u}");
        }
        assert!(info_for(s("http://nope")).is_err());
    }

    #[test]
    fn defaults_to_localhost_and_resp3() {
        let i = info_for(obj(&[])).unwrap();
        assert!(matches!(i.addr(), ConnectionAddr::Tcp(h, 6379) if h == "localhost"));
        assert_eq!(i.redis_settings().protocol(), ProtocolVersion::RESP3);
        assert_eq!(i.redis_settings().db(), 0);
        // A bare URL also gets RESP3…
        let i = info_for(s("redis://h")).unwrap();
        assert_eq!(i.redis_settings().protocol(), ProtocolVersion::RESP3);
        // …unless it names a protocol itself.
        let i = info_for(s("redis://h/?protocol=2")).unwrap();
        assert_eq!(i.redis_settings().protocol(), ProtocolVersion::RESP2);
    }

    #[test]
    fn object_fields_apply() {
        let i = info_for(obj(&[
            ("host", s("cache.internal")),
            ("port", Value::Number(6380.0)),
            ("user", s("app")),
            ("password", s("pw")),
            ("db", Value::Number(3.0)),
            ("protocol", Value::Number(2.0)),
        ]))
        .unwrap();
        assert!(matches!(i.addr(), ConnectionAddr::Tcp(h, 6380) if h == "cache.internal"));
        let rs = i.redis_settings();
        assert_eq!(rs.username(), Some("app"));
        assert_eq!(rs.password(), Some("pw"));
        assert_eq!(rs.db(), 3);
        assert_eq!(rs.protocol(), ProtocolVersion::RESP2);
    }

    #[test]
    fn fields_override_url_parts() {
        let i = info_for(obj(&[
            ("url", s("redis://u:p@h:7000/1")),
            ("db", Value::Number(5.0)),
            ("tls", Value::Bool(true)),
        ]))
        .unwrap();
        assert!(matches!(i.addr(), ConnectionAddr::TcpTls { host, port: 7000, .. } if host == "h"));
        assert_eq!(i.redis_settings().db(), 5);
        assert_eq!(i.redis_settings().username(), Some("u"));
    }

    #[test]
    fn db_is_reread_from_the_handle() {
        // `v.db = 2` in the DSL is a field assignment on the handle object.
        let mut h = connection_from_config(&s("redis://h")).unwrap();
        h.set_field("db", Value::Number(2.0));
        let i = VkConf::from_handle(&h).unwrap().connection_info().unwrap();
        assert_eq!(i.redis_settings().db(), 2);
    }

    #[test]
    fn unknown_config_field_is_an_error() {
        let err = connection_from_config(&obj(&[("host", s("h")), ("sslInsecure", Value::Bool(true))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown config field(s): sslInsecure"), "got: {err}");
        assert!(err.contains("tlsInsecure"), "error should list known fields: {err}");
    }

    #[test]
    fn bad_values_fail_at_construction() {
        assert!(connection_from_config(&obj(&[("port", Value::Number(70000.0))])).is_err());
        assert!(connection_from_config(&obj(&[("db", Value::Number(-1.0))])).is_err());
        assert!(connection_from_config(&obj(&[("protocol", Value::Number(4.0))])).is_err());
        assert!(connection_from_config(&obj(&[("tls", s("yes"))])).is_err());
        assert!(connection_from_config(&Value::Number(1.0)).is_err());
        assert!(connection_from_config(&Value::Null).is_err());
    }

    #[test]
    fn tls_options_need_tls_and_exclude_each_other() {
        let err = connection_from_config(&obj(&[("tlsInsecure", Value::Bool(true))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("need TLS"), "got: {err}");

        let err = connection_from_config(&obj(&[
            ("tls", Value::Bool(true)),
            ("tlsInsecure", Value::Bool(true)),
            ("tlsRootCert", s("/x.pem")),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");

        let i = info_for(obj(&[("tls", Value::Bool(true)), ("tlsInsecure", Value::Bool(true))])).unwrap();
        assert!(matches!(i.addr(), ConnectionAddr::TcpTls { insecure: true, .. }));
    }

    #[test]
    fn root_cert_must_hold_a_certificate() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("empty.pem");
        std::fs::write(&p, "# nothing here\n").unwrap();
        let err = load_root_cert(p.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("no CERTIFICATE block"), "got: {err}");
        let err = load_root_cert("/nonexistent/ca.pem").unwrap_err().to_string();
        assert!(err.contains("cannot read tlsRootCert"), "got: {err}");
    }

    // ---- replies / args ----

    #[test]
    fn reply_mapping() {
        assert_eq!(reply_to_value(R::Nil).unwrap(), Value::Null);
        assert_eq!(reply_to_value(R::Int(7)).unwrap(), Value::Number(7.0));
        assert_eq!(reply_to_value(R::Okay).unwrap(), s("OK"));
        assert_eq!(reply_to_value(R::BulkString(b"hi".to_vec())).unwrap(), s("hi"));
        assert_eq!(reply_to_value(R::Boolean(true)).unwrap(), Value::Bool(true));
        assert_eq!(reply_to_value(R::Double(1.5)).unwrap(), Value::Number(1.5));
        assert_eq!(
            reply_to_value(R::Array(vec![R::Int(1), R::Nil])).unwrap(),
            Value::Array(vec![Value::Number(1.0), Value::Null])
        );
        let m = reply_to_value(R::Map(vec![
            (R::BulkString(b"b".to_vec()), R::BulkString(b"2".to_vec())),
            (R::BulkString(b"a".to_vec()), R::BulkString(b"1".to_vec())),
        ]))
        .unwrap();
        assert_eq!(m.get_field("a"), s("1"));
        // Server order is kept.
        assert_eq!(m.to_display_string(), obj(&[("b", s("2")), ("a", s("1"))]).to_display_string());
        // Invalid UTF-8 is replaced rather than failing.
        assert_eq!(reply_to_value(R::BulkString(vec![0x66, 0xff])).unwrap(), s("f\u{fffd}"));
    }

    #[test]
    fn args_encode_and_null_is_rejected() {
        assert_eq!(arg_text(&Value::Number(5.0), 0).unwrap(), "5");
        assert_eq!(arg_text(&Value::Number(1.5), 0).unwrap(), "1.5");
        assert_eq!(arg_text(&Value::Bool(true), 0).unwrap(), "true");
        assert_eq!(arg_text(&obj(&[("a", Value::Number(1.0))]), 0).unwrap(), r#"{"a":1}"#);
        let err = arg_text(&Value::Null, 1).unwrap_err().to_string();
        assert!(err.contains("argument 2 is null"), "got: {err}");
    }

    #[test]
    fn multi_word_command_names_split() {
        let c = build_cmd("config  get", &[s("maxmemory")]).unwrap();
        let packed = String::from_utf8(c.get_packed_command()).unwrap();
        assert!(packed.contains("config") && packed.contains("get") && packed.contains("maxmemory"));
        assert!(build_cmd("   ", &[]).is_err());
    }

    #[test]
    fn endpoint_label_hides_values() {
        assert_eq!(endpoint_label("set", &[s("k"), s("secret")]), "VALKEY SET k");
        assert_eq!(endpoint_label("AUTH", &[s("user"), s("pw")]), "VALKEY AUTH");
        assert_eq!(endpoint_label("ping", &[]), "VALKEY PING");
    }

    #[test]
    fn dispatch_validates() {
        let h = connection_from_config(&s("redis://localhost")).unwrap();
        assert!(dispatch_method(kind::CONNECTION, &h, "frobnicate", &[], &sc()).is_err());
        assert!(dispatch_method(kind::CONNECTION, &h, "cmd", &[], &sc()).is_err());
        assert!(dispatch_method(kind::CONNECTION, &h, "cmd", &[Value::Number(1.0)], &sc()).is_err());
        assert!(dispatch_method(kind::CONNECTION, &h, "getJson", &[], &sc()).is_err());
        assert!(dispatch_method(kind::CONNECTION, &h, "scan", &[s("a"), s("b")], &sc()).is_err());
    }

    /// Live: full round-trip against a real Valkey. Skipped unless
    /// `TSTR_VALKEY_TEST_URL` is set (e.g. `valkey://127.0.0.1:6379`).
    #[test]
    fn round_trip_live() {
        let url = match std::env::var("TSTR_VALKEY_TEST_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skipping round_trip_live (set TSTR_VALKEY_TEST_URL=valkey://host:port)");
                return;
            }
        };
        let v = connection_from_config(&s(&url)).unwrap();
        let c = |args: &[Value]| dispatch_method(kind::CONNECTION, &v, "cmd", args, &sc());
        let p = format!("tstr:it:{}:", std::process::id());
        let k = |n: &str| s(&format!("{p}{n}"));

        assert_eq!(c(&[s("PING")]).unwrap(), s("PONG"));

        // strings + nil
        assert_eq!(c(&[s("SET"), k("str"), Value::Number(42.0)]).unwrap(), s("OK"));
        assert_eq!(c(&[s("GET"), k("str")]).unwrap(), s("42"));
        assert_eq!(c(&[s("GET"), k("missing")]).unwrap(), Value::Null);
        assert_eq!(c(&[s("INCR"), k("str")]).unwrap(), Value::Number(43.0));

        // object arg → JSON blob → getJson round trip
        c(&[s("SET"), k("json"), obj(&[("id", Value::Number(7.0)), ("tag", s("x"))])]).unwrap();
        let j = dispatch_method(kind::CONNECTION, &v, "getJson", &[k("json")], &sc()).unwrap();
        assert_eq!(j.get_field("id"), Value::Number(7.0));
        assert_eq!(dispatch_method(kind::CONNECTION, &v, "getJson", &[k("missing")], &sc()).unwrap(), Value::Null);
        assert!(dispatch_method(kind::CONNECTION, &v, "getJson", &[k("str")], &sc()).is_ok()); // "43" is valid JSON
        c(&[s("SET"), k("notjson"), s("{oops")]).unwrap();
        assert!(dispatch_method(kind::CONNECTION, &v, "getJson", &[k("notjson")], &sc()).is_err());

        // RESP3 hash → object
        c(&[s("HSET"), k("h"), s("name"), s("alpha"), s("n"), Value::Number(1.0)]).unwrap();
        let h = c(&[s("HGETALL"), k("h")]).unwrap();
        assert_eq!(h.get_field("name"), s("alpha"));
        assert_eq!(h.get_field("n"), s("1"));

        // lists, TTL
        c(&[s("RPUSH"), k("l"), s("a"), s("b")]).unwrap();
        assert_eq!(c(&[s("LRANGE"), k("l"), Value::Number(0.0), Value::Number(-1.0)]).unwrap(), Value::Array(vec![s("a"), s("b")]));
        c(&[s("EXPIRE"), k("l"), Value::Number(100.0)]).unwrap();
        assert!(matches!(c(&[s("TTL"), k("l")]).unwrap(), Value::Number(n) if n > 0.0 && n <= 100.0));

        // server error surfaces as an EvalError
        let err = c(&[s("LPUSH"), k("h"), s("x")]).unwrap_err().to_string();
        assert!(err.contains("WRONGTYPE"), "got: {err}");

        // scan sees every key we made
        let keys = dispatch_method(kind::CONNECTION, &v, "scan", &[s(&format!("{p}*"))], &sc()).unwrap();
        let Value::Array(keys) = keys else { panic!("scan should return an array") };
        assert_eq!(keys.len(), 5, "got: {keys:?}");

        // db switch isolates keys
        let mut v1 = v.clone();
        v1.set_field("db", Value::Number(1.0));
        assert_eq!(dispatch_method(kind::CONNECTION, &v1, "cmd", &[s("GET"), k("str")], &sc()).unwrap(), Value::Null);

        // cleanup
        let mut del = vec![s("DEL")];
        del.extend(keys);
        assert_eq!(c(&del).unwrap(), Value::Number(5.0));
    }
}
