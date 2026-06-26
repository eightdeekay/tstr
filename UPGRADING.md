# Upgrading

Migration steps for releases that need action on existing suites. Each section
cross-links to the full change list in [CHANGELOG.md](CHANGELOG.md).

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
  directory.
