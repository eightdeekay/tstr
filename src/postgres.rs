//! PostgreSQL support — compiled only under `--features postgres`.
//!
//! ## Model: plain-data handles, one short-lived connection per op
//!
//! Mirrors `src/kafka.rs` deliberately — it's the proven shape for "optional
//! external system exposed as a `$.xxx(config)` handle". Everything the DSL
//! passes around is an ordinary `Value::Object` tagged with a `__kind` marker
//! (see [`kind`]): a connection is just its config fields, a pagination cursor
//! is `{conn, sql, pageSize}`. There is no registry of live connections and no
//! new `Value` variant — the object *is* the handle, so it stays cloneable,
//! comparable, and renderable like any other value. Method dispatch keys on
//! `__kind` to route `.query` / `.paginate` / `.page` / `.total`.
//!
//! Like the HTTP client (`http.rs`) and the Kafka ops, each coarse operation
//! opens a fresh connection, runs, and drops it. tokio-postgres is async and
//! the tstr evaluator is blocking across rayon workers, so each op spins up its
//! own current-thread Tokio runtime (see [`run_op`]). Connection pooling is a
//! later perf pass, not a v1 concern — correctness over throughput.
//!
//! ## Query surface
//!
//! - `$.postgres(config)` → a connection handle (config = a `postgres://…` URL
//!   string or an object with `host`/`port`/`database`/`user`/`password`/
//!   `schema`/`sslmode`/`sslInsecure`). No connection opened until the first op.
//! - `handle.query(sql, ...params)` → runs any statement. Params bind as
//!   *text* and Postgres coerces them to the inferred column types (see
//!   [`TextParam`]). Returns `{ rows: [ {col: val, …} ], count }` — `count` is
//!   rows returned for a result-set, else rows affected.
//! - `handle.paginate(sql, pageSize)` → a cursor. `cursor.page(n)` fetches the
//!   0-indexed n-th page (stateless LIMIT/OFFSET re-issue); `cursor.total()`
//!   returns `count(*)` over the wrapped query.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio_postgres::types::{to_sql_checked, FromSql, Format, IsNull, ToSql, Type};
use tokio_postgres::{Client, Config, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::eval::{EvalError, Scope};
use crate::value::Value;

/// Wall-clock backstop for any single Postgres op. Bites only when a server is
/// unreachable or a statement hangs; the current-thread runtime would otherwise
/// block a rayon worker forever.
const OP_TIMEOUT_SECS: u64 = 30;

/// `__kind` tag values identifying tstr objects that stand in for Postgres
/// resources.
pub mod kind {
    /// `$.postgres(config)` → a connection handle (config fields + tag).
    pub const CONNECTION: &str = "pg.connection";
    /// `handle.paginate(sql, size)` → a pagination cursor (`{conn, sql, pageSize}`).
    pub const CURSOR: &str = "pg.cursor";
}

/// Field name carrying the `__kind` tag on our tagged objects.
pub const KIND_FIELD: &str = "__kind";

/// If `value` is a tagged Postgres object, return its `__kind`; else `None`.
/// Method dispatch uses this to tell a connection from a cursor from an
/// arbitrary user object.
pub fn kind_of(value: &Value) -> Option<String> {
    // Scoped to the `pg.` namespace — see the note on `kafka::kind_of`; both
    // subsystems share the `__kind` field, so each must claim only its own.
    match value.get_field(KIND_FIELD) {
        Value::String(s) if s.starts_with("pg.") => Some(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Async bridge (same pattern as kafka.rs)
// ---------------------------------------------------------------------------

/// Run a future to completion on a fresh current-thread Tokio runtime.
fn run_async<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build Postgres runtime")
        .block_on(fut)
}

/// Run a fallible Postgres future with the op-timeout backstop. The inner
/// future yields `Result<T, String>` (a human error); a timeout becomes its own
/// error. Both surface to the DSL as an `EvalError`.
fn run_op<F, T>(fut: F) -> Result<T, EvalError>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let out = run_async(async move {
        match tokio::time::timeout(Duration::from_secs(OP_TIMEOUT_SECS), fut).await {
            Ok(inner) => inner,
            Err(_) => Err(format!("postgres operation timed out after {OP_TIMEOUT_SECS}s")),
        }
    });
    out.map_err(EvalError::new)
}

// ---------------------------------------------------------------------------
// Text-coercing parameter adapter
// ---------------------------------------------------------------------------

/// A DSL `Value` bound as a **text-format** query parameter.
///
/// tokio-postgres uses the extended protocol: it prepares the statement, so the
/// server infers each param's type (e.g. `$1` in `where id = $1` is inferred as
/// `int4`). Binding a Rust `String` for an `int4` param would fail the binary
/// `ToSql` type check. Instead we send the value's text representation and set
/// [`Format::Text`], so Postgres parses the text into whatever type it inferred
/// — the same coercion a literal in SQL gets. Numbers, bools, timestamps, uuids
/// all coerce from text; objects/arrays are sent as JSON (pair with `$1::jsonb`
/// when the target is jsonb); `null` is a real SQL NULL.
#[derive(Debug)]
struct TextParam(Value);

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        let text = match &self.0 {
            Value::Null => return Ok(IsNull::Yes),
            Value::Object(_) | Value::Array(_) => crate::http::value_to_json_string(&self.0),
            other => other.to_display_string(),
        };
        out.extend_from_slice(text.as_bytes());
        Ok(IsNull::No)
    }

    // Accept any inferred type — we always send text and let Postgres coerce.
    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

// ---------------------------------------------------------------------------
// Connection config
// ---------------------------------------------------------------------------

/// Connection settings read off a handle. Either `url` is set (built from a
/// `postgres://…` string) or the discrete `host`/`user`/… fields are.
struct PgConf {
    url: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
    schema: Option<String>,
    sslmode: String,
    insecure: bool,
}

impl PgConf {
    /// Read a `PgConf` off a connection handle (or a cursor's stored `conn`).
    fn from_handle(h: &Value) -> Result<PgConf, EvalError> {
        let opt_str = |field: &str| match h.get_field(field) {
            Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        };
        let port = match h.get_field("port") {
            Value::Number(n) if n > 0.0 => Some(n as u16),
            Value::String(s) => s.parse::<u16>().ok(),
            _ => None,
        };
        let sslmode = match h.get_field("sslmode") {
            Value::String(s) if !s.is_empty() => s,
            _ => "prefer".to_string(),
        };
        Ok(PgConf {
            url: opt_str("url"),
            host: opt_str("host"),
            port,
            database: opt_str("database"),
            user: opt_str("user"),
            password: opt_str("password"),
            schema: opt_str("schema"),
            sslmode,
            insecure: h.get_field("sslInsecure").is_truthy(),
        })
    }

    /// Build a `tokio_postgres::Config`. URL form is parsed as-is (its own
    /// `sslmode` respected); object form sets fields explicitly.
    fn build(&self) -> Result<Config, EvalError> {
        if let Some(url) = &self.url {
            return Config::from_str(url)
                .map_err(|e| EvalError::new(format!("$.postgres: invalid connection URL: {e}")));
        }
        let mut c = Config::new();
        c.host(self.host.as_deref().unwrap_or("localhost"));
        c.port(self.port.unwrap_or(5432));
        match &self.user {
            Some(u) => {
                c.user(u);
            }
            None => {
                return Err(EvalError::new(
                    "$.postgres: the config object needs a `user`",
                ))
            }
        }
        if let Some(p) = &self.password {
            c.password(p);
        }
        // Default the database to the user name (libpq convention).
        let db = self.database.clone().or_else(|| self.user.clone());
        if let Some(d) = db {
            c.dbname(&d);
        }
        c.ssl_mode(map_ssl_mode(&self.sslmode));
        Ok(c)
    }
}

/// Map a DSL `sslmode` string to tokio-postgres' `SslMode`. This version of
/// tokio-postgres has only Disable/Prefer/Require; the stricter `verify-*`
/// modes still attempt TLS (Require) — actual certificate verification is
/// governed by the rustls config (`sslInsecure`), not this enum.
fn map_ssl_mode(mode: &str) -> tokio_postgres::config::SslMode {
    use tokio_postgres::config::SslMode;
    match mode.to_ascii_lowercase().as_str() {
        "disable" => SslMode::Disable,
        "require" | "verify-ca" | "verify-full" => SslMode::Require,
        _ => SslMode::Prefer,
    }
}

/// Build the rustls TLS connector. Always handed to tokio-postgres; it's only
/// used when `ssl_mode != Disable`, so it's safe to construct unconditionally.
/// `insecure` swaps in a verifier that accepts any certificate — for test
/// databases with self-signed certs. **Never** use it against production.
fn make_tls(insecure: bool) -> Result<MakeRustlsConnect, EvalError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| EvalError::new(format!("postgres TLS init failed: {e}")))?;
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(danger::NoVerify(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(MakeRustlsConnect::new(config))
}

/// The accept-any-certificate verifier used when `sslInsecure` is set. Kept in
/// its own module so the `dangerous` API is contained and greppable.
mod danger {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    pub(super) struct NoVerify(pub Arc<CryptoProvider>);

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

/// Connect, spawn the connection driver, and apply `search_path` if a schema is
/// set. Returns a ready `Client`. The driver future is detached with
/// `tokio::spawn`; it lives until the `Client` is dropped at end of op.
async fn connect(cfg: Config, tls: MakeRustlsConnect, schema: Option<String>) -> Result<Client, String> {
    let (client, connection) = cfg
        .connect(tls)
        .await
        .map_err(|e| format!("postgres connect failed: {e}"))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    if let Some(s) = schema {
        client
            .batch_execute(&format!("SET search_path TO {}", quote_ident(&s)))
            .await
            .map_err(|e| format!("postgres set search_path to '{s}' failed: {e}"))?;
    }
    Ok(client)
}

/// Double-quote a SQL identifier (schema name), escaping embedded quotes.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Row / result shaping
// ---------------------------------------------------------------------------

/// Turn a result row into a `{ column: value, … }` object.
fn row_to_object(row: &Row) -> Value {
    let mut map = HashMap::with_capacity(row.columns().len());
    for (i, col) in row.columns().iter().enumerate() {
        map.insert(col.name().to_string(), column_value(row, i, col.type_()));
    }
    Value::Object(map)
}

/// Decode one column into a `Value`. Postgres sends binary, so each type is
/// read via its `FromSql` impl and (for non-JSON scalars) stringified. Types
/// not handled here fall back to a text read, then to `Null` — never an error,
/// so an exotic column can't fail an otherwise-good query.
fn column_value(row: &Row, i: usize, ty: &Type) -> Value {
    if *ty == Type::BOOL {
        scalar::<bool, _>(row, i, Value::Bool)
    } else if *ty == Type::INT2 {
        scalar::<i16, _>(row, i, |v| Value::Number(v as f64))
    } else if *ty == Type::INT4 {
        scalar::<i32, _>(row, i, |v| Value::Number(v as f64))
    } else if *ty == Type::INT8 {
        scalar::<i64, _>(row, i, |v| Value::Number(v as f64))
    } else if *ty == Type::FLOAT4 {
        scalar::<f32, _>(row, i, |v| Value::Number(v as f64))
    } else if *ty == Type::FLOAT8 {
        scalar::<f64, _>(row, i, Value::Number)
    } else if *ty == Type::NUMERIC {
        scalar::<rust_decimal::Decimal, _>(row, i, |v| Value::String(v.to_string()))
    } else if *ty == Type::JSON || *ty == Type::JSONB {
        scalar::<serde_json::Value, _>(row, i, |v| crate::http::json_to_value(&v))
    } else if *ty == Type::UUID {
        scalar::<uuid::Uuid, _>(row, i, |v| Value::String(v.to_string()))
    } else if *ty == Type::TIMESTAMP {
        scalar::<chrono::NaiveDateTime, _>(row, i, |v| Value::String(v.to_string()))
    } else if *ty == Type::TIMESTAMPTZ {
        scalar::<chrono::DateTime<chrono::Utc>, _>(row, i, |v| Value::String(v.to_rfc3339()))
    } else if *ty == Type::DATE {
        scalar::<chrono::NaiveDate, _>(row, i, |v| Value::String(v.to_string()))
    } else if *ty == Type::TIME {
        scalar::<chrono::NaiveTime, _>(row, i, |v| Value::String(v.to_string()))
    } else {
        // text/varchar/bpchar/name/char and any other text-decodable type.
        scalar::<String, _>(row, i, Value::String)
    }
}

/// Fetch column `i` as `Option<T>` and map it; NULL or a decode failure → Null.
fn scalar<T, F>(row: &Row, i: usize, f: F) -> Value
where
    T: for<'a> FromSql<'a>,
    F: FnOnce(T) -> Value,
{
    match row.try_get::<usize, Option<T>>(i) {
        Ok(Some(v)) => f(v),
        Ok(None) => Value::Null,
        Err(_) => Value::Null,
    }
}

/// `{ rows: [...], count: N }` — the uniform result shape.
fn result_object(rows: Vec<Value>, count: f64) -> Value {
    Value::Object(HashMap::from([
        ("rows".to_string(), Value::Array(rows)),
        ("count".to_string(), Value::Number(count)),
    ]))
}

/// Prepare + run one statement. A statement that returns columns (SELECT, or
/// INSERT/UPDATE/DELETE … RETURNING) yields its rows with `count` = rows
/// returned; a statement with no result columns yields `count` = rows affected
/// and empty `rows`.
async fn run_sql(client: &Client, sql: &str, params: &[TextParam]) -> Result<Value, String> {
    let stmt = client
        .prepare(sql)
        .await
        .map_err(|e| format!("postgres query failed to prepare: {e}"))?;
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    if stmt.columns().is_empty() {
        let affected = client
            .execute(&stmt, &refs)
            .await
            .map_err(|e| format!("postgres statement failed: {e}"))?;
        Ok(result_object(Vec::new(), affected as f64))
    } else {
        let rows = client
            .query(&stmt, &refs)
            .await
            .map_err(|e| format!("postgres query failed: {e}"))?;
        let values: Vec<Value> = rows.iter().map(row_to_object).collect();
        let count = values.len() as f64;
        Ok(result_object(values, count))
    }
}

/// One-line summary of a SQL string for the run-log endpoint field.
fn sql_summary(sql: &str) -> String {
    let flat: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() > 80 {
        format!("{}…", &flat[..80])
    } else {
        flat
    }
}

// ---------------------------------------------------------------------------
// Handle constructors
// ---------------------------------------------------------------------------

/// `$.postgres(config)` → a connection handle. `config` is a `postgres://…` URL
/// string or an object carrying discrete fields. No connection is opened here.
pub fn connection_from_config(config: &Value) -> Result<Value, EvalError> {
    let mut fields: HashMap<String, Value> = HashMap::new();
    fields.insert(
        KIND_FIELD.to_string(),
        Value::String(kind::CONNECTION.to_string()),
    );
    match config {
        Value::String(s) => {
            fields.insert("url".to_string(), Value::String(s.clone()));
        }
        Value::Object(m) => {
            // Copy the recognized fields verbatim; PgConf::from_handle reads
            // them back. Unknown keys are simply ignored.
            for key in [
                "host",
                "port",
                "database",
                "user",
                "password",
                "schema",
                "sslmode",
                "sslInsecure",
                "url",
            ] {
                if let Some(v) = m.get(key) {
                    fields.insert(key.to_string(), v.clone());
                }
            }
        }
        other => {
            return Err(EvalError::new(format!(
                "$.postgres(config) expects a connection URL string or a config object, got {}",
                other.type_name()
            )))
        }
    }
    Ok(Value::Object(fields))
}

/// `handle.paginate(sql, pageSize)` → a cursor over `sql`, `pageSize` rows per
/// page. Stateless: it stores the connection + SQL + page size, and each
/// `.page(n)` re-issues the query with `LIMIT/OFFSET`.
fn paginate(handle: &Value, args: &[Value], _scope: &Scope) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::new(
            "pg handle.paginate(sql, pageSize) takes 2 arguments",
        ));
    }
    let sql = as_sql(&args[0], "paginate")?;
    let page_size = match &args[1] {
        Value::Number(n) if *n >= 1.0 => *n as i64,
        _ => {
            return Err(EvalError::new(
                "pg handle.paginate(sql, pageSize): pageSize must be a positive integer",
            ))
        }
    };
    Ok(Value::Object(HashMap::from([
        (KIND_FIELD.to_string(), Value::String(kind::CURSOR.to_string())),
        ("conn".to_string(), handle.clone()),
        ("sql".to_string(), Value::String(sql)),
        ("pageSize".to_string(), Value::Number(page_size as f64)),
    ])))
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

/// Route a method call on a Postgres-tagged object. `args` are already
/// evaluated by the caller (`eval::eval_method_call`).
pub fn dispatch_method(
    kind: &str,
    obj: &Value,
    method: &str,
    args: &[Value],
    scope: &Scope,
) -> Result<Value, EvalError> {
    match (kind, method) {
        (kind::CONNECTION, "query") => query(obj, args, scope),
        (kind::CONNECTION, "paginate") => paginate(obj, args, scope),
        (kind::CONNECTION, m) => Err(EvalError::new(format!(
            "unknown Postgres handle method '.{m}()' (expected query or paginate)"
        ))),
        (kind::CURSOR, "page") => page(obj, args, scope),
        (kind::CURSOR, "total") => total(obj, args, scope),
        (kind::CURSOR, m) => Err(EvalError::new(format!(
            "unknown Postgres cursor method '.{m}()' (expected page or total)"
        ))),
        (k, m) => Err(EvalError::new(format!(
            "'.{m}()' is not valid on a Postgres {k} value"
        ))),
    }
}

/// `handle.query(sql, ...params)` → `{ rows, count }`.
fn query(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.is_empty() {
        return Err(EvalError::new(
            "pg handle.query(sql, ...params) needs at least the SQL string",
        ));
    }
    let sql = as_sql(&args[0], "query")?;
    let params: Vec<TextParam> = args[1..].iter().map(|v| TextParam(v.clone())).collect();
    scope.set_endpoint(format!("PG {}", sql_summary(&sql)));

    let pc = PgConf::from_handle(handle)?;
    let cfg = pc.build()?;
    let tls = make_tls(pc.insecure)?;
    let schema = pc.schema.clone();
    run_op(async move {
        let client = connect(cfg, tls, schema).await?;
        run_sql(&client, &sql, &params).await
    })
}

/// `cursor.page(n)` → the 0-indexed n-th page: `{ rows, count, page, pageSize,
/// done }`. Wraps the cursor's SQL in a `LIMIT/OFFSET` subquery (stateless).
fn page(cursor: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::new("pg cursor.page(n) takes 1 argument"));
    }
    let n = match &args[0] {
        Value::Number(n) if *n >= 0.0 => *n as i64,
        _ => {
            return Err(EvalError::new(
                "pg cursor.page(n): n must be a non-negative integer",
            ))
        }
    };
    let conn = cursor.get_field("conn");
    let sql = cursor_sql(cursor)?;
    let page_size = cursor_page_size(cursor)?;
    let offset = n * page_size;
    let wrapped = format!(
        "SELECT * FROM ({}) _tstr_page LIMIT {} OFFSET {}",
        sql, page_size, offset
    );
    scope.set_endpoint(format!("PG page {n} ({}/page)", page_size));

    let pc = PgConf::from_handle(&conn)?;
    let cfg = pc.build()?;
    let tls = make_tls(pc.insecure)?;
    let schema = pc.schema.clone();
    let result = run_op(async move {
        let client = connect(cfg, tls, schema).await?;
        run_sql(&client, &wrapped, &[]).await
    })?;

    // Enrich the plain { rows, count } with page metadata.
    let count = match result.get_field("count") {
        Value::Number(c) => c,
        _ => 0.0,
    };
    let mut map = match result {
        Value::Object(m) => m,
        _ => HashMap::new(),
    };
    map.insert("page".to_string(), Value::Number(n as f64));
    map.insert("pageSize".to_string(), Value::Number(page_size as f64));
    // A page shorter than pageSize (including empty) is the last one.
    map.insert("done".to_string(), Value::Bool((count as i64) < page_size));
    Ok(Value::Object(map))
}

/// `cursor.total()` → total row count of the wrapped query (`count(*)`).
fn total(cursor: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if !args.is_empty() {
        return Err(EvalError::new("pg cursor.total() takes no arguments"));
    }
    let conn = cursor.get_field("conn");
    let sql = cursor_sql(cursor)?;
    let wrapped = format!("SELECT count(*) AS n FROM ({}) _tstr_count", sql);
    scope.set_endpoint("PG count(*)".to_string());

    let pc = PgConf::from_handle(&conn)?;
    let cfg = pc.build()?;
    let tls = make_tls(pc.insecure)?;
    let schema = pc.schema.clone();
    let result = run_op(async move {
        let client = connect(cfg, tls, schema).await?;
        run_sql(&client, &wrapped, &[]).await
    })?;
    // count(*) is int8 → decoded as a Number under key "n".
    Ok(result.get_field("rows").get_index(0).get_field("n"))
}

// ---------------------------------------------------------------------------
// Small readers / validators
// ---------------------------------------------------------------------------

/// Require an argument to be a non-empty SQL string. `sql` is trimmed and a
/// single trailing `;` removed (so paginate's subquery wrapping stays valid).
fn as_sql(v: &Value, op: &str) -> Result<String, EvalError> {
    match v {
        Value::String(s) => {
            let t = s.trim().trim_end_matches(';').trim().to_string();
            if t.is_empty() {
                Err(EvalError::new(format!("pg {op}: the SQL string is empty")))
            } else {
                Ok(t)
            }
        }
        other => Err(EvalError::new(format!(
            "pg {op}: the first argument must be a SQL string, got {}",
            other.type_name()
        ))),
    }
}

fn cursor_sql(cursor: &Value) -> Result<String, EvalError> {
    match cursor.get_field("sql") {
        Value::String(s) => Ok(s),
        _ => Err(EvalError::new("pg cursor is missing its SQL")),
    }
}

fn cursor_page_size(cursor: &Value) -> Result<i64, EvalError> {
    match cursor.get_field("pageSize") {
        Value::Number(n) if n >= 1.0 => Ok(n as i64),
        _ => Err(EvalError::new("pg cursor has an invalid pageSize")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc() -> Scope {
        Scope::new()
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn kind_of_tags() {
        let c = connection_from_config(&Value::String("postgres://localhost/db".into())).unwrap();
        assert_eq!(kind_of(&c).as_deref(), Some(kind::CONNECTION));
        assert_eq!(kind_of(&Value::Object(HashMap::new())), None);
        assert_eq!(kind_of(&Value::Number(1.0)), None);
    }

    #[test]
    fn config_from_string_stores_url() {
        let c = connection_from_config(&Value::String("postgres://u@h/db".into())).unwrap();
        let pc = PgConf::from_handle(&c).unwrap();
        assert_eq!(pc.url.as_deref(), Some("postgres://u@h/db"));
        // A URL handle builds a Config without error.
        assert!(pc.build().is_ok());
    }

    #[test]
    fn config_object_copies_fields() {
        let cfg = obj(&[
            ("host", Value::String("db.internal".into())),
            ("port", Value::Number(6543.0)),
            ("database", Value::String("appdb".into())),
            ("user", Value::String("tester".into())),
            ("password", Value::String("secret".into())),
            ("schema", Value::String("reporting".into())),
            ("sslmode", Value::String("require".into())),
            ("sslInsecure", Value::Bool(true)),
        ]);
        let handle = connection_from_config(&cfg).unwrap();
        let pc = PgConf::from_handle(&handle).unwrap();
        assert_eq!(pc.host.as_deref(), Some("db.internal"));
        assert_eq!(pc.port, Some(6543));
        assert_eq!(pc.database.as_deref(), Some("appdb"));
        assert_eq!(pc.user.as_deref(), Some("tester"));
        assert_eq!(pc.schema.as_deref(), Some("reporting"));
        assert_eq!(pc.sslmode, "require");
        assert!(pc.insecure);
        assert!(pc.build().is_ok());
    }

    #[test]
    fn config_object_defaults_and_requires_user() {
        // No user → build errors with a clear message.
        let handle = connection_from_config(&obj(&[("host", Value::String("h".into()))])).unwrap();
        let err = PgConf::from_handle(&handle).unwrap().build().unwrap_err();
        assert!(err.to_string().contains("user"), "got: {err}");
        // sslmode defaults to prefer.
        assert_eq!(PgConf::from_handle(&handle).unwrap().sslmode, "prefer");
    }

    #[test]
    fn config_rejects_non_string_non_object() {
        assert!(connection_from_config(&Value::Number(1.0)).is_err());
        assert!(connection_from_config(&Value::Null).is_err());
    }

    #[test]
    fn ssl_mode_mapping() {
        use tokio_postgres::config::SslMode;
        assert!(matches!(map_ssl_mode("disable"), SslMode::Disable));
        assert!(matches!(map_ssl_mode("require"), SslMode::Require));
        assert!(matches!(map_ssl_mode("verify-full"), SslMode::Require));
        assert!(matches!(map_ssl_mode("prefer"), SslMode::Prefer));
        assert!(matches!(map_ssl_mode("anything-else"), SslMode::Prefer));
    }

    #[test]
    fn text_param_encoding() {
        let mut out = BytesMut::new();
        let is_null = TextParam(Value::Number(42.0)).to_sql(&Type::INT4, &mut out).unwrap();
        assert!(matches!(is_null, IsNull::No));
        assert_eq!(&out[..], b"42");

        out.clear();
        let is_null = TextParam(Value::Null).to_sql(&Type::INT4, &mut out).unwrap();
        assert!(matches!(is_null, IsNull::Yes));
        assert!(out.is_empty());

        out.clear();
        TextParam(Value::Bool(true)).to_sql(&Type::BOOL, &mut out).unwrap();
        assert_eq!(&out[..], b"true");

        out.clear();
        let o = obj(&[("a", Value::Number(1.0))]);
        TextParam(o).to_sql(&Type::JSONB, &mut out).unwrap();
        assert_eq!(&out[..], br#"{"a":1}"#);
    }

    #[test]
    fn quote_ident_escapes() {
        assert_eq!(quote_ident("public"), "\"public\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn as_sql_trims_trailing_semicolon() {
        assert_eq!(as_sql(&Value::String("  select 1 ;  ".into()), "q").unwrap(), "select 1");
        assert!(as_sql(&Value::String("   ".into()), "q").is_err());
        assert!(as_sql(&Value::Number(1.0), "q").is_err());
    }

    #[test]
    fn paginate_builds_cursor() {
        let handle = connection_from_config(&Value::String("postgres://localhost/db".into())).unwrap();
        let cur = paginate(
            &handle,
            &[Value::String("select * from t order by id".into()), Value::Number(50.0)],
            &sc(),
        )
        .unwrap();
        assert_eq!(kind_of(&cur).as_deref(), Some(kind::CURSOR));
        assert_eq!(cur.get_field("sql"), Value::String("select * from t order by id".into()));
        assert_eq!(cur.get_field("pageSize"), Value::Number(50.0));
        assert_eq!(kind_of(&cur.get_field("conn")).as_deref(), Some(kind::CONNECTION));
    }

    #[test]
    fn paginate_validates_args() {
        let handle = connection_from_config(&Value::String("postgres://localhost/db".into())).unwrap();
        assert!(paginate(&handle, &[Value::String("select 1".into())], &sc()).is_err());
        assert!(paginate(
            &handle,
            &[Value::String("select 1".into()), Value::Number(0.0)],
            &sc()
        )
        .is_err());
    }

    #[test]
    fn dispatch_rejects_unknown_methods() {
        let handle = connection_from_config(&Value::String("postgres://localhost/db".into())).unwrap();
        assert!(dispatch_method(kind::CONNECTION, &handle, "frobnicate", &[], &sc()).is_err());
        let cur = paginate(
            &handle,
            &[Value::String("select 1".into()), Value::Number(10.0)],
            &sc(),
        )
        .unwrap();
        assert!(dispatch_method(kind::CURSOR, &cur, "wibble", &[], &sc()).is_err());
    }

    /// Live: full round-trip against a real Postgres. Skipped unless
    /// `TSTR_PG_TEST_URL` is set (e.g. `postgres://tstr:tstr@localhost:5432/tstr`).
    #[test]
    fn round_trip_live() {
        let url = match std::env::var("TSTR_PG_TEST_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skipping round_trip_live (set TSTR_PG_TEST_URL=postgres://user:pass@host/db)");
                return;
            }
        };
        let pg = connection_from_config(&Value::String(url)).unwrap();
        let q = |sql: &str, params: &[Value]| {
            let mut args = vec![Value::String(sql.to_string())];
            args.extend_from_slice(params);
            dispatch_method(kind::CONNECTION, &pg, "query", &args, &sc())
        };

        let table = format!("tstr_pg_it_{}", std::process::id());
        q(&format!("drop table if exists {table}"), &[]).unwrap();
        q(
            &format!("create table {table}(id serial primary key, name text, amount numeric, tags jsonb)"),
            &[],
        )
        .unwrap();

        // insert ... returning → count = rows returned, rows carry the id.
        let ins = q(
            &format!("insert into {table}(name, amount, tags) values($1, $2, $3) returning id"),
            &[
                Value::String("alpha".into()),
                Value::Number(12.5),
                obj(&[("k", Value::String("v".into()))]),
            ],
        )
        .unwrap();
        assert_eq!(ins.get_field("count"), Value::Number(1.0));
        let id = ins.get_field("rows").get_index(0).get_field("id");
        assert!(matches!(id, Value::Number(_)));

        // select → row shape + type mapping (numeric→string, jsonb→object).
        let sel = q(&format!("select * from {table} where name = $1"), &[Value::String("alpha".into())]).unwrap();
        assert_eq!(sel.get_field("count"), Value::Number(1.0));
        let row = sel.get_field("rows").get_index(0);
        assert_eq!(row.get_field("name"), Value::String("alpha".into()));
        assert_eq!(row.get_field("amount"), Value::String("12.5".into()));
        assert_eq!(row.get_field("tags").get_field("k"), Value::String("v".into()));

        // update / delete without returning → count = rows affected, rows empty.
        let upd = q(&format!("update {table} set name = $1 where name = $2"), &[Value::String("beta".into()), Value::String("alpha".into())]).unwrap();
        assert_eq!(upd.get_field("count"), Value::Number(1.0));
        assert_eq!(upd.get_field("rows"), Value::Array(vec![]));

        // pagination: seed 5 rows, page by 2.
        for i in 0..5 {
            q(&format!("insert into {table}(name) values($1)"), &[Value::String(format!("row{i}"))]).unwrap();
        }
        let cur = dispatch_method(
            kind::CONNECTION,
            &pg,
            "paginate",
            &[Value::String(format!("select id from {table} order by id")), Value::Number(2.0)],
            &sc(),
        )
        .unwrap();
        let total = dispatch_method(kind::CURSOR, &cur, "total", &[], &sc()).unwrap();
        assert_eq!(total, Value::Number(6.0)); // 1 (beta) + 5

        let p0 = dispatch_method(kind::CURSOR, &cur, "page", &[Value::Number(0.0)], &sc()).unwrap();
        assert_eq!(p0.get_field("count"), Value::Number(2.0));
        assert_eq!(p0.get_field("done"), Value::Bool(false));
        let p_last = dispatch_method(kind::CURSOR, &cur, "page", &[Value::Number(2.0)], &sc()).unwrap();
        assert_eq!(p_last.get_field("count"), Value::Number(2.0));
        let p_empty = dispatch_method(kind::CURSOR, &cur, "page", &[Value::Number(3.0)], &sc()).unwrap();
        assert_eq!(p_empty.get_field("count"), Value::Number(0.0));
        assert_eq!(p_empty.get_field("done"), Value::Bool(true));

        q(&format!("drop table if exists {table}"), &[]).unwrap();
    }
}
