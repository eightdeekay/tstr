#!/usr/bin/env python3
"""Migrate the body-statement `disabled "reason";` marker to metadata `disabled:`.

As of 0.5.0 the file-off marker lives in the header-region metadata block, not
the body. This rewrites:

    a, b --> {
      x = 1;
      disabled "I-123: fix pending";
    }

into:

    disabled: I-123: fix pending
    a, b --> {
      x = 1;
    }

The body `disabled "..."` statement is removed and its reason is hoisted to a
`disabled:` line at the very top of the file (the metadata value is the rest of
the line, unquoted). Files that already have a `disabled:` metadata line, or no
body marker at all, are left untouched — so re-runs are safe.

Usage:
    python3 scripts/migrate-disabled.py path/to/suite/**/*.tstr
    find suite -name '*.tstr' -exec python3 scripts/migrate-disabled.py {} +
"""
import re
import sys

# A body-statement marker: `disabled "reason";` (reason may contain escaped
# quotes). Allowed to be indented inside the `{ }` block.
BODY_MARKER_RE = re.compile(r'^\s*disabled\s+"((?:[^"\\]|\\.)*)"\s*;\s*$')
# An existing metadata marker: `disabled:` at the start of a line.
META_MARKER_RE = re.compile(r'^\s*disabled\s*:')


def already_migrated(lines):
    """True if any line is already a `disabled:` metadata directive."""
    return any(META_MARKER_RE.match(ln) for ln in lines)


def unescape(reason):
    """Turn the quoted-string escapes (\\" and \\\\) back into literals."""
    return reason.replace('\\"', '"').replace('\\\\', '\\')


def migrate(text):
    """Return (new_text, changed). changed is False when nothing applied."""
    lines = text.rstrip('\n').split('\n')
    if already_migrated(lines):
        return text, False

    reason = None
    kept = []
    for ln in lines:
        m = BODY_MARKER_RE.match(ln)
        if m and reason is None:
            reason = unescape(m.group(1))  # hoist the first marker's reason
        elif m:
            continue  # drop any stray extra markers (a file is off once)
        else:
            kept.append(ln)

    if reason is None:
        return text, False  # no body marker — leave the file alone

    return f"disabled: {reason}\n" + '\n'.join(kept) + '\n', True


def main(paths):
    changed = 0
    for path in paths:
        with open(path) as f:
            text = f.read()
        new_text, did = migrate(text)
        if not did:
            print(f"skip (no body marker / already migrated): {path}")
            continue
        with open(path, 'w') as f:
            f.write(new_text)
        print(f"migrated: {path}")
        changed += 1
    print(f"\n{changed} file(s) migrated.")


if __name__ == '__main__':
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    main(sys.argv[1:])
