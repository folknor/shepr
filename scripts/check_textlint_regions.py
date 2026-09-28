#!/usr/bin/env python3
"""Witness that no `region = "code"` textlint pattern needs a string literal.

brokkr's `region = "code"` masks every string literal, its delimiting quotes
included: a `"127.0.0.1:8477"` literal and a `=> "x"` match arm both pass a
code-region rule untouched. A rule whose pattern contains a `"` therefore can
never match under that region, and passes silently over every file it scopes.

brokkr validates neither the pairing nor whether a rule matched anything, so
the pairing is refused here, at load: any textlint whose `region` is `code`
and whose `pattern` carries a double quote. The fix is to drop the region (the
rule then also sees comments, which is the cheaper failure) or to rewrite the
pattern over code tokens alone.
"""

from __future__ import annotations

import sys
import tomllib

from _brokkr_config import ROOT

# A Debug format spec (`{x:?}`) is spelled only inside a format string literal,
# so a code-region rule looking for one is inert with no quote in its pattern.
# This is the one such shape known; a pattern matching literal contents some
# other way still passes here unseen.
FORMAT_SPEC = r":\?"


def main() -> int:
    with (ROOT / "brokkr.toml").open("rb") as stream:
        config = tomllib.load(stream)
    rules = config.get("textlint", [])
    presets = config.get("textlint_preset", {})
    if not rules:
        print("brokkr.toml carries no textlint rules - a pass over zero rules is not a pass")
        return 1
    problems = []
    code_rules = 0
    for rule in rules:
        # A rule's own region wins; otherwise the first listed preset that sets one.
        region = rule.get("region")
        if region is None:
            names = rule.get("preset", [])
            for preset in [names] if isinstance(names, str) else names:
                region = presets.get(preset, {}).get("region")
                if region is not None:
                    break
        if region != "code":
            continue
        code_rules += 1
        pattern = rule.get("pattern", "")
        name = rule.get("name", "<unnamed>")
        if '"' in pattern:
            problems.append(
                f"[{name}] region = \"code\" masks string literals, "
                "quotes included, so this pattern containing a double quote can never match"
            )
        elif FORMAT_SPEC in pattern:
            problems.append(
                f"[{name}] region = \"code\" masks string literals, and a `:?` format "
                "spec exists only inside a format string, so this pattern can never match"
            )
    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} inert textlint region pairing(s)")
        return 1
    print(f"{code_rules} code-region textlint rules checked")
    print("textlint regions ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
