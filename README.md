# tstr

A CLI HTTP API test runner with a custom DSL. Structural execution model (phase → directory → lex order), library functions as first-class primitives, project-wide constants via `tstr.yaml`, and per-directory introspection.

## Quick Start

```bash
cargo build --release
ln -s ~/dev/tstr/target/release/tstr ~/bin/tstr

tstr run                          # run all tests (walks up to find tstr.yaml)
tstr run notify                   # run tests under notify/  (filter TODO; runs all for now)
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

- Phases run in order across the whole suite: const → setup → test → cleanup
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

**Tests live in leaf directories only.** A `.test.tstr` (or `.fetch.tstr`) is
allowed only in a directory that has no subdirectories. A directory with
subdirectories is *scaffolding* — const/setup/cleanup/lib only — whose setups
cascade down into the leaves. This makes "test group = leaf directory" an
invariant (one slot per group, each leaf's tests sequential, leaves parallel).
Putting a test in a non-leaf dir is a hard error at startup.

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

Scalars replace, lists append (so `--import` adds to defaults rather than replacing).

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

### `disabled`

```
disabled "I-123: API returns groupId not id, fix pending";
```

Turns the **whole file off** — a known-broken test whose fix is postponed.
Unlike `if` (which conditionally runs *part* of a file), `disabled` is
unconditional, turns off the *whole* file, and carries a **mandatory
reason**. The runner short-circuits before any statement executes — so the
marker's position in the file is irrelevant, and no HTTP calls or assertions
fire — and reports the file as a distinct **DISABLED** status (cyan), not a
plain skip. `disabled` stays usable as an ordinary identifier everywhere it
isn't followed by a quoted reason (`disabledCount = 0;` still parses).

List every disabled file and its reason without running anything:

```bash
tstr list --disabled
```

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

```
# tests/tag-crud/01-create.setup.tstr
--> {
  req = { urlPrefix: ${orgService.baseUrl} };
  req.body = { name: "test-tag", type: "label" };
  r = req.post("/v4/tags") ? 2xx | "create failed";
  export req, r.id as tagId;
}

# tests/tag-crud/02-replace.test.tstr
req, tagId --> {
  req.body = { name: "test-tag-replaced" };
  req.put("/v4/tags/{{tagId}}") ? 2xx | "replace failed";
}

# tests/tag-crud/03-add-item.test.tstr
req, tagId --> {
  req.body = { itemId: "abc-123" };
  req.post("/v4/tags/{{tagId}}/items") ? 2xx | "add-item failed";
}

# tests/tag-crud/04-cleanup.cleanup.tstr
req, tagId --> {
  req.delete("/v4/tags/{{tagId}}") ? 204 | "cleanup failed";
}
```

No fake gate variables. Order is the filename order. Setup's `export` broadcasts `req` and `tagId` to every subsequent file in the dir.

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
    01-setup.setup.tstr          # any project setup
    02-test.test.tstr            # calls createOrg("alpha") — uses orgService's req, not profile's
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

- **`@path`** — load file content: `template = @fixtures/group.json;` (JSON files auto-parsed)
- **`{{interpolation}}`** — variable substitution in strings and URLs
- **JSON construction** — `req.body = { name: "Test", count: 3 };`
- **Field mutation** — `req.headers."content-type" = "text/plain";` or `req.headers["content-type"] = "text/plain";`

## CLI

```
tstr run [target]                 # run tests under target (or cwd)
tstr list [target]                # per-directory tables of files visible
tstr --config path/to/yaml ...    # explicit config (overrides project tstr.yaml)
tstr --version
```

**`run` flags:**

| Flag | Effect |
|---|---|
| `--url <base>` | shorthand for `--set urlPrefix=<base>` |
| `--set 'KEY=VALUE'` | set an ambient variable (repeatable) |
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
| `--disabled` | list only files turned off via a `disabled "reason"` marker, with each one's reason (ignores `--type`/`--flat`) |

Example `list` output:

```
profile/sso-user/crud/tests
| name               | role    | params           | returns |
|--------------------|---------|------------------|---------|
| 01-cleanup         | setup   | —                | orgId   |
| 02-create-sso-user | test    | orgId            | —       |
| 03-list-sso-users  | test    | orgId            | —       |
| 04-get-sso-user    | test    | orgId, userId    | —       |
| 05-delete-sso-user | cleanup | orgId, userId    | —       |
```

## Output Modes

- **Interactive** (default in terminal) — one slot row per top-level directory (or per child of the target when scoped). Per-test glyphs (`✓✗-·`) when there's room, or a colored bucketed bar otherwise — gradient hue from green (all pass) through yellow (skip-leaning) to red (all fail). `--display=bars` forces bars on short rows.
- **Normal** (`-v` or piped) — streaming PASS/FAIL/SKIP per test.
- **Verbose** — streaming + timing, scope changes, log output.
- **Quiet** (`-q`) — only summary and failures.

## Run Log

`tstr-last-run.log` (in cwd) captures **every** test run, regardless of pass/fail and verbosity. Per-test entries include:

- PASS / FAIL / SKIP label, test name, source path
- HTTP endpoint that was called
- All assertion failures (and runtime errors, with prior failures preserved)
- A table of ambient variables in scope at file start: source, name, value (truncated)
- `$.log()` messages

The log holds only the most recent run — it's overwritten (truncated) at the start of every run.

## Failure Output

```
  FAIL  05 Set Override  (refunds/05_set_override.test.tstr)
        PUT https://api.example.com/v4/overrides
        line 7: Failed to set override (got 404)
        line 9: wrong dashboard type (got null, expected "Platform")
```

## v0.3.0 Known Limitations / TODOs

Tracked here for visibility; none are blockers:

- **`--repeat N`** — not yet rewired through the structural runner. Single-run only for now.
- **`--stop-on-error`** — accepted but not propagated.
- **Pattern filtering** (`tstr run path/to/foo`) — not yet wired; structural runner runs the whole suite. Warns and continues if a pattern is given.
- **Matrix fan-out** — was DAG-coupled; needs reimplementation for the structural model.
- **`.const.tstr` integration with `${name}`** — currently const returns flow into ambient scope; strict `${name}`-only access for const files is a follow-up.
- **Library call caching** — every call re-executes; opt-in memoization will land when the semantics are pinned down.
- **`--reachable`** for `tstr list --type lib` — call-graph analysis to limit listed libs to those actually invoked.

## File form

Every file is a function: a mandatory input header, a braced body, and
`export` for whatever it publishes.

```
a, b --> {
  ... statements ...
  export x, r.id as id, payIntentId;
}
```

- **Input header is required.** `a, b -->` declares the ambient values the file
  consumes; a file that takes none still writes a bare `-->`.
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
