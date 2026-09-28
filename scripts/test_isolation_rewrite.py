#!/usr/bin/env python3
"""One-off rewrite of test call sites when test-only helpers left the
production crates' test features for dev-only fixture crates.

Each rule is a literal replacement applied to every `.rs` file under the
listed roots. Prints the files it changed and how many replacements each got.
"""

import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# (roots relative to the repository, old text, new text)
RULES = [
    (["crates/shepr-server/src"], "shepr_api::error::test_success(", "crate::test_support::test_success("),
    (["crates/shepr-server/src"], "shepr_api::error::test_error(", "crate::test_support::test_error("),
    (["crates/shepr-server/src"], "shepr_api::error::test_json(", "crate::test_support::test_json("),
    (["crates/shepr-server/src"], "shepr_agent::agent::resume::test_codex_plan(", "crate::test_support::test_codex_plan("),
    (["crates/shepr-server/src"], "shepr_mux::persist::capture_history(", "crate::test_support::capture_history("),
    (
        ["crates/shepr-server/src", "crates/shepr-client/src", "crates/shepr-mux/src"],
        "shepr_protocol::codec::to_vec(",
        "shepr_test_fixtures::encode_to_vec(",
    ),
    (
        ["crates/shepr-server/src", "crates/shepr-client/src", "crates/shepr-mux/src", "crates/shepr-remote/src", "crates/shepr-api/src", "src"],
        "AppPaths::default()",
        "AppPaths::test_default()",
    ),
    (
        ["crates/shepr-server/src", "crates/shepr-client/src", "crates/shepr-mux/src", "crates/shepr-remote/src", "crates/shepr-api/src", "src"],
        "AppPaths::test_with_context(",
        "AppPaths::rooted_at(",
    ),
    (
        ["crates/shepr-server/src", "crates/shepr-client/src", "crates/shepr-mux/src", "src"],
        "shepr_termio::input::raw_input::parse_raw_input_bytes_sync(",
        "shepr_test_fixtures::parse_raw_input_bytes_sync(",
    ),
    (
        ["crates/shepr-client/src"],
        "shepr_termio::input::mouse::parse_report(",
        "shepr_test_fixtures::parse_sgr_mouse_report(",
    ),
]


def main() -> int:
    changed: dict[pathlib.Path, int] = {}
    for roots, old, new in RULES:
        for root in roots:
            for path in sorted((ROOT / root).rglob("*.rs")):
                text = path.read_text()
                count = text.count(old)
                if count:
                    path.write_text(text.replace(old, new))
                    changed[path] = changed.get(path, 0) + count
    for path, count in sorted(changed.items()):
        print(f"{path.relative_to(ROOT)}: {count}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
