#!/usr/bin/env python3
"""Replace every occurrence of a literal string in the given files.

Usage: replace_literal.py OLD NEW FILE...

Prints each file it changed with the number of replacements.
"""

import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    old, new, *files = sys.argv[1:]
    for name in files:
        path = Path(name)
        text = path.read_text()
        count = text.count(old)
        if count:
            path.write_text(text.replace(old, new))
            print(f"{name}: {count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
