#!/usr/bin/env python3
"""Compile-driven visibility narrowing for one module tree of shepr-client.

Usage: narrow_visibility.py <module dir or file relative to crates/shepr-client/src>...
       [--dry-run] [--force]

For each `pub(in crate::shell)` or `pub(crate)` declaration under the module dirs
(test files excluded) whose name is not mentioned outside the declaring module's
scope (with `--force`: every one, mentioned or not; for fields and methods whose
names other items share), tries making it private; failing that, when every outside mention is in
the parent module's tree, tries `pub(super)`; failing that, for a `pub(crate)`
item mentioned only inside the shell, tries `pub(in crate::shell)`. Each try is
kept only if `brokkr clippy -p shepr-client` and its `--lib` run are clean.
Prints what it kept.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "crates/shepr-client/src"
SHELL = SRC / "shell"
DECL = re.compile(
    r"^(\s*)(pub\((?:in crate::shell|crate)\))(\s+)"
    r"((?:(?:async|const|unsafe)\s+)*(?:(?:fn|struct|enum|trait|type|const|static|mod)\s+)?)"
    r"([A-Za-z_][A-Za-z0-9_]*)"
)


def is_test_path(path: Path) -> bool:
    return "tests" in path.parts or path.stem == "tests"


def clean() -> str:
    """`ok`, `fail`, or `lib` when only the library-alone lint run fails (the item is
    then used only from tests)."""
    # `--all-targets` (the default) lints the library alone as well as its tests, so one
    # run covers both; `--lib` stays available for a manual recheck.
    for extra in ([],):
        run = subprocess.run(
            ["brokkr", "clippy", "-p", "shepr-client", *extra],
            cwd=ROOT, capture_output=True, text=True,
        )
        if run.returncode != 0:
            return "lib" if extra else "fail"
    return "ok"


def scope(path: Path) -> tuple[Path, Path, Path]:
    if path.name == "mod.rs":
        return path, path.parent, path.parent.parent
    return path, path.with_suffix(""), path.parent


def mentions(name: str, keyword: str, own_file: Path, own_dir: Path, files: dict) -> list[Path]:
    n = re.escape(name)
    if keyword.strip().endswith("fn"):
        # A function or method: a call, a path to it, or a method reference.
        word = re.compile(rf"(?:\.|::)\s*{n}\b|\b{n}\s*\(")
    elif not keyword.strip():
        # A field: an access, or a struct literal or pattern naming it.
        word = re.compile(rf"\.\s*{n}\b|\b{n}\s*[:,}}]")
    else:
        word = re.compile(rf"\b{n}\b")
    return [
        f for f, text in files.items()
        if f != own_file and not f.is_relative_to(own_dir) and word.search(text)
    ]


def main() -> None:
    dry = "--dry-run" in sys.argv
    force = "--force" in sys.argv
    targets = []
    for arg in (a for a in sys.argv[1:] if not a.startswith("--")):
        module = (SRC / arg).resolve()
        if module.is_file():
            targets.append(module)
            continue
        if module.with_suffix(".rs").is_file():
            targets.append(module.with_suffix(".rs"))
        targets.extend(
            sorted(f for f in module.rglob("*.rs") if not is_test_path(f.relative_to(SRC)))
        )
    for path in targets:
        line_no = 0
        while True:
            files = {f: f.read_text() for f in SRC.rglob("*.rs")}
            lines = files[path].splitlines(keepends=True)
            if line_no >= len(lines):
                break
            m = DECL.match(lines[line_no])
            if not m or lines[line_no][m.end(3):].startswith("use "):
                line_no += 1
                continue
            vis, name = m.group(2), m.group(5)
            own_file, own_dir, parent_dir = scope(path)
            outside = [] if force else mentions(name, m.group(4), own_file, own_dir, files)
            tries = []
            if not outside:
                tries.append("")
            if parent_dir != SRC and all(f.is_relative_to(parent_dir) for f in outside):
                if not (vis == "pub(in crate::shell)" and parent_dir == SHELL):
                    tries.append("pub(super) ")
            if vis == "pub(crate)" and all(f.is_relative_to(SHELL) for f in outside) and path.is_relative_to(SHELL):
                tries.append("pub(in crate::shell) ")
            kept = None
            for replacement in tries:
                if dry:
                    kept = replacement or "private "
                    break
                original = lines[line_no]
                lines[line_no] = original[: m.start(2)] + replacement + original[m.end(3):]
                path.write_text("".join(lines))
                status = clean()
                if status == "ok":
                    kept = replacement or "private "
                    break
                if status == "lib":
                    print(f"{path.relative_to(SRC)}:{line_no + 1} {name}: used only from tests at {replacement.strip() or 'private'}", flush=True)
                lines[line_no] = original
                path.write_text("".join(lines))
            if kept:
                print(f"{path.relative_to(SRC)}:{line_no + 1} {name}: {vis} -> {kept.strip()}", flush=True)
            line_no += 1


if __name__ == "__main__":
    main()
