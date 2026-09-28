#!/usr/bin/env python3
"""One-off: drop every line whose stripped text is exactly LINE from the
`.rs` files under ROOT_DIR. Usage: test_isolation_drop_line.py ROOT_DIR LINE
"""

import pathlib
import sys


def main() -> int:
    root = pathlib.Path(sys.argv[1])
    target = sys.argv[2]
    for path in sorted(root.rglob("*.rs")):
        lines = path.read_text().split("\n")
        kept = [line for line in lines if line.strip() != target]
        if len(kept) != len(lines):
            path.write_text("\n".join(kept))
            print(f"dropped {len(lines) - len(kept)}: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
