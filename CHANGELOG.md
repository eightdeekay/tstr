# Changelog

All notable changes to tstr are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/); versioning follows
[SemVer](https://semver.org/). Pre-1.0, **breaking changes bump the minor**
(`0.3.x → 0.4.0`), not the patch.

Releases with a ⚠️ block require action on existing suites — the migration steps
live in [UPGRADING.md](UPGRADING.md), cross-linked per version.

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
