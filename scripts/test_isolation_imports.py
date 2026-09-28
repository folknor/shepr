#!/usr/bin/env python3
"""One-off: bring the fixture traits into scope in the test modules that call
fixture methods, after those methods moved from inherent test-feature items
onto extension traits in dev-only fixture code.

For every `.rs` file under a crate root whose test code names a fixture
method, insert the crate's fixture glob import into its test module: after
the `use super::*;` of its `mod tests` block, or, for a file that is itself a
test module, after its own top-level `use super::*;` (or at its top).
Idempotent: a file that already has the import is left alone.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

TRIGGERS = re.compile(
    r"ValidatedConfig::test_(?:default|from_config)|AppPaths::test_(?:default|at)\b"
    r"|Workspace::test_new|\.test_split\(|\.test_add_tab\(|test_adversarial_identity_state"
    r"|assert_invariants_for_test|clear_tabs_for_test|PaneRuntime::test_with_"
    r"|\.test_process_pty_bytes\(|\.test_contend_during_dirty_collection\("
    r"|\.current_size\(\)|\.recent_unwrapped_text\(|\.set_detected_state\("
    r"|\.set_hook_authority\(|GitStatusRefreshDemand::ALL|\.events_after\("
    r"|\.public_tab_number_for_pane\(|\.resolved_identity_cwd\(\)|\.close_pane\("
    r"|runtimes\.drain\(\)|registry\.drain\(\)"
)

CRATES = {
    "crates/shepr-server/src": "use crate::test_support::*;",
    "crates/shepr-client/src": "use shepr_test_fixtures::*;",
    "crates/shepr-mux/src": "use shepr_test_fixtures::*;",
    "src": "use shepr_test_fixtures::*;",
}

SKIP = {
    "crates/shepr-server/src/test_support.rs",
}


def is_test_file(path: pathlib.Path) -> bool:
    name = path.name
    return name == "tests.rs" or name.endswith("_tests.rs") or "tests" in path.parts[-3:-1]


def insert(text: str, import_line: str, test_file: bool) -> str | None:
    lines = text.split("\n")
    if test_file:
        for index, line in enumerate(lines):
            if line == "use super::*;":
                lines.insert(index + 1, import_line)
                return "\n".join(lines)
        lines.insert(0, import_line)
        return "\n".join(lines)
    for index, line in enumerate(lines):
        if re.match(r"^(\s*)mod tests \{$", line):
            indent = re.match(r"^(\s*)", line).group(1) + "    "
            for inner in range(index + 1, min(index + 12, len(lines))):
                if lines[inner].strip() == "use super::*;":
                    lines.insert(inner + 1, indent + import_line)
                    return "\n".join(lines)
            lines.insert(index + 1, indent + import_line)
            return "\n".join(lines)
    return None


def main() -> int:
    for root, import_line in CRATES.items():
        for path in sorted((ROOT / root).rglob("*.rs")):
            relative = str(path.relative_to(ROOT))
            if relative in SKIP:
                continue
            text = path.read_text()
            if import_line in text or not TRIGGERS.search(text):
                continue
            updated = insert(text, import_line, is_test_file(path))
            if updated is None:
                print(f"no test module found: {relative}")
                continue
            path.write_text(updated)
            print(f"imported: {relative}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
