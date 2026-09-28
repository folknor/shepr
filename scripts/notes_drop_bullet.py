#!/usr/bin/env python3
"""Remove single top-level bullets from a notes document.

Usage: notes_drop_bullet.py <file> <prefix> [<prefix> ...]
A bullet is dropped when its first line starts with "- " followed by one of the
given prefixes. It runs until the next top-level bullet, blank line or heading.
Each prefix must match exactly one bullet, or the file is left untouched.
"""
import sys


def main() -> int:
    path, prefixes = sys.argv[1], sys.argv[2:]
    lines = open(path, encoding="utf-8").read().split("\n")
    hits = {p: 0 for p in prefixes}
    out, skipping = [], False
    for line in lines:
        if line.startswith("- "):
            matched = [p for p in prefixes if line[2:].startswith(p)]
            for p in matched:
                hits[p] += 1
            skipping = bool(matched)
        elif line == "" or line.startswith("#"):
            skipping = False
        if not skipping:
            out.append(line)
    bad = {p: n for p, n in hits.items() if n != 1}
    if bad:
        for p, n in bad.items():
            print(f"prefix matched {n} bullets, expected 1: {p!r}")
        return 1
    open(path, "w", encoding="utf-8").write("\n".join(out))
    print(f"dropped {len(prefixes)} bullet(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
