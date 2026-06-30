# Upgrading

Migration steps for releases that need action on existing suites. Each section
cross-links to the full change list in [CHANGELOG.md](CHANGELOG.md).

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
