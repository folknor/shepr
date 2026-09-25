#!/usr/bin/env python3
"""Replace `.unwrap()` / `.unwrap_err()` in test code for clippy's `unwrap_used`.

Test code is: whole files named tests.rs, *_tests.rs or test_support.rs, files
under a `tests/` directory, and everything from the first `#[cfg(test)]` that
introduces a `mod` to the end of the file (the tree keeps test modules last).

Test-code calls become `.expect("test precondition")` / `.expect_err(...)`.
Production-code calls are only listed, for hand-written error handling.

Dry run by default; pass --apply to write. Paths under --skip are ignored.
"""

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MESSAGE = "test precondition"
CALL = re.compile(r"\.unwrap(_err)?\(\)")
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
MOD_LINE = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?mod\s+\w+")
ATTR_LINE = re.compile(r"^\s*#\[")


def whole_file_is_test(rel: Path) -> bool:
    name = rel.name
    return (
        name == "tests.rs"
        or name.endswith("_tests.rs")
        or name == "test_support.rs"
        or "tests" in rel.parts[:-1]
    )


def test_module_start(lines: list[str]) -> int | None:
    """Index of the first `#[cfg(test)]` that is followed by a `mod` item."""
    for i, line in enumerate(lines):
        if not CFG_TEST.match(line):
            continue
        j = i + 1
        while j < len(lines) and (ATTR_LINE.match(lines[j]) or not lines[j].strip()):
            j += 1
        if j < len(lines) and MOD_LINE.match(lines[j]):
            return i
    return None


def replace(match: re.Match) -> str:
    return f'.expect_err("{MESSAGE}")' if match.group(1) else f'.expect("{MESSAGE}")'


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--skip", action="append", default=[], help="path prefix to ignore")
    args = parser.parse_args()
    skips = [Path(s) for s in args.skip]

    fixed = 0
    production = []
    for path in sorted((ROOT / "src").rglob("*.rs")):
        rel = path.relative_to(ROOT)
        if any(rel.is_relative_to(s) for s in skips):
            continue
        text = path.read_text(encoding="utf-8")
        if not CALL.search(text):
            continue
        lines = text.splitlines(keepends=True)
        start = 0 if whole_file_is_test(rel) else test_module_start(lines)
        for n, line in enumerate(lines):
            if start is not None and n >= start:
                count = len(CALL.findall(line))
                if count:
                    fixed += count
                    lines[n] = CALL.sub(replace, line)
            elif CALL.search(line):
                production.append(f"{rel}:{n + 1}: {line.strip()}")
        if args.apply:
            path.write_text("".join(lines), encoding="utf-8")

    print(f"test-code calls {'replaced' if args.apply else 'to replace'}: {fixed}")
    print(f"production-code calls left for hand edits: {len(production)}")
    for entry in production:
        print(f"  {entry}")
    if not args.apply:
        print("\ndry run; pass --apply to write")
    return 0


if __name__ == "__main__":
    sys.exit(main())
