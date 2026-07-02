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
use crate::value::Value;

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
pub fn kind_of(value: &Value) -> Option<String> {
    match value.get_field(KIND_FIELD) {
        Value::String(s) => Some(s),
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

/// `$.kafka("host:9092")` → a broker handle. Just tagged config data; no
/// connection is opened until the first `produce`/`since`/`find`.
pub fn broker(bootstrap: &str) -> Value {
    Value::Object(std::collections::HashMap::from([
        (KIND_FIELD.to_string(), Value::String(kind::BROKER.to_string())),
        ("bootstrap".to_string(), Value::String(bootstrap.to_string())),
    ]))
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
        (kind::BROKER, "produce") => produce(obj, args, scope),
        (kind::BROKER, "since") => since(obj, args, scope),
        (kind::BROKER, m) => Err(EvalError::new(format!(
            "unknown Kafka broker method '.{m}()'"
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

/// `broker.produce(topic, value [, key])` → `{ partition, offset }` ack.
/// `value`/`key` are serialized like an HTTP body: strings go raw, objects and
/// arrays are JSON-encoded, `null` value means a tombstone (no payload).
fn produce(broker: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(EvalError::new(
            "broker.produce(topic, value [, key]) takes 2 or 3 arguments",
        ));
    }
    let bootstrap = broker_bootstrap(broker)?;
    let topic = as_string(&args[0], "topic")?;
    scope.set_endpoint(format!("KAFKA produce {topic}"));
    let value = value_to_payload(&args[1]);
    let key = match args.get(2) {
        None | Some(Value::Null) => None,
        Some(k) => Some(as_string(k, "key")?.into_bytes()),
    };

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
            headers: BTreeMap::new(),
            timestamp: now_utc(),
        };
        partition
            .produce(vec![record], Compression::NoCompression)
            .await
            .map_err(|e| format!("kafka produce to '{topic}' failed: {e}"))
    })?;

    let offset = offsets.first().copied().unwrap_or(-1);
    Ok(ack(PRODUCE_PARTITION, offset))
}

/// `broker.since(topic)` → a cursor marking the topic's current end offsets,
/// one per partition. Seeking back to this mark later (`cursor.find`, phase 4)
/// reads exactly the messages produced *after* this call — the tight window.
///
/// A topic that doesn't exist yet yields an **empty** offsets map rather than an
/// error: there are no prior messages to skip, so `find` will read each
/// partition from the earliest offset once the topic appears. That's what makes
/// `since` safe to call before the downstream service has created the topic.
fn since(broker: &Value, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::new("broker.since(topic) takes 1 argument"));
    }
    let bootstrap = broker_bootstrap(broker)?;
    let topic = as_string(&args[0], "topic")?;
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
    let off_map: std::collections::HashMap<String, Value> = offsets
        .iter()
        .map(|(p, o)| (p.to_string(), Value::Number(*o as f64)))
        .collect();
    Value::Object(std::collections::HashMap::from([
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
    let headers: HashMap<String, Value> = ro
        .record
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(String::from_utf8_lossy(v).into_owned())))
        .collect();
    Value::Object(HashMap::from([
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
    Value::Object(std::collections::HashMap::from([
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
    use std::collections::HashMap;

    /// Fresh scope for exercising ops that now record an endpoint.
    fn sc() -> Scope {
        Scope::new()
    }

    #[test]
    fn kind_of_reads_tag() {
        let broker = Value::Object(HashMap::from([
            (KIND_FIELD.to_string(), Value::String(kind::BROKER.to_string())),
            ("bootstrap".to_string(), Value::String("localhost:9092".to_string())),
        ]));
        assert_eq!(kind_of(&broker).as_deref(), Some(kind::BROKER));
    }

    #[test]
    fn kind_of_untagged_is_none() {
        assert_eq!(kind_of(&Value::Object(HashMap::new())), None);
        assert_eq!(kind_of(&Value::Number(1.0)), None);
        assert_eq!(kind_of(&Value::Null), None);
    }

    #[test]
    fn run_async_drives_a_future() {
        assert_eq!(run_async(async { 40 + 2 }), 42);
    }

    #[test]
    fn broker_is_a_tagged_handle() {
        let b = broker("localhost:9092");
        assert_eq!(kind_of(&b).as_deref(), Some(kind::BROKER));
        assert_eq!(broker_bootstrap(&b).unwrap(), "localhost:9092");
    }

    #[test]
    fn payload_serialization_matches_http_bodies() {
        // strings go raw (no JSON quoting)
        assert_eq!(value_to_payload(&Value::String("hi".into())), Some(b"hi".to_vec()));
        // null is a tombstone — no bytes
        assert_eq!(value_to_payload(&Value::Null), None);
        // objects are JSON-encoded
        let obj = Value::Object(HashMap::from([("a".to_string(), Value::Number(1.0))]));
        assert_eq!(value_to_payload(&obj), Some(br#"{"a":1}"#.to_vec()));
        // non-string scalars fall back to display form
        assert_eq!(value_to_payload(&Value::Number(3.0)), Some(b"3".to_vec()));
    }

    #[test]
    fn produce_arg_arity_is_enforced() {
        let b = broker("localhost:9092");
        assert!(produce(&b, &[Value::String("t".into())], &sc()).is_err());
        assert!(produce(&b, &[], &sc()).is_err());
    }

    #[test]
    fn dispatch_rejects_unknown_broker_method() {
        let b = broker("localhost:9092");
        let err = dispatch_method(kind::BROKER, &b, "frobnicate", &[], &sc()).unwrap_err();
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
    fn since_arity_is_enforced() {
        let b = broker("localhost:9092");
        assert!(since(&b, &[], &sc()).is_err());
        assert!(since(&b, &[Value::String("a".into()), Value::String("b".into())], &sc()).is_err());
    }

    /// Live: produce N records, then `since` must report end offset == N for
    /// partition 0. Also checks a never-seen topic yields empty offsets.
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
        let b = broker(&addr);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // A topic we never touch → empty offsets, no error.
        let ghost = since(&b, &[Value::String(format!("tstr-ghost-{nonce}"))], &sc()).unwrap();
        match ghost.get_field("offsets") {
            Value::Object(m) => assert!(m.is_empty(), "ghost topic should have no offsets"),
            other => panic!("expected object, got {other:?}"),
        }

        // Produce 3, then the mark should sit at offset 3 on partition 0.
        let topic = format!("tstr-since-{nonce}");
        for i in 0..3 {
            produce(&b, &[Value::String(topic.clone()), Value::Number(i as f64)], &sc()).unwrap();
        }
        let cur = since(&b, &[Value::String(topic)], &sc()).unwrap();
        assert_eq!(cur.get_field("offsets").get_field("0"), Value::Number(3.0));
    }

    #[test]
    fn find_validates_args() {
        let c = cursor("localhost:9092", "t", &[(0, 0)]);
        // wrong arity
        assert!(find(&c, &[Value::String("x".into())], &sc()).is_err());
        // bad regex
        let bad = find(&c, &[Value::String("(unclosed".into()), Value::Number(10.0)], &sc());
        assert!(bad.unwrap_err().to_string().contains("invalid regex"));
        // non-numeric timeout
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
                headers: BTreeMap::new(),
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
    }

    /// Live: mark before an action, produce a matching message, `find` catches
    /// it by full-body regex; a non-matching pattern returns Null within its
    /// timeout. Exercises late topic creation too (topic is created by the
    /// produce, after `since`). Skipped unless `TSTR_KAFKA_TEST_BROKER` is set.
    #[test]
    fn find_catches_and_times_out_live() {
        let addr = match std::env::var("TSTR_KAFKA_TEST_BROKER") {
            Ok(a) if !a.is_empty() => a,
            _ => {
                eprintln!("skipping find_catches_and_times_out_live (set TSTR_KAFKA_TEST_BROKER=host:9092)");
                return;
            }
        };
        let b = broker(&addr);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let topic = format!("tstr-find-{nonce}");
        let needle = format!("needle-{nonce}");

        // Mark before the topic even exists → empty marks; find reads earliest.
        let cur = since(&b, &[Value::String(topic.clone())], &sc()).unwrap();
        produce(
            &b,
            &[
                Value::String(topic.clone()),
                Value::Object(HashMap::from([("id".to_string(), Value::String(needle.clone()))])),
            ],
            &sc(),
        )
        .unwrap();

        let msg = find(&cur, &[Value::String(needle.clone()), Value::Number(10_000.0)], &sc())
            .expect("find errored");
        assert_eq!(msg.get_field("body").get_field("id"), Value::String(needle));
        assert_eq!(msg.get_field("partition"), Value::Number(0.0));

        // Nothing matches this → Null within the (short) timeout.
        let none = find(&cur, &[Value::String("no-such-thing".into()), Value::Number(500.0)], &sc())
            .expect("find errored");
        assert_eq!(none, Value::Null);
    }

    /// Live round-trip against a real broker. Skipped unless
    /// `TSTR_KAFKA_TEST_BROKER=host:9092` is set, so the default suite stays
    /// hermetic. Produces to a fresh topic and asserts the first record lands
    /// at offset 0 (proof the broker accepted and sequenced it).
    #[test]
    fn produce_to_real_broker() {
        let addr = match std::env::var("TSTR_KAFKA_TEST_BROKER") {
            Ok(a) if !a.is_empty() => a,
            _ => {
                eprintln!("skipping produce_to_real_broker (set TSTR_KAFKA_TEST_BROKER=host:9092)");
                return;
            }
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let topic = format!("tstr-produce-test-{nonce}");
        let b = broker(&addr);
        let args = [
            Value::String(topic),
            Value::Object(HashMap::from([("hello".to_string(), Value::String("world".to_string()))])),
        ];
        let ack = produce(&b, &args, &sc()).expect("produce failed");
        assert_eq!(ack.get_field("partition"), Value::Number(0.0));
        assert_eq!(ack.get_field("offset"), Value::Number(0.0));
    }
}
