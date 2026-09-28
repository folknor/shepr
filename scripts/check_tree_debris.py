#!/usr/bin/env python3
"""No test debris in the source tree.

`cargo test` runs each test binary with the crate directory as its working
directory, so a fixture that hands anything a relative path writes into the
source tree. Scratch belongs under the build tree
(`shepr_test_support::ScratchDir`), and root `clippy.toml` gives every child
process a stated working directory; this is the backstop for a leak by any
route those do not see.

Three legs, each where a crate-relative path can land:

  - a crate root holds only what a crate is made of - a small closed set, so an
    allowlist rather than a list of known leak names: whatever the next relative
    path is called, it is refused by name;
  - inside a crate's `src/`, `tests/`, `benches/` and `examples/`, a file git
    does not track (ignored or not) must carry an extension the tracked files
    there already use. A new `.rs` file still being written passes; a
    `tests/last-run.json` a fixture wrote does not. The extension set is read
    from the tracked tree, so a new fixture type is admitted by committing the
    first one;
  - at the workspace root, which a `../../` path from a test reaches, every
    entry is tracked or gitignored.
"""

from __future__ import annotations

import os
import subprocess
import sys

from _brokkr_config import ROOT

CRATE_ROOT_ALLOWED = {"Cargo.toml", "README.md", "build.rs", "src", "tests", "benches", "examples"}
SUBTREES = {"src", "tests", "benches", "examples"}

REMEDY = (
    "A test wrote through a path relative to its working directory: find the fixture "
    "that names it, give it a shepr_test_support::ScratchDir (a child process takes "
    "shepr_test_support::command_in_scratch), then delete this entry."
)


def git_paths(*args: str) -> list[str]:
    """NUL-separated paths from a `git ls-files` query, run against this checkout.

    The ambient `GIT_*` family is stripped, so a check run from a git hook or
    `rebase --exec` reads this repository and no other.
    """
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    run = subprocess.run(
        ["git", "ls-files", "-z", *args],
        cwd=ROOT,
        env=env,
        capture_output=True,
        check=False,
    )
    if run.returncode != 0:
        raise RuntimeError(f"git ls-files {' '.join(args)} failed: {run.stderr.decode(errors='replace').strip()}")
    return [path for path in run.stdout.decode().split("\0") if path]


def in_subtree(path: str) -> bool:
    parts = path.split("/")
    return len(parts) > 3 and parts[0] == "crates" and parts[2] in SUBTREES


def extension(path: str) -> str:
    return os.path.splitext(path)[1]


def crate_root_problems() -> tuple[list[str], int]:
    crates = sorted(path for path in (ROOT / "crates").iterdir() if path.is_dir())
    problems = [
        f"crates/{crate.name}/{entry.name} is not part of a crate. {REMEDY} If it is a new "
        "part of the crate layout, add it to CRATE_ROOT_ALLOWED in scripts/check_tree_debris.py."
        for crate in crates
        for entry in sorted(crate.iterdir())
        if entry.name not in CRATE_ROOT_ALLOWED
    ]
    return problems, len(crates)


def subtree_problems() -> tuple[list[str], set[str]]:
    admitted = {extension(path) for path in git_paths("crates") if in_subtree(path)}
    # `--others` without `--exclude-standard` lists ignored files too: a leak
    # that happens to match an ignore pattern is still a leak.
    problems = [
        f"{path} is untracked and its extension {extension(path) or '(none)'!r} is not one "
        f"the tracked crate sources use ({', '.join(sorted(admitted))}). {REMEDY} If it is "
        "a new fixture type, commit it."
        for path in sorted(git_paths("--others", "crates"))
        if in_subtree(path) and extension(path) not in admitted
    ]
    return problems, admitted


def workspace_root_problems() -> list[str]:
    # `--directory` reports an untracked directory once, as `name/`, rather than
    # every file beneath it.
    return [
        f"{entry.rstrip('/')} at the workspace root is neither tracked nor gitignored. {REMEDY} "
        "If it is meant to be there, commit it or add it to .gitignore."
        for entry in sorted(git_paths("--others", "--exclude-standard", "--directory"))
        if "/" not in entry.rstrip("/")
    ]


def main() -> int:
    root_problems, crate_count = crate_root_problems()
    if crate_count == 0:
        print("crates/ holds no crate directories - a pass over zero crates is not a pass")
        return 1
    inner_problems, admitted = subtree_problems()
    if not admitted:
        print("no tracked file under any crate's src/tests/benches/examples - a pass over nothing is not a pass")
        return 1
    problems = root_problems + inner_problems + workspace_root_problems()
    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} debris entr{'y' if len(problems) == 1 else 'ies'} in the source tree")
        return 1
    print(f"checked {crate_count} crate roots, their source subtrees and the workspace root")
    print("tree debris ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
