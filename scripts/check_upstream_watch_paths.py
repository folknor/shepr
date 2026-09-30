#!/usr/bin/env python3
"""AGENTS.md lists exactly the shepr paths `scripts/upstream_watch.py` maps to.

The watcher's tables say which shepr files follow upstream herdr; AGENTS.md
repeats their shepr side under "Upstream tracking" so someone changing those
files sees it. Two lists drift, so this check compares them, and refuses a
listed path that no longer exists (the watcher would otherwise keep pointing
porting work at a file that was renamed or deleted).
"""

from __future__ import annotations

import re
import sys

from _brokkr_config import ROOT
from upstream_watch import shepr_paths

AGENTS = ROOT / "AGENTS.md"
HEADING = "## Upstream tracking"
BULLET = re.compile(r"^- `([^`]+)`\s*$")


def listed() -> list[str] | None:
    """The backticked bullet paths of the AGENTS.md section, or None when the
    section is missing."""
    lines = AGENTS.read_text().splitlines()
    try:
        start = lines.index(HEADING) + 1
    except ValueError:
        return None
    paths = []
    for line in lines[start:]:
        if line.startswith("## "):
            break
        match = BULLET.match(line)
        if match:
            paths.append(match.group(1))
    return paths


def main() -> int:
    documented = listed()
    if documented is None:
        print(f'AGENTS.md has no "{HEADING}" section - it is the list this check reads')
        return 1
    watched = shepr_paths()
    if not watched:
        print("upstream_watch.py maps to no shepr path - a pass over nothing is not a pass")
        return 1

    problems = []
    for path in sorted(set(watched) - set(documented)):
        problems.append(f"upstream_watch.py maps to {path}, which AGENTS.md does not list")
    for path in sorted(set(documented) - set(watched)):
        problems.append(f"AGENTS.md lists {path}, which upstream_watch.py does not map to")
    for path in watched:
        if not (ROOT / path).exists():
            problems.append(f"upstream_watch.py maps to {path}, which does not exist")

    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} upstream watch path problem(s)")
        return 1
    print(f"checked {len(watched)} upstream-tracked path(s)")
    print("upstream watch paths ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
