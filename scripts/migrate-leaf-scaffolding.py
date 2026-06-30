#!/usr/bin/env python3
"""Migrate `.setup.tstr` / `.cleanup.tstr` files out of leaf directories.

As of 0.6.0 setup/cleanup are scaffolding-only: they may live only in a *non-leaf*
directory (one that has subdirectories), where they establish or tear down scope
for the leaves below them. A setup/cleanup in a leaf is now a hard error.

This codemod fixes the common case automatically. For each leaf directory that
holds a `*.setup.tstr` or `*.cleanup.tstr` **and** one or more tests, it creates a
`cases/` subdirectory and moves the runnable test files down into it:

    # before — leaf holds setup + tests + cleanup
    tag-crud/
      00-create.setup.tstr
      01-replace.test.tstr
      99-cleanup.cleanup.tstr

    # after — setup/cleanup scaffold the new cases/ leaf
    tag-crud/
      00-create.setup.tstr
      99-cleanup.cleanup.tstr
      cases/
        01-replace.test.tstr

What moves down: `*.test.tstr`, `*.fetch.tstr`, and bare `*.tstr` (a file with no
middle extension is a test). What stays put (it's scope/scaffolding for the leaf
below): `*.const.tstr`, `*.setup.tstr`, `*.cleanup.tstr`, `*.lib.tstr`,
`*.exporter.tstr`.

A leaf that has setup/cleanup but **no** tests can't be migrated mechanically —
there's nothing for it to scaffold — so it's reported for you to handle by hand
(move it up to a real scaffolding parent, or delete it).

Re-running is safe: once a directory has the `cases/` child it's no longer a leaf,
so it's skipped.

Usage:
    python3 scripts/migrate-leaf-scaffolding.py path/to/suite [cases-subdir-name]
"""
import os
import sys
from pathlib import Path

# File roles that STAY in the (now non-leaf) parent — scope/scaffolding.
SCAFFOLD_EXTS = {"const", "setup", "cleanup", "lib", "exporter"}
# Roles that trigger the migration when found in a leaf.
TRIGGER_EXTS = {"setup", "cleanup"}


def middle_ext(filename):
    """The role tag of a .tstr file, mirroring the parser.

    "x.setup.tstr" -> "setup", "health.tstr" -> "" (bare = test).
    Returns None for anything that isn't a .tstr file.
    """
    if not filename.endswith(".tstr"):
        return None
    stem = filename[: -len(".tstr")]  # drop the ".tstr"
    if "." in stem:
        return stem.rsplit(".", 1)[1]  # the part after the last dot
    return ""  # no middle extension -> a bare test


def is_test_file(ext):
    """Does this role belong in a leaf (i.e. should be moved down)?"""
    return ext in ("test", "fetch", "")


def all_dirs(root):
    """Every directory under `root` (inclusive), skipping logs/ and hidden dirs."""
    found = []
    for dirpath, dirnames, _ in os.walk(root):
        # Don't descend into run-log output or hidden/VCS dirs.
        dirnames[:] = [d for d in dirnames if d != "logs" and not d.startswith(".")]
        found.append(Path(dirpath))
    return found


def is_leaf(d):
    """A leaf has no subdirectories (ignoring logs/ and hidden dirs)."""
    for child in d.iterdir():
        if child.is_dir() and child.name != "logs" and not child.name.startswith("."):
            return False
    return True


def main(args):
    root = Path(args[0]) if args else Path(".")
    subdir = args[1] if len(args) > 1 else "cases"

    if not root.is_dir():
        print(f"error: not a directory: {root}")
        return 1

    migrated = 0
    manual = []

    # Snapshot the leaf list up front. Processing a leaf creates a child dir,
    # but we already captured which dirs were leaves, so there's no surprise
    # re-traversal.
    leaves = [d for d in all_dirs(root) if is_leaf(d)]

    for d in leaves:
        # Leave lib subtrees alone — bare setup.tstr there is a lib-scope helper,
        # and a real *.setup.tstr in a lib dir is a separate problem.
        if "lib" in d.parts:
            continue

        files = [f for f in d.iterdir() if f.is_file()]
        roles = {f.name: middle_ext(f.name) for f in files}
        roles = {name: ext for name, ext in roles.items() if ext is not None}

        has_trigger = any(ext in TRIGGER_EXTS for ext in roles.values())
        if not has_trigger:
            continue

        tests = [name for name, ext in roles.items() if is_test_file(ext)]
        if not tests:
            manual.append(d)
            continue

        target = d / subdir
        target.mkdir(exist_ok=True)
        for name in sorted(tests):
            os.rename(d / name, target / name)
            print(f"  moved: {d / name} -> {target / name}")
        print(f"migrated leaf: {d}  ({len(tests)} test file(s) -> {subdir}/)")
        migrated += 1

    print(f"\n{migrated} leaf director(ies) migrated.")
    if manual:
        print(f"\n{len(manual)} need manual attention — setup/cleanup with no "
              f"tests to scaffold (move them up to a real parent, or delete):")
        for d in manual:
            print(f"  {d}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] in ("-h", "--help"):
        print(__doc__)
        sys.exit(0)
    sys.exit(main(sys.argv[1:]))
