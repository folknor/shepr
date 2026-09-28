#!/usr/bin/env python3
"""Witness that no `[[textlint]]` `skip_after` releases production code.

brokkr's `skip_after` is line-based: every line after the first match of its
pattern (here always a file's first `#[cfg(test)]`) is exempt from the rule.
That is the right scope only when the test item runs to the end of the file.
A file with an early `#[cfg(test)]` item - a gated helper, an out-of-line
`mod tests;` declared above the production items, a test-only `use` or `impl` -
exempts every production line below it, and a violation written there passes
the rule silently.

brokkr offers no item-scoped exemption, so the assertion lives here. For every
textlint carrying `skip_after`, and every file in its scope, this walks the
top-level items that follow the first `skip_after` line and re-applies the
rule's own `pattern` (minus its `except` patterns and `allow_marker`) to every
item that is not itself gated on a test cfg. A hit is a rule violation the
textlint could not see, reported under the rule's name.

The brokkr man page and top-level help describe textlint but expose no
item-level matcher entry point, so this narrow witness keeps its own glob and
lexical-scope handling.

The walk is a small lexer rather than a parser: it tracks brace depth outside
comments, strings, raw strings and char literals, which is enough to find where
each top-level item ends. An item counts as test-gated only when a cfg
attribute directly above it cannot be satisfied with `test = false`; for
example, `cfg(all(test, feature = "x"))` is test-only, while
`cfg(any(test, feature = "x"))` also ships when that feature is enabled.
Unparseable cfg syntax is treated as production, so it cannot release code from
the witness. Matches inside comments are
ignored, the same as the rules' `region = "code"`; for the rules that set no
region this is slightly narrower than brokkr, which only errs toward silence
on a comment that restates a refused spelling.
"""

from __future__ import annotations

import fnmatch
import re
import sys
import tomllib

from _brokkr_config import ROOT, repository_sources

CFG_TOKEN = re.compile(
    r'\s*(?:(?P<ident>[A-Za-z_]\w*)|(?P<string>"(?:\\.|[^"\\])*?")|(?P<punct>[(),=]))'
)


def cfg_tokens(source: str) -> list[str] | None:
    """Tokenize cfg meta syntax; unknown syntax fails closed as production."""
    tokens: list[str] = []
    cursor = 0
    while cursor < len(source):
        if not source[cursor:].strip():
            break
        match = CFG_TOKEN.match(source, cursor)
        if match is None:
            return None
        tokens.append(match.group("ident") or match.group("string") or match.group("punct"))
        cursor = match.end()
    return tokens


def parse_cfg(tokens: list[str]) -> tuple | None:
    """Parse cfg all, any, not, flags and key/value predicates."""
    cursor = 0

    def expression() -> tuple | None:
        nonlocal cursor
        if cursor >= len(tokens):
            return None
        name = tokens[cursor]
        cursor += 1
        if cursor < len(tokens) and tokens[cursor] == "(":
            cursor += 1
            arguments = []
            while cursor < len(tokens) and tokens[cursor] != ")":
                argument = expression()
                if argument is None:
                    return None
                arguments.append(argument)
                if cursor < len(tokens) and tokens[cursor] == ",":
                    cursor += 1
                    continue
                if cursor >= len(tokens) or tokens[cursor] != ")":
                    return None
                break
            if cursor >= len(tokens) or tokens[cursor] != ")":
                return None
            cursor += 1
            if name in {"all", "any"}:
                return (name, tuple(arguments))
            if name == "not" and len(arguments) == 1:
                return ("not", arguments[0])
            return ("opaque", name, tuple(arguments))
        if cursor < len(tokens) and tokens[cursor] == "=":
            cursor += 1
            if cursor >= len(tokens):
                return None
            value = tokens[cursor]
            cursor += 1
            return ("atom", name, value)
        return ("test",) if name == "test" else ("atom", name)

    parsed = expression()
    return parsed if cursor == len(tokens) else None


def cfg_may_be_true_without_test(expression: tuple) -> bool:
    """Whether a cfg expression could match a non-test build.

    Independent unknown cfg predicates are allowed to take either value. That
    approximation can keep an actually test-only item in the scan, but never
    hides an item that can ship.
    """
    kind = expression[0]
    if kind == "test":
        return False
    if kind in {"atom", "opaque"}:
        return True
    if kind == "not":
        return cfg_may_be_false_without_test(expression[1])
    children = expression[1]
    if kind == "all":
        return all(cfg_may_be_true_without_test(child) for child in children)
    if kind == "any":
        return any(cfg_may_be_true_without_test(child) for child in children)
    return True


def cfg_may_be_false_without_test(expression: tuple) -> bool:
    """Whether a cfg expression could fail in a non-test build."""
    kind = expression[0]
    if kind == "test":
        return True
    if kind in {"atom", "opaque"}:
        return True
    if kind == "not":
        return cfg_may_be_true_without_test(expression[1])
    children = expression[1]
    if kind == "all":
        return any(cfg_may_be_false_without_test(child) for child in children)
    if kind == "any":
        return all(cfg_may_be_false_without_test(child) for child in children)
    return True


def test_only_cfg_attribute(lines: list[str]) -> bool:
    """Prove that one attached cfg attribute excludes every non-test build."""
    uncommented = " ".join(code_mask("\n".join(lines)))
    match = re.fullmatch(r"\s*#\s*\[\s*cfg\s*\((.*)\)\s*\]\s*", uncommented)
    if match is None:
        return False
    tokens = cfg_tokens(match.group(1))
    parsed = parse_cfg(tokens) if tokens is not None else None
    return parsed is not None and not cfg_may_be_true_without_test(parsed)


def glob_matches(path: str, pattern: str) -> bool:
    """brokkr-style glob, globset's defaults: `**/` spans zero or more
    directories, and `*` and `?` cross `/` too (brokkr builds its matchers
    without `literal_separator`)."""
    regex = ""
    index = 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            regex += "(?:.*/)?"
            index += 3
        elif pattern.startswith("**", index):
            regex += ".*"
            index += 2
        elif pattern[index] == "*":
            regex += ".*"
            index += 1
        elif pattern[index] == "?":
            regex += "."
            index += 1
        else:
            regex += re.escape(pattern[index])
            index += 1
    return re.fullmatch(regex, path) is not None


def code_mask(text: str) -> list[str]:
    """Per line, the text with comments blanked and string/char contents kept.

    Also returns nothing about depth; `item_spans` does its own lexing. Kept
    separate so the pattern sees string literals (some rules refuse a literal)
    but never a comment.
    """
    out = []
    in_block = 0
    for line in text.splitlines():
        result = []
        index = 0
        in_string = False
        while index < len(line):
            char = line[index]
            if in_block:
                if line.startswith("*/", index):
                    in_block -= 1
                    index += 2
                    continue
                if line.startswith("/*", index):
                    in_block += 1
                    index += 2
                    continue
                result.append(" ")
                index += 1
                continue
            if in_string:
                result.append(char)
                if char == "\\":
                    if index + 1 < len(line):
                        result.append(line[index + 1])
                    index += 2
                    continue
                if char == '"':
                    in_string = False
                index += 1
                continue
            if line.startswith("//", index):
                break
            if line.startswith("/*", index):
                in_block += 1
                index += 2
                continue
            if char == '"':
                in_string = True
            result.append(char)
            index += 1
        out.append("".join(result))
    return out


CONTAINER = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:impl|mod|trait)\b")


def ungated_lines(lines: list[str], start: int, stop: int) -> list[int]:
    """Lines in `[start, stop)` belonging to no test-gated item.

    Recurses into ungated `impl`, `mod` and `trait` blocks, because a
    `#[cfg(test)]` helper method inside a production `impl` is a test item too.
    """
    out: list[int] = []
    for begin, end, gated in item_spans(lines, start, stop):
        if gated:
            continue
        head = next(
            (number for number in range(begin, end + 1) if not lines[number].strip().startswith("#")),
            begin,
        )
        opener = next((number for number in range(head, end + 1) if "{" in lines[number]), None)
        if CONTAINER.match(lines[head].strip()) and opener is not None and opener < end:
            out.extend(range(begin, opener + 1))
            out.extend(ungated_lines(lines, opener + 1, end))
            out.append(end)
        else:
            out.extend(range(begin, end + 1))
    return out


def item_spans(lines: list[str], start_line: int, stop_line: int) -> list[tuple[int, int, bool]]:
    """Items at the depth of `start_line` (0-based) up to `stop_line`: (first, last, test-gated)."""
    spans: list[tuple[int, int, bool]] = []
    depth = 0
    in_block = 0
    in_string = False
    raw_hashes: int | None = None
    item_start: int | None = None
    gated = False
    attribute_depth = 0
    attribute_lines: list[str] = []
    for number in range(start_line, stop_line):
        line = lines[number]
        stripped = line.strip()
        if attribute_depth:
            # A multi-line attribute (`#[expect(\n reason = ".."\n)]`): string
            # contents are stripped before counting brackets.
            attribute_lines.append(stripped)
            bare = re.sub(r'"(?:\\.|[^"\\])*"', '""', stripped)
            attribute_depth += bare.count("[") - bare.count("]")
            if attribute_depth == 0:
                gated = gated or test_only_cfg_attribute(attribute_lines)
                attribute_lines = []
            continue
        if depth == 0 and item_start is None and not in_block and not in_string and raw_hashes is None:
            if not stripped or stripped.startswith("//"):
                continue
            if stripped.startswith("#[") or stripped.startswith("#!["):
                attribute_lines = [stripped]
                bare = re.sub(r'"(?:\\.|[^"\\])*"', '""', stripped)
                attribute_depth = max(0, bare.count("[") - bare.count("]"))
                if attribute_depth == 0:
                    gated = gated or test_only_cfg_attribute(attribute_lines)
                    attribute_lines = []
                continue
            item_start = number
        index = 0
        while index < len(line):
            char = line[index]
            if in_block:
                if line.startswith("*/", index):
                    in_block -= 1
                    index += 2
                elif line.startswith("/*", index):
                    in_block += 1
                    index += 2
                else:
                    index += 1
                continue
            if raw_hashes is not None:
                closer = '"' + "#" * raw_hashes
                if line.startswith(closer, index):
                    raw_hashes = None
                    index += len(closer)
                else:
                    index += 1
                continue
            if in_string:
                if char == "\\":
                    index += 2
                    continue
                if char == '"':
                    in_string = False
                index += 1
                continue
            if line.startswith("//", index):
                break
            if line.startswith("/*", index):
                in_block += 1
                index += 2
                continue
            raw = re.match(r'b?r(#*)"', line[index:])
            if raw and (index == 0 or not (line[index - 1].isalnum() or line[index - 1] == "_")):
                raw_hashes = len(raw.group(1))
                index += raw.end()
                continue
            if char == '"':
                in_string = True
                index += 1
                continue
            if char == "'":
                literal = re.match(r"'(?:\\.[^']*|[^'\\])'", line[index:])
                if literal:
                    index += literal.end()
                    continue
                index += 1
                continue
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0 and item_start is not None:
                    rest = line[index + 1 :].strip()
                    if not rest.startswith(";") and not rest.startswith(","):
                        spans.append((item_start, number, gated))
                        item_start, gated = None, False
            elif char == ";" and depth == 0 and item_start is not None:
                spans.append((item_start, number, gated))
                item_start, gated = None, False
            index += 1
        if depth == 0 and item_start is not None and stripped.endswith("};"):
            spans.append((item_start, number, gated))
            item_start, gated = None, False
    if item_start is not None:
        spans.append((item_start, stop_line - 1, gated))
    return spans


def resolved(rule: dict, presets: dict) -> dict:
    """The rule with its presets applied the way brokkr applies them.

    A scalar the rule sets wins, then the first listed preset's; `paths`,
    `exclude` and `except` concatenate, presets first in declaration order.
    """
    names = rule.get("preset", [])
    names = [names] if isinstance(names, str) else list(names)
    merged: dict = {}
    for key in ("paths", "exclude", "except"):
        values = [item for name in names for item in presets.get(name, {}).get(key, [])]
        values += rule.get(key, [])
        if values:
            merged[key] = values
    for source in [rule] + [presets.get(name, {}) for name in names]:
        for key, value in source.items():
            if key not in ("paths", "exclude", "except", "preset") and key not in merged:
                merged[key] = value
    return merged


def main() -> int:
    with (ROOT / "brokkr.toml").open("rb") as stream:
        config = tomllib.load(stream)
    presets = config.get("textlint_preset", {})
    rules = [resolved(rule, presets) for rule in config.get("textlint", [])]
    scoped = [rule for rule in rules if rule.get("skip_after")]
    if not scoped:
        print("no textlint carries skip_after; nothing to witness")
        print("skip_after scopes ok")
        return 0
    sources = [path.relative_to(ROOT).as_posix() for path in repository_sources({".rs"})]
    failures: list[str] = []
    files_walked = 0
    for rule in scoped:
        name = rule.get("name", "<unnamed>")
        pattern = re.compile(rule["pattern"])
        skip_after = re.compile(rule["skip_after"])
        excepts = [re.compile(item) for item in rule.get("except", [])]
        marker = rule.get("allow_marker")
        marker_above = int(rule.get("allow_marker_above", 0))
        paths = rule.get("paths", ["**/*.rs"])
        excludes = rule.get("exclude", [])
        for relative in sources:
            if not any(glob_matches(relative, glob) for glob in paths):
                continue
            if any(glob_matches(relative, glob) for glob in excludes):
                continue
            text = (ROOT / relative).read_text()
            lines = text.splitlines()
            first = next((number for number, line in enumerate(lines) if skip_after.search(line)), None)
            if first is None:
                continue
            files_walked += 1
            masked = code_mask(text)
            for number in ungated_lines(lines, first, len(lines)):
                if not pattern.search(masked[number]):
                    continue
                if any(item.search(lines[number]) for item in excepts):
                    continue
                if marker and any(
                    marker in lines[above] for above in range(max(0, number - marker_above), number + 1)
                ):
                    continue
                failures.append(
                    f"{relative}:{number + 1}: [{name}] production code below the file's first test cfg "
                    f"(line {first + 1}) escapes skip_after: {lines[number].strip()}"
                )
    if failures:
        print("\n".join(failures))
        print(
            f"{len(failures)} rule violation(s) hidden by skip_after; move the test item to the end of the "
            "file or fix the line"
        )
        return 1
    print(f"{len(scoped)} skip_after rules, {files_walked} files with a test cfg walked")
    print("skip_after scopes ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
