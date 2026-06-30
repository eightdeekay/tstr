# Changelog

All notable changes to tstr are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/); versioning follows
[SemVer](https://semver.org/). Pre-1.0, **breaking changes bump the minor**
(`0.3.x → 0.4.0`), not the patch.

Releases with a ⚠️ block require action on existing suites — the migration steps
live in [UPGRADING.md](UPGRADING.md), cross-linked per version.

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
