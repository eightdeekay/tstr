#!/usr/bin/env python3
"""Migrate pre-0.4.0 .tstr files to the function form.

Old form:
    [a, b -->]
    ...body...
    [<-- x, y]

New form (0.4.0):
    [a, b] --> {
      ...body...
      [export x, y;]
    }

Wraps the body in `--> { }`, adds a bare `-->` header when one is missing, and
converts a file-level `<-- x, y` output line into `export x, y;`. Files already
in function form (body starts with `--> {`) are left untouched, so re-runs are
safe.

Usage:
    python3 scripts/migrate-syntax.py path/to/suite/**/*.tstr
    find suite -name '*.tstr' -exec python3 scripts/migrate-syntax.py {} +
"""
import re
import sys

# A file-level input header: optional `a, b` then `-->` alone on the line.
HEADER_RE = re.compile(r'^\s*([A-Za-z_][\w,\s]*\s+)?-->\s*$')
# A file-level output line: `<-- x, y` alone on the line (NOT an inline lambda
# `<--`, which never starts a line).
OUTPUT_RE = re.compile(r'^\s*<--\s*(.+?)\s*$')


def already_migrated(text):
    """True if the first non-blank, non-comment line opens a function body."""
    for line in text.splitlines():
        s = line.strip()
        if not s or s.startswith('//'):
            continue
        return '-->' in s and s.rstrip().endswith('{')
    return False


def migrate(text):
    lines = text.rstrip('\n').split('\n')

    # Locate the header among leading blank/comment lines.
    i = 0
    while i < len(lines) and (lines[i].strip() == '' or lines[i].strip().startswith('//')):
        i += 1
    if i < len(lines) and HEADER_RE.match(lines[i]):
        header = lines[i].strip()
        body_lines = lines[:i] + lines[i + 1:]
    else:
        header = '-->'
        body_lines = lines[:]

    # Pull out the `<--` output line, if any.
    outputs = []
    kept = []
    for ln in body_lines:
        m = OUTPUT_RE.match(ln)
        if m:
            outputs = [v.strip() for v in m.group(1).split(',') if v.strip()]
        else:
            kept.append(ln)

    while kept and kept[0].strip() == '':
        kept.pop(0)
    while kept and kept[-1].strip() == '':
        kept.pop()

    body = '\n'.join(('  ' + l) if l.strip() else l for l in kept)
    export = ('\n  export ' + ', '.join(outputs) + ';') if outputs else ''
    inner = (body + export).strip('\n')
    return f"{header} {{\n{inner}\n}}\n"


def main(paths):
    changed = 0
    for path in paths:
        with open(path) as f:
            text = f.read()
        if already_migrated(text):
            print(f"skip (already migrated): {path}")
            continue
        with open(path, 'w') as f:
            f.write(migrate(text))
        print(f"migrated: {path}")
        changed += 1
    print(f"\n{changed} file(s) migrated.")


if __name__ == '__main__':
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    main(sys.argv[1:])
