#!/usr/bin/env python3
"""Compare test functions between HEAD and the working tree for a set of paths.

Usage: compare_tests.py <path>...

Lists tests that vanished or appeared (by name) and, for tests present on both
sides, those whose count of assert-like macros or `.expect(`/`panic!` changed.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEST_ATTR = re.compile(r"#\[(?:tokio::)?test(?:\([^)]*\))?\]")
FN = re.compile(r"\bfn\s+([A-Za-z0-9_]+)")
CHECK = re.compile(r"\b(assert(?:_eq|_ne)?!|debug_assert!|panic!|\.expect\(|matches!)")


def head_files(paths):
    out = subprocess.run(
        ["git", "-C", str(ROOT), "ls-tree", "-r", "--name-only", "HEAD", "--", *paths],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    return {
        f: subprocess.run(
            ["git", "-C", str(ROOT), "show", f"HEAD:{f}"],
            capture_output=True, text=True, check=True,
        ).stdout
        for f in out if f.endswith(".rs")
    }


def tree_files(paths):
    files = {}
    for p in paths:
        base = ROOT / p
        candidates = [base] if base.is_file() else base.rglob("*.rs")
        for f in candidates:
            files[str(f.relative_to(ROOT))] = f.read_text()
    return files


def body_end(text, start):
    depth = 0
    i = text.index("{", start)
    while i < len(text):
        c = text[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return len(text)


def tests(files):
    found = {}
    for name, text in files.items():
        for m in TEST_ATTR.finditer(text):
            f = FN.search(text, m.end())
            if not f:
                continue
            end = body_end(text, f.end())
            body = text[f.end():end]
            key = f.group(1)
            entry = (name, len(CHECK.findall(body)), "".join(body.split()))
            if key in found:
                key = f"{key}@{name}"
            found[key] = entry
    return found


def main():
    paths = sys.argv[1:]
    before = tests(head_files(paths))
    after = tests(tree_files(paths))
    gone = sorted(set(before) - set(after))
    new = sorted(set(after) - set(before))
    print(f"HEAD {len(before)} tests, tree {len(after)} tests")
    print("\nGone:")
    for k in gone:
        print(f"  {k} ({before[k][0]}, checks {before[k][1]})")
    print("\nNew:")
    for k in new:
        print(f"  {k} ({after[k][0]}, checks {after[k][1]})")
    print("\nChecks changed:")
    for k in sorted(set(before) & set(after)):
        if before[k][1] != after[k][1]:
            print(f"  {k}: {before[k][1]} -> {after[k][1]} ({before[k][0]} -> {after[k][0]})")
    print("\nBodies changed (whitespace-insensitive):")
    for k in sorted(set(before) & set(after)):
        if before[k][2] != after[k][2]:
            print(f"  {k} ({before[k][0]} -> {after[k][0]})")


if __name__ == "__main__":
    main()
