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
4. **Verify the README reflects the change (pre-commit gate).** Inspect the
   uncommitted diff. If it contains any **user-facing** change — a CLI flag, a
   DSL/config surface, or behavior a user would observe — confirm `README.md`
   already describes it. If it doesn't, either update `README.md` now or **abort
   the release** (which path you take doesn't matter; an inconsistent README must
   not ship). Purely internal changes (refactors, tests, comments) need no README
   update — say so and continue. README is updated when a feature is implemented,
   not generated here; this step only catches a doc that fell out of sync.
5. **`cargo test`** — must pass. This compiles the crate (debug), which rewrites
   the `name = "tstr"` line in `Cargo.lock` to the new version. If tests fail,
   stop and report — do not commit.
6. **`cargo build --release`** — required, not optional. Doug's on-PATH `tstr`
   (`~/bin/tstr`) is a symlink to `target/release/tstr`, and Rust keeps debug and
   release artifacts separate — so the debug compile in step 5 does **not** update
   the binary he actually runs. This step both gates the release (a release-only
   compile failure blocks it) and refreshes his installed binary. If it fails,
   stop and report — do not commit.
7. **Stage** — `git add -A` (Cargo.toml, Cargo.lock, CHANGELOG.md, any
   UPGRADING.md/codemod, and the source change all ride in one commit).
8. **Commit** — use the new CHANGELOG entry as the message: first line is the
   `## [x.y.z] - date` heading, body is the rest of the entry. End the message
   with:
   `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
9. **Tag** — `git tag -a vX.Y.Z -m "vX.Y.Z"` matching the bumped version.
10. **Push** — `git push --follow-tags` to the current branch (so the commit and
   its tag go together).
11. **Mirror the README to layer-services** — copy `README.md` to
   `~/dev/layer-services/tstr/README.md` so that repo's vendored copy stays in
   sync. This only writes the file; it does not commit in layer-services — tell
   Doug that repo now has an uncommitted README change for him to handle there.
12. **Announce on Slack** — `scripts/announce-release.sh`. It derives the version
   from `Cargo.toml` and the one-line summary from the newest CHANGELOG entry, so
   it takes no arguments. Runs last, after the push, so the tag link resolves.
   Needs `SLACK_BOT_TOKEN` and `TSTR_RELEASE_SLACK_CHANNEL` in the environment;
   if either is unset the script exits 2 with a message — report that the release
   itself succeeded but the announcement didn't go out, and do not treat it as a
   failed release. Use `--dry-run` to inspect the payload without posting.

## Notes

- **Two compiles, two purposes.** `cargo test` (step 5, debug) proves correctness
  and syncs `Cargo.lock`. `cargo build --release` (step 6) is what actually
  updates the binary on Doug's PATH — debug compiles never touch
  `target/release/`. Both are required; neither substitutes for the other.
- Lock sync is a side effect of step 5, not a hook. There is intentionally no
  pre/post-commit hook for it — bumping `Cargo.toml` makes the lock stale, and
  the test run in the same working tree refreshes it before staging.
- One commit per release: the version bump, lock sync, and the change itself are
  never split into a trailing "Sync Cargo.lock" commit.
- The README gate (step 4) is a consistency check, not a content generator: it
  fails the release only when a user-facing change shipped without its docs.
- **The Slack announcement is configured by environment, not by this file.** The
  channel lives in `TSTR_RELEASE_SLACK_CHANNEL` because this repo is public and
  the target is an internal workspace channel — don't hardcode it here or in
  `scripts/announce-release.sh`. Likewise never echo `SLACK_BOT_TOKEN` or run
  curl with `-v` (which prints the Authorization header).
