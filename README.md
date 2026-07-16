# tstr

A CLI HTTP API test runner with a custom DSL. Structural execution model (phase → directory → lex order), library functions as first-class primitives, project-wide constants via `tstr.yaml`, and per-directory introspection.

## Quick Start

```bash
cargo build --release
ln -s ~/dev/tstr/target/release/tstr ~/bin/tstr

tstr run                          # run all tests (walks up to find tstr.yaml)
tstr run notify                   # scope the run to the notify/ subdirectory
tstr list                         # per-directory tables
tstr list --type lib              # libraries instead of tests
tstr list --disabled              # disabled tests + their reasons
```

## Mental Model

**Every `.tstr` file is a function.** Its role is determined by the middle extension:

| Extension | Role | Auto-runs? | Output goes to |
|---|---|---|---|
| `*.const.tstr` | Constants | yes (phase 1) | ambient scope of its dir + descendants |
| `*.setup.tstr` | Setup (broadcast) | yes (phase 2) | ambient scope of its dir + descendants |
| `*.test.tstr` / `*.tstr` | Test (assertions) | yes (phase 3) | not propagated |
| `*.cleanup.tstr` | Cleanup | yes (phase 4) | not propagated |
| `*.lib.tstr` | Library (callable) | **no** — invoked on demand | bound at call site |

**Execution rules:**

- Within each directory, phases run in order: const → setup → test → cleanup. It's not one global sweep — a directory's children run in parallel *between* its setup and its tests, so a child's tests can run while the parent sits between phases.
- Within a phase + within a directory: files run in **lex order**
- Within a directory: sequential (no in-dir parallelism)
- Across sibling directories: parallel (rayon work-stealing pool; `-j N` or `RAYON_NUM_THREADS` to tune)
- Parent-dir setups cascade to children's scope
- `lib.tstr` files never auto-run — only when called

**Skip on unavailable inputs (failure propagation).** A test isn't run when
its inputs aren't there — it's **skipped**, not failed, so a single broken
setup doesn't bury the run under a pile of cascading failures:

- A file that declares inputs (`orgId -->`) is skipped if any declared input
  is null/absent in scope. Reason: `input parameter 'orgId' not available`.
- If a const/setup doesn't complete cleanly (fails, is skipped, or is
  `disabled`), every dependent file in that directory **and its descendants**
  is skipped. Dependents that lean on ambient vars without declaring them
  (e.g. `req`) are still caught, citing the broken setup's declared outputs:
  `input parameter 'orgId' not available (setup '00-login' failed)`.
- The broken setup itself still reports as **FAIL** (the one real problem to
  fix); the run's exit code reflects it. The cascade becomes skips, not noise.

This automatic skipping covers most "bail if a precondition is missing" cases:
a dependent that declares `orgId -->` is skipped before it runs, so you rarely
need to guard on it by hand. When you *do* want conditional behavior within a
file, use `if` (see below).

**Tests live in leaf directories; scaffolding lives above them.** The directory
hierarchy splits cleanly by role, and the split is enforced at startup:

- A `.test.tstr` / `.fetch.tstr` is allowed **only in a leaf** (a directory with
  no subdirectories). A test in a non-leaf dir is a hard error.
- A `.setup.tstr` / `.cleanup.tstr` is allowed **only in a non-leaf** dir, where
  it scaffolds the leaves below it — its setups cascade down, its cleanups tear
  down afterward. A setup/cleanup in a leaf is a hard error (move it to a
  non-leaf parent, or rename it to `.test` if it's really just a test).
- `const` and `lib` files are allowed anywhere.

This makes "test group = leaf directory" an invariant (one slot per group, each
leaf's tests sequential, leaves parallel), and keeps `setup`/`cleanup`
unambiguously in their scaffolding role: cascade-blocking when they fail, and
hidden from the slot display / summary table.

**Two state-sharing mechanisms (picked deliberately):**

- **Setup files: broadcast.** `export a, b` (named bindings) merges into ambient scope for subsequent files.
- **Library functions: request/response.** Called explicitly with site-local args; return value bound at the call site.

Filename order matters. Use numeric prefixes (`01-`, `02-`, ...) when you want explicit ordering. Zero-pad to avoid lex-sort surprises (`02-` vs `10-`, not `2-` vs `10-`).

## Configuration: `tstr.yaml`

A YAML file at the suite root holds CLI defaults and project-wide constants. **It does not encode test structure** — that's the filesystem's job. Its *presence* marks the suite root: the runner walks up from cwd until it finds one.

```yaml
defaults:
  import:
    - ~/.tstr/shared-libs
    - /opt/corp/tstr-libs
  display: bars
  repeat_mode: concurrent # how `--repeat N` runs: sequential (default) or concurrent.
                          # Declares the suite's own safety; --repeat-mode overrides it.

log_retention: 10        # per-run logs to keep under <root>/logs/ (0 = keep all; default 10)

constants:
  apiVersion: v4
  orgService:
    baseUrl: https://api.example.com/${apiVersion}
    auth: bearer ${ORG_TOKEN}
    headers:
      X-Tenant: prod
```

**Loading order** (later overrides earlier):

1. ALL_CAPS environment variables (seeded as the lowest-priority constants layer)
2. `~/.config/tstr/config.yaml` — user global
3. `<suite-root>/tstr.yaml` — project local
4. `--config <path>` — explicit CLI override

Under `defaults`, scalars replace and lists append (so `--import` adds to defaults
rather than replacing).

**Constants deep-merge.** When two layers define the same object constant, their
keys union and the later layer wins only on the fields it actually sets. This lets
a user config and a project config co-own one object — the developer supplies the
machine-specific fields, the project supplies the shared ones:

```yaml
# ~/.config/tstr/config.yaml — per-developer
constants:
  db:
    host: layer-dk-do-user-1149.b.db.ondigitalocean.com
    port: 25060
    sslInsecure: true      # a field the project file doesn't set

# ./tstr.yaml — checked in, shared
constants:
  db:
    database: notify       # per-suite
    sslmode: require
```

The handle `$.postgres(${db})` sees all five fields. Anything that isn't a mapping
— scalars *and lists* — is replaced wholesale by the later layer rather than
merged, since a parent layer's leftover list elements riding along is rarely what
anyone means.

Note the precedence direction: the project layer loads *after* the user layer, so
it wins on any field it sets. A developer can **add** fields the project omits, but
can't override one the project defines. Use `--set` or `--config` for that.

### Interpolation Inside `tstr.yaml`

Constant string values can reference other constants via `${name}` — including
environment variables and constants from a higher layer. References resolve after
all layers merge, recursively (cycles are a load-time error):

```yaml
# ~/.config/tstr/config.yaml — user global
constants:
  namespace: dk

# ./tstr.yaml — project
constants:
  apiVersion: v4
  apiHost: "profile.${namespace}:8080"          # -> profile.dk:8080  (from user layer)
  orgService:
    baseUrl: "http://${apiHost}/${apiVersion}"  # -> http://profile.dk:8080/v4
    auth: "bearer ${ORG_TOKEN}"                 # -> from $ORG_TOKEN in the environment
```

- **Env vars must be ALL_CAPS** (`[A-Z][A-Z0-9_]*`) to be visible — keeps them from
  colliding with camelCase yaml constants.
- A `${X}` that resolves to neither a constant nor an env var is a **load-time error**
  naming the offending reference. (So yaml referencing `${ORG_TOKEN}` requires
  `ORG_TOKEN` to be set.)
- Only string values are walked; numbers and bools get stringified when substituted
  into a string, but objects/lists can't be inlined.

### Secrets: `!secret <path>`

A constant tagged `!secret` takes its value from a file instead of the yaml, so a
password never sits in a config you might paste, commit, or open in front of an
audience:

```yaml
# ~/.config/tstr/config.yaml
constants:
  dbPassword: !secret ~/.config/tstr/pgpass
  db: postgres://doadmin:${dbPassword}@${dbHost}:${dbPort}/defaultdb
```

The file is read as UTF-8 and **one trailing newline is stripped** (the shape
`printf 'pw\n' > pgpass` leaves) — a password with a stray `\n` fails
authentication in a way that points nowhere near this config.

Paths resolve as:

| Path | Resolves against |
|---|---|
| `~/.config/tstr/pgpass` | `$HOME` |
| `/etc/tstr/pgpass` | taken as-is |
| `pgpass` | **the directory of the config file that declares it** |

A relative path is *not* relative to the working directory, so a suite behaves
the same whether you run it from its own root or from anywhere else. Each config
layer resolves its own relative paths, so a `pgpass` named in
`~/.config/tstr/config.yaml` is found next to that file, not next to the
project's `tstr.yaml`.

Secret tags resolve *before* `${name}` substitution, so a secret composes into
other constants normally, as `db` does above.

tstr then **masks the value in its own output**. Anywhere a report, variable
table, or run log would print a secret — on its own or buried inside a composed
string — it renders as `[redacted]`:

```
+ conn = postgres://doadmin:[redacted]@db.example.com:25060/defaultdb
+ pw = [redacted]
```

Two things this deliberately does *not* do:

- **It doesn't mask on the wire.** Requests, queries, and Kafka payloads carry the
  real value — that's the point of having it. Masking applies to display only.
- **It doesn't protect short values.** Secrets under 6 characters are never
  registered, since masking by content would censor unrelated output that happens
  to contain the same substring.

Reading an unreadable or missing file is a load-time error, as is any tag other
than `!secret`.

## Constants and Variables

Three categories of named values:

- **`${name}` — constants, bare-expression form.** Sourced from yaml `constants:` (and, future: `.const.tstr` returns). Immutable. Dotted access works: `${orgService.baseUrl}`. Use it where an expression is expected — assignments, arguments, JSON values. **`${name}` is NOT interpolated inside string literals** (a `${...}` sequence inside `"..."` is passed through verbatim, since `$`-templating commonly appears in API payloads). To put a constant inside a string, use `{{name}}`.
- **`name` (bare) — ambient scope variables.** Published by `setup.tstr` `export` statements. Scope-bound to the publishing file's directory, cascading to children.
- **`{{name}}` — string interpolation.** The in-string form. Resolves a name against **ambient scope first, then the constants namespace**, so it works for both. Dotted access works: `{{orgService.baseUrl}}`.

```
req = ${orgService};                        # constant as a bare expression
url = "/orgs/{{orgId}}";                     # ambient var, inside a string
auth = "bearer {{apiToken}}";                # constant, inside a string — {{}} resolves it
id  = ${ACTION_X};                           # constant as a bare JSON/expression value
```

Rule of thumb: **inside a string literal, always use `{{name}}`** (resolves ambient or constant). Use `${name}` only where a bare expression is expected.

## File Body: Statements

Files are sequences of statements. Semicolons terminate every statement. `//` line comments, `/* */` block comments. Whitespace is cosmetic.

### `export`

The output mechanism. Publishes named bindings — a comma list of
`expr [as name]` (bare identifier self-names; computed needs `as`):

```
export r.id as tagId, r.name as tagName;
export tagId;                              // self-named
export { meta: r } as detail;             // object value, for nested shapes
```

- In **setup**: merges into ambient scope for subsequent files.
- In **lib**: the exported object is bound at the call site as the lib's value.
- In **const**: exported values become constants (full integration TODO — for
  now they flow into ambient scope like setup).
- In **test**: tests assert; exporting is allowed but usually pointless.

### `return`

Control flow, not output. At a file's top level `return;` is **void** — it just
halts execution; `return <value>` there is an error. A *value* `return` belongs
inside a lambda, where it's the block's yield (`{ x --> ...; return v; }`).

### Assignment

```
x = 42;
req.headers."content-type" = "application/json";    // nested field mutation
url = "{{baseUrl}}/orgs/{{orgId}}";                 // interpolated string
```

### Assertion

`expression | "failure message"` — fails the test if the expression is falsy/null.

```
r.id != null | "missing id";
r.items.size > 0 | "no items";
r.name == "Test Group" | "wrong name: {{r.name}}";
```

All assertions in a file run — failures are collected, not short-circuited.

### Guard

`|` works in assignments too — asserts non-null:

```
groupId = r.groups[0]?.id | "no group id found";
```

### `if` / `else`

```
if existing != null {
    junk = req.delete("/v4/payments/providers/{{existing.id}}");
}
```

Conditional execution. Braces delimit each branch; the condition is a bare
expression (no parens). `else` and `else if` chains are supported:

```
if status == "active" {
    r = req.post("/v4/orders") ? 2xx | "create failed";
} else if status == "pending" {
    r = req.get("/v4/orders/pending") ? 2xx | "fetch failed";
} else {
    skipped = true;
}
```

Only the chosen branch runs; the file continues normally afterward. A failing
assertion inside a branch reports **its own** source line. Unlike a whole-file
skip, an `if` whose condition is false simply runs nothing in that branch — it
does **not** mark the file skipped, so it never cascades to sibling files.

> This replaces the old `exitIf` guard clause. "Delete it *if* it exists" is a
> conditional, not an early-exit — and an `exitIf` in a setup used to skip the
> file, which cascaded and skipped every test in the group. `if` scopes the
> conditional to just the statements that need it.

### Metadata block

Static, file-level directives live in a **metadata block** above the function
block — `key: value` lines, like HTTP headers. No sigil; the value is the rest
of the line, unquoted. Order is fixed: **metadata → optional param header →
braced body.** Unknown keys are a hard error (a typo shouldn't silently no-op).

```
requires: >= 0.5.3
disabled: I-123: API returns groupId not id, fix pending
blast-radius: 2

a, b --> {
  ...
}
```

#### `requires:` — minimum tstr version

```
requires: >= 0.5.3
```

A version constraint (`>=`, `>`, `=`, `<=`, `<`; a bare version means `>=`). If
the running binary doesn't satisfy it, the file is reported **INCOMPATIBLE**
(`needs >= 0.5.3, have 0.4.6`) and skipped — **not** a hard error, so a newer
test on an older binary bails loudly instead of failing cryptically.

#### `disabled:` — turn the whole file off

```
disabled: I-123: fix pending
```

A known-broken file whose fix is postponed. Unlike `if` (which conditionally
runs *part* of a file), `disabled:` is unconditional and carries a **mandatory
reason**. The runner short-circuits before any statement executes — no HTTP
calls or assertions fire — and reports a distinct **DISABLED** status (cyan),
not a plain skip. List every disabled file and its reason without running:

```bash
tstr list --disabled
```

#### `blast-radius:` — skip downstream collateral

```
blast-radius: 2
```

Declares how much *collateral* this file owns. When a file is `disabled:` **or**
fails at runtime, its blast radius turns off the next tests in the leaf — the
ones that depend on its **side effects** (a resource it created), which the
input-cascade can't see because they declare no missing input. Collateral shows
as `SKIP  blast-radius from <culprit>`, traceable to the cause.

- `disabled:` + `blast-radius: N` → skip self **+** the next N tests.
- a runtime **failure** + `blast-radius: N` → self reports **FAIL**, the next N **SKIP**.

It's leaf-local and forward-only — it never reaches into child directories
(which run concurrently) and cleanups still run. Value forms:

| Form | Meaning |
|------|---------|
| `N` | the next N tests (saturates at the leaf's remaining count) |
| `all` / `*` | every remaining test in the leaf |
| `<=PREFIX` | through the first file whose name starts with `PREFIX`, inclusive (e.g. `<=05`, `<=create-org`) |

This works because a leaf runs its tests **sequentially, in filename order** —
so "the next N" is always well-defined and hasn't started yet.

## HTTP Requests

**Verbs:** `get`, `post`, `put`, `patch`, `delete`, `head`, `options`. Reserved — can't be used as identifier names.

**Function-call form** (req is the first argument):

```
r = get(req, "/v4/groups") ? 2xx | "Failed";
r = post(req, "/v4/groups") ? 200 201 | "Unexpected status";
r = delete(req, "/v4/groups/{{groupId}}") ? 204 | "Expected no content";
```

**UFCS form** (idiomatic — receiver-first reads naturally):

```
r = req.get("/v4/groups") ? 2xx | "Failed";
r = req.post("/v4/groups") ? 201 | "Failed";
```

**Request object** must contain the things the call needs. Recognized fields: `urlPrefix`, `headers`, `body`, `query`.

```
req.headers = { "content-type": "application/json", "authorization": "Bearer {{token}}" };
req.body = { name: "Test Group" };
r = req.post("/v4/groups") ? 2xx | "Failed";
```

For relative URLs (`/...`), the request object must contain `urlPrefix`. Absolute URLs (`http://...`) ignore it.

**Status patterns:** `200`, `2xx`, `200-204`, `>=200`, `<500`.

**Response object** — `r` holds the parsed body; `_response` holds HTTP metadata (`.code`, `.headers`, `.version`, `.format`).

Body parsing is determined by **sniffing the body itself**, not by trusting `Content-Type` (services lie — that's what we test):

| `_response.format` | When | `r` shape |
|---|---|---|
| `"sse"` | body has SSE field-lines (`data:`, `event:`, `id:`, `retry:`, or `:` comments) | array of event objects |
| `"json"` | body parses as a single JSON value | parsed JSON |
| `"ndjson"` | every non-empty line parses as JSON, ≥2 lines | array of parsed objects |
| `"text"` | none of the above | raw string |

```
_response.format == "ndjson" | "expected stream";
```

## Retry / Polling

Some state is eventually consistent: you `POST` to service A, A fires an async
message (Kafka, a queue, a webhook), service B consumes it, and only *then*
does a `GET` on B reflect the change. A test that checks B immediately after
the POST is flaky — it races the propagation.

`retry` wraps a block and re-runs it until **every assertion inside passes**,
or a bound is reached:

```
post-then-poll.test.tstr

r = req.post("/v4/groups") ? 2xx | "create failed";
groupId = r.id | "no group id";

retry(max: 10, interval: 500ms, timeout: 30s) {
    g = req.get("/v4/groups/{{groupId}}") ? 2xx | "not visible yet";
    g.status == "active" | "B hasn't caught up";
}
```

**Arguments** (at least one of `max`/`timeout` is required):

| Arg | Meaning | Default |
|---|---|---|
| `max` | total attempts, including the first (bare count, no unit) | — |
| `interval` | delay between attempts (`ms` / `s` / `m`) | `250ms` |
| `timeout` | wall-clock cap (`ms` / `s` / `m`) | — |

**Semantics:**

- **Fail-fast within an attempt** — the first failing `|` assertion is the
  retry trigger; the block waits `interval` and runs again from the top.
- A clean pass stops immediately. Exhausting the bounds reports the last
  failure, annotated `(retry exhausted after N attempts, T.Ts)`.
- A failing **HTTP status check** (`? 2xx`) or a connection error counts as a
  failure too — so a `404` while B is still catching up, or a service that
  isn't up yet, both retry naturally.
- The `interval` sleep is clamped so it never overshoots `timeout`.
- `return` and `matrix` are **not allowed** inside a retry body (they don't
  compose with re-execution) — using one is a runtime error. `if` *is* allowed:
  a conditional assertion just becomes the retry trigger.

Failures inside a retry report at the failing assertion's own line, annotated
with the attempt count and elapsed time.

## Kafka

*Built by default. For a lean build without Kafka (dropping the rskafka deps and
the Rust ≥1.85 floor), use `cargo build --no-default-features`. Plaintext brokers
only (no TLS/SASL yet).*

For flows that emit Kafka messages, tstr can assert on what a service produced —
and send messages to drive a flow. Because Kafka is a durable **log**, you don't
subscribe ahead of time: you **mark the topic's position before the action**,
fire the action, then seek back to the mark and scan forward for the message you
expect.

`$.kafka(config)` returns a **handle** you configure and reuse. Config splits in
two: the **global** bits (broker address and requirement guardrails) come from
the config object; the **dynamic** bits (`topic`, `key`, `headers`) are set as
fields on the handle in the test, right before `send`/`since` — the same shape
as configuring a `req` object and then calling it.

```yaml
# tstr.yaml — the global config, kept out of individual tests (like urlPrefix)
constants:
  kafka:
    bootstrap: "broker.internal:9092"
    requiresTypeId: true    # send errors unless a __TypeId__ header is set
    requiresKey: true       # send errors unless a key is set
```

```
order-event.test.tstr

k = $.kafka(${kafka});               // handle from the global config object
k.topic = "orders.events";

// mark the window BEFORE the action that emits the message
cur = k.since();

r = req.post("/v4/orders") ? 2xx | "create failed";

// full-body regex + bounded wait; returns the message, or null on timeout
msg = cur.find("\"orderId\":\"{{r.id}}\"", 30s) | "no kafka event for order";
msg.body.status == "confirmed"                 | "wrong status";

// send: set the dynamic fields, then send the value
k.topic   = "orders.commands";
k.key     = r.id;
k.headers = { "__TypeId__": "com.acme.CancelCommand" };
k.send({ type: "cancel", orderId: r.id });
```

A bare string still works for the simple case: `k = $.kafka("localhost:9092")`
(no requirements). A handle opens no connection until the first `send`/`since`,
so one built in a parent `setup.tstr` and `export`ed cascades to the whole tree.

**Operations:**

| Call | Returns |
|---|---|
| `$.kafka(config)` | a handle — `config` is a bootstrap string or `{ bootstrap, requiresTypeId?, requiresKey? }` |
| `handle.since()` | a cursor marking `.topic`'s current end offsets |
| `cursor.find(regex, timeout)` | first message after the mark whose raw payload matches `regex`, or `null` on timeout |
| `handle.send(value)` | sends `value` to `.topic` with `.key`/`.headers`; returns `{ partition, offset }` |

**Dynamic fields** set on the handle:

| Field | Used by | |
|---|---|---|
| `.topic` | `send`, `since` | topic name (required) |
| `.key` | `send` | message key (required if `requiresKey`) |
| `.headers` | `send` | object of header name → value (must include `__TypeId__` if `requiresTypeId`) |

**The message** `find` returns mirrors an HTTP response:

| Field | |
|---|---|
| `body` | payload parsed with the same sniffing as a response body (JSON / ndjson / SSE / text) |
| `raw` | the untouched payload string — what the regex matched against |
| `format` | `"json"` / `"ndjson"` / `"sse"` / `"text"` |
| `key` · `partition` · `offset` · `timestamp` · `headers` | record metadata |

**Semantics:**

- **Global vs dynamic.** Broker address and the `requires*` guardrails are global
  (the config object, typically a `tstr.yaml` constant); `topic`/`key`/`headers`
  are dynamic, set per test. The guardrails are enforced at `send` *before* any
  network work, so a missing key or `__TypeId__` fails fast with a clear message.
- **Mark before the action.** `since` must run before whatever emits the
  message — it snapshots the end offsets, and `find` reads forward from there,
  so a message produced after the mark is caught while pre-existing ones are
  skipped. A topic that doesn't exist yet marks as empty (and `find` reads it
  from the start once it appears), so it's safe to mark before the downstream
  service has created the topic.
- **Full-body regex.** The pattern is tested against the whole payload string,
  not a parsed field. `find` returns `null` on timeout, so `| "message"` fires
  as an ordinary assertion — and it *is* the wait primitive, so it doesn't need
  wrapping in `retry` (though it composes with one if you like).
- **`timeout`** takes a bare duration literal (`30s`, `500ms`, `2m`) or a plain
  number of milliseconds.
- **`send`** serializes `value` like an HTTP body — strings raw, objects/arrays
  as JSON, `null` as a tombstone — and always targets partition 0. The handle
  keeps its `.key`/`.headers` between sends, so reset them when they should not
  carry over.

Run the live Kafka tests against a throwaway broker with `scripts/kafka-it.sh`
(spins up Redpanda in Docker, points the tests at it, tears it down).

## PostgreSQL

*Built by default. For a lean build without Postgres (dropping the tokio-postgres
/ rustls deps), use `cargo build --no-default-features`. TLS is rustls (pure Rust,
`ring` provider — no OpenSSL/C toolchain).*

For flows that read or seed a database directly — verify a row landed, set up a
fixture, tear it down, assert on a large result set page by page — `$.postgres`
opens a connection handle you configure once and reuse. Like `req`/`$.kafka`, the
global bits (host, credentials, TLS) live in a `tstr.yaml` constant; the handle
opens no connection until the first query, so one built in a parent `setup.tstr`
and `export`ed cascades to the whole tree. Multiple databases = multiple handles.

```yaml
# tstr.yaml — the connection config, kept out of individual tests
constants:
  db:
    host: localhost
    port: 5432
    database: appdb
    user: tester
    password: ${PG_PASSWORD}
    schema: reporting      # optional; SET search_path before each op
    sslmode: prefer        # disable | prefer | require | verify-full  (default: prefer)
    sslRootCert: ~/.config/tstr/do-ca.crt   # verify against this PEM CA bundle
    sslInsecure: false      # true = accept any cert (test DBs only); default false
```

An unrecognized field is an error, not a no-op — a typo like `sslinsecure` or a
plausible-but-wrong `caCert` fails the run naming the key, rather than being
dropped and resurfacing as a baffling TLS failure.

### Managed clusters with a private CA

DigitalOcean, RDS, and Cloud SQL sign their server certificates with a *per-project
CA* that chains to no public root, so the default public root store rejects them
and the handshake fails. Point `sslRootCert` at the CA bundle the provider gives
you (DO: console → Connection Details → *Download CA certificate*, or
`doctl databases ca get <cluster-id>`):

```yaml
constants:
  db:
    sslmode: require
    sslRootCert: ~/.auth/certs/ca-certificate.crt
```

The path takes a leading `~/`. A missing file, a file with no `CERTIFICATE` block,
or an unusable certificate each fail at connect time with a message naming the
path. `sslRootCert` and `sslInsecure` are mutually exclusive — one performs the
verification the other skips — and setting both is an error.

```
db-checks.test.tstr

pg = $.postgres(${db});                 // handle; a bare "postgres://…" URL string also works

// raw parameterized SQL — one primitive, covers select/insert/update/delete.
// Params ($1, $2, …) bind as text and Postgres coerces them to the column types.
r = pg.query("select * from users where org = $1 and active = $2", orgId, true);
r.count > 0                 | "no users";
r.rows[0].email != null     | "missing email";

r = pg.query("insert into tags(name) values($1) returning id", nm);
tagId = r.rows[0].id        | "insert returned no id";

pg.query("delete from tags where id = $1", tagId);   // r.count = rows affected

// pagination — a stateless cursor; each .page(n) re-issues with LIMIT/OFFSET
c  = pg.paginate("select * from users order by id", 50);
p0 = c.page(0);             // rows 1-50
p1 = c.page(1);             // rows 51-100
c.total() > 0               | "no rows";
```

**Operations:**

| Call | Returns |
|---|---|
| `$.postgres(config)` | a connection handle — `config` is a `postgres://…` URL string or a config object (see above) |
| `handle.query(sql, ...params)` | `{ rows, count }` — runs any statement |
| `handle.paginate(sql, pageSize)` | a cursor over `sql`, `pageSize` rows per page |
| `cursor.page(n)` | the 0-indexed n-th page: `{ rows, count, page, pageSize, done }` |
| `cursor.total()` | total row count of the query (`count(*)`) |

**Result shape.** Every `query` returns `{ rows, count }`:

- **SELECT** (or any statement with `RETURNING`) → `rows` is an array of
  `{ column: value }` objects; `count` is the number of rows returned.
- **INSERT / UPDATE / DELETE** without `RETURNING` → `rows` is empty; `count` is
  the number of rows affected.

`cursor.page(n)` adds `page`, `pageSize`, and `done` (true when the page came
back shorter than `pageSize` — i.e. the last one).

**Semantics:**

- **Parameters bind as text.** `$1`, `$2`, … are sent in text format and
  Postgres coerces them to the inferred column types — so a number, bool,
  timestamp, or uuid all just work without casts. Objects/arrays are sent as
  JSON (pair with `$1::jsonb` when the target column is `jsonb`); `null` is a
  real SQL `NULL`. Always parameterize values rather than interpolating them.
- **Column types.** `int`/`float`/`numeric` come back as numbers (numeric as a
  string, to preserve precision), `bool` as a bool, `json`/`jsonb` as parsed
  objects, and `uuid`/timestamp/date/time as strings. Unmapped exotic types come
  back as `null` rather than failing the query.
- **`delete` is not a method.** Since `delete` (like `get`/`post`/…) is a
  reserved HTTP verb, deletes go through `pg.query("delete from …")`, not a
  `pg.delete(...)` method.
- **Pagination is stateless** — `.page(n)` takes an explicit page index and
  re-runs the query with `LIMIT/OFFSET` each time (no server-side cursor, no
  connection held open between pages). Give the query a stable `ORDER BY` so
  pages don't overlap.
- **Schema — settable per op.** `schema` runs `SET search_path` on the
  connection before each op; omit it to use the database default (or
  schema-qualify in the SQL). It's read off the handle *every* time, so you can
  set it dynamically mid-test — the same way you'd configure a `req` object —
  and the change applies to the next query (and every one after, until changed):

  ```
  pg = $.postgres(${db});          // no schema → database default
  pg.schema = "tenant_a";
  a = pg.query("select * from accounts");   // runs against tenant_a

  pg.schema = "tenant_b";
  b = pg.query("select * from accounts");   // now against tenant_b
  ```

  A schema set in the `tstr.yaml` config is just the starting value; assigning
  `pg.schema` overrides it from that point on.
- **TLS.** `sslmode: disable` connects in plaintext; `require`/`verify-full`
  use TLS. `sslInsecure: true` skips certificate verification for test servers
  with self-signed certs — never use it against production.

Each op opens a fresh connection, runs, and drops it (same no-keepalive stance
as the HTTP client) — connection pooling is a later perf pass.

Run the live Postgres tests against a throwaway server with `scripts/pg-it.sh`
(spins up Postgres in Docker, points the tests at it, tears it down).

## Library Functions

Libraries are `*.lib.tstr` files: callable functions with explicit parameters.

### Defining a lib

```
# lib/createTag.lib.tstr
name, type --> {
  req.body = { name, type };
  r = req.post("/v4/tags") ? 2xx | "create-tag failed";
  export r.id as id;
}
```

The `name, type -->` header declares the parameters and the `{ ... }` block is
the body. `req` and any other ambient names come from the lib's **own**
directory hierarchy (see Scope below).

### Calling a lib

```
result = createTag("foo", "label");              # direct call
result = "foo".createTag("label");               # UFCS — first param is the receiver
tagId = createTag("foo", "label").id;            # chain access
```

No `call` keyword. Library calls share a namespace with built-in HTTP verbs — verb names (`get`, `post`, etc.) are reserved.

### Resolution

When a test calls `createTag(...)`, the runner walks the caller's directory chain from innermost to outermost (stopping at the suite root), checking at each level:

1. The dir's `lib/` subdirectory (recursive — subdirs allowed for organization, flat namespace)
2. Any bare `*.lib.tstr` files directly at that level

If no in-suite match, `--import` directories are checked in order. **Closest scope wins. Collisions at the same tier are an error.**

A `lib/` directory holds callable definitions only. It never claims a row in the slot display, and a `*.test.tstr` file placed there is rejected — tests belong in a leaf, not the lib tree.

```
my-project/
  tstr.yaml
  lib/
    createTag.lib.tstr            # visible everywhere in the suite
    orgService/
      setup.tstr                  # builds req for orgService libs
      createOrg.lib.tstr          # uses sibling setup's req
  tests/profile/
    helper.lib.tstr               # visible only to tests/profile/ and its descendants
    01-create.test.tstr           # can call createTag, createOrg, helper
```

### Lib scope

A `lib.tstr` evaluates with the ambient scope of **its own directory hierarchy**, not the caller's. Libs are self-contained: behavior depends only on the lib's own setups, constants, and imports — never on where it was invoked from.

- For in-suite libs, the cascade stops at the suite root.
- For imported libs (`--import`), the cascade stops at the imported directory.
- Project constants (`${name}` from yaml) are visible to in-project libs but **not** to imported libs.

To accept caller-specific values, declare them as explicit params. To make an external lib use a project service, pass it explicitly: `${orgService}.externalLib(args)`.

## Examples

### Single-file test

```
# tests/health.test.tstr
req = { urlPrefix: "http://localhost:8080" };
r = req.get("/health") ? 200 | "service down";
r.status == "ok" | "unhealthy: {{r.status}}";
```

### Setup broadcast → ordered mutation chain

`tag-crud/` scaffolds; the tests live in its `ops/` leaf. The setup runs first
and broadcasts `req` and `tagId` down into the leaf, the tests run in filename
order, and the cleanup tears down afterward.

```
# tests/tag-crud/00-create.setup.tstr      (scaffolding — tag-crud is non-leaf)
--> {
  req = { urlPrefix: ${orgService.baseUrl} };
  req.body = { name: "test-tag", type: "label" };
  r = req.post("/v4/tags") ? 2xx | "create failed";
  export req, r.id as tagId;
}

# tests/tag-crud/99-cleanup.cleanup.tstr    (scaffolding — runs after the leaf)
req, tagId --> {
  req.delete("/v4/tags/{{tagId}}") ? 204 | "cleanup failed";
}

# tests/tag-crud/ops/01-replace.test.tstr   (ops/ is the leaf)
req, tagId --> {
  req.body = { name: "test-tag-replaced" };
  req.put("/v4/tags/{{tagId}}") ? 2xx | "replace failed";
}

# tests/tag-crud/ops/02-add-item.test.tstr
req, tagId --> {
  req.body = { itemId: "abc-123" };
  req.post("/v4/tags/{{tagId}}/items") ? 2xx | "add-item failed";
}
```

No fake gate variables. Order is the filename order. The setup's `export`
broadcasts `req` and `tagId` into the `ops/` leaf below it, and the cleanup runs
last regardless of how the tests fared.

### Per-service libs

```
lib/
  orgService/
    setup.tstr                   # --> { ...; export req; }
    createOrg.lib.tstr           # name --> ...uses req from sibling setup... export r.id as id;
  tagService/
    setup.tstr                   # different req
    createTag.lib.tstr

tests/
  profile/
    01-setup.setup.tstr          # any project setup; cascades into cases/
    cases/
      01-test.test.tstr          # calls createOrg("alpha") — uses orgService's req, not profile's
```

Each service's libs are self-contained — they see only their own scope cascade.

## Expressions

### Operators

| Operator | Meaning |
|---|---|
| `==` `!=` | Equality |
| `>` `<` `>=` `<=` | Comparison |
| `&&` `\|\|` `!` | Logical |
| `+` `-` `*` `/` `%` | Arithmetic |
| `~` | Regex extract (returns match/capture group) |
| `~?` | Regex test (returns boolean) |
| `!~` | Regex non-match |

### Property and Index Access

```
r.id                            // dot notation
r."hyphenated-field"            // quoted for special chars
r.user?.address?.city           // optional chaining (null-safe)
r.items[0]                      // array index
r.items[-1]                     // negative index (from end)
r.items[0:3]                    // slice
r.items[].id                    // collect field from all elements
```

### Collection Properties

- `.length` — string character count
- `.size` — array/object entry count

### Collection Methods

```
match = r.items.find({ item --> item.name == "test" });
active = r.items.filter({ item --> item.active == true });
ids = r.items.map({ item --> result = item.id; <-- result; });
r.items.each({ item --> item.id != null | "null id found"; });
```

### Pipe Operations

```
r.items | any({ i --> i.active == true }) | "no active items";
r.items | all({ i --> i.id != null }) | "found null ids";
```

### Built-in Functions

```
id = $.uuid();                                  // random UUID v4
name = $.string(10);                            // random alphanumeric
email = $.randEmail();                          // random@example.com
email = $.randEmail("doug@example.com");        // doug+rand@example.com
timestamp = $.now();                            // unix timestamp
$.log("checkpoint: groupId =", groupId);        // log message

sig = $.hmacSha256(secret, payload);            // HMAC-SHA256, lowercase hex
sig = $.hmacSha256(secret, payload, "base64");  // ...or standard base64
header = $.stripeSign(whsec, body);             // "t=<now>,v1=<hex>"
header = $.stripeSign(whsec, body, 1700000000); // ...with explicit timestamp
```

`$.log()` messages are collected per-test and shown for failures (normal mode) or always (verbose mode).

`$.stripeSign(secret, payload)` emulates Stripe's `Stripe-Signature` header: it
HMAC-SHA256s `"{timestamp}.{payload}"` and returns the `t=…,v1=…` value Stripe's
`v1` scheme expects. The timestamp defaults to the current time; pass an explicit
one for deterministic tests or to exercise replay-tolerance windows. For other
providers' signing schemes, build the header yourself from `$.hmacSha256()`.

### Other Features

- **Duration literals** — `30s`, `500ms`, `2m` evaluate to a number of milliseconds anywhere a number is expected (e.g. `cursor.find(regex, 30s)`). Units: `ms` / `s` / `m`.
- **`@path`** — load file content: `template = @fixtures/group.json;` (JSON files auto-parsed)
- **`{{interpolation}}`** — variable substitution in strings and URLs
- **JSON construction** — `req.body = { name: "Test", count: 3 };`
- **Field mutation** — `req.headers."content-type" = "text/plain";` or `req.headers["content-type"] = "text/plain";`

## CLI

```
tstr run [dir]                    # run the suite, or scope to a subdirectory (default: cwd)
tstr list [target]                # per-directory tables of files visible
tstr clean [dir]                  # remove the logs/ dir + tstr-last-run.log under the suite root
tstr --config path/to/yaml ...    # explicit config (overrides project tstr.yaml)
tstr --version
```

**`run` flags:**

| Flag | Effect |
|---|---|
| `--url <base>` | shorthand for `--set urlPrefix=<base>` |
| `--set 'KEY=VALUE'` | set an ambient variable (repeatable) |
| `--repeat <N>` | run the whole suite N times (default `1`). Totals accumulate across iterations; the summary shows `(N iterations x M tests)`. |
| `--repeat-mode <sequential\|concurrent>` | how `--repeat` runs. `sequential` (default) does one pass after another — safe, good for flushing out flaky failures. `concurrent` runs N overlapping passes at once (requires a suite that tolerates copies of itself). In a terminal, `concurrent` renders one bucketed bar per directory, each spanning that dir's `tests × repeat` cells and filling as the passes complete; piped / off-terminal it's summary-only. Overrides the suite's `defaults.repeat_mode`. |
| `--display auto\|bars` | slot-display style (`bars` forces colored bucketed bar) |
| `--timeout <SECONDS>` | per-request HTTP timeout (default: `60`). `0` disables the timeout. |
| `-j` / `--jobs <N>` | max concurrent worker threads (default: CPU count). HTTP work is I/O-bound — each blocking request parks a worker — so a value well above CPU count often raises throughput. `-j 1` forces serial. |
| `-v` / `--verbose` | streaming PASS/FAIL + timing + scope changes |
| `-q` / `--quiet` | only summary and failures |

The summary's per-suite **Time** column is summed *work-time* (each file's own
elapsed), so it reads the same whether the run was parallel or serial. A
separate **wall-clock** line below the TOTAL shows actual elapsed time and the
parallel speedup when one occurred.

**`list` flags:**

| Flag | Effect |
|---|---|
| `--type ROLES` | comma-separated: `test`, `setup`, `cleanup`, `const`, `fetch`, `exporter`, `lib`, `all` (default: `test,setup,cleanup,const,fetch` — i.e. everything except `exporter` and `lib`) |
| `--flat` | one path per line (for piping) |
| `--disabled` | list only files turned off via a `disabled:` metadata marker, with each one's reason (ignores `--type`/`--flat`) |

Example `list` output:

```
profile/sso-user
| name        | role    | params        | returns |
|-------------|---------|---------------|---------|
| 00-login    | setup   | —             | orgId   |
| 99-teardown | cleanup | orgId         | —       |

profile/sso-user/crud
| name               | role | params        | returns |
|--------------------|------|---------------|---------|
| 01-create-sso-user | test | orgId         | userId  |
| 02-list-sso-users  | test | orgId         | —       |
| 03-get-sso-user    | test | orgId, userId | —       |
| 04-delete-sso-user | test | orgId, userId | —       |
```

`sso-user/` scaffolds (its `setup`/`cleanup` cascade down); the tests live in the
`crud/` leaf below it.

## Output Modes

- **Interactive** (default in terminal) — one slot row per top-level directory (or per child of the target when scoped). Per-test glyphs (`✓✗-·`) when there's room, or a colored bucketed bar otherwise — gradient hue from green (all pass) through yellow (skip-leaning) to red (all fail). `--display=bars` forces bars on short rows. Under sequential `--repeat`, the slots reset to all-pending at the start of each pass (so every iteration animates fresh) and the status line carries an `Iter k/N` marker. Under concurrent `--repeat`, each directory is a single bucketed bar spanning all its `tests × repeat` cells, filling as the overlapping passes complete.
- **Normal** (piped / non-interactive) — one streamed line per file: PASS / FAIL / SKIP / DISABLED / INCOMPATIBLE, plus LOAD when a `const` file loads.
- **Verbose** — streaming + timing, scope changes, log output.
- **Quiet** (`-q`) — only summary and failures.

## Run Log

Each run writes a numbered log under **`<suite-root>/logs/tstr-<NNNN>.log`** (not
the current directory — so logs never litter wherever you happened to invoke
`tstr`). A **`tstr-last-run.log`** symlink in the suite root always points at the
most recent run. Every run is captured **regardless of pass/fail and verbosity**.
Per-test entries include:

- PASS / FAIL / SKIP / DISABLED / INCOMPATIBLE label, test name, source path
- HTTP endpoint that was called
- All assertion failures (and runtime errors, with prior failures preserved)
- A table of ambient variables in scope at file start: source, name, value (truncated)
- `$.log()` messages

`const` loads aren't tests, so they get no log entry (they stream as `LOAD`, but
the run log records only `test`/`setup`/`cleanup` outcomes).

History is kept so you can compare runs (handy for intermittent failures). The
`logs/` directory is auto-pruned to the most recent **10** runs by default — set
`log_retention:` in `tstr.yaml` to change it (`0` keeps everything). A
`logs/.gitignore` is created automatically so run logs aren't committed. `tstr
clean` removes the whole `logs/` directory and the symlink.

## Timing Stats & Scheduling

Each run maintains **`<suite-root>/.tstr-stats.json`** — a per-leaf-directory
duration ledger:

```json
{
  "commerce/carts": { "last_ms": 44120, "avg_ms": 44080, "runs": 31 },
  "groups":         { "last_ms": 950,   "avg_ms": 1012,  "runs": 31 }
}
```

- A leaf records its wall-clock only when it ran **clean** — every file passed
  (deterministic `disabled:` / `when:`-incompatible skips are fine). Failures
  and circumstantial skips (missing inputs, blocked setup, blast collateral)
  would poison the number with fast-fails, so those runs record nothing.
- `avg_ms` is an exponentially weighted moving average (70% history, 30%
  latest), so it converges within a few runs after a leaf gets faster or
  slower — it's the number the scheduler trusts.
- The runner sorts sibling subtrees **longest-first** from these averages, so
  slow leaves start early and their waits overlap the rest of the suite
  instead of dangling off the end. Unmeasured subtrees sort last and earn a
  number on their first clean run.

The file is machine-local (timings are environment-specific) and self-healing —
delete it any time and it rebuilds over the next runs. Add `.tstr-stats.json`
to your suite's `.gitignore`.

### `--skip-slow` — fast deploy-gate runs

```bash
tstr run --skip-slow          # skip leaves averaging over 10s
tstr run --skip-slow=30s      # custom threshold (= required; ms/s/m, bare number = seconds)
```

Leaf directories whose recorded average exceeds the threshold are skipped
wholesale — every file in them reports SKIP with the reason
(`slow: avg 44.1s exceeds --skip-slow 10s`). Unmeasured leaves always run
(can't skip what hasn't been met), and a skipped leaf's stats hold at their
last measured value, so it stays recognized as slow on the next
`--skip-slow` run.

## Failure Output

```
  FAIL  05 Set Override  (refunds/05_set_override.test.tstr)
        PUT https://api.example.com/v4/overrides
        line 7: Failed to set override (got 404)
        line 9: wrong dashboard type (got null, expected "Platform")
```

## v0.3.0 Known Limitations / TODOs

Tracked here for visibility; none are blockers:

- **`--stop-on-error`** — accepted but not propagated.
- **`run` scoping is directory-only.** `tstr run path/to/sub` scopes the run to
  that subdirectory; a non-directory target is an error. There is no name/glob
  filtering and no single-file execution (by design — leaf tests aren't run in
  isolation). `tstr list` still supports a name pattern for searching.
- **Matrix fan-out** — was DAG-coupled; needs reimplementation for the structural model.
- **`.const.tstr` integration with `${name}`** — currently const returns flow into ambient scope; strict `${name}`-only access for const files is a follow-up.
- **Library call caching** — every call re-executes; opt-in memoization will land when the semantics are pinned down.
- **`--reachable`** for `tstr list --type lib` — call-graph analysis to limit listed libs to those actually invoked.

## File form

Every file is a function: an input header (when it takes parameters), a braced
body, and `export` for whatever it publishes.

```
a, b --> {
  ... statements ...
  export x, r.id as id, payIntentId;
}
```

A file that takes no inputs skips the header and opens straight into its body:

```
{
  ... statements ...
  export x;
}
```

- **Input header, only when there are params.** `a, b -->` declares the ambient
  values the file consumes — the `-->` is required once you list params. A file
  that takes none opens directly with `{ ... }`; the bare `--> { ... }` form is
  still accepted as an explicit synonym.
- **Body is braced.** `{ ... }` wraps the statements.
- **`export` publishes named bindings.** A comma list of `expr [as name]`. A
  bare identifier self-names (`export payIntentId`); anything computed needs an
  alias (`export r.id as id` — `export r.id` alone is an error). For a
  setup/test these names broadcast into ambient scope; for a lib they're the
  returned object. A lone `export { ... };` publishes the object's fields, for
  nested shapes. `export` doesn't halt and may appear more than once.
- **`return` is control flow, not output.** At a file's top level `return;` is
  void — it just halts execution; `return <value>` there is an error (use
  `export`). A *value* `return` belongs inside a lambda (the block's yield).
- **Metadata sits above the block.** Optional `key: value` directives
  (`requires:`, `disabled:`, `blast-radius:`) precede the param header — see
  [Metadata block](#metadata-block).

```
disabled: I-123: fix pending
blast-radius: 1

a, b --> {
  ... statements ...
}
```

> Note: the block-collect arrow inside lambdas (`map({ x --> ... <-- v; })`) is a
> separate construct and is unchanged. The legacy `_in.X` object is still seeded
> into scope for in-body reads.

## Tech Stack

- **Rust** — fast, single binary
- **winnow** — parser combinator library for the DSL
- **reqwest** — HTTP client (blocking, with connection pooling)
- **regex** — regular expression engine
- **clap** — CLI argument parsing
- **serde** + **serde_yaml** — config loading
- **serde_json** — JSON parsing/serialization
- **rskafka** — Kafka client (optional, `kafka` feature)
- **tokio-postgres** + **rustls** — PostgreSQL client with pure-Rust TLS (optional, `postgres` feature)

## Editor Support

### IntelliJ / JetBrains

Settings → Editor → TextMate Bundles → add `editor/textmate` directory.

### Neovim

```bash
ln -s ~/dev/tstr/editor/vim/syntax/tstr.vim ~/.config/nvim/syntax/tstr.vim
ln -s ~/dev/tstr/editor/vim/ftdetect/tstr.vim ~/.config/nvim/ftdetect/tstr.vim
```

## Development

tstr was built collaboratively with [Claude Code](https://claude.com/claude-code),
Anthropic's CLI coding agent. I drove the language design and the
architectural decisions — and used the project to learn Rust — while Claude
served as an implementation pair: drafting the parser and evaluator, working
through borrow-checker puzzles, writing the test suite, and talking through
design trade-offs as the DSL evolved. The result is a genuine collaboration,
and I've tried to keep this README honest about how it came together.

Built by Doug Kress — **8DK**
