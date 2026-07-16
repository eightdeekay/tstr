# Changelog

All notable changes to tstr are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/); versioning follows
[SemVer](https://semver.org/). Pre-1.0, **breaking changes bump the minor**
(`0.3.x → 0.4.0`), not the patch.

Releases with a ⚠️ block require action on existing suites — the migration steps
live in [UPGRADING.md](UPGRADING.md), cross-linked per version.

<a id="v0.9.1"></a>
## [0.9.1] — 2026-07-16

Slow tests stop bogging down the suite: their waits now overlap other work,
they're scheduled first, and `--skip-slow` excludes them from deploy-gate runs.

### Added
- **Per-leaf timing stats** — each run maintains `<suite-root>/.tstr-stats.json`,
  recording every leaf directory's wall-clock (`last_ms`, EWMA `avg_ms`, `runs`).
  Only clean runs record: failures and circumstantial skips (missing inputs,
  blocked setup, blast collateral) would poison the number with fast-fails;
  deterministic `disabled:`/`when:`-incompatible skips don't disqualify. The
  file is machine-local and self-healing — gitignore it in suites; delete it
  any time.
- **`--skip-slow[=DURATION]` on `tstr run`** — skip leaf directories whose
  recorded average exceeds the threshold (bare flag = 10s; `=` required for a
  value: `--skip-slow=30s`, `=500ms`, `=2m`, bare number = seconds). Skipped
  files report SKIP with the reason (`slow: avg 44.1s exceeds --skip-slow 10s`).
  Unmeasured leaves always run, and a skipped leaf's stats hold at their last
  measured value so it stays recognized as slow.

### Changed
- **Sibling subtrees now run longest-first** (greedy LPT over the stats
  averages; a scaffolding dir costs the max of its parallel children). Slow
  leaves start their waits early instead of dangling off the end of the run.
  Unmeasured subtrees sort last and earn a number on their first clean run.
- **A waiting `retry` no longer parks its worker thread** — the sleep donates
  the thread to the pool (rayon work-stealing), running other pending tests
  and re-polling when they finish. Two overlapping waits on a single worker
  now cost the longest wait, not the sum. An attempt can fire later than
  `interval` when stolen work ran long; nesting is capped (8 deep per worker)
  before falling back to a plain sleep.

<a id="v0.9.0"></a>
## [0.9.0] — 2026-07-09

⚠️ Two changes need a look at existing suites — constants deep-merge across
config layers, and an unrecognized `$.postgres` field is now an error. Both fail
loudly. See [UPGRADING § 0.9.0](UPGRADING.md#v0.9.0). No codemod: neither is a
mechanical rewrite.

### Added
- **`sslRootCert` on the `$.postgres` handle.** Verify the server certificate
  against a PEM CA bundle instead of the public root store. Managed clusters
  (DigitalOcean, RDS, Cloud SQL) sign with a per-project CA that chains to no
  public root, so the previous choice was `sslInsecure` or nothing. A leading
  `~/` expands. A missing file, a file with no `CERTIFICATE` block, or an
  unusable certificate each fail at connect time naming the path. Setting both
  `sslRootCert` and `sslInsecure` is an error — one performs the verification the
  other skips.

### Changed
- ⚠️ **An unrecognized `$.postgres` config field is now an error** listing the
  offending key and the known fields. Unknown keys were silently dropped, so a
  typo (`sslinsecure`) or a plausible-but-wrong name (`caCert`) produced no
  diagnostic and resurfaced as an inscrutable TLS or auth failure. Handles
  carrying stray keys that happened to be ignored will now fail loudly.
- ⚠️ **Object constants now deep-merge across config layers** instead of being
  replaced per top-level key. When `~/.config/tstr/config.yaml` and a project
  `tstr.yaml` both define `db:`, their fields union and the project layer wins
  only on the keys it actually sets — so a developer can supply `db.host` and
  `db.sslInsecure` while the checked-in file owns `db.database`. Previously the
  project's `db` obliterated the user's, which is why per-developer values had to
  be smuggled in as flat scalars (`dbHost`) and re-composed with `${dbHost}`.
  That indirection still works and needs no change.

  Mappings merge; **everything else, including sequences, replaces** — a later
  layer's list wins outright rather than appending. `defaults.import` is
  unaffected and still appends.

  Suites that relied on a later layer wholly *discarding* an earlier layer's
  object constant will now see the earlier layer's extra fields survive. In
  practice this only bites if a field is set in one layer and deliberately absent
  in another.

<a id="v0.8.3"></a>
## [0.8.3] — 2026-07-08

### Fixed
- **A relative `!secret` path now resolves against the config file that declares
  it**, not the process working directory. As shipped in 0.8.2 a suite loaded
  fine when run from its own root and failed with `No such file or directory`
  from anywhere else. Tags are resolved per-layer during load, so a `pgpass`
  named in `~/.config/tstr/config.yaml` is found beside that file rather than
  beside the project's `tstr.yaml`. `~/` and absolute paths are unchanged.
- **A failed `!secret` read now reports where it looked** when the resolved path
  differs from the one written in the yaml.

<a id="v0.8.2"></a>
## [0.8.2] — 2026-07-08

### Added
- **`!secret <path>` constants in `tstr.yaml`.** A constant tagged `!secret` reads
  its value from a file rather than the yaml, keeping passwords out of a config
  that gets pasted, committed, or shared on a screen. A leading `~/` expands
  against `$HOME` and one trailing newline is stripped. Tags resolve before
  `${name}` substitution, so a secret composes into other constants
  (`db: postgres://u:${dbPassword}@host`) as usual.
- **Secret redaction in tstr's own output.** Registered secret values are masked as
  `[redacted]` wherever a report, variable table, or run log would print them —
  including when the value is buried inside a composed string. Masking is
  display-only: requests, queries, and Kafka payloads still carry the real value.
  Values under 6 characters are not registered, since content-based masking would
  censor unrelated output.

### Fixed
- **Unknown yaml tags on constants are now a load-time error naming the tag.**
  Tagged scalars previously fell through to `null` with no diagnostic, so a typo
  like `!secrets` would have yielded a null constant and a failure far from its
  cause.

<a id="v0.8.1"></a>
## [0.8.1] — 2026-07-08

### Changed
- **Editor syntax files brought back in sync with the language.** The vim and
  TextMate grammars (`editor/`) had drifted well behind the parser; they now
  cover HTTP verbs `head`/`options`, the keywords `export`/`retry`/`as`/`matrix`,
  the metadata block keys (`requires:`/`disabled:`/`blast-radius:`), `${name}`
  constant references, duration literals (`30s`/`500ms`/`2m`), and the full
  `$.`-builtin set (`hmacSha256`, `stripeSign`, `kafka`, `postgres`). The two
  `tmLanguage.json` copies are kept byte-identical. No change to the language,
  CLI, or binary — editor highlighting only.

<a id="v0.8.0"></a>
## [0.8.0] — 2026-07-08

Rounds out the PostgreSQL support introduced in 0.7.2 and promotes it to a minor
milestone. No breaking changes.

### Added
- **Per-op schema selection on the connection handle.** A connection's `schema`
  is read fresh before every operation and applied as `SET search_path`, so it
  can be set dynamically mid-test — `pg.schema = "tenant_a"` — the same way you
  configure a `req` object. The next query (and every one after, until changed)
  runs against that schema; a `tstr.yaml` `schema` is just the starting value.
  Now documented in the README and covered by a live round-trip test
  (`scripts/pg-it.sh`).

<a id="v0.7.2"></a>
## [0.7.2] — 2026-07-08

### Added
- **PostgreSQL support**, behind an opt-in `postgres` cargo feature that is
  **on by default** (like `kafka`); `cargo build --no-default-features` drops
  it. `$.postgres(config)` opens a connection handle — `config` is a
  `postgres://…` URL string or an object with
  `host`/`port`/`database`/`user`/`password`/`schema`/`sslmode`/`sslInsecure`.
  Two methods cover everything:
  - `handle.query(sql, ...params)` runs any statement (select/insert/update/
    delete) and returns `{ rows, count }`. Params (`$1`, `$2`, …) bind as text
    and Postgres coerces them to the inferred column types, so no casts are
    needed; objects/arrays bind as JSON, `null` as a real SQL NULL.
  - `handle.paginate(sql, pageSize)` returns a stateless cursor; `cursor.page(n)`
    fetches the 0-indexed n-th page via `LIMIT/OFFSET` and `cursor.total()`
    returns `count(*)`.

  Multiple connections = multiple handles. An optional `schema` runs
  `SET search_path` before each op. TLS is rustls (pure Rust, `ring` provider —
  no OpenSSL/C toolchain); `sslInsecure: true` accepts self-signed certs for
  test servers. Result columns map to native values — numbers, bools, parsed
  `json`/`jsonb`, and `numeric`/`uuid`/timestamps as strings. Each op opens a
  fresh connection (no keepalive), matching the HTTP client. Live round-trips
  run via `scripts/pg-it.sh` (throwaway Postgres in Docker);
  `cargo test --features postgres` covers the hermetic units. Requires Rust
  ≥ 1.85 (shared with the `kafka` feature's edition-2024 floor).

### Fixed
- **`kafka::kind_of` no longer claims non-Kafka handles.** It matched any object
  carrying a `__kind` field, so with a second `__kind` user (the new Postgres
  handles) a `pg.connection` was routed into Kafka method dispatch and reported
  a bogus `'.query()' is not valid on a Kafka pg.connection value`. Both
  subsystems now scope `kind_of` to their own namespace (`kafka.` / `pg.`).

<a id="v0.7.1"></a>
## [0.7.1] — 2026-07-02

### Changed
- **The `kafka` feature is now on by default.** `cargo build` / `cargo install`
  (and the release build behind `/release`) now include Kafka support, so the
  on-PATH `tstr` carries `$.kafka` without a special build flag — previously a
  default release build silently omitted it, surfacing as
  `unknown built-in function '$.kafka()'` at runtime. The default build now
  inherits the rskafka dependency set and the Rust ≥1.85 (edition 2024) floor;
  for a lean build without Kafka, use `cargo build --no-default-features`.

<a id="v0.7.0"></a>
## [0.7.0] — 2026-07-02

The Kafka DSL (feature-gated, added in 0.6.6) moves to a configured **handle**
model: `$.kafka(config)` returns a handle you set `.topic` / `.key` / `.headers`
on, then `send` / `since`. This adds message headers, a message key, and broker
`requiresTypeId` / `requiresKey` guardrails — at the cost of a breaking change to
the Kafka primitives.

→ **Migration:** [UPGRADING.md § 0.7.0](UPGRADING.md#v0.7.0)

### ⚠️ Breaking
- **Kafka: `$.kafka` takes a config object, `produce` → `send`, `since` takes no
  argument.** `$.kafka(config)` now accepts `{ bootstrap, requiresTypeId?,
  requiresKey? }` (a bare bootstrap string still works). The topic moved onto the
  handle as a `.topic` field, so `broker.since(topic)` becomes `k.topic = "…";
  k.since()`, and `broker.produce(topic, value [, key])` is replaced by
  `k.send(value)`, which reads `.topic` / `.key` / `.headers` off the handle.
  Only affects suites built against the 0.6.x Kafka feature (opt-in, two patch
  releases old); migrate by hand per UPGRADING — no codemod, since the reshape
  from positional args to field assignments isn't a mechanical substitution.

### Added
- **Kafka message headers and a key**, set as `.headers` (an object) and `.key`
  on the handle before `send`; `find`'s returned message already surfaced both.
- **`requiresTypeId` / `requiresKey` guardrails** on the broker config. When set,
  `send` fails fast — before connecting — unless a `__TypeId__` header (the
  Spring Kafka type hint), respectively a key, is present. For topics whose
  consumers can't deserialize a message without them.

<a id="v0.6.7"></a>
## [0.6.7] — 2026-07-02

### Documentation
- **Kafka broker-address configuration.** The README's Kafka section now covers
  keeping the broker address out of individual tests — in `tstr.yaml` constants
  referenced via `$.kafka(${kafka.bootstrap})` / `$.kafka("{{kafka.bootstrap}}")`,
  or built once in a parent `setup.tstr` and `export`ed to cascade — mirroring
  how HTTP suites handle `urlPrefix`. Docs-only; no code change from 0.6.6.

<a id="v0.6.6"></a>
## [0.6.6] — 2026-07-02

### Added
- **Kafka produce/consume**, behind an opt-in `kafka` cargo feature
  (`cargo build --features kafka`); the default build is unchanged. Four
  primitives: `$.kafka(bootstrap)` opens a broker handle, `broker.since(topic)`
  marks the topic's current end offsets as a cursor, `cursor.find(regex,
  timeout)` seeks back to that mark and scans forward for the first message whose
  full payload matches `regex` (returning a response-shaped `{body, raw, format,
  key, partition, offset, timestamp, headers}` message, or `null` on timeout),
  and `broker.produce(topic, value [, key])` sends and returns a `{partition,
  offset}` ack. Built on the **drain + tight-window** model — mark before the
  action, so a message produced afterward is caught while pre-existing ones are
  skipped (a not-yet-created topic marks as empty and reads from the start once
  it appears). Message bodies get the same JSON/ndjson/SSE/text sniffing as HTTP
  responses; failure output carries a `KAFKA find <topic> /<regex>/` context
  line. Pure-Rust (`rskafka`, no C toolchain); plaintext brokers only for now.
  Live round-trips run via `scripts/kafka-it.sh` (throwaway Redpanda);
  `cargo test --features kafka` covers the hermetic units.
- **Duration literals** — `30s`, `500ms`, `2m` now evaluate to a number of
  milliseconds in any expression (previously only inside `retry(...)`), so
  `cursor.find(regex, 30s)` and the like read naturally. Guarded by a word
  boundary, so `30something` is untouched.

### Changed
- `serde`/`serde_json` relaxed from exact pins to caret `1.0` (the `kafka`
  feature pulls `rsasl`, which requires newer minimums). Default builds are
  unaffected. Note: the `kafka` feature requires Rust ≥ 1.85 (rskafka is
  edition 2024).

<a id="v0.6.5"></a>
## [0.6.5] — 2026-06-30

### Added
- **Concurrent `--repeat` now renders live wide bars** instead of falling back to
  summary-only. In a terminal, each directory gets one bucketed bar sized to its
  `tests × repeat` cells, filling as the N overlapping passes complete (forced to
  bars mode — per-test glyphs wouldn't fit). The slots are pre-sized once up
  front so the concurrent runs report into shared, correctly-sized bars. Off a
  terminal (piped) it's still summary-only. Adds tests for the `tests × repeat`
  wide layout and for repeat totals accumulating across passes (both modes).

<a id="v0.6.4"></a>
## [0.6.4] — 2026-06-30

### Changed
- **Slot-display rendering is now pure and unit-tested.** The bar/glyph row and
  the status line were extracted into `render_slot_row` / `render_status_line`
  (returning `String`); `write_slot_row`, `draw_status`, and the initial draw all
  route through them, so the live display and its tests share one source of truth.
  No visible change. Adds 10 tests covering glyph rows, bucketed bars, forced
  bars mode, all-pending placeholders, the `tests × repeat` wide layout, and the
  `Iter k/N` marker — the first real coverage of the progress display.

### Docs
- README: documented that sequential `--repeat` resets the slot display each
  pass and shows an `Iter k/N` marker (the 0.6.3 behavior).

<a id="v0.6.3"></a>
## [0.6.3] — 2026-06-30

### Fixed
- **Sequential `--repeat` now resets the interactive slot display each
  iteration.** The first pass filled the bars; every later pass wrote past the
  full bar and the status counter ran past its denominator (`Tests: 0/2
  Passed: 6`). Each iteration now clears the boxes back to pending, zeroes the
  live counters and the error panel, and re-fills — with an `Iter k/N` marker on
  the status line so you can see which pass is running. Single runs (no
  `--repeat`) are visually unchanged. Piped / `-q` output was never affected.

<a id="v0.6.2"></a>
## [0.6.2] — 2026-06-30

`--repeat N` is implemented — it used to warn ("not yet supported") and run once.

### Added
- **`--repeat N`** runs the whole suite N times, accumulating totals across
  iterations (the summary shows `(N iterations x M tests)`). Its main use is
  surfacing flaky/intermittent failures.
- **`--repeat-mode <sequential|concurrent>`** chooses how the iterations run:
  - `sequential` (default) — one pass after another. Safe; never races a suite
    against copies of itself.
  - `concurrent` — N independent passes at once (via rayon). Requires a suite
    that tolerates copies of itself (no colliding fixed-name resources). Output
    drops to summary-only, since per-test slots/streaming can't represent N
    overlapping runs.
- **`defaults.repeat_mode`** in `tstr.yaml` — a suite declares its own repeat
  safety. Precedence: `--repeat-mode` flag → suite config → `sequential`. An
  unrecognized config value warns and falls back to sequential.

<a id="v0.6.1"></a>
## [0.6.1] — 2026-06-30

### Fixed
- **README accuracy pass (docs only).** Three descriptions now match the runner:
  - Phases run in order **per directory**, not as one global sweep — a
    directory's children run in parallel *between* its setup and its tests, so a
    child's tests can run while the parent sits between phases. ("Phases run in
    order across the whole suite" was misleading.)
  - Streaming output (Normal mode) emits the full label set —
    `PASS` / `FAIL` / `SKIP` / `DISABLED` / `INCOMPATIBLE`, plus `LOAD` when a
    `const` file loads — not just `PASS`/`FAIL`/`SKIP`.
  - `const` loads stream as `LOAD` but get **no run-log entry**; the log records
    only `test`/`setup`/`cleanup` outcomes.

<a id="v0.6.0"></a>
## [0.6.0] — 2026-06-30

`setup`/`cleanup` are now scaffolding-only — a `.setup.tstr` or `.cleanup.tstr`
in a leaf directory is rejected at startup instead of being run as a regular
test. This removes the leaf-tolerance shim added in 0.4.0.

→ **Migration:** [UPGRADING.md § 0.6.0](UPGRADING.md#v0.6.0)

### ⚠️ Breaking
- **`.setup.tstr` / `.cleanup.tstr` in a leaf directory is now a hard error.**
  Setup/cleanup scaffold the directories *below* them, so they only belong in a
  non-leaf dir. Previously they were tolerated at a leaf (run as regular tests
  with a warning); now `tstr run` exits with an error listing the offending
  files. Move them to a non-leaf parent (whose setup cascades into the leaf), or
  rename them to `.test` if they're really tests. Run
  `scripts/migrate-leaf-scaffolding.py` to migrate mechanically.

### Changed
- The directory-role rule is now symmetric and fully enforced: `test`/`fetch`
  live only in leaf dirs, `setup`/`cleanup` only in non-leaf dirs, `const`/`lib`
  anywhere. README's "Mental Model" rewritten to match; the obsolete leaf-fold
  path is gone from the runner.

### Fixed
- `examples/demo` runs again. A `fixtures/` data dir had made the example a
  non-leaf directory holding tests (tripping the leaf-only-tests rule); its tests
  now live in a `cases/` leaf, the producer/consumer pair is ordered, the
  `@fixtures/...` reference is suite-root-relative, and a `tstr.yaml` marks the
  root.

<a id="v0.5.2"></a>
## [0.5.2] — 2026-06-26

### Changed
- **Run logs moved to `<suite-root>/logs/tstr-<NNNN>.log`.** They no longer drop
  a `tstr-last-run.log` in whatever directory you happened to run from. Each run
  gets its own zero-padded, incrementing numbered file, and a
  `tstr-last-run.log` **symlink** in the suite root points at the most recent.
  History is kept so you can compare runs (handy for intermittent failures).

### Added
- **Auto-prune of run logs.** `logs/` is pruned to the most recent **10** runs by
  default; set `log_retention:` in `tstr.yaml` to change it (`0` keeps all). A
  `logs/.gitignore` is written automatically so run logs aren't committed.
- **`tstr clean [dir]`** — removes tstr's run-log artifacts (`tstr-*.log`, the
  managed `.gitignore`, and the symlink) under the suite root. Surgical: it
  preserves any non-tstr files and won't delete a non-empty `logs/` directory.

### Fixed
- A root-level `logs/` directory is now skipped by discovery, so it can't turn
  the suite root into a non-leaf (which would otherwise trip the "tests live only
  in leaf directories" rule on every run after the first).

<a id="v0.5.1"></a>
## [0.5.1] — 2026-06-26

### Fixed
- **`tstr run` with an invalid target no longer hangs.** A non-directory target
  used to fall through to a "pattern" path that resolved the root to the current
  working directory and walked the entire tree (e.g. running `tstr run asdf` from
  a repo root above the suite). `run` now takes a **directory only** — a
  non-existent or non-directory target fails immediately (`error: no such
  directory: '…'`). There is no name/glob filtering and no single-file execution
  for `run` (`tstr list` keeps its name-search pattern).
- **Relative `@file` references resolve against the suite root**, not the process
  working directory. A test that did `req.body = @notify/x.json;` only worked when
  invoked from inside the suite; now it resolves correctly regardless of where
  `tstr` is run from. Absolute paths are unchanged; the suite root is threaded
  through the scope (not the process cwd), so it stays correct under the
  concurrent runner.

### Changed
- Removed the dead "pattern filtering not yet supported; running entire suite"
  warning from `run`.

<a id="v0.5.0"></a>
## [0.5.0] — 2026-06-26

Files gain a **metadata block** — `key: value` directives above the function
block, like HTTP headers. The `disabled` marker moves there from the body, which
is a breaking change.

→ **Migration:** [UPGRADING.md § 0.5.0](UPGRADING.md#v0.5.0)

### ⚠️ Breaking
- **The body-statement `disabled "reason";` marker is removed.** Turn a file off
  with a `disabled:` line in the metadata block instead (reason unquoted). Run
  `scripts/migrate-disabled.py` over your suite to convert automatically.
  `disabled` is now an ordinary identifier everywhere in the body.

### Added
- **Metadata block.** Optional `key: value` directives above the function block
  (fixed order: metadata → param header → braced body). No sigil; the value is
  the rest of the line, unquoted. Unknown keys are a hard error.
- **`requires:`** — a minimum tstr version (`>= 0.5.3`, bare version means `>=`).
  A binary that doesn't satisfy it reports the file **INCOMPATIBLE** (a distinct
  status — `needs >= 0.5.3, have 0.5.0`) and skips it, rather than failing
  cryptically.
- **`disabled:`** — the file-off marker, now in metadata. Mandatory reason, no
  quotes; reported as **DISABLED** as before.
- **`blast-radius:`** — skip the downstream collateral a disabled/failed file
  owns (the side-effect dependents the input-cascade can't see). Leaf-local,
  forward-only. Forms: `N` (next N tests), `all`/`*` (the rest of the leaf), and
  `<=PREFIX` (through the first file whose name starts with `PREFIX`, inclusive).
  Collateral shows as `SKIP  blast-radius from <culprit>`.

### Changed
- **`disabled` is no longer a keyword.** With the marker gone from the body, it
  parses as a plain identifier (`disabled = false;`, `disabledCount`, etc.)
  without the old quoted-reason special case.

<a id="v0.4.6"></a>
## [0.4.6] — 2026-06-25

### Changed
- **No-input files can drop the `-->` and open straight into `{ ... }`.** The
  input header arrow is now required only when a file actually declares params
  (`a, b --> { ... }`). A file that takes no inputs can now be written as a bare
  `{ ... }` body instead of the left-empty `--> { ... }`. The explicit
  `--> { ... }` form still parses as a synonym, so existing suites are
  unaffected.

<a id="v0.4.5"></a>
## [0.4.5] — 2026-06-25

### Fixed
- **No more spurious "pattern filtering not yet supported" warning on
  directory-scoped runs.** A directory target (e.g. `tstr run commerce`) is
  scoped via `target_dir` during discovery, but it also produced a redundant
  glob pattern that tripped the not-yet-implemented warning. The warning now
  fires only for a genuine glob target (no `target_dir`), where the run really
  is unfiltered.

<a id="v0.4.4"></a>
## [0.4.4] — 2026-06-23

Follow-up to 0.4.3, which made `lib/` subtrees discoverable on leaf-scoped runs.

### Changed
- **`lib/` files no longer claim a row in the slot display.** Now that lib
  subtrees are discovered, the bar/slot sizing skips `lib` files (as it already
  did for consts and non-leaf scaffolding) — libraries are callable definitions,
  not tests, so they stay out of the run output.

### Fixed
- **A `.test.tstr` file inside a `lib/` directory is now rejected** with an error
  instead of being silently discovered. Lib dirs hold callable definitions only;
  runnable tests belong in a leaf.

<a id="v0.4.3"></a>
## [0.4.3] — 2026-06-23

### Fixed
- **Leaf-scoped runs now load libs from ancestor `lib/` subtrees.** Targeting a
  single leaf (e.g. `tstr run commerce/payment`) pruned any sibling `lib/`
  directory hanging off an ancestor before it was scanned, so a `createCharge`
  call that resolved fine under `tstr run commerce` errored with "unknown lib"
  under the leaf run. Discovery now keeps `lib/` subtrees along the target's
  ancestor chain (harvesting only their `.lib.tstr` files), matching what the
  lib resolution rule already promised. Sibling-*branch* libs (not on the
  ancestor chain) stay correctly excluded.

<a id="v0.4.2"></a>
## [0.4.2] — 2026-06-22

### Changed
- **Interactive display lists one row per test when the run target is a leaf**
  (e.g. `tstr run commerce/payment/success`) — each row labeled by test name and
  live-updating — instead of collapsing every test into a single `(root)` bar.
  Broader runs still use the grouped per-directory bars.

<a id="v0.4.1"></a>
## [0.4.1] — 2026-06-22

### Documentation
- README now documents leaf `setup`/`cleanup` behavior (they run as regular
  tests with no fail-fast cascade) and the non-leaf scaffolding display
  exclusion — previously only in CHANGELOG/UPGRADING.

<a id="v0.4.0"></a>
## [0.4.0] — 2026-06-22

Files are now **functions**. This is a breaking grammar change: every `.tstr`
file must be migrated.

→ **Migration:** [UPGRADING.md § 0.4.0](UPGRADING.md#v0.4.0)

### ⚠️ Breaking
- **Function form is mandatory.** Every file needs an input header (`a, b -->`,
  or a bare `-->` for none) and a braced `{ ... }` body. Bare statement bodies
  no longer parse.
- **`<--` output lines removed** (at file level). Publishing is now `export`.
  The block-collect `<--` *inside lambdas* is unchanged.
- **`return` no longer publishes.** A top-level `return;` is void (it only
  halts); a top-level `return <value>` is a parse error — use `export`.
- **Leaf `setup`/`cleanup` run as regular tests** — no fail-fast cascade at a
  leaf (a warning names them). Move them to a non-leaf dir to keep cascade
  semantics.

### Added
- **`export expr [as name], ...`** — publishes named bindings (ambient broadcast
  for setup/test/const; the value bound at the call site for a lib). A bare
  identifier self-names; computed values need `as` (`export r.id as id`).
  Non-terminating and repeatable.
- **Scalar `return` inside lambdas** — `{ x --> ...; return v; }` yields `v`.

### Changed
- **Display** — non-leaf `setup`/`cleanup` are kept out of the slot bars and the
  per-suite summary table. Their failures still stream, get a table row, and set
  the exit code; only passing/skipped scaffolding is hidden.

### Fixed
- **Test → test variable passing** — a test now sees an earlier test's exports
  within the same directory. The directory scope was frozen before the test
  phase, so test exports were silently discarded.

---

_For changes before 0.4.0, see the git history (`git log`)._
