#!/usr/bin/env python3
"""Remove whole findings entries (## <ID> - ...) from a notes document.

Usage: notes_drop.py <file> <ID> [<ID> ...]
An entry runs from its heading to the next "## " or "# " heading.
If any ID is not found, nothing is written and the exit status is 1.
"""
import re
import sys


def main() -> int:
    path, ids = sys.argv[1], set(sys.argv[2:])
    lines = open(path, encoding="utf-8").read().split("\n")
    out, skipping, dropped = [], False, set()
    for line in lines:
        m = re.match(r"^## ([A-Z]+-\d{3}) ", line)
        if m:
            skipping = m.group(1) in ids
            if skipping:
                dropped.add(m.group(1))
        elif line.startswith("# "):
            skipping = False
        if not skipping:
            out.append(line)
    missing = ids - dropped
    if missing:
        print("not found, nothing written:", " ".join(sorted(missing)), file=sys.stderr)
        return 1
    open(path, "w", encoding="utf-8").write("\n".join(out).rstrip("\n") + "\n")
    print("dropped:", " ".join(sorted(dropped)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
