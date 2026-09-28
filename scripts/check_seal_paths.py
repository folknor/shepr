#!/usr/bin/env python3
"""The clippy seals in the root `clippy.toml` bind every crate.

A seal is a `disallowed-methods` or `disallowed-macros` entry in the root
`clippy.toml`, denied workspace-wide in the root `Cargo.toml`. Three ways it
can stop binding without any build failing:

  - a path stops resolving (renamed or moved upstream). Clippy refuses that
    itself on the pinned toolchain: it reports "`<path>` does not refer to a
    reachable function" (or macro) as an error-level diagnostic against
    `clippy.toml`, and the clippy phase fails on it. This script does not
    repeat that leg; it checks that every seal path is spelled as a path
    (`crate::...::item`), so an entry cannot be written in a form clippy
    skips;
  - a nested `clippy.toml` (or `.clippy.toml`) under a crate shadows the root
    one for every crate below it, silently dropping every seal there. Only the
    root file may exist;
  - the lints stop being denied: clippy's `disallowed_*` lints are warn by
    default, so the root `[workspace.lints.clippy]` must deny both.
"""

from __future__ import annotations

import re
import sys
import tomllib

from _brokkr_config import ROOT, repository_sources

SEAL_KEYS = ("disallowed-methods", "disallowed-macros")
DENIED_LINTS = ("disallowed_methods", "disallowed_macros")
PATH_SHAPE = re.compile(r"^[a-z_][a-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+$")
CLIPPY_CONFIG_NAMES = {"clippy.toml", ".clippy.toml"}


def main() -> int:
    problems: list[str] = []

    root_config = ROOT / "clippy.toml"
    with root_config.open("rb") as stream:
        clippy = tomllib.load(stream)
    seals = 0
    for key in SEAL_KEYS:
        for entry in clippy.get(key, []):
            path = entry.get("path") if isinstance(entry, dict) else entry
            reason = entry.get("reason") if isinstance(entry, dict) else None
            if not isinstance(path, str) or not PATH_SHAPE.match(path):
                problems.append(f"clippy.toml {key} entry {entry!r} does not name a path")
                continue
            if not reason:
                problems.append(f"clippy.toml {key} {path} carries no reason naming the sanctioned alternative")
            seals += 1
    if seals == 0:
        print("clippy.toml carries no seals - a pass over nothing is not a pass")
        return 1

    extra_configs = sorted(
        path.relative_to(ROOT)
        for path in repository_sources({".toml"})
        if path.name in CLIPPY_CONFIG_NAMES and path != root_config
    )
    for path in extra_configs:
        problems.append(f"{path} shadows the root clippy.toml for every crate below it; fold it into the root file")

    with (ROOT / "Cargo.toml").open("rb") as stream:
        lints = tomllib.load(stream).get("workspace", {}).get("lints", {}).get("clippy", {})
    for lint in DENIED_LINTS:
        level = lints.get(lint)
        level = level.get("level") if isinstance(level, dict) else level
        if level not in {"deny", "forbid"}:
            problems.append(f"Cargo.toml [workspace.lints.clippy] {lint} is {level!r}, so the seals only warn; deny it")

    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} seal problem(s)")
        return 1
    print(f"checked {seals} seal path(s), the lint levels and that clippy.toml is the only one")
    print("seal paths ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
