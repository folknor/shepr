#!/usr/bin/env python3
"""Every backticked repository path in a comment or a durable document exists.

A citation is claimed only when it is anchored in one of this repository's own
top-level directories, and for `crates/` only when the second component is a
real crate, so a dependency's own `crates/...` spelling and a sibling-relative
`pane/foo.rs` are left alone. Suffix matching cannot tell a deleted file from a
renamed one, so unanchored citations are not claimed at all.

Documents are the root markdown files plus everything under `docs/` and
`reference/`; their relative markdown links are checked too. Sources are
checked on comment lines only.
"""

from __future__ import annotations

import re
import sys

from _brokkr_config import ROOT, repository_sources, skipped_dir

EXEMPT_MARKER = "doc-conventions-ok"
EXEMPT_LOOKBACK = 2
CODE_SPAN_PATH = re.compile(
    r"`([A-Za-z0-9_.][A-Za-z0-9_./-]*/[A-Za-z0-9_.-]+"
    r"\.(?:rs|md|toml|py|sh|ts|js|json|jsonl|yaml))`"
)
MARKDOWN_LINK = re.compile(r"\[[^\]]*\]\(([^)#]+)\)")
UNCHECKED_PREFIXES = ("research/", "target/", "http")
COMMENT_LINE = re.compile(r"^\s*(?:///|//!|//|#|\*)")
SOURCE_SUFFIXES = {".rs", ".toml", ".py", ".sh", ".ts", ".js", ".yaml"}


def roots() -> set[str]:
    return {path.name for path in ROOT.iterdir() if path.is_dir() and not skipped_dir(path.name)}


def claimed(target: str, anchors: set[str]) -> bool:
    parts = target.split("/")
    if parts[0] not in anchors:
        return False
    return parts[0] != "crates" or (len(parts) > 1 and (ROOT / "crates" / parts[1]).is_dir())


def exempted(lines: list[str], index: int) -> bool:
    return any(EXEMPT_MARKER in line for line in lines[max(0, index - EXEMPT_LOOKBACK) : index + 1])


def main() -> int:
    anchors = roots()
    documents = sorted(path for path in ROOT.iterdir() if path.is_file() and path.suffix == ".md")
    documents += [
        path for path in repository_sources({".md"}) if path.parts[len(ROOT.parts)] in {"docs", "reference"}
    ]
    sources = repository_sources(SOURCE_SUFFIXES)
    if not sources:
        print("no repository sources found - a pass over nothing is not a pass")
        return 1
    violations = []
    for path in documents + sources:
        relative = path.relative_to(ROOT)
        comments_only = path.suffix != ".md"
        lines = path.read_text(errors="replace").splitlines()
        for number, line in enumerate(lines, start=1):
            if comments_only and not COMMENT_LINE.match(line):
                continue
            if exempted(lines, number - 1):
                continue
            for target in CODE_SPAN_PATH.findall(line):
                if target.startswith(UNCHECKED_PREFIXES) or not claimed(target, anchors):
                    continue
                if not (ROOT / target).is_file():
                    violations.append(f"{relative}:{number}: cites {target}, which is not in the tree")
            if not comments_only:
                for target in MARKDOWN_LINK.findall(line):
                    target = target.strip()
                    if target and ":" not in target and not (path.parent / target).exists():
                        violations.append(f"{relative}:{number}: link to {target} does not exist")
    for violation in violations:
        print(violation)
    if violations:
        print(f"{len(violations)} dangling citation(s)")
        return 1
    print(f"checked {len(documents)} document(s) and {len(sources)} source file(s)")
    print("cited paths ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
