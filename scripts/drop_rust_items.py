#!/usr/bin/env python3
"""Remove top-level Rust items (with their attributes and doc comments) from a file.

Usage: drop_rust_items.py FILE FN_NAME...

Each FN_NAME names a top-level `fn` (possibly `pub`, `pub(...)` or `async`).
The item runs from the attribute and doc-comment lines directly above its `fn`
line to the line that closes its body at column 0 (`}`), plus one following
blank line. An attribute spread over several lines is not recognised: only
lines starting with `#[` or `///` count, so check the result.
"""

import re
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(sys.argv[1])
    lines = path.read_text().splitlines(keepends=True)
    for name in sys.argv[2:]:
        pattern = re.compile(rf"^(pub(\([^)]*\))? )?(async )?fn {re.escape(name)}\b")
        start = next((i for i, line in enumerate(lines) if pattern.match(line)), None)
        if start is None:
            print(f"{name}: not found", file=sys.stderr)
            return 1
        while start > 0 and (lines[start - 1].startswith("#[") or lines[start - 1].startswith("///")):
            start -= 1
        end = next((i for i in range(start, len(lines)) if lines[i].rstrip("\n") == "}"), None)
        if end is None:
            print(f"{name}: no closing brace at column 0", file=sys.stderr)
            return 1
        end += 1
        if end < len(lines) and lines[end].strip() == "":
            end += 1
        del lines[start:end]
        print(f"{name}: removed")
    path.write_text("".join(lines).rstrip("\n") + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
