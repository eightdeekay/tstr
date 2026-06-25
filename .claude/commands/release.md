Cut a release of tstr in one step: version bump → CHANGELOG → test (which syncs
`Cargo.lock`) → commit → tag → push. Run this from the repo root with the
change already implemented and tests green.

## Decide the bump

Inspect the working/staged diff and pick the new version from the current
`Cargo.toml` version:

- **patch** (x.y.Z+1) — bug fix or backwards-compatible change. The default.
- **minor** (x.Y+1.0) — a breaking change to the DSL or CLI surface.

If unsure whether something is breaking, say so and ask before bumping.

## Steps

1. **Bump `Cargo.toml`** — set `version` to the new number.
2. **CHANGELOG.md** — add a new entry at the top:
   `## [x.y.z] - YYYY-MM-DD` (today's date), followed by the change summary.
   Keep the existing entry format.
3. **Breaking changes only** — also update `UPGRADING.md` and add/extend the
   codemod under `scripts/` so users can migrate mechanically.
4. **`cargo test`** — must pass. This compiles the crate (debug), which rewrites
   the `name = "tstr"` line in `Cargo.lock` to the new version. If tests fail,
   stop and report — do not commit.
5. **`cargo build --release`** — required, not optional. Doug's on-PATH `tstr`
   (`~/bin/tstr`) is a symlink to `target/release/tstr`, and Rust keeps debug and
   release artifacts separate — so the debug compile in step 4 does **not** update
   the binary he actually runs. This step both gates the release (a release-only
   compile failure blocks it) and refreshes his installed binary. If it fails,
   stop and report — do not commit.
6. **Stage** — `git add -A` (Cargo.toml, Cargo.lock, CHANGELOG.md, any
   UPGRADING.md/codemod, and the source change all ride in one commit).
7. **Commit** — use the new CHANGELOG entry as the message: first line is the
   `## [x.y.z] - date` heading, body is the rest of the entry. End the message
   with:
   `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
8. **Tag** — `git tag -a vX.Y.Z -m "vX.Y.Z"` matching the bumped version.
9. **Push** — `git push --follow-tags` to the current branch (so the commit and
   its tag go together).

## Notes

- **Two compiles, two purposes.** `cargo test` (step 4, debug) proves correctness
  and syncs `Cargo.lock`. `cargo build --release` (step 5) is what actually
  updates the binary on Doug's PATH — debug compiles never touch
  `target/release/`. Both are required; neither substitutes for the other.
- Lock sync is a side effect of step 4, not a hook. There is intentionally no
  pre/post-commit hook for it — bumping `Cargo.toml` makes the lock stale, and
  the test run in the same working tree refreshes it before staging.
- One commit per release: the version bump, lock sync, and the change itself are
  never split into a trailing "Sync Cargo.lock" commit.
