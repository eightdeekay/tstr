# Upgrading

Migration steps for releases that need action on existing suites. Each section
cross-links to the full change list in [CHANGELOG.md](CHANGELOG.md).

<a id="v0.12.0"></a>
## 0.12.0 — A failure halts its leaf; `--stop-on-error` → `--continue-on-error`

→ **Full change list:** [CHANGELOG § 0.12.0](CHANGELOG.md#v0.12.0)

No codemod: no `.tstr` file changes at all. The flag rename only touches how you
invoke `tstr`, and it fails loudly — `--stop-on-error` no longer parses, so one
run of your CI command finds every place that passes it.

### Drop `--stop-on-error`

```
# before
tstr run --stop-on-error .

# after
tstr run .
```

The flag was accepted but never propagated — a documented no-op since the
structural runner landed — so removing it changes no behavior on its own. It is
now **rejected** rather than ignored:

```
error: unexpected argument '--stop-on-error' found

  tip: a similar argument exists: '--continue-on-error'
```

### A failure now halts the rest of its leaf

This is the change that needs a look at your suites. Previously, when a test
failed, the remaining tests in that leaf ran anyway. Now they don't — they report
`SKIP  halted: <culprit> failed`, because once a test fails the ones after it in
the same leaf are running against unknown state.

Nothing to edit; what changes is what a broken run *looks like*. Expect a failing
leaf to show one `FAIL` and a run of `SKIP`s where it used to show a `FAIL`
followed by a pile of cascading failures. Suite totals shift accordingly — skips
up, passes and fails down. The exit status is unchanged: a failure still fails
the run.

The halt is **leaf-local**. Sibling leaves and directories run to completion, so
one broken leaf never masks the rest of the suite's verdict.

### If you want the old behavior

```
tstr run --continue-on-error .
```

Every test in a leaf runs regardless of what failed before it, exactly as it did
pre-0.12.

### `blast-radius:` now *narrows* the fallout

A file's own `blast-radius:` is still honored verbatim, and it wins over the
default halt — which means declaring one now **bounds** the damage instead of
extending it:

| Culprit | Result |
|---|---|
| fails, no `blast-radius:` | rest of the leaf skips (the new default) |
| fails, `blast-radius: 2` | exactly the next 2 skip; the leaf **resumes** at the third |
| fails, `blast-radius: all` | rest of the leaf skips (same as the default) |
| `disabled:`, no `blast-radius:` | nothing skips — it never ran, so it broke nothing |
| `disabled:`, `blast-radius: N` | the next N skip (unchanged) |

So a leaf where a known-flaky early test shouldn't stop everything after it is
best served by giving that test an explicit radius covering just its real
collateral, rather than reaching for `--continue-on-error` suite-wide.

<a id="v0.10.0"></a>
## 0.10.0 — Flat config (no `defaults:`); `--jobs`→`--threads`, `--repeat-mode`→`--stress`

→ **Full change list:** [CHANGELOG § 0.10.0](CHANGELOG.md#v0.10.0)

No codemod: the config edit is a few lines you own, and the flag renames only
touch how you invoke `tstr`, not any file. Every change fails loudly — a stale
config errors at load and names the offending key; a removed flag errors at
parse — so one `tstr run` finds them all.

### Config settings move to the top level

The `defaults:` wrapper is gone. Its keys (`import`, `display`) now sit at the
top level, next to `log_retention` and `constants`. And **unknown keys are now
rejected** rather than silently ignored, so the old nesting fails loudly.

```yaml
# before
defaults:
  import:
    - ~/.tstr/shared-libs
  display: bars
  repeat_mode: concurrent   # removed — see --stress below
log_retention: 10
constants: { ... }

# after
import:
  - ~/.tstr/shared-libs
display: bars
threads: 16                 # new: was CLI-only, now settable here too
log_retention: 10
constants: { ... }
```

The load error names the key and lists what's valid, e.g.:

```
config error: failed to parse tstr.yaml: unknown field `defaults`,
expected one of `import`, `display`, `threads`, `constants`, `log_retention`
```

The `repeat_mode:` key is dropped entirely — the concurrent-repeat behavior it
enabled is now the `--stress` flag (below), chosen per-invocation rather than
declared as a suite default.

### `-j` / `--jobs` → `-t` / `--threads`

Same knob (worker-pool size), new name — it's a pool, not a count of "jobs."
`--jobs`/`-j` no longer parse. It's now also settable in config as `threads:`;
the flag overrides the config value.

```
# before
tstr run -j 32 .
# after
tstr run -t 32 .          # or set `threads: 32` in tstr.yaml
```

### `--repeat-mode concurrent` → `--stress N`; `--repeat` is always sequential

The `sequential`/`concurrent` mode enum is gone, split into two flags by intent:

- `--repeat N` — N passes, **sequential** (soak / flake-hunt). Unchanged meaning.
- `--stress N` — N passes, **overlapping** (stress / load). What
  `--repeat N --repeat-mode concurrent` used to do.

They're mutually exclusive, and `--stress` requires N ≥ 2.

```
# before
tstr run --repeat 20 --repeat-mode concurrent .
# after
tstr run --stress 20 .
```

### `-c` is now shorthand for `--config`

`--config` gained a `-c` short form. If you have a shell alias or script binding
`-c` to something else for `tstr`, rename it.

<a id="v0.9.0"></a>
## 0.9.0 — Constants deep-merge; unknown `$.postgres` fields are errors

→ **Full change list:** [CHANGELOG § 0.9.0](CHANGELOG.md#v0.9.0)

No codemod: neither change is a mechanical rewrite. The first needs a decision
about which config layer owns which field; the second only surfaces keys that
never did anything. Both fail loudly, so a single `tstr run` finds them.

### Object constants deep-merge across layers

When two layers define the same object constant, their fields now **union**
rather than the later layer replacing the whole object. Most suites see no
change — a difference only appears when a field is set in one layer and absent
in another.

The payoff: a project `tstr.yaml` and a user `~/.config/tstr/config.yaml` can
co-own one object, so the `${dbHost}` scalar-indirection dance is no longer
forced.

```yaml
# before — per-developer values smuggled in as flat scalars
# ~/.config/tstr/config.yaml
constants:
  dbHost: db.example.com
# tstr.yaml
constants:
  db:
    host: ${dbHost}
    database: notify

# after — each layer owns its own fields
# ~/.config/tstr/config.yaml
constants:
  db:
    host: db.example.com
# tstr.yaml
constants:
  db:
    database: notify
```

The old shape still works untouched. If you adopt the new one, **delete the
`host: ${dbHost}` line from the project file** — the project layer loads last and
wins on every key it sets, so leaving it there overrides the user's value with a
reference to a constant that no longer exists. That surfaces as
`unresolved constant reference(s)` at load.

Note the precedence direction: a later layer can be *added to* but not overridden.
A developer may supply fields the project omits; to override one the project sets,
use `--set` or `--config`.

Mappings merge. Everything else — scalars **and sequences** — is replaced by the
later layer. `defaults.import` is unaffected and still appends.

### Unrecognized `$.postgres` config fields now fail

An unknown key on the handle object used to be silently dropped. It's now an
error naming the key:

```
$.postgres: unknown config field(s): caCert. Known fields: host, port, database,
user, password, schema, sslmode, sslInsecure, sslRootCert, url
```

Fix the name or remove the key. The most common cases are a miscased
`sslinsecure` and a `caCert`/`sslCert` invented for the CA bundle — that field is
`sslRootCert`, new in this release.

<a id="v0.7.0"></a>
## 0.7.0 — Kafka moves to a configured handle (`send`, `.topic` / `.key` / `.headers`)

→ **Full change list:** [CHANGELOG § 0.7.0](CHANGELOG.md#v0.7.0)

Only affects suites using the opt-in `kafka` feature (added in 0.6.6). All four
primitives changed:

- `$.kafka("host:9092")` still works, but broker options now live in a config
  object: `$.kafka({ bootstrap: "host:9092", requiresTypeId: true })` — usually a
  `tstr.yaml` constant passed as `$.kafka(${kafka})`.
- The **topic moved onto the handle** — set `k.topic = "…"` instead of passing it
  to `since` / `produce`.
- `broker.produce(topic, value [, key])` → `k.send(value)`, reading `.topic` /
  `.key` / `.headers` off the handle.
- `broker.since(topic)` → `k.since()` (reads `.topic`).

### Migrate by hand

```
# before (0.6.x)
broker = $.kafka("localhost:9092");
cur = broker.since("orders.events");
broker.produce("orders.commands", { type: "cancel" }, orderId);

# after (0.7.0)
k = $.kafka("localhost:9092");        # or $.kafka(${kafka}) with a config object
k.topic = "orders.events";
cur = k.since();

k.topic = "orders.commands";
k.key   = orderId;
k.send({ type: "cancel" });
```

No codemod ships: the change from positional `produce(...)` arguments to field
assignments on the handle is a structural reshape, not a mechanical
substitution, and the feature is new enough (two patch releases) that hand
migration is trivial.

<a id="v0.6.0"></a>
## 0.6.0 — `setup`/`cleanup` are scaffolding-only (not allowed in a leaf)

→ **Full change list:** [CHANGELOG § 0.6.0](CHANGELOG.md#v0.6.0)

A `.setup.tstr` / `.cleanup.tstr` file in a **leaf** directory (one with no
subdirectories) is now rejected at startup. Setup/cleanup scaffold the
directories *below* them, and a leaf has nothing below it. In 0.4.0 a leaf
setup/cleanup was tolerated — run as a regular test with a warning — and that
shim is now gone.

### Two ways to migrate

1. **Move the setup/cleanup up to a non-leaf parent.** The parent's setup
   cascades into the leaf below it, and its cleanup runs afterward. This is the
   right move when the setup/cleanup really is shared scaffolding:

```
# before — leaf holds setup + tests + cleanup
tag-crud/
  00-create.setup.tstr
  01-replace.test.tstr
  99-cleanup.cleanup.tstr

# after — setup/cleanup scaffold the cases/ leaf
tag-crud/
  00-create.setup.tstr
  99-cleanup.cleanup.tstr
  cases/
    01-replace.test.tstr
```

2. **Rename it to `.test`** if it was never really scaffolding — just a step that
   happened to be tagged setup/cleanup. It then runs as an ordinary test in the
   leaf.

### Automated (recommended)

Run the codemod over your suite. For each leaf dir that has a setup/cleanup
**and** tests, it moves the `*.test.tstr` / `*.fetch.tstr` files down into a
`cases/` subdirectory — leaving the setup/cleanup behind in what is now a
non-leaf parent:

```bash
python3 scripts/migrate-leaf-scaffolding.py path/to/suite
```

A leaf that holds setup/cleanup but **no** tests can't be migrated mechanically
(there's nothing for it to scaffold) — the script lists those for you to handle
by hand (move them up, or delete them). Re-running is safe: once a dir has the
`cases/` child it's no longer a leaf, so it's skipped. Review the diff and commit.

<a id="v0.5.0"></a>
## 0.5.0 — `disabled` moves to the metadata block

→ **Full change list:** [CHANGELOG § 0.5.0](CHANGELOG.md#v0.5.0)

The body-statement `disabled "reason";` marker is gone. A file is now turned off
with a `disabled:` line in the header-region metadata block — above the function
block, alongside `requires:` and `blast-radius:`. The reason is the rest of the
line, unquoted.

```
# before
a, b --> {
  x = 1;
  disabled "I-123: fix pending";
}

# after
disabled: I-123: fix pending
a, b --> {
  x = 1;
}
```

Why: `disabled` was a body statement whose position was explicitly irrelevant —
file-level config masquerading as code, and a context-sensitive keyword that only
meant "off" when followed by a quoted string. Moving it to metadata makes it
unambiguous, drops the mandatory quotes, and frees `disabled` to be an ordinary
identifier everywhere in the body.

### Automated (recommended)

Run the codemod over your suite:

```bash
find path/to/suite -name '*.tstr' -exec python3 scripts/migrate-disabled.py {} +
```

It hoists each body `disabled "reason";` to a `disabled:` metadata line at the
top of the file (unescaping any `\"` in the reason). Files already using the
metadata form, or with no marker, are skipped — so re-running is safe. Review the
diff and commit.

### Manual

Delete the `disabled "reason";` line from the body and add `disabled: reason` as
the first line of the file (no quotes).

<a id="v0.4.0"></a>
## 0.4.0 — function form, `export` / `return` split

→ **Full change list:** [CHANGELOG § 0.4.0](CHANGELOG.md#v0.4.0)

Every `.tstr` file moves to the function form:

```
# before
req, groupId -->
r = req.post("/v4/groups") ? 2xx | "failed";
newId = r.id;
<-- newId

# after
req, groupId --> {
  r = req.post("/v4/groups") ? 2xx | "failed";
  newId = r.id;
  export newId;
}
```

### Automated (recommended)

Run the codemod over your suite:

```bash
find path/to/suite -name '*.tstr' -exec python3 scripts/migrate-syntax.py {} +
```

It wraps each body in `--> { }`, adds a bare `-->` header where one is missing,
and rewrites the file-level `<-- a, b` output line to `export a, b;`. Files
already in function form are skipped, so re-running is safe. Review the diff and
commit.

### Manual checklist

If you'd rather not script it, per file:

- [ ] Add a `-->` header (bare `-->` if the file takes no inputs).
- [ ] Wrap the body in `{ ... }`.
- [ ] Replace the `<-- a, b` line with `export a, b;`.
- [ ] Replace any value-`return` that was publishing a value with `export`.

### Things the codemod can't see

- **`return` semantics.** A top-level `return;` is now void (halt only); a
  top-level `return <value>` is an error. A *value* `return` is only valid
  inside a lambda, where it's the block's yield (`{ x --> ...; return v; }`).
  If you relied on `return` to publish, switch it to `export`.
- **`export … as …` for renames.** `<--` could only re-export a same-named
  variable. To publish a computed value under a name, use the alias form:
  `export r.id as id;` (a bare `export r.id` is an error — it needs `as`).
- **Leaf `setup`/`cleanup` behavior.** In a *leaf* directory these now run as
  regular tests with **no fail-fast cascade** — a failed leaf setup no longer
  skips the rest of the leaf. You'll get a one-line warning naming them. If you
  want the old cascade-blocking, move that scaffolding to a non-leaf parent
  directory. *(Superseded in [0.6.0](#v0.6.0): leaf setup/cleanup are no longer
  tolerated at all — they're rejected at startup.)*
