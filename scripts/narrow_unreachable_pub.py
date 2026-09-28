#!/usr/bin/env python3
"""One-off: narrow every `unreachable_pub` item a clippy run reported to `pub(crate)`.

Usage: narrow_unreachable_pub.py CLIPPY_OUTPUT

Reads brokkr's one-line diagnostics (`error[unreachable_pub] <file>:<line>:<col>
...`), where the position is the item's `pub` token, and rewrites that token in
place. A position whose text is not a bare `pub` followed by whitespace is
reported and left alone, so a stale report cannot corrupt a file.
"""

from __future__ import annotations

import os
import re
import sys
from collections import defaultdict

from _brokkr_config import ROOT

DIAGNOSTIC = re.compile(r"error\[unreachable_pub\] (\S+):(\d+):(\d+) ")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    sites: dict[str, set[tuple[int, int]]] = defaultdict(set)
    with open(sys.argv[1], encoding="utf-8") as stream:
        for line in stream:
            match = DIAGNOSTIC.search(line)
            if match:
                path = os.path.normpath(match.group(1))
                sites[path].add((int(match.group(2)), int(match.group(3))))
    changed = 0
    skipped = 0
    for path, positions in sorted(sites.items()):
        file = ROOT / path
        lines = file.read_text(encoding="utf-8").split("\n")
        # Right to left within a line, so an earlier column stays valid.
        for number, column in sorted(positions, key=lambda site: (site[0], -site[1])):
            text = lines[number - 1]
            index = column - 1
            if not re.match(r"pub\s", text[index:]):
                print(f"{path}:{number}:{column}: not a bare pub: {text.strip()}")
                skipped += 1
                continue
            lines[number - 1] = text[:index] + "pub(crate)" + text[index + 3 :]
            changed += 1
        file.write_text("\n".join(lines), encoding="utf-8")
    print(f"narrowed {changed} item(s), skipped {skipped}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
