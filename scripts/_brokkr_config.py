#!/usr/bin/env python3
"""The facts every script check in this directory reads the same way.

`repository_sources()` is the one definition of "a file this repository
authored". A per-script copy drifts: one prunes `research/` but walks a nested
`target/`, another reports a gitignored `.brokkr/` scratch file as a source, and
a check hanging off the wrong definition gives a wrong answer rather than a
style difference.

The prune set is applied at every level:

  - `research/` holds read-only upstream copies. Not ours to govern or lint.
  - `target/` is build output, at any depth.
  - `node_modules/` is vendored third-party JavaScript.
  - tool-state dot-directories are skipped by name.

Discovery rather than enumeration: an enumerated directory list under-scans
silently, because a directory nobody added to it is not an error, just a
smaller file count.
"""

from __future__ import annotations

import os
import pathlib
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

PRUNED_DIRS = {
    "research",
    "target",
    "node_modules",
    "__pycache__",
    ".git",
    ".brokkr",
    ".codex",
    ".agents",
    ".claude",
    ".plans",
    ".local",
    ".venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
}


def skipped_dir(name: str) -> bool:
    """Whether a directory of this name is outside what this repository authored."""
    return name in PRUNED_DIRS


def workspace_member_manifests() -> list[pathlib.Path]:
    """Manifest paths the root workspace names, root package included.

    The root `Cargo.toml` is itself a package (the `shepr` binary), and the
    workspace names its members with a glob (`crates/*`), so both are read here
    rather than refused.
    """
    with (ROOT / "Cargo.toml").open("rb") as stream:
        root = tomllib.load(stream)
    paths = [ROOT / "Cargo.toml"] if "package" in root else []
    for member in root["workspace"]["members"]:
        if any(char in member for char in "*?["):
            matches = sorted(path for path in ROOT.glob(member) if path.is_dir())
        else:
            matches = [ROOT / member]
        if not matches:
            raise RuntimeError(f"workspace member {member!r} matches nothing")
        for directory in matches:
            path = directory / "Cargo.toml"
            if not path.is_file():
                raise RuntimeError(f"workspace member manifest is missing: {path}")
            paths.append(path)
    return paths


def skipped(relative: pathlib.Path) -> bool:
    """Whether a repository-relative path sits under any pruned directory."""
    return any(skipped_dir(part) for part in relative.parts[:-1])


def repository_sources(suffixes: set[str] | frozenset[str]) -> list[pathlib.Path]:
    """Every first-party file under the repository carrying one of these suffixes.

    Absolute paths, sorted. Suffixes are spelled with the dot (`{".rs"}`).
    """
    found: list[pathlib.Path] = []
    for directory, subdirectories, names in os.walk(ROOT):
        # Prune in place: `target/` alone is large enough that walking it and
        # then discarding the results costs more than the rest of the sweep.
        subdirectories[:] = [name for name in subdirectories if not skipped_dir(name)]
        for name in names:
            path = pathlib.Path(directory) / name
            if path.suffix in suffixes:
                found.append(path)
    return sorted(found)
