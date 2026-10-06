#!/usr/bin/env python3
"""Explicitly regenerate the integration assets; test filters never write them."""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys

from _brokkr_config import ROOT

PREFIX = "SHEPR_BUNDLED_ASSETS="


def main() -> int:
    result = subprocess.run(
        ["brokkr", "test", "-p", "shepr-integration", "emit_bundled_assets"],
        cwd=ROOT, capture_output=True, text=True, check=False,
    )
    if result.returncode:
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        return result.returncode
    # With one test thread under --nocapture, libtest prints `test <name> ... `
    # without a newline before the test's own output, so the marker need not
    # start its line.
    emitted = [json.loads(line[line.index(PREFIX) + len(PREFIX):])
               for line in result.stdout.splitlines() if PREFIX in line]
    if not emitted or any(assets != emitted[0] for assets in emitted):
        print("generator emitted no assets or disagreeing sweeps", file=sys.stderr)
        return 1
    assets = emitted[0]
    if not isinstance(assets, dict) or not assets:
        print("generator emitted no asset map", file=sys.stderr)
        return 1
    directory = ROOT / "crates/shepr-integration/src/assets"
    # Validate the whole map before writing any committed file.
    for name, text in assets.items():
        relative = pathlib.PurePosixPath(name)
        if (relative.is_absolute() or ".." in relative.parts or not isinstance(text, str)
                or not (directory / relative).is_file()):
            print(f"generator emitted an invalid asset: {name!r}", file=sys.stderr)
            return 1
    for name, text in assets.items():
        (directory / name).write_text(text)
    print(f"regenerated {len(assets)} bundled assets")
    return 0


if __name__ == "__main__":
    sys.exit(main())
