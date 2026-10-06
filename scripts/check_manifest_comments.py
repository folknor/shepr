#!/usr/bin/env python3
"""Check manifest text outside TOML strings, which contain agent screen glyphs.

brokkr only offers directory exclusions. Keep its manifest exclusion, and
restore checking of comments and keys here. Quoted screen examples in
comments retain their glyphs too. The refused codepoints mirror
brokkr's built-in gremlins plus the project's horizontal ellipsis spelling.
"""

from __future__ import annotations

import re
import sys
import tomllib

from _brokkr_config import ROOT

REFUSED = set(
    "\u0003\u000b\u200b\u200c\u200d\u2060\ufeff\u00a0\u202f\u00ad"
    "\u2028\u2029\u200e\u200f\u202a\u202b\u202c\u202d\u202e"
    "\u2066\u2067\u2068\u2069\u2013\u2014\u2018\u2019\u201a\u201b"
    "\u201c\u201d\u201e\u201f\ufffc\u2026"
)


def outside_strings(text: str):
    """Yield (offset, character) outside TOML basic and literal strings."""
    cursor = 0
    quote = None
    while cursor < len(text):
        if quote is not None:
            if quote.startswith('"') and text[cursor] == "\\":
                cursor += 2
            elif text.startswith(quote, cursor):
                cursor += len(quote)
                quote = None
            else:
                cursor += 1
        elif text[cursor] == "#":
            end = text.find("\n", cursor)
            end = len(text) if end < 0 else end
            comment = text[cursor:end]
            # Only explicitly quoted screen examples are exempt, not prose.
            comment = re.sub(r'`[^`]*`|"(?:\\.|[^"\\])*"',
                             lambda match: " " * len(match.group()), comment)
            yield from enumerate(comment, start=cursor)
            cursor = end
        elif text[cursor] in "\"'":
            char = text[cursor]
            quote = char * (3 if text.startswith(char * 3, cursor) else 1)
            cursor += len(quote)
        else:
            yield cursor, text[cursor]
            cursor += 1


def refused(char: str) -> bool:
    code = ord(char)
    return char in REFUSED or (
        0x2600 <= code <= 0x27BF or code in {0x2B1B, 0x2B1C, 0x2B50, 0x2B55}
        or 0xFE00 <= code <= 0xFE0F or 0x1F000 <= code <= 0x1FAFF
    )


def main() -> int:
    paths = sorted((ROOT / "crates/shepr-detect/src/manifests").glob("*.toml"))
    if not paths:
        print("no detection manifests found")
        return 1
    problems = []
    for path in paths:
        text = path.read_text()
        tomllib.loads(text)  # Invalid syntax must not hide text from the lexer.
        for offset, char in outside_strings(text):
            if refused(char):
                line = text.count("\n", 0, offset) + 1
                problems.append(f"{path.relative_to(ROOT)}:{line}: U+{ord(char):04X} outside a TOML string")
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"{len(paths)} manifest comments checked")
    print("manifest comments ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
