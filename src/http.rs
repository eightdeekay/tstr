use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json;

use crate::ast::*;
use crate::eval::{self, EvalError, Scope};
use crate::value::{Value, ValueMap};

/// Process-wide HTTP clients, keyed by target host. One client per host so each
/// can carry that host's pinned DNS answer (see `client_for`); hosts we don't
/// pin share `FALLBACK_CLIENT`. Built lazily on first use; timeouts must be
/// configured before the first request.
static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<Client>>>> = OnceLock::new();
static FALLBACK_CLIENT: OnceLock<Arc<Client>> = OnceLock::new();
static TIMEOUT_SECS: OnceLock<u64> = OnceLock::new();
static CONNECT_TIMEOUT_SECS: OnceLock<u64> = OnceLock::new();

/// Attempts for the once-per-host DNS lookup, and the backoff between them.
/// Because the lookup happens once per host per process rather than once per
/// request, retrying here is cheap and turns a transient resolver hiccup into a
/// short pause instead of a failed test.
const DNS_ATTEMPTS: u32 = 3;
const DNS_RETRY_BACKOFF_MS: u64 = 250;

/// Configure the HTTP timeout in seconds. Must be called before the first
/// HTTP call; subsequent calls are no-ops. Defaults to 60s if never called.
pub fn set_timeout(secs: u64) {
    let _ = TIMEOUT_SECS.set(secs);
}

/// Configure the TCP connect timeout in seconds. Must be called before the
/// first HTTP call; subsequent calls are no-ops. Defaults to 10s if never
/// called. This bounds connect (and TLS) only — the whole-request budget is
/// `set_timeout`. Splitting them means a tunnel that accepts no connection
/// fails in seconds with a connect error, instead of burning the full request
/// timeout and reporting an ambiguous one.
pub fn set_connect_timeout(secs: u64) {
    let _ = CONNECT_TIMEOUT_SECS.set(secs);
}

/// The settings every client shares, whatever its DNS pinning.
fn base_builder() -> reqwest::blocking::ClientBuilder {
    let secs = *TIMEOUT_SECS.get_or_init(|| 60);
    let connect_secs = *CONNECT_TIMEOUT_SECS.get_or_init(|| 10);
    // Do not reuse idle keep-alive connections. Services reached through the
    // telepresence tunnel close idle connections on their own timeout; when
    // reqwest pulls such a connection from the pool just as the server is
    // closing it, the request goes out on a dead socket and surfaces as a
    // flaky "connection closed before message completed" (~2% of requests
    // under load, scattered across unrelated endpoints). Opening a fresh
    // connection per request makes the reuse race structurally impossible —
    // correctness over throughput for a test runner.
    let mut builder = Client::builder().pool_max_idle_per_host(0);
    if secs > 0 {
        builder = builder.timeout(Duration::from_secs(secs));
    }
    // secs == 0 → no timeout (old behavior: relies on OS TCP limits)
    if connect_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(connect_secs));
    }
    builder
}

/// Client for hosts we deliberately don't pin (IP literals, unparseable URLs).
fn fallback_client() -> Arc<Client> {
    Arc::clone(FALLBACK_CLIENT.get_or_init(|| {
        Arc::new(base_builder().build().expect("failed to build HTTP client"))
    }))
}

/// Resolve `host` to socket addresses, retrying a few times before giving up.
///
/// The port is irrelevant: reqwest's DNS override ignores whatever port is in
/// the address and uses the one from the URL, so we look up with port 0 and
/// only care about the addresses.
fn resolve_host(host: &str) -> Result<Vec<SocketAddr>, String> {
    let mut last_err = String::from("no addresses returned");
    for attempt in 0..DNS_ATTEMPTS {
        match (host, 0u16).to_socket_addrs() {
            Ok(addrs) => {
                let addrs: Vec<SocketAddr> = addrs.collect();
                if !addrs.is_empty() {
                    return Ok(addrs);
                }
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt + 1 < DNS_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(
                DNS_RETRY_BACKOFF_MS * u64::from(attempt + 1),
            ));
        }
    }
    Err(last_err)
}

/// The client to use for `url`, resolving its host exactly once per process.
///
/// Pinning matters at suite scale. With keep-alive pooling off, every request
/// opens a fresh connection, and hyper's default resolver would call
/// `getaddrinfo` for each one — thousands of identical lookups over a run,
/// which is enough to make a tunnelled resolver start dropping answers. Here
/// the first request to a host does the one lookup and every later request to
/// that host reuses the answer via reqwest's DNS override, so DNS leaves the
/// hot path entirely.
///
/// The map lock is deliberately held across the lookup: it serializes the first
/// request to each host, but that is the point — otherwise every worker thread
/// starting at once would fire the same lookup simultaneously, which is the
/// stampede we're removing. After the first request per host it's a pure cache
/// hit.
fn client_for(url: &str) -> Result<Arc<Client>, EvalError> {
    let host = match reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)) {
        // An IP literal never reaches the resolver, and a URL we can't parse is
        // reqwest's problem to report with its own error. Neither gets pinned.
        Some(h) if h.parse::<IpAddr>().is_err() => h,
        _ => return Ok(fallback_client()),
    };

    let clients = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = guard.get(&host) {
        return Ok(Arc::clone(c));
    }

    let addrs = resolve_host(&host).map_err(|e| {
        EvalError::new(format!(
            "DNS lookup for '{}' failed after {} attempts: {} — the host is unresolvable from here (VPN/tunnel down?), not a slow server",
            host, DNS_ATTEMPTS, e
        ))
    })?;
    let client = Arc::new(
        base_builder()
            .resolve_to_addrs(&host, &addrs)
            .build()
            .expect("failed to build HTTP client"),
    );
    guard.insert(host, Arc::clone(&client));
    Ok(client)
}

/// Execute an HTTP call statement and return the response body as a Value.
/// Also populates the `_response` variable in scope with metadata.
pub fn execute_http_call(
    method: &HttpMethod,
    url_expr: &Expr,
    request_obj: &Expr,
    status_check: &Option<StatusCheck>,
    scope: &mut Scope,
) -> Result<Value, EvalError> {
    // Evaluate URL and request object up front (request object holds urlPrefix
    // for relative URLs, so we need it before composing the final URL).
    let url_val = eval::eval_expr(url_expr, scope)?;
    let raw_url = url_val.to_display_string();
    let req_val = eval::eval_expr(request_obj, scope)?;

    // Resolve full URL: relative URLs require the request object to provide
    // a `urlPrefix` field. There is no implicit fallback in scope.
    let full_url = if raw_url.starts_with('/') {
        let prefix = req_val.get_field("urlPrefix");
        match prefix {
            Value::String(p) => format!("{}{}", p.trim_end_matches('/'), raw_url),
            Value::Null => {
                return Err(EvalError::new(format!(
                    "relative URL '{}' but no urlPrefix in request object — pass a request with `.urlPrefix` (e.g. from `_in.req`)",
                    raw_url
                )));
            }
            _ => {
                return Err(EvalError::new(format!(
                    "urlPrefix must be a string, got {}", prefix.type_name()
                )));
            }
        }
    } else {
        raw_url
    };

    // Record the endpoint for failure output
    let method_str = match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Head => "HEAD",
        HttpMethod::Options => "OPTIONS",
    };
    scope.set_endpoint(format!("{} {}", method_str, full_url));

    // Build request
    let c = client_for(&full_url)?;
    let mut builder = match method {
        HttpMethod::Get => c.get(&full_url),
        HttpMethod::Post => c.post(&full_url),
        HttpMethod::Put => c.put(&full_url),
        HttpMethod::Patch => c.patch(&full_url),
        HttpMethod::Delete => c.delete(&full_url),
        HttpMethod::Head => c.head(&full_url),
        HttpMethod::Options => c.request(reqwest::Method::OPTIONS, &full_url),
    };

    // Apply request object — fields are individually optional, so a bare
    // `req = {}` works and reads each field as Null/missing.
    {
        // Headers
        let headers_val = req_val.get_field("headers");
        if let Value::Object(headers_map) = headers_val {
            let mut header_map = HeaderMap::new();
            for (k, v) in &headers_map {
                if let Ok(name) = HeaderName::from_bytes(k.as_bytes()) {
                    if let Ok(val) = HeaderValue::from_str(&v.to_display_string()) {
                        header_map.insert(name, val);
                    }
                }
            }
            builder = builder.headers(header_map);
        }

        // Body
        let body_val = req_val.get_field("body");
        match body_val {
            Value::Null => {} // no body
            Value::String(s) => {
                builder = builder.body(s);
            }
            Value::Object(_) | Value::Array(_) => {
                let json_str = value_to_json_string(&body_val);
                builder = builder.body(json_str);
            }
            _ => {
                builder = builder.body(body_val.to_display_string());
            }
        }

        // Query parameters
        let query_val = req_val.get_field("query");
        if let Value::Object(query_map) = query_val {
            let pairs: Vec<(String, String)> = query_map.iter()
                .map(|(k, v)| (k.clone(), v.to_display_string()))
                .collect();
            builder = builder.query(&pairs);
        }
    }

    // Execute the request
    let response = builder.send()
        .map_err(|e| EvalError::new(format!("HTTP request failed: {}", e)))?;

    // Extract response metadata
    let status_code = response.status().as_u16();
    let response_headers = extract_headers(&response);
    let version = format!("{:?}", response.version());

    // Read body and decide format from the body itself (don't trust the
    // content-type header — services can lie, and that's exactly what we test).
    let body_text = response.text()
        .map_err(|e| EvalError::new(format!("failed to read response body: {}", e)))?;
    let (body_value, format) = parse_body(&body_text);

    // Populate _response with format included up front.
    let mut response_meta = ValueMap::new();
    response_meta.insert("code".to_string(), Value::Number(status_code as f64));
    response_meta.insert("headers".to_string(), Value::Object(response_headers));
    response_meta.insert("version".to_string(), Value::String(version));
    response_meta.insert("format".to_string(), Value::String(format.to_string()));
    scope.set("_response".to_string(), Value::Object(response_meta));

    // Check status if required (after _response is set so the message can
    // reference _response.code / _response.format / etc.).
    if let Some(check) = status_check {
        if !status_matches(status_code, &check.patterns) {
            let msg = eval::interpolate_string_pub(&check.message, scope)?;
            return Err(EvalError::new(format!(
                "{} (got {})", msg, status_code
            )));
        }
    }

    Ok(body_value)
}

/// Parse a raw payload into a `(Value, format-label)` pair using the same
/// sniffing as HTTP response bodies (SSE → JSON → ndjson → text). Reused by the
/// Kafka consumer (`crate::kafka`) so a message body reads identically to a
/// response body — hence gated to the `kafka` feature (nothing else calls it).
#[cfg(feature = "kafka")]
pub(crate) fn parse_payload(body: &str) -> (Value, String) {
    let (v, fmt) = parse_body(body);
    (v, fmt.to_string())
}

/// Detected body format. Surfaced via `_response.format` so tests can assert
/// on it (e.g. `_response.format == "ndjson"`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum BodyFormat { Json, Ndjson, Sse, Text }

impl BodyFormat {
    fn to_string(self) -> String {
        match self {
            BodyFormat::Json => "json",
            BodyFormat::Ndjson => "ndjson",
            BodyFormat::Sse => "sse",
            BodyFormat::Text => "text",
        }.to_string()
    }
}

/// Sniff the body and parse it to a `Value`. Detection order is
/// SSE → JSON → ndjson → text. Detection is purely body-based; content-type
/// headers are ignored (services may lie about them).
fn parse_body(body: &str) -> (Value, BodyFormat) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return (Value::String(String::new()), BodyFormat::Text);
    }
    if looks_like_sse(trimmed) {
        return (parse_sse(body), BodyFormat::Sse);
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        return (json_to_value(&json), BodyFormat::Json);
    }
    if let Some(arr) = try_parse_ndjson(body) {
        return (arr, BodyFormat::Ndjson);
    }
    (Value::String(body.to_string()), BodyFormat::Text)
}

/// True if the body has at least one SSE field-line (`data:`, `event:`, `id:`,
/// `retry:`) or a comment line (starting with `:`). One match is enough — SSE
/// streams that don't carry any of these aren't SSE.
fn looks_like_sse(body: &str) -> bool {
    body.lines().any(|line| {
        let l = line.trim_start();
        l.starts_with("data:")
            || l.starts_with("event:")
            || l.starts_with("id:")
            || l.starts_with("retry:")
            || l.starts_with(":")
    })
}

/// Parse SSE stream into an array of event objects. Each event is:
///   { event: "message", data: <string|json>, id: ..., retry: ... }
/// Multi-line `data:` fields are concatenated with `\n`. `data:` strings that
/// parse as JSON are auto-parsed; otherwise left as string. Comments and
/// unknown fields are dropped.
fn parse_sse(body: &str) -> Value {
    let mut events: Vec<Value> = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut event_name: Option<String> = None;
    let mut event_id: Option<String> = None;
    let mut retry: Option<f64> = None;

    let flush = |events: &mut Vec<Value>,
                 data_lines: &mut Vec<String>,
                 event_name: &mut Option<String>,
                 event_id: &mut Option<String>,
                 retry: &mut Option<f64>| {
        if data_lines.is_empty() && event_name.is_none() && event_id.is_none() && retry.is_none() {
            return;
        }
        let mut obj: ValueMap = ValueMap::new();
        obj.insert("event".to_string(),
            Value::String(event_name.take().unwrap_or_else(|| "message".to_string())));
        let data_str = data_lines.join("\n");
        let data_val = if data_str.is_empty() {
            Value::Null
        } else if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data_str) {
            json_to_value(&json)
        } else {
            Value::String(data_str)
        };
        obj.insert("data".to_string(), data_val);
        obj.insert("id".to_string(), event_id.take().map(Value::String).unwrap_or(Value::Null));
        obj.insert("retry".to_string(), retry.take().map(Value::Number).unwrap_or(Value::Null));
        data_lines.clear();
        events.push(Value::Object(obj));
    };

    for line in body.lines() {
        // Blank line ends the current event.
        if line.trim().is_empty() {
            flush(&mut events, &mut data_lines, &mut event_name, &mut event_id, &mut retry);
            continue;
        }
        // Comment line.
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                // Per spec: a single leading space in the value is stripped.
                let v = v.strip_prefix(' ').unwrap_or(v);
                (&line[..i], v)
            }
            None => (line, ""),
        };
        match field {
            "data" => data_lines.push(value.to_string()),
            "event" => event_name = Some(value.to_string()),
            "id" => event_id = Some(value.to_string()),
            "retry" => retry = value.parse::<f64>().ok(),
            _ => {} // unknown field — ignore per spec
        }
    }
    // Final event if the body didn't end with a blank line.
    flush(&mut events, &mut data_lines, &mut event_name, &mut event_id, &mut retry);

    Value::Array(events)
}

/// Try to parse the body as ndjson — every non-empty line must parse as JSON,
/// and there must be at least 2 such lines (a single line is ambiguous and
/// should fall through to the regular JSON path or text fallback).
fn try_parse_ndjson(body: &str) -> Option<Value> {
    let mut items = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let json = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
        items.push(json_to_value(&json));
    }
    if items.len() < 2 {
        return None;
    }
    Some(Value::Array(items))
}

/// Check if a status code matches any of the given patterns.
pub fn status_matches(code: u16, patterns: &[StatusPattern]) -> bool {
    patterns.iter().any(|p| match p {
        StatusPattern::Exact(n) => code == *n,
        StatusPattern::Wildcard(prefix) => code / 100 == *prefix as u16,
        StatusPattern::Range(lo, hi) => code >= *lo && code <= *hi,
        StatusPattern::Comparison(op, n) => match op {
            CompOp::Gt => code > *n,
            CompOp::Lt => code < *n,
            CompOp::Gte => code >= *n,
            CompOp::Lte => code <= *n,
        },
    })
}

/// Convert a serde_json::Value to our Value type.
pub fn json_to_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Value::Array(arr.iter().map(json_to_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let map: ValueMap = obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect();
            Value::Object(map)
        }
    }
}

/// Convert a Value to a JSON string for request bodies. Also reused by the
/// Kafka producer (`crate::kafka`) to serialize object/array message payloads.
pub(crate) fn value_to_json_string(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if *n == (*n as i64) as f64 {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        Value::String(s) => format!("\"{}\"", s.replace('"', "\\\"")),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(value_to_json_string).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            // Declaration order, NOT sorted. This used to `pairs.sort()` to get
            // deterministic output out of a HashMap's random iteration order,
            // which meant tstr silently rewrote every JSON body it sent — fatal
            // for any API where the first-declared key wins. `Value::Object` is
            // now an IndexMap, so insertion order is both stable and correct.
            let pairs: Vec<String> = map.iter()
                .map(|(k, v)| format!("\"{}\":{}", k, value_to_json_string(v)))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
    }
}

/// Extract response headers into a Value::Object.
fn extract_headers(response: &Response) -> ValueMap {
    let mut map = ValueMap::new();
    for (name, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            map.insert(name.as_str().to_string(), Value::String(v.to_string()));
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of `client_for`: one DNS lookup per host for the whole run,
    /// not one per request. Same host in, same client out — which is what
    /// carries the pinned address, so nothing after the first call resolves.
    #[test]
    fn client_for_pins_a_host_and_reuses_it() {
        let a = client_for("http://localhost:1/one").expect("localhost resolves");
        let b = client_for("http://localhost:2/two").expect("localhost resolves");
        assert!(
            Arc::ptr_eq(&a, &b),
            "same host should hand back the same pinned client, not re-resolve"
        );
    }

    /// An IP literal never reaches a resolver, so there is nothing to pin and
    /// it shares the unpinned fallback client.
    #[test]
    fn ip_literals_share_the_fallback_client() {
        let a = client_for("http://127.0.0.1:8080/x").expect("no lookup needed");
        let b = client_for("http://127.0.0.1:9090/y").expect("no lookup needed");
        assert!(Arc::ptr_eq(&a, &b), "IP literals should share one client");
        assert!(Arc::ptr_eq(&a, &fallback_client()));
    }

    /// A host that can't be resolved has to say so in those words. It used to
    /// surface as a generic request failure after the full request timeout,
    /// which read like a slow server rather than a down tunnel.
    #[test]
    fn unresolvable_host_reports_dns_not_a_generic_failure() {
        let err = client_for("http://no-such-host.invalid/x")
            .expect_err("`.invalid` is reserved and never resolves");
        let msg = err.to_string();
        assert!(msg.contains("DNS lookup"), "got: {}", msg);
        assert!(msg.contains("no-such-host.invalid"), "got: {}", msg);
    }

    /// Object keys go on the wire in declaration order, not sorted. An API may
    /// resolve conflicting keys by "first one wins", so a test that declares
    /// `Sequence.2` before `Days.30` has to actually send it that way. tstr used
    /// to sort keys for deterministic output, which silently flipped the pair
    /// (D < S) and made the test assert against a request it never sent.
    #[test]
    fn json_body_keys_keep_declaration_order() {
        let obj = Value::Object(ValueMap::from([
            ("Sequence.2".to_string(), Value::String("seq".to_string())),
            ("Days.30".to_string(), Value::String("days".to_string())),
        ]));
        assert_eq!(
            value_to_json_string(&obj),
            r#"{"Sequence.2":"seq","Days.30":"days"}"#
        );

        // Reversing the declaration reverses the wire order — the sorted
        // implementation produced the same string for both.
        let flipped = Value::Object(ValueMap::from([
            ("Days.30".to_string(), Value::String("days".to_string())),
            ("Sequence.2".to_string(), Value::String("seq".to_string())),
        ]));
        assert_eq!(
            value_to_json_string(&flipped),
            r#"{"Days.30":"days","Sequence.2":"seq"}"#
        );
    }

    /// A parsed response keeps the key order the server sent (serde_json's
    /// `preserve_order` feature), so round-tripping a body doesn't reorder it.
    #[test]
    fn parsed_response_keeps_wire_order() {
        let (val, format) = parse_body(r#"{"zeta":1,"alpha":2,"mid":3}"#);
        assert_eq!(format.to_string(), "json");
        if let Value::Object(map) = &val {
            let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
            assert_eq!(keys, vec!["zeta", "alpha", "mid"]);
        } else {
            panic!("expected object, got {}", val.type_name());
        }
        assert_eq!(value_to_json_string(&val), r#"{"zeta":1,"alpha":2,"mid":3}"#);
    }

    /// Display is what shows up in failure output — it has to agree with what
    /// went on the wire, or a key-order bug is invisible in the diagnostics.
    #[test]
    fn display_keeps_declaration_order() {
        let obj = Value::Object(ValueMap::from([
            ("zeta".to_string(), Value::Number(1.0)),
            ("alpha".to_string(), Value::Number(2.0)),
        ]));
        assert_eq!(obj.to_string(), r#"{"zeta": 1, "alpha": 2}"#);
    }

    #[test]
    fn test_status_matches_exact() {
        assert!(status_matches(200, &[StatusPattern::Exact(200)]));
        assert!(!status_matches(201, &[StatusPattern::Exact(200)]));
    }

    #[test]
    fn test_status_matches_wildcard() {
        assert!(status_matches(200, &[StatusPattern::Wildcard(2)]));
        assert!(status_matches(204, &[StatusPattern::Wildcard(2)]));
        assert!(!status_matches(404, &[StatusPattern::Wildcard(2)]));
    }

    #[test]
    fn test_status_matches_range() {
        assert!(status_matches(200, &[StatusPattern::Range(200, 204)]));
        assert!(status_matches(204, &[StatusPattern::Range(200, 204)]));
        assert!(!status_matches(205, &[StatusPattern::Range(200, 204)]));
    }

    #[test]
    fn test_status_matches_comparison() {
        assert!(status_matches(500, &[StatusPattern::Comparison(CompOp::Gte, 400)]));
        assert!(!status_matches(200, &[StatusPattern::Comparison(CompOp::Gte, 400)]));
        assert!(status_matches(200, &[StatusPattern::Comparison(CompOp::Lt, 400)]));
    }

    #[test]
    fn parse_body_json_object() {
        let (v, f) = parse_body(r#"{"a": 1, "b": "two"}"#);
        assert_eq!(f, BodyFormat::Json);
        assert!(matches!(v, Value::Object(_)));
    }

    #[test]
    fn parse_body_json_array() {
        let (v, f) = parse_body(r#"[{"a":1},{"a":2}]"#);
        assert_eq!(f, BodyFormat::Json);
        assert!(matches!(v, Value::Array(_)));
    }

    #[test]
    fn parse_body_ndjson() {
        let body = "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let (v, f) = parse_body(body);
        assert_eq!(f, BodyFormat::Ndjson);
        if let Value::Array(arr) = v {
            assert_eq!(arr.len(), 3);
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn parse_body_single_line_json_is_not_ndjson() {
        // One JSON value on one line should be Json, not Ndjson
        let (_, f) = parse_body(r#"{"a":1}"#);
        assert_eq!(f, BodyFormat::Json);
    }

    #[test]
    fn parse_body_sse_basic() {
        let body = "event: ping\ndata: {\"ts\": 123}\n\ndata: hello\ndata: world\n\n";
        let (v, f) = parse_body(body);
        assert_eq!(f, BodyFormat::Sse);
        if let Value::Array(events) = v {
            assert_eq!(events.len(), 2);
            // First event: data is parsed JSON
            if let Value::Object(e) = &events[0] {
                assert_eq!(e.get("event"), Some(&Value::String("ping".to_string())));
                assert!(matches!(e.get("data"), Some(Value::Object(_))));
            } else { panic!("expected object"); }
            // Second event: multi-line data joined with \n, not JSON → string
            if let Value::Object(e) = &events[1] {
                assert_eq!(e.get("event"), Some(&Value::String("message".to_string())));
                assert_eq!(e.get("data"), Some(&Value::String("hello\nworld".to_string())));
            } else { panic!("expected object"); }
        } else {
            panic!("expected array");
        }
    }

    #[test]
    fn parse_body_sse_with_comments_and_id() {
        let body = ": this is a comment\ndata: payload\nid: 42\n\n";
        let (v, f) = parse_body(body);
        assert_eq!(f, BodyFormat::Sse);
        if let Value::Array(events) = v {
            assert_eq!(events.len(), 1);
            if let Value::Object(e) = &events[0] {
                assert_eq!(e.get("id"), Some(&Value::String("42".to_string())));
            } else { panic!(); }
        } else { panic!(); }
    }

    #[test]
    fn parse_body_text_fallback() {
        let (v, f) = parse_body("not json at all, just plain text");
        assert_eq!(f, BodyFormat::Text);
        assert!(matches!(v, Value::String(_)));
    }

    #[test]
    fn parse_body_empty() {
        let (_, f) = parse_body("");
        assert_eq!(f, BodyFormat::Text);
    }

    #[test]
    fn test_status_matches_multiple() {
        let patterns = vec![StatusPattern::Exact(200), StatusPattern::Exact(201)];
        assert!(status_matches(200, &patterns));
        assert!(status_matches(201, &patterns));
        assert!(!status_matches(202, &patterns));
    }

    #[test]
    fn test_json_to_value() {
        let json: serde_json::Value = serde_json::json!({
            "id": 123,
            "name": "Test",
            "active": true,
            "tags": ["a", "b"],
            "meta": null
        });
        let val = json_to_value(&json);
        assert_eq!(val.get_field("id"), Value::Number(123.0));
        assert_eq!(val.get_field("name"), Value::String("Test".to_string()));
        assert_eq!(val.get_field("active"), Value::Bool(true));
        assert_eq!(val.get_field("meta"), Value::Null);
        match val.get_field("tags") {
            Value::Array(arr) => assert_eq!(arr.len(), 2),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn test_value_to_json_string() {
        let val = Value::Object(ValueMap::from([
            ("name".to_string(), Value::String("Test".to_string())),
            ("count".to_string(), Value::Number(3.0)),
        ]));
        let json = value_to_json_string(&val);
        assert!(json.contains("\"name\":\"Test\""));
        assert!(json.contains("\"count\":3"));
    }
}
