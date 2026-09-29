#!/usr/bin/env python3
"""Mechanical fixes for the compile errors left by typing the API id fields.

One-off migration aid. Reads a `brokkr check` log and rewrites, at each
reported position:

- E0277 "`X: From<&str>` is not satisfied" (the column names the `into`
  token): `EXPR.into()` becomes `shepr_test_fixtures::id(EXPR)`, where EXPR
  is a string literal, a plain identifier or a `&format!(..)` call;
- E0308 "expected `String`, found `<id type>`" (the column names the start of
  the expression): the single-line expression gains `.to_string()`, and for
  "expected `Option<String>`" it gains `.map(|id| id.to_string())`.

Only files under `tests/` directories or named `*tests.rs`/`tests.rs`, or
lines inside a `#[cfg(test)]` region, are touched. Every position it cannot
rewrite is printed for a hand fix.
"""

import pathlib
import re
import sys

HELPER = "shepr_test_fixtures::id"
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
ID_TYPES = r"(?:WorkspaceId|PublicChildId<'[tp]'>|TerminalId)"
FROM_STR = re.compile(r"error\[E0277\] (\S+):(\d+):(\d+) the trait bound `\S+: std::convert::From<&str>`")
TO_STRING = re.compile(
    r"error\[E0308\] (\S+):(\d+):(\d+) mismatched types - expected `(String|Option<String>)`, found `(?:Option<)?"
    + ID_TYPES
)


def into_start(line: str, end: int) -> int | None:
    """Start of the expression that ends just before index `end`."""
    last = end - 1
    if last < 0:
        return None
    if line[last] == '"':
        index = last - 1
        while index >= 0:
            if line[index] == '"' and (index == 0 or line[index - 1] != "\\"):
                return index
            index -= 1
        return None
    if line[last] == ")":
        depth = 0
        for index in range(last, -1, -1):
            if line[index] == ")":
                depth += 1
            elif line[index] == "(":
                depth -= 1
                if depth == 0:
                    prefix = "&format!"
                    if line[:index].endswith(prefix):
                        return index - len(prefix)
                    return None
        return None
    index = end
    while index > 0 and (line[index - 1].isalnum() or line[index - 1] == "_"):
        index -= 1
    return index if index < end and IDENT.fullmatch(line[index:end]) else None


def expression_end(line: str, start: int) -> int | None:
    """End of the expression starting at `start`, if it closes on this line."""
    depth = 0
    in_string = False
    index = start
    while index < len(line):
        ch = line[index]
        if in_string:
            if ch == "\\":
                index += 2
                continue
            if ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                return index
            depth -= 1
        elif ch in ",;" and depth == 0:
            return index
        index += 1
    return None if depth or in_string else len(line.rstrip())


def test_code(path: pathlib.Path, lines: list[str], line_number: int) -> bool:
    if "tests" in path.parts or path.name == "tests.rs" or path.name.endswith("_tests.rs"):
        return True
    return any(line.strip().startswith("#[cfg(test)]") for line in lines[:line_number])


def main() -> int:
    log = pathlib.Path(sys.argv[1]).read_text()
    edits: dict[pathlib.Path, list[tuple[int, int, str]]] = {}
    for match in FROM_STR.finditer(log):
        edits.setdefault(pathlib.Path(match[1]), []).append((int(match[2]), int(match[3]), "id"))
    for match in TO_STRING.finditer(log):
        kind = "option" if match[4].startswith("Option") else "string"
        edits.setdefault(pathlib.Path(match[1]), []).append((int(match[2]), int(match[3]), kind))
    rewritten = 0
    skipped = 0
    for path, positions in edits.items():
        if not path.is_file():
            continue
        lines = path.read_text().split("\n")
        # Right to left, so earlier columns on one line stay valid.
        for line_number, column, kind in sorted(set(positions), reverse=True):
            line = lines[line_number - 1]
            if not test_code(path, lines, line_number):
                print(f"{path}:{line_number}:{column}: production code, fix by hand")
                skipped += 1
                continue
            if kind == "id":
                end = column - 2
                start = into_start(line, end) if line.startswith(".into()", end) else None
                if start is None:
                    print(f"{path}:{line_number}:{column}: no EXPR.into() here: {line.strip()}")
                    skipped += 1
                    continue
                lines[line_number - 1] = (
                    line[:start] + f"{HELPER}({line[start:end]})" + line[end + len(".into()") :]
                )
            else:
                start = column - 1
                end = expression_end(line, start)
                if end is None or end == start:
                    print(f"{path}:{line_number}:{column}: expression not on one line: {line.strip()}")
                    skipped += 1
                    continue
                suffix = ".to_string()" if kind == "string" else ".map(|id| id.to_string())"
                lines[line_number - 1] = line[:end] + suffix + line[end:]
            rewritten += 1
        path.write_text("\n".join(lines))
    print(f"rewrote {rewritten} site(s), {skipped} skipped")
    return 1 if skipped else 0


if __name__ == "__main__":
    sys.exit(main())
