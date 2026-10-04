#!/usr/bin/env python3
"""Remove findings entries or single bullets from a notes document.

Usage: notes_drop.py <file> <ID> [<ID> ...]
       notes_drop.py <file> --bullet <prefix> [<prefix> ...]

By ID: an entry runs from its "## <ID> - ..." heading to the next "## " or
"# " heading. Every ID must be found.

With --bullet: a top-level bullet is dropped when its first line starts with
"- " followed by one of the prefixes. It runs until the next top-level bullet,
blank line or heading. Each prefix must match exactly one bullet.

On any miss nothing is written and the exit status is 1.
"""
import re
import sys


def drop_entries(lines: list[str], ids: list[str]) -> tuple[list[str], list[str], str]:
    wanted, dropped = set(ids), set()
    out, skipping = [], False
    for line in lines:
        m = re.match(r"^## ([A-Z]+-\d{3}) ", line)
        if m:
            skipping = m.group(1) in wanted
            if skipping:
                dropped.add(m.group(1))
        elif line.startswith("# ") or line.startswith("## "):
            skipping = False
        if not skipping:
            out.append(line)
    errors = [f"not found: {i}" for i in sorted(wanted - dropped)]
    return out, errors, " ".join(sorted(dropped))


def drop_bullets(lines: list[str], prefixes: list[str]) -> tuple[list[str], list[str], str]:
    # A repeated prefix is one prefix, not a second match of the same bullet.
    hits = dict.fromkeys(prefixes, 0)
    out, skipping, dropped = [], False, 0
    for line in lines:
        if line.startswith("- "):
            matched = [p for p in hits if line[2:].startswith(p)]
            for p in matched:
                hits[p] += 1
            skipping = bool(matched)
            dropped += skipping
        elif line == "" or line.startswith("#"):
            skipping = False
        if not skipping:
            out.append(line)
    errors = [
        f"prefix matched {n} bullets, expected 1: {p!r}" for p, n in hits.items() if n != 1
    ]
    return out, errors, f"{dropped} bullet(s)"


def main() -> int:
    args = sys.argv[1:]
    if len(args) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    path, rest = args[0], args[1:]
    lines = open(path, encoding="utf-8").read().split("\n")
    if rest[0] == "--bullet":
        if len(rest) < 2:
            print(__doc__, file=sys.stderr)
            return 2
        out, errors, what = drop_bullets(lines, rest[1:])
    else:
        out, errors, what = drop_entries(lines, rest)
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        print("nothing written", file=sys.stderr)
        return 1
    open(path, "w", encoding="utf-8").write("\n".join(out).rstrip("\n") + "\n")
    print("dropped:", what)
    return 0


if __name__ == "__main__":
    sys.exit(main())
