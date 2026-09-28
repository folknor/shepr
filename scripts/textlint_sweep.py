#!/usr/bin/env python3
"""Diagnostic: run brokkr.toml's textlint rules without building anything.

An approximation of brokkr's textlint phase for sweeping a new or widened rule
before it is wired in: presets are merged the way brokkr merges them, files
come from `git ls-files --cached --others --exclude-standard`, globs follow
globset's defaults (`*` crosses `/`), and `region` is emulated with a small
Rust lexer. Not a gate: `brokkr check --textlint NAME` is the authority.

Usage: python3 scripts/textlint_sweep.py [RULE_NAME ...]
"""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib

from _brokkr_config import ROOT

LIST_KEYS = ("paths", "exclude", "except")
BOM = chr(0xFEFF)


def resolved(rule: dict, presets: dict) -> dict:
    names = rule.get("preset", [])
    names = [names] if isinstance(names, str) else list(names)
    merged: dict = {}
    for key in LIST_KEYS:
        merged[key] = [item for name in names for item in presets.get(name, {}).get(key, [])]
        merged[key] += rule.get(key, [])
    for source in [rule] + [presets.get(name, {}) for name in names]:
        for key, value in source.items():
            if key not in LIST_KEYS and key != "preset" and key not in merged:
                merged[key] = value
    return merged


def glob_regex(pattern: str) -> re.Pattern:
    out = ""
    index = 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            out += "(?:.*/)?"
            index += 3
        elif pattern.startswith("/**", index) and index + 3 == len(pattern):
            out += "/.*"
            index += 3
        elif pattern.startswith("**", index):
            out += ".*"
            index += 2
        elif pattern[index] == "*":
            out += ".*"
            index += 1
        elif pattern[index] == "?":
            out += "."
            index += 1
        else:
            out += re.escape(pattern[index])
            index += 1
    return re.compile(out + r"\Z")


def classify(text: str) -> list[str]:
    """Per character: 'c' code, 's' string or char literal, 'm' comment."""
    regions = ["c"] * len(text)
    index = 0
    length = len(text)
    while index < length:
        char = text[index]
        if text.startswith("//", index):
            end = text.find("\n", index)
            end = length if end < 0 else end
            regions[index:end] = ["m"] * (end - index)
            index = end
            continue
        if text.startswith("/*", index):
            depth = 0
            start = index
            while index < length:
                if text.startswith("/*", index):
                    depth += 1
                    index += 2
                elif text.startswith("*/", index):
                    depth -= 1
                    index += 2
                    if depth == 0:
                        break
                else:
                    index += 1
            regions[start:index] = ["m"] * (index - start)
            continue
        raw = re.match(r'(?:b|c)?r(#*)"', text[index : index + 260])
        if raw and (index == 0 or not (text[index - 1].isalnum() or text[index - 1] == "_")):
            closer = '"' + raw.group(1)
            end = text.find(closer, index + raw.end())
            end = length if end < 0 else end + len(closer)
            regions[index:end] = ["s"] * (end - index)
            index = end
            continue
        prefixed = re.match(r'(?:b|c)?"', text[index : index + 2])
        if prefixed and (index == 0 or not (text[index - 1].isalnum() or text[index - 1] == "_")):
            start = index
            index += prefixed.end()
            while index < length:
                if text[index] == "\\":
                    index += 2
                    continue
                if text[index] == '"':
                    index += 1
                    break
                index += 1
            regions[start:index] = ["s"] * (index - start)
            continue
        if char == "'":
            literal = re.match(r"'(?:\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|\\.|[^'\\\n])'", text[index : index + 12])
            if literal:
                regions[index : index + literal.end()] = ["s"] * literal.end()
                index += literal.end()
                continue
        index += 1
    return regions


def main() -> int:
    wanted = set(sys.argv[1:])
    with (ROOT / "brokkr.toml").open("rb") as stream:
        config = tomllib.load(stream)
    presets = config.get("textlint_preset", {})
    rules = []
    for raw_rule in config.get("textlint", []):
        if wanted and raw_rule["name"] not in wanted:
            continue
        rule = resolved(raw_rule, presets)
        rule["_paths"] = [glob_regex(glob) for glob in rule["paths"]]
        rule["_exclude"] = [glob_regex(glob) for glob in rule["exclude"]]
        rule["_except"] = [re.compile(item) for item in rule["except"]]
        rule["_pattern"] = re.compile(rule["pattern"])
        rule["_skip"] = re.compile(rule["skip_after"]) if rule.get("skip_after") else None
        rules.append(rule)
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    ).stdout.split(b"\0")
    files = sorted(item.decode() for item in listed if item)
    count = 0
    for relative in files:
        applicable = [
            rule
            for rule in rules
            if any(glob.match(relative) for glob in rule["_paths"])
            and not any(glob.match(relative) for glob in rule["_exclude"])
        ]
        if not applicable:
            continue
        path = ROOT / relative
        if not path.is_file():
            continue
        data = path.read_bytes()
        if b"\0" in data[:8000]:
            continue
        text = data.decode("utf-8", errors="replace").removeprefix(BOM)
        regions = classify(text) if any(rule.get("region") for rule in applicable) else None
        lines = text.split("\n")
        offsets = []
        position = 0
        for line in lines:
            offsets.append(position)
            position += len(line) + 1
        for rule in applicable:
            target = {"code": "c", "string": "s", "comment": "m"}.get(rule.get("region", ""))
            marker = rule.get("allow_marker")
            above = int(rule.get("allow_marker_above", 0))
            for number, line in enumerate(lines):
                line = line.removesuffix("\r")
                if number > 0 and rule["_skip"] is not None and rule["_skip"].search(lines[number - 1]):
                    break
                hay = line
                if target is not None:
                    base = offsets[number]
                    hay = "".join(
                        char if regions[base + column] == target else " " for column, char in enumerate(line)
                    )
                if not rule["_pattern"].search(hay):
                    continue
                if marker and any(marker in lines[row] for row in range(max(0, number - above), number + 1)):
                    continue
                if any(item.search(line) for item in rule["_except"]):
                    continue
                count += 1
                print(f"{relative}:{number + 1}: [{rule['name']}] {line.strip()[:160]}")
    print(f"{count} violation(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
