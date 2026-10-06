#!/usr/bin/env python3
"""The bun tests of the agent integration plugins pass.

Pi, Oh My Pi, opencode and Kilo load their integration as a JavaScript or
TypeScript module inside the agent's own process, so the state machines those
plugins carry (blocked counting, settle handling, session-report ordering,
delivery retry, opencode's session hydration) can only run under a JavaScript
runtime. Their tests are bun tests beside the assets; bun is a development
dependency, and this check is how the gate runs them.

Each test file runs in its own bun process, because bun's module mocks and the
environment a file sets last for the whole process. The verdict is bun's own
summary, not its exit status alone: every run must exit cleanly, report at least
one passing test and no failing one, and have run exactly its one file, so a run
that found nothing to test, or silently skipped a file, is not a pass.
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys

from _brokkr_config import ROOT

ASSETS = ROOT / "crates" / "shepr-integration" / "src" / "assets"

PASS = re.compile(r"^\s*(\d+) pass\s*$", re.MULTILINE)
FAIL = re.compile(r"^\s*(\d+) fail\s*$", re.MULTILINE)
FILES = re.compile(r"^Ran \d+ tests? across (\d+) files?\.", re.MULTILINE)


def main() -> int:
    bun = shutil.which("bun")
    if bun is None:
        print("bun is not installed; it is a development dependency of shepr (https://bun.sh)")
        return 1
    test_files = sorted(ASSETS.rglob("*.test.ts"))
    if not test_files:
        print(f"no *.test.ts under {ASSETS.relative_to(ROOT)} - a pass over zero files is not a pass")
        return 1

    problems = []
    outputs = []
    total_passed = 0
    for test_file in test_files:
        # A leading `./` makes bun take the argument as this file's path rather
        # than as a name filter that could match other test files.
        run = subprocess.run(
            [bun, "test", f"./{test_file.relative_to(ROOT)}"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        # Bun module mocks are process-wide. Run each file in its own process
        # so fake node:net modules and environment cannot affect other files.
        output = run.stdout + run.stderr
        outputs.append(output)
        passed = PASS.search(output)
        failed = FAIL.search(output)
        files = FILES.search(output)
        name = str(test_file.relative_to(ROOT))
        if run.returncode != 0:
            problems.append(f"{name}: bun test exited with status {run.returncode}")
        if passed is None or int(passed.group(1)) == 0:
            problems.append(f"{name}: bun reported no passing test")
        else:
            total_passed += int(passed.group(1))
        if failed is None:
            problems.append(f"{name}: bun printed no failure count")
        elif int(failed.group(1)) != 0:
            problems.append(f"{name}: bun reported {failed.group(1)} failing test(s)")
        if files is None:
            problems.append(f"{name}: bun printed no file count")
        elif int(files.group(1)) != 1:
            problems.append(f"{name}: bun ran {files.group(1)} test files, expected one")

    if problems:
        for output in outputs:
            print(output, end="")
        for problem in problems:
            print(problem)
        return 1
    print(f"ran {total_passed} bun test(s) across {len(test_files)} files")
    print("agent asset tests ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
