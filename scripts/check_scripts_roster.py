#!/usr/bin/env python3
"""Every file in this directory states its standing, and the statement is checked.

An unwired check reads as coverage, and an unlisted manual tool is hard to find.
This roster makes the roles explicit and verifies them against the files and
`brokkr.toml` commands.

Three legs, all accumulating:

  - every file here has a row in `README.md`;
  - every row names a file that is here;
  - every row's standing is one of the three the document defines, and every row
    declared `gate` is actually named by a `[[script_check]]` command in
    `brokkr.toml` - which is the leg that matters, because "declared wired" and
    "wired" drifting apart is the original defect wearing a table.

The reverse of that last leg holds too: a script `brokkr.toml` runs and the
table calls a tool is the same drift from the other side.
"""

from __future__ import annotations

import re
import sys
import tomllib

from _brokkr_config import ROOT

SCRIPTS = ROOT / "scripts"
ROSTER = SCRIPTS / "README.md"
STANDINGS = {"gate", "tool", "diagnostic"}
SHARED_GATE_FILES = {"_brokkr_config.py"}

ROW = re.compile(r"^\|\s*`([^`]+)`\s*\|\s*([a-z]+)\s*\|")


def declared() -> dict[str, str]:
    rows = {}
    for line in ROSTER.read_text().splitlines():
        match = ROW.match(line)
        if match:
            rows[match.group(1)] = match.group(2)
    return rows


def gate_commands() -> set[str]:
    """The script filenames `brokkr.toml` actually runs as script checks."""
    with (ROOT / "brokkr.toml").open("rb") as config:
        checks = tomllib.load(config).get("script_check", [])
    found = set()
    for entry in checks:
        for token in str(entry.get("command", "")).split():
            if token.startswith("scripts/"):
                found.add(token.removeprefix("scripts/"))
    return found


def main() -> int:
    problems: list[str] = []

    if not ROSTER.is_file():
        print("scripts/README.md is missing - it is the roster this check reads")
        return 1

    rows = declared()
    if not rows:
        print("scripts/README.md carries no roster rows - a pass over zero rows is not a pass")
        return 1

    # Every file, not every `.py`: a shell or other script dropped in here is as
    # much an unstated standing as a Python one. Directories (`__pycache__`) and
    # the roster itself are not scripts.
    present = {path.name for path in SCRIPTS.iterdir() if path.is_file() and path.name != "README.md"}
    wired = gate_commands()

    for name in sorted(present - set(rows)):
        problems.append(
            f"scripts/{name} has no row in scripts/README.md - state its standing "
            f"({', '.join(sorted(STANDINGS))})"
        )
    for name in sorted(set(rows) - present - {"README.md"}):
        problems.append(f"scripts/README.md has a row for {name}, which is not in scripts/")
    for name, standing in sorted(rows.items()):
        if standing not in STANDINGS:
            problems.append(
                f"scripts/README.md gives {name} the standing {standing!r}, which is "
                f"not one of {', '.join(sorted(STANDINGS))}"
            )
            continue
        if name == "README.md":
            continue
        # Shared support files are declared `gate` without appearing in any
        # command line, so they are exempt from this one leg and from nothing
        # else - a missing row or a bad standing still fires for them. Named
        # rather than matched by a leading underscore, so every exemption stays
        # a deliberate edit here.
        if standing == "gate" and name not in wired and name not in SHARED_GATE_FILES:
            problems.append(
                f"scripts/README.md calls {name} a gate, and no [[script_check]] in "
                "brokkr.toml runs it. Wire it or restate its standing."
            )
        if standing != "gate" and name in wired:
            problems.append(
                f"brokkr.toml runs {name} as a script check, and scripts/README.md "
                f"calls it a {standing}. Restate its standing as `gate`."
            )

    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} scripts roster violation(s)")
        return 1
    print(f"checked {len(present)} script(s) against the roster")
    print("scripts roster ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
