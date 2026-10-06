#!/usr/bin/env python3
"""Refuse stale textlint exclusions, including reviewed clippy escapes."""

from __future__ import annotations

import re
import sys
import tomllib

from _brokkr_config import ROOT, repository_sources
from check_skip_after_scopes import glob_matches, resolved


def main() -> int:
    with (ROOT / "brokkr.toml").open("rb") as stream:
        config = tomllib.load(stream)
    sources = [path.relative_to(ROOT).as_posix() for path in repository_sources(
        {".rs", ".toml", ".md", ".py", ".sh", ".js", ".ts", ".txt"}
    )]
    problems = []
    checked = 0
    for entry in config.get("textlint", []):
        rule = resolved(entry, config.get("textlint_preset", {}))
        for exclude in rule.get("exclude", []):
            checked += 1
            wildcard = any(char in exclude for char in "*?[")
            matches = [path for path in sources if glob_matches(path, exclude)] if wildcard else (
                [exclude] if (ROOT / exclude).exists() else []
            )
            # A deliberate exclusion of an existing ignored source tree
            # (for example research) is valid even though source discovery
            # correctly prunes it from every gate's input.
            if not matches and exclude.endswith("/**"):
                directory = exclude[:-3]
                if not any(char in directory for char in "*?[") and (ROOT / directory).is_dir():
                    matches = [directory]
            if not matches:
                problems.append(f"[{rule['name']}] exclusion {exclude!r} matches no existing path")
            elif rule["name"] == "disallowed-escapes-are-allowlisted":
                for relative in matches:
                    path = ROOT / relative
                    if not path.is_file() or not re.search(rule["pattern"], path.read_text()):
                        problems.append(f"[{rule['name']}] {relative} contains no reviewed escape")
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"{checked} textlint exclusions checked")
    print("textlint excludes ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
