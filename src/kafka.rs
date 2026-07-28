//! Kafka produce/consume support — compiled only under `--features kafka`.
//!
//! ## Model: drain + tight window (not live subscription)
//!
//! Kafka is a durable *log*, not a live stream: a produced message stays in the
//! log, so we don't need to be subscribed before an action to observe the
//! message that action triggers. Instead we snapshot the topic's end offsets
//! *before* the action (`broker.since(topic)` → a cursor), fire the action,
//! then seek back to that mark and drain forward until a regex match or timeout
//! (`cursor.find(regex, timeout)`). No background threads, no live consumer
//! handles.
//!
//! ## Plain-data handles
//!
//! Everything the DSL passes around is an ordinary `Value::Object` tagged with
//! a `__kind` marker (see [`kind`]) — a broker is just `{bootstrap}`, a cursor
//! is `{bootstrap, topic, offsets}`. There is no registry of live resources and
//! no new `Value` variant; the object *is* the handle, so it stays cloneable,
//! comparable, and renderable like any other value. The evaluator's method
//! dispatch keys on `__kind` to route `.produce` / `.since` / `.find`.
//!
//! Phase 1 lays down this skeleton (tag constants + the async bridge). Produce,
//! since, and find land in later phases and will consume these helpers.
#![allow(dead_code)] // some helpers are wired up in phases 3–4

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use rskafka::client::partition::{Compression, OffsetAt, PartitionClient, UnknownTopicHandling};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::{Record, RecordAndOffset};

use crate::eval::{EvalError, Scope};
use crate::value::{Value, ValueMap};

/// Wall-clock backstop for any single Kafka op. `produce`/`since` should be
/// near-instant; this only bites when a broker is unreachable or a topic never
/// materializes under `UnknownTopicHandling::Retry` (which would otherwise hang
/// the current-thread runtime forever). `find` layers its own, usually shorter,
/// caller-supplied timeout on top (phase 4).
const OP_TIMEOUT_SECS: u64 = 30;

/// Produce always targets partition 0 for now. Fine for a test runner — a
/// consumer drains every partition regardless (phase 4), so which partition a
/// message lands on doesn't affect assertions. A `partition:` option can come
/// later if ordering-across-partitions ever matters.
const PRODUCE_PARTITION: i32 = 0;

/// Max bytes requested per fetch — 1 MiB is ample for test messages and caps
/// per-poll broker work.
const FETCH_MAX_BYTES: i32 = 1024 * 1024;

/// Per-fetch long-poll wait. Short so `find` rotates across partitions and
/// re-checks its overall deadline promptly. A caught-up single-partition topic
/// blocks at most this long per loop before the deadline is re-evaluated.
const FETCH_WAIT_MS: i32 = 500;

/// How long to nap before re-checking for a topic that doesn't exist yet
/// (a downstream service may create it mid-`find`).
const REDISCOVER_NAP_MS: u64 = 200;

/// `__kind` tag values identifying tstr objects that stand in for Kafka
/// resources.
pub mod kind {
    /// `$.kafka("host:9092")` → broker config object.
    pub const BROKER: &str = "kafka.broker";
    /// `broker.since("topic")` → window cursor (bootstrap + topic + offsets).
    pub const CURSOR: &str = "kafka.cursor";
    /// value returned by `cursor.find(...)` — a single consumed message.
    pub const MESSAGE: &str = "kafka.message";
}

/// Field name carrying the `__kind` tag on our tagged objects.
pub const KIND_FIELD: &str = "__kind";

/// If `value` is a tagged Kafka object, return its `__kind` string; otherwise
/// `None`. Method dispatch uses this to tell a broker from a cursor from an
/// arbitrary user object.
///
/// Scoped to the `kafka.` namespace: other subsystems (e.g. `postgres`) tag
/// their handles with the same `__kind` field, so this must claim only its own
/// kinds — otherwise the kafka method-dispatch interception in `eval.rs` would
/// swallow a `pg.connection` and report a bogus "not valid on a Kafka …" error.
pub fn kind_of(value: &Value) -> Option<String> {
    match value.get_field(KIND_FIELD) {
        Value::String(s) if s.starts_with("kafka.") => Some(s),
        _ => None,
    }
}

/// Run a future to completion on a fresh current-thread Tokio runtime.
///
/// rskafka is async; the tstr evaluator is blocking and runs across rayon
/// worker threads. Rather than share one runtime — which invites
/// `block_on`-from-within-a-runtime hazards under parallel file execution —
/// each coarse Kafka op (produce / since / find) spins up its own runtime for
/// the duration of the call. Same "correctness over throughput" tradeoff as the
/// no-keepalive HTTP client in `http.rs`. Reusing connections across ops is a
/// later perf pass, not a v1 concern.
pub fn run_async<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build Kafka runtime")
        .block_on(fut)
}

/// Run a fallible Kafka future to completion with the op-timeout backstop.
/// The inner future yields `Result<T, String>` (a human error); a timeout
/// becomes its own error. Both surface to the DSL as an `EvalError`.
fn run_op<F, T>(fut: F) -> Result<T, EvalError>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    let out = run_async(async move {
        match tokio::time::timeout(Duration::from_secs(OP_TIMEOUT_SECS), fut).await {
            Ok(inner) => inner,
            Err(_) => Err(format!("kafka operation timed out after {OP_TIMEOUT_SECS}s")),
        }
    });
    out.map_err(EvalError::new)
}

/// `$.kafka(config)` → a topic handle. `config` is either a bare bootstrap
/// string or a config object carrying the **global** bits — `bootstrap` (the
/// only required field) plus the `requiresTypeId` / `requiresKey` guardrails.
/// The **dynamic** bits (`topic`, `key`, `headers`) are set as fields on the
/// returned handle in the test before `send`/`since`. No connection is opened
/// until the first `send`/`since`.
///
/// The handle is a plain mutable object seeded with empty dynamic fields so the
/// test can assign them (`k.topic = "…"; k.headers = {…}`).
pub fn broker_from_config(config: &Value) -> Result<Value, EvalError> {
    let (bootstrap, requires_type_id, requires_key) = match config {
        Value::String(s) => (s.clone(), false, false),
        Value::Object(_) => {
            let bootstrap = match config.get_field("bootstrap") {
                Value::String(s) => s,
                Value::Null => {
                    return Err(EvalError::new(
                        "$.kafka(config): the config object needs a `bootstrap` string",
                    ))
                }
                other => {
                    return Err(EvalError::new(format!(
                        "$.kafka(config): `bootstrap` must be a string, got {}",
                        other.type_name()
                    )))
                }
            };
            (
                bootstrap,
                config.get_field("requiresTypeId").is_truthy(),
                config.get_field("requiresKey").is_truthy(),
            )
        }
        other => {
            return Err(EvalError::new(format!(
                "$.kafka(config) expects a bootstrap string or a config object, got {}",
                other.type_name()
            )))
        }
    };
    Ok(Value::Object(ValueMap::from([
        (KIND_FIELD.to_string(), Value::String(kind::BROKER.to_string())),
        ("bootstrap".to_string(), Value::String(bootstrap)),
        ("requiresTypeId".to_string(), Value::Bool(requires_type_id)),
        ("requiresKey".to_string(), Value::Bool(requires_key)),
        // dynamic fields, set per-test before send/since
        ("topic".to_string(), Value::Null),
        ("key".to_string(), Value::Null),
        ("headers".to_string(), Value::Null),
    ])))
}

/// Route a method call on a Kafka-tagged object. `args` are already evaluated
/// by the caller (`eval::eval_method_call`). Keeps all Kafka method logic here
/// rather than scattering `#[cfg(feature = "kafka")]` arms through the big
/// method-dispatch match in eval.rs.
pub fn dispatch_method(
    kind: &str,
    obj: &Value,
    method: &str,
    args: &[Value],
    scope: &Scope,
) -> Result<Value, EvalError> {
    match (kind, method) {
        (kind::BROKER, "send") => send(obj, args, scope),
        (kind::BROKER, "since") => since(obj, args, scope),
        (kind::BROKER, m) => Err(EvalError::new(format!(
            "unknown Kafka handle method '.{m}()'"
        ))),
        (kind::CURSOR, "find") => find(obj, args, scope),
        (kind::CURSOR, m) => Err(EvalError::new(format!(
            "unknown Kafka cursor method '.{m}()'"
        ))),
        (k, m) => Err(EvalError::new(format!(
            "'.{m}()' is not valid on a Kafka {k} value"
        ))),
    }
}

/// `handle.send(value)` → `{ partition, offset }` ack. Reads the dynamic
/// `.topic` / `.key` / `.headers` fields off the handle; `value`/`key` are
/// serialized like an HTTP body (strings raw, objects/arrays JSON, `null` value
/// = tombstone). Enforces the handle's `requiresKey` / `requiresTypeId`
/// guardrails *before* connecting, so a misconfigured send fails fast.
fn send(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::new("kafka handle.send(value) takes 1 argument"));
    }
    let bootstrap = broker_bootstrap(handle)?;
    let topic = handle_topic(handle, "send")?;
    scope.set_endpoint(format!("KAFKA send {topic}"));

    let key = value_to_payload(&handle.get_field("key"));
    let headers = headers_from(&handle.get_field("headers"))?;

    // Guardrails (before any network work).
    if handle.get_field("requiresKey").is_truthy() && key.is_none() {
        return Err(EvalError::new(
            "kafka send: this broker requires a message key (requiresKey) — set `.key` before sending",
        ));
    }
    if handle.get_field("requiresTypeId").is_truthy() && !headers.contains_key("__TypeId__") {
        return Err(EvalError::new(
            "kafka send: this broker requires a `__TypeId__` header (requiresTypeId) — set `.headers.\"__TypeId__\"` before sending",
        ));
    }

    let value = value_to_payload(&args[0]);
    let offsets = run_op(async move {
        let client = ClientBuilder::new(vec![bootstrap])
            .build()
            .await
            .map_err(|e| format!("kafka connect failed: {e}"))?;
        let partition = client
            .partition_client(topic.clone(), PRODUCE_PARTITION, UnknownTopicHandling::Retry)
            .await
            .map_err(|e| format!("kafka partition_client({topic}) failed: {e}"))?;
        let record = Record {
            key,
            value,
            headers,
            timestamp: now_utc(),
        };
        partition
            .produce(vec![record], Compression::NoCompression)
            .await
            .map_err(|e| format!("kafka send to '{topic}' failed: {e}"))
    })?;

    let offset = offsets.first().copied().unwrap_or(-1);
    Ok(ack(PRODUCE_PARTITION, offset))
}

/// Read the handle's `.topic` dynamic field, requiring it to be a non-empty
/// string. `op` names the operation for the error.
fn handle_topic(handle: &Value, op: &str) -> Result<String, EvalError> {
    match handle.get_field("topic") {
        Value::String(s) if !s.is_empty() => Ok(s),
        _ => Err(EvalError::new(format!(
            "kafka {op}: set `.topic` on the handle first (e.g. `k.topic = \"my.topic\";`)"
        ))),
    }
}

/// Convert a handle's `.headers` field into Kafka record headers. `null` (unset)
/// yields no headers; anything but an object is an error. Values are stringified
/// (headers are text on the wire).
fn headers_from(v: &Value) -> Result<BTreeMap<String, Vec<u8>>, EvalError> {
    match v {
        Value::Null => Ok(BTreeMap::new()),
        Value::Object(m) => Ok(m
            .iter()
            .map(|(k, val)| (k.clone(), val.to_display_string().into_bytes()))
            .collect()),
        other => Err(EvalError::new(format!(
            "kafka send: `.headers` must be an object, got {}",
            other.type_name()
        ))),
    }
}

/// `handle.since()` → a cursor marking the current end offsets of the handle's
/// `.topic`, one per partition. Seeking back to this mark later (`cursor.find`)
/// reads exactly the messages produced *after* this call — the tight window.
///
/// A topic that doesn't exist yet yields an **empty** offsets map rather than an
/// error: there are no prior messages to skip, so `find` will read each
/// partition from the earliest offset once the topic appears. That's what makes
/// `since` safe to call before the downstream service has created the topic.
fn since(handle: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if !args.is_empty() {
        return Err(EvalError::new(
            "kafka handle.since() takes no arguments — set `.topic` on the handle",
        ));
    }
    let bootstrap = broker_bootstrap(handle)?;
    let topic = handle_topic(handle, "since")?;
    scope.set_endpoint(format!("KAFKA since {topic}"));
    let (b2, t2) = (bootstrap.clone(), topic.clone());

    let offsets = run_op(async move {
        let client = ClientBuilder::new(vec![b2])
            .build()
            .await
            .map_err(|e| format!("kafka connect failed: {e}"))?;
        let partitions = topic_partitions(&client, &t2).await?;
        let mut offs: Vec<(i32, i64)> = Vec::with_capacity(partitions.len());
        for p in partitions {
            let pc = client
                .partition_client(t2.clone(), p, UnknownTopicHandling::Error)
                .await
                .map_err(|e| format!("kafka partition_client({t2}/{p}) failed: {e}"))?;
            let end = pc
                .get_offset(OffsetAt::Latest)
                .await
                .map_err(|e| format!("kafka get_offset({t2}/{p}) failed: {e}"))?;
            offs.push((p, end));
        }
        Ok(offs)
    })?;

    Ok(cursor(&bootstrap, &topic, &offsets))
}

/// Partition ids of `topic`, or an empty vec if the topic doesn't exist yet.
/// Empty is a valid answer here — see `since`.
async fn topic_partitions(client: &Client, topic: &str) -> Result<Vec<i32>, String> {
    let topics = client
        .list_topics()
        .await
        .map_err(|e| format!("kafka list_topics failed: {e}"))?;
    Ok(topics
        .into_iter()
        .find(|t| t.name == topic)
        .map(|t| t.partitions.into_iter().collect())
        .unwrap_or_default())
}

/// Build a cursor value: bootstrap + topic + per-partition mark offsets
/// (`{ "0": 5, "1": 3 }`). Phase 4's `find` seeks each partition to its mark
/// (or earliest, for partitions absent from the map).
fn cursor(bootstrap: &str, topic: &str, offsets: &[(i32, i64)]) -> Value {
    let off_map: crate::value::ValueMap = offsets
        .iter()
        .map(|(p, o)| (p.to_string(), Value::Number(*o as f64)))
        .collect();
    Value::Object(ValueMap::from([
        (KIND_FIELD.to_string(), Value::String(kind::CURSOR.to_string())),
        ("bootstrap".to_string(), Value::String(bootstrap.to_string())),
        ("topic".to_string(), Value::String(topic.to_string())),
        ("offsets".to_string(), Value::Object(off_map)),
    ]))
}

/// `cursor.find(regex, timeout)` → the first message at or after the cursor's
/// mark whose raw payload matches `regex`, or `Null` if none arrives before
/// `timeout` elapses. `timeout` is milliseconds — a bare `30s` duration literal
/// works. The match is **full-body**: the pattern is tested against the whole
/// UTF-8 payload string, not a parsed field.
///
/// Unlike `produce`/`since`, `find` drives its own deadline (that's the point),
/// so it does not use the `run_op` backstop — its loop is bounded by `timeout`,
/// and each fetch caps its wait at the remaining time.
fn find(cursor: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.len() != 2 {
        return Err(EvalError::new(
            "cursor.find(regex, timeout) takes 2 arguments",
        ));
    }
    let pattern = as_string(&args[0], "regex")?;
    let re = regex::Regex::new(&pattern)
        .map_err(|e| EvalError::new(format!("kafka find: invalid regex: {e}")))?;
    let topic = cursor_field(cursor, "topic")?;
    scope.set_endpoint(format!("KAFKA find {topic} /{pattern}/"));
    let timeout_ms = match &args[1] {
        Value::Number(n) if *n >= 0.0 => *n as u64,
        _ => {
            return Err(EvalError::new(
                "cursor.find(regex, timeout) expects timeout to be a non-negative number of ms — e.g. 30s",
            ))
        }
    };
    let bootstrap = cursor_field(cursor, "bootstrap")?;
    let marks = cursor_marks(cursor);

    let result: Result<Value, String> = run_async(async move {
        let client = ClientBuilder::new(vec![bootstrap])
            .build()
            .await
            .map_err(|e| format!("kafka connect failed: {e}"))?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // (partition, client, next-offset-to-read). Built lazily so a topic
        // created after `since` (or after `find` starts) is still picked up.
        let mut readers: Vec<(i32, PartitionClient, i64)> = Vec::new();

        loop {
            if readers.is_empty() {
                readers = open_readers(&client, &topic, &marks).await?;
            }

            for (p, pc, pos) in readers.iter_mut() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(Value::Null);
                }
                let wait = (remaining.as_millis() as i32).clamp(0, FETCH_WAIT_MS);
                let (records, _high) = pc
                    .fetch_records(*pos, 1..FETCH_MAX_BYTES, wait)
                    .await
                    .map_err(|e| format!("kafka fetch({topic}/{p}) failed: {e}"))?;
                for ro in records {
                    *pos = ro.offset + 1;
                    let raw = String::from_utf8_lossy(
                        ro.record.value.as_deref().unwrap_or(&[]),
                    )
                    .into_owned();
                    if re.is_match(&raw) {
                        return Ok(message(*p, &ro, raw));
                    }
                }
            }

            if Instant::now() >= deadline {
                return Ok(Value::Null);
            }
            // Topic still absent — nap (bounded by the deadline) then re-look.
            if readers.is_empty() {
                let nap = Duration::from_millis(REDISCOVER_NAP_MS)
                    .min(deadline.saturating_duration_since(Instant::now()));
                if nap.is_zero() {
                    return Ok(Value::Null);
                }
                tokio::time::sleep(nap).await;
            }
        }
    });
    result.map_err(EvalError::new)
}

/// Open a `PartitionClient` per partition of `topic`, each positioned at its
/// mark offset (or the partition's earliest offset when the mark doesn't cover
/// it — e.g. the topic didn't exist at `since` time). Empty if the topic still
/// doesn't exist.
async fn open_readers(
    client: &Client,
    topic: &str,
    marks: &HashMap<i32, i64>,
) -> Result<Vec<(i32, PartitionClient, i64)>, String> {
    let mut readers = Vec::new();
    for p in topic_partitions(client, topic).await? {
        let pc = client
            .partition_client(topic.to_string(), p, UnknownTopicHandling::Error)
            .await
            .map_err(|e| format!("kafka partition_client({topic}/{p}) failed: {e}"))?;
        let start = match marks.get(&p) {
            Some(o) => *o,
            None => pc
                .get_offset(OffsetAt::Earliest)
                .await
                .map_err(|e| format!("kafka get_offset earliest ({topic}/{p}) failed: {e}"))?,
        };
        readers.push((p, pc, start));
    }
    Ok(readers)
}

/// Build the message value handed back to the DSL. Mirrors `_response`: the
/// payload is sniffed into `body` + `format` exactly like an HTTP response, with
/// `raw` preserving the untouched string the regex matched against.
fn message(partition: i32, ro: &RecordAndOffset, raw: String) -> Value {
    let (body, format) = crate::http::parse_payload(&raw);
    let key = ro
        .record
        .key
        .as_deref()
        .map(|k| Value::String(String::from_utf8_lossy(k).into_owned()))
        .unwrap_or(Value::Null);
    let headers: ValueMap = ro
        .record
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(String::from_utf8_lossy(v).into_owned())))
        .collect();
    Value::Object(ValueMap::from([
        ("body".to_string(), body),
        ("raw".to_string(), Value::String(raw)),
        ("format".to_string(), Value::String(format)),
        ("key".to_string(), key),
        ("partition".to_string(), Value::Number(partition as f64)),
        ("offset".to_string(), Value::Number(ro.offset as f64)),
        (
            "timestamp".to_string(),
            Value::Number(ro.record.timestamp.timestamp_millis() as f64),
        ),
        ("headers".to_string(), Value::Object(headers)),
    ]))
}

/// Read a required string field off a cursor handle.
fn cursor_field(cursor: &Value, field: &str) -> Result<String, EvalError> {
    match cursor.get_field(field) {
        Value::String(s) => Ok(s),
        _ => Err(EvalError::new(format!(
            "kafka cursor is missing its {field}"
        ))),
    }
}

/// Extract the cursor's mark offsets as `partition -> offset`.
fn cursor_marks(cursor: &Value) -> HashMap<i32, i64> {
    let mut marks = HashMap::new();
    if let Value::Object(offs) = cursor.get_field("offsets") {
        for (k, v) in offs {
            if let (Ok(p), Value::Number(o)) = (k.parse::<i32>(), &v) {
                marks.insert(p, *o as i64);
            }
        }
    }
    marks
}

/// Current wall-clock as a chrono `DateTime<Utc>` for a Record timestamp.
/// Built from `SystemTime` millis because rskafka pulls chrono without its
/// `clock` feature, so `Utc::now()` isn't available — but `timestamp_millis_opt`
/// is. Matches how the `$.now()` builtin reads the clock.
fn now_utc() -> rskafka::chrono::DateTime<rskafka::chrono::Utc> {
    use rskafka::chrono::TimeZone;
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    rskafka::chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_default()
}

/// `{ partition, offset }` — the result of a successful produce.
fn ack(partition: i32, offset: i64) -> Value {
    Value::Object(ValueMap::from([
        ("partition".to_string(), Value::Number(partition as f64)),
        ("offset".to_string(), Value::Number(offset as f64)),
    ]))
}

/// Read the `bootstrap` field off a broker handle.
fn broker_bootstrap(broker: &Value) -> Result<String, EvalError> {
    match broker.get_field("bootstrap") {
        Value::String(s) => Ok(s),
        _ => Err(EvalError::new("kafka broker handle is missing its bootstrap address")),
    }
}

/// Coerce a DSL value to a message payload, mirroring HTTP body handling:
/// strings raw, objects/arrays as JSON, `null` → tombstone (no bytes).
fn value_to_payload(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone().into_bytes()),
        Value::Object(_) | Value::Array(_) => {
            Some(crate::http::value_to_json_string(v).into_bytes())
        }
        _ => Some(v.to_display_string().into_bytes()),
    }
}

/// Require a value to be a string, with a Kafka-flavored error otherwise.
fn as_string(v: &Value, what: &str) -> Result<String, EvalError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(EvalError::new(format!(
            "kafka: expected {what} to be a string, got {}",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh scope for ops that record an endpoint.
    fn sc() -> Scope {
        Scope::new()
    }

    /// A handle from a bare bootstrap string (no requirements set).
    fn handle(bootstrap: &str) -> Value {
        broker_from_config(&Value::String(bootstrap.to_string())).unwrap()
    }

    /// Set a dynamic field on a handle (what `k.topic = …` does in the DSL).
    fn set(mut h: Value, field: &str, v: Value) -> Value {
        if let Value::Object(ref mut m) = h {
            m.insert(field.to_string(), v);
        }
        h
    }

    #[test]
    fn kind_of_reads_tag() {
        let h = handle("localhost:9092");
        assert_eq!(kind_of(&h).as_deref(), Some(kind::BROKER));
    }

    #[test]
    fn kind_of_untagged_is_none() {
        assert_eq!(kind_of(&Value::Object(ValueMap::new())), None);
        assert_eq!(kind_of(&Value::Number(1.0)), None);
        assert_eq!(kind_of(&Value::Null), None);
    }

    #[test]
    fn run_async_drives_a_future() {
        assert_eq!(run_async(async { 40 + 2 }), 42);
    }

    #[test]
    fn handle_from_string_defaults() {
        let h = handle("localhost:9092");
        assert_eq!(broker_bootstrap(&h).unwrap(), "localhost:9092");
        assert_eq!(h.get_field("requiresTypeId"), Value::Bool(false));
        assert_eq!(h.get_field("requiresKey"), Value::Bool(false));
        // dynamic fields start unset
        assert_eq!(h.get_field("topic"), Value::Null);
        assert_eq!(h.get_field("key"), Value::Null);
        assert_eq!(h.get_field("headers"), Value::Null);
    }

    #[test]
    fn config_object_reads_globals() {
        let cfg = Value::Object(ValueMap::from([
            ("bootstrap".to_string(), Value::String("host:9092".into())),
            ("requiresTypeId".to_string(), Value::Bool(true)),
            ("requiresKey".to_string(), Value::Bool(true)),
        ]));
        let h = broker_from_config(&cfg).unwrap();
        assert_eq!(broker_bootstrap(&h).unwrap(), "host:9092");
        assert_eq!(h.get_field("requiresTypeId"), Value::Bool(true));
        assert_eq!(h.get_field("requiresKey"), Value::Bool(true));
    }

    #[test]
    fn config_object_requires_bootstrap() {
        let cfg = Value::Object(ValueMap::from([(
            "requiresKey".to_string(),
            Value::Bool(true),
        )]));
        assert!(broker_from_config(&cfg).unwrap_err().to_string().contains("bootstrap"));
        // a bare number is neither a string nor a config object
        assert!(broker_from_config(&Value::Number(1.0)).is_err());
    }

    #[test]
    fn payload_serialization_matches_http_bodies() {
        assert_eq!(value_to_payload(&Value::String("hi".into())), Some(b"hi".to_vec()));
        assert_eq!(value_to_payload(&Value::Null), None); // tombstone
        let obj = Value::Object(ValueMap::from([("a".to_string(), Value::Number(1.0))]));
        assert_eq!(value_to_payload(&obj), Some(br#"{"a":1}"#.to_vec()));
        assert_eq!(value_to_payload(&Value::Number(3.0)), Some(b"3".to_vec()));
    }

    #[test]
    fn headers_from_variants() {
        assert!(headers_from(&Value::Null).unwrap().is_empty());
        let h = headers_from(&Value::Object(ValueMap::from([
            ("__TypeId__".to_string(), Value::String("com.foo.Bar".into())),
            ("n".to_string(), Value::Number(7.0)),
        ])))
        .unwrap();
        assert_eq!(h.get("__TypeId__").map(|v| v.as_slice()), Some(b"com.foo.Bar".as_slice()));
        assert_eq!(h.get("n").map(|v| v.as_slice()), Some(b"7".as_slice())); // stringified
        assert!(headers_from(&Value::String("nope".into())).is_err());
    }

    #[test]
    fn send_requires_topic_and_one_arg() {
        let h = handle("localhost:9092"); // no .topic set
        assert!(send(&h, &[Value::String("v".into())], &sc())
            .unwrap_err()
            .to_string()
            .contains("topic"));
        // arity: exactly one value
        let h = set(handle("localhost:9092"), "topic", Value::String("t".into()));
        assert!(send(&h, &[], &sc()).is_err());
        assert!(send(&h, &[Value::Null, Value::Null], &sc()).is_err());
    }

    #[test]
    fn send_enforces_required_key() {
        // requiresKey with no .key set → error before any network work.
        let cfg = Value::Object(ValueMap::from([
            ("bootstrap".to_string(), Value::String("localhost:9092".into())),
            ("requiresKey".to_string(), Value::Bool(true)),
        ]));
        let h = set(broker_from_config(&cfg).unwrap(), "topic", Value::String("t".into()));
        let err = send(&h, &[Value::String("v".into())], &sc()).unwrap_err().to_string();
        assert!(err.contains("requiresKey"), "got: {err}");
    }

    #[test]
    fn send_enforces_required_type_id() {
        // requiresTypeId with headers lacking __TypeId__ → error before network.
        let cfg = Value::Object(ValueMap::from([
            ("bootstrap".to_string(), Value::String("localhost:9092".into())),
            ("requiresTypeId".to_string(), Value::Bool(true)),
        ]));
        let mut h = set(broker_from_config(&cfg).unwrap(), "topic", Value::String("t".into()));
        // headers present but missing __TypeId__
        h = set(h, "headers", Value::Object(ValueMap::from([(
            "x".to_string(),
            Value::String("y".into()),
        )])));
        let err = send(&h, &[Value::String("v".into())], &sc()).unwrap_err().to_string();
        assert!(err.contains("__TypeId__"), "got: {err}");
    }

    #[test]
    fn dispatch_rejects_unknown_method() {
        let h = handle("localhost:9092");
        let err = dispatch_method(kind::BROKER, &h, "frobnicate", &[], &sc()).unwrap_err();
        assert!(err.to_string().contains("frobnicate"));
    }

    #[test]
    fn cursor_shape_and_offsets() {
        let c = cursor("localhost:9092", "orders", &[(0, 5), (1, 3)]);
        assert_eq!(kind_of(&c).as_deref(), Some(kind::CURSOR));
        assert_eq!(c.get_field("topic"), Value::String("orders".into()));
        let offs = c.get_field("offsets");
        assert_eq!(offs.get_field("0"), Value::Number(5.0));
        assert_eq!(offs.get_field("1"), Value::Number(3.0));
    }

    #[test]
    fn cursor_with_no_partitions_has_empty_offsets() {
        let c = cursor("localhost:9092", "ghost", &[]);
        match c.get_field("offsets") {
            Value::Object(m) => assert!(m.is_empty()),
            other => panic!("expected empty offsets object, got {other:?}"),
        }
    }

    #[test]
    fn since_needs_topic_and_no_args() {
        let h = handle("localhost:9092"); // no .topic
        assert!(since(&h, &[], &sc()).unwrap_err().to_string().contains("topic"));
        let h = set(handle("localhost:9092"), "topic", Value::String("t".into()));
        assert!(since(&h, &[Value::String("extra".into())], &sc()).is_err());
    }

    #[test]
    fn find_validates_args() {
        let c = cursor("localhost:9092", "t", &[(0, 0)]);
        assert!(find(&c, &[Value::String("x".into())], &sc()).is_err()); // arity
        let bad = find(&c, &[Value::String("(unclosed".into()), Value::Number(10.0)], &sc());
        assert!(bad.unwrap_err().to_string().contains("invalid regex"));
        assert!(find(&c, &[Value::String("x".into()), Value::String("10s".into())], &sc()).is_err());
    }

    #[test]
    fn cursor_marks_parses_offsets() {
        let c = cursor("localhost:9092", "t", &[(0, 7), (2, 3)]);
        let marks = cursor_marks(&c);
        assert_eq!(marks.get(&0), Some(&7));
        assert_eq!(marks.get(&2), Some(&3));
        assert_eq!(marks.get(&1), None);
    }

    #[test]
    fn message_shaping_mirrors_response() {
        let ro = RecordAndOffset {
            record: Record {
                key: Some(b"k1".to_vec()),
                value: Some(br#"{"status":"ok"}"#.to_vec()),
                headers: BTreeMap::from([("__TypeId__".to_string(), b"com.foo.Bar".to_vec())]),
                timestamp: now_utc(),
            },
            offset: 12,
        };
        let raw = String::from_utf8_lossy(ro.record.value.as_deref().unwrap()).into_owned();
        let m = message(1, &ro, raw);
        assert_eq!(m.get_field("body").get_field("status"), Value::String("ok".into()));
        assert_eq!(m.get_field("format"), Value::String("json".into()));
        assert_eq!(m.get_field("raw"), Value::String(r#"{"status":"ok"}"#.into()));
        assert_eq!(m.get_field("key"), Value::String("k1".into()));
        assert_eq!(m.get_field("partition"), Value::Number(1.0));
        assert_eq!(m.get_field("offset"), Value::Number(12.0));
        assert_eq!(
            m.get_field("headers").get_field("__TypeId__"),
            Value::String("com.foo.Bar".into())
        );
    }

    /// Live: mark via a handle's `.topic`, send 3, `since` reports end offset 3.
    /// Skipped unless `TSTR_KAFKA_TEST_BROKER` is set.
    #[test]
    fn since_captures_end_offset_live() {
        let addr = match std::env::var("TSTR_KAFKA_TEST_BROKER") {
            Ok(a) if !a.is_empty() => a,
            _ => {
                eprintln!("skipping since_captures_end_offset_live (set TSTR_KAFKA_TEST_BROKER=host:9092)");
                return;
            }
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // Ghost topic → empty offsets.
        let ghost = set(handle(&addr), "topic", Value::String(format!("tstr-ghost-{nonce}")));
        match since(&ghost, &[], &sc()).unwrap().get_field("offsets") {
            Value::Object(m) => assert!(m.is_empty(), "ghost topic should have no offsets"),
            other => panic!("expected object, got {other:?}"),
        }

        let topic = format!("tstr-since-{nonce}");
        let h = set(handle(&addr), "topic", Value::String(topic.clone()));
        for i in 0..3 {
            send(&h, &[Value::Number(i as f64)], &sc()).unwrap();
        }
        let cur = since(&h, &[], &sc()).unwrap();
        assert_eq!(cur.get_field("offsets").get_field("0"), Value::Number(3.0));
    }

    /// Live: mark, send a message carrying a key + `__TypeId__` header, then
    /// `find` catches it by full-body regex and the key/header round-trip. Also
    /// checks a non-matching pattern times out to Null. Skipped unless
    /// `TSTR_KAFKA_TEST_BROKER` is set.
    #[test]
    fn send_find_headers_key_round_trip_live() {
        let addr = match std::env::var("TSTR_KAFKA_TEST_BROKER") {
            Ok(a) if !a.is_empty() => a,
            _ => {
                eprintln!("skipping send_find_headers_key_round_trip_live (set TSTR_KAFKA_TEST_BROKER=host:9092)");
                return;
            }
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let topic = format!("tstr-send-{nonce}");
        let needle = format!("needle-{nonce}");

        // Mark before the topic exists (empty marks → find reads earliest).
        let marker = set(handle(&addr), "topic", Value::String(topic.clone()));
        let cur = since(&marker, &[], &sc()).unwrap();

        // Configure the send handle: topic + key + __TypeId__ header.
        let mut h = set(handle(&addr), "topic", Value::String(topic.clone()));
        h = set(h, "key", Value::String("site-42".into()));
        h = set(
            h,
            "headers",
            Value::Object(ValueMap::from([(
                "__TypeId__".to_string(),
                Value::String("com.foo.OrderEvent".into()),
            )])),
        );
        let ack = send(
            &h,
            &[Value::Object(ValueMap::from([(
                "id".to_string(),
                Value::String(needle.clone()),
            )]))],
            &sc(),
        )
        .expect("send failed");
        assert_eq!(ack.get_field("partition"), Value::Number(0.0));

        let msg = find(&cur, &[Value::String(needle.clone()), Value::Number(10_000.0)], &sc())
            .expect("find errored");
        assert_eq!(msg.get_field("body").get_field("id"), Value::String(needle));
        assert_eq!(msg.get_field("key"), Value::String("site-42".into()));
        assert_eq!(
            msg.get_field("headers").get_field("__TypeId__"),
            Value::String("com.foo.OrderEvent".into())
        );

        let none = find(&cur, &[Value::String("no-such-thing".into()), Value::Number(500.0)], &sc())
            .expect("find errored");
        assert_eq!(none, Value::Null);
    }
}

