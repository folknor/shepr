#!/usr/bin/env python3
"""Report what changed in upstream herdr since the commit shepr last caught up to.

shepr is a hard fork of https://github.com/herdrdev/herdr. This script keeps a
blobless clone of upstream in `research/herdr-upstream/` (untracked), fetches it,
and reports, per watched path, the files added, removed and modified between the
recorded baseline (`scripts/upstream_baseline.txt`) and upstream HEAD, each with
the shepr file it concerns.

  scripts/upstream_watch.py                report
  scripts/upstream_watch.py --diff         report with the upstream diffs
  scripts/upstream_watch.py --log          also list the commits touching watched paths
  scripts/upstream_watch.py --no-fetch     do not touch the network
  scripts/upstream_watch.py --advance [REV]  record REV (default: upstream HEAD) as
                                           the new baseline, once caught up
  scripts/upstream_watch.py --fork-point   rank upstream commits by how many watched
                                           files are byte-identical to ours, to check
                                           the recorded baseline is the fork commit

The baseline is a full sha. Advancing it is a deliberate act: only after the
reported changes were applied to shepr or judged irrelevant.
"""

from __future__ import annotations

import argparse
import difflib
import subprocess
import sys

from _brokkr_config import ROOT

UPSTREAM_URL = "https://github.com/herdrdev/herdr.git"
CLONE = ROOT / "research" / "herdr-upstream"
BASELINE = ROOT / "scripts" / "upstream_baseline.txt"

# (upstream prefix, shepr prefix). Longest upstream prefix wins. A path under an
# upstream prefix maps to the same relative path under the shepr prefix.
MAPPING = [
    ("src/integration/assets/", "crates/shepr-agent/src/integration/assets/"),
    ("src/integration/", "crates/shepr-agent/src/integration/"),
    ("src/detect/manifests/", "crates/shepr-agent/src/detect/manifests/"),
    ("src/detect/", "crates/shepr-agent/src/detect/"),
]

# Watched upstream paths with no fixed shepr counterpart: where the agent list,
# resume definitions and hook wiring live upstream. A change here is reported
# with the shepr area to compare by hand.
LOOSE = [
    ("src/detect.rs", "crates/shepr-agent/src/detect/"),
    ("src/integration.rs", "crates/shepr-agent/src/integration/"),
    ("src/agent", "crates/shepr-agent/src/ (agent list, resume definitions)"),
    ("src/resume", "crates/shepr-agent/src/ (resume definitions)"),
]

WATCHED =["src/integration/", "src/detect/"] + [p for p, _ in LOOSE]


def git(*args: str, cwd=CLONE, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )
    if check and result.returncode != 0:
        sys.exit(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def ensure_clone(fetch: bool) -> None:
    if not (CLONE / ".git").exists():
        CLONE.parent.mkdir(parents=True, exist_ok=True)
        print(f"cloning {UPSTREAM_URL} (blobless) into {CLONE.relative_to(ROOT)}", file=sys.stderr)
        subprocess.run(
            ["git", "clone", "--filter=blob:none", "--no-checkout", UPSTREAM_URL, str(CLONE)],
            check=True,
        )
    elif fetch:
        git("fetch", "--quiet", "origin")


def head_rev() -> str:
    return git("rev-parse", "origin/HEAD").strip()


def baseline() -> str:
    text = BASELINE.read_text().strip()
    if not text:
        sys.exit(f"{BASELINE.relative_to(ROOT)} is empty")
    return git("rev-parse", "--verify", f"{text}^{{commit}}").strip()


def ours(path: str) -> str:
    best = max((m for m in MAPPING if path.startswith(m[0])), key=lambda m: len(m[0]), default=None)
    if best is not None:
        return best[1] + path[len(best[0]):]
    for up, area in LOOSE:
        if path.startswith(up):
            return area
    return "(no mapping)"


def changes(base: str, head: str) -> list[tuple[str, str]]:
    out = git("diff", "--name-status", "--no-renames", base, head, "--", *WATCHED)
    rows = []
    for line in out.splitlines():
        status, _, path = line.partition("\t")
        rows.append((status, path))
    return rows


def report(show_diff: bool, show_log: bool, do_fetch: bool) -> int:
    ensure_clone(do_fetch)
    base, head = baseline(), head_rev()
    print(f"baseline {base[:12]}  upstream HEAD {head[:12]}")
    if base == head:
        print("upstream has not moved past the baseline")
        return 0
    count = git("rev-list", "--count", f"{base}..{head}").strip()
    print(f"{count} upstream commit(s) since the baseline")
    if show_log:
        print("\ncommits touching watched paths:")
        print(git("log", "--format=%h %ad %s", "--date=short", f"{base}..{head}", "--", *WATCHED), end="")
    rows = changes(base, head)
    if not rows:
        print("\nno watched file changed")
        return 0
    names = {"A": "added", "D": "removed", "M": "modified"}
    for status in ("A", "D", "M"):
        group = [p for s, p in rows if s == status]
        if not group:
            continue
        print(f"\n{names.get(status, status)} ({len(group)}):")
        for path in group:
            print(f"  {path}\n      -> {ours(path)}")
    if show_diff:
        for status, path in rows:
            print(f"\n===== {names.get(status, status)}: {path}  ->  {ours(path)}")
            print(git("diff", "--no-renames", base, head, "--", path), end="")
    return 0


def advance(rev: str | None, do_fetch: bool) -> int:
    ensure_clone(do_fetch)
    target = git("rev-parse", "--verify", f"{rev or 'origin/HEAD'}^{{commit}}").strip()
    BASELINE.write_text(target + "\n")
    print(f"baseline is now {target}")
    return 0


def normalized(text: str) -> list[str]:
    """Upstream text with the herdr names respelled as shepr's."""
    return text.replace("herdr", "shepr").replace("HERDR", "SHEPR").replace("Herdr", "Shepr").splitlines()


def distance(upstream: list[str], mine: list[str]) -> int:
    """Lines added or removed to turn one file into the other."""
    return sum(1 for line in difflib.ndiff(upstream, mine) if line[:1] in "+-")


def fork_point(do_fetch: bool, limit: int = 300) -> int:
    """Rank upstream commits by how far their manifests are from ours.

    Detection manifests are the files shepr edits least, so the commit whose
    manifests are closest to ours, after respelling herdr as shepr, is the best
    fork-point candidate. Manifests that shepr dropped or added are skipped.
    """
    ensure_clone(do_fetch)
    manifests = "src/detect/manifests/"
    mine_dir = ROOT / "crates/shepr-agent/src/detect/manifests"
    mine = {p.name: p.read_text().splitlines() for p in mine_dir.glob("*.toml")}
    revs = git("rev-list", f"--max-count={limit}", "origin/HEAD", "--", manifests).split()
    cache: dict[tuple[str, str], int] = {}
    scored = []
    for rev in revs:
        total = 0
        compared = 0
        for line in git("ls-tree", rev, manifests).splitlines():
            meta, _, path = line.partition("\t")
            name = path.rsplit("/", 1)[-1]
            if name not in mine:
                continue
            blob = meta.split()[2]
            if (blob, name) not in cache:
                cache[(blob, name)] = distance(normalized(git("cat-file", "-p", blob)), mine[name])
            total += cache[(blob, name)]
            compared += 1
        scored.append((total, compared, rev))
    scored.sort(key=lambda s: s[0])
    print(f"commits (of the last {limit} touching manifests) closest to our manifests:")
    for total, compared, rev in scored[:8]:
        subject = git("log", "-1", "--format=%ad %s", "--date=short", rev).strip()
        print(f"  {rev[:12]}  {total:5d} lines apart over {compared} manifests  {subject}")
    print("a tie means no manifest changed between those commits; the fork commit is at or\n"
          "after the newest commit in the closest group.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--diff", action="store_true", help="include upstream diffs")
    parser.add_argument("--log", action="store_true", help="list commits touching watched paths")
    parser.add_argument("--no-fetch", action="store_true", help="skip git fetch")
    parser.add_argument("--advance", nargs="?", const="", metavar="REV", help="record REV (default upstream HEAD) as baseline")
    parser.add_argument("--fork-point", action="store_true", help="rank commits by similarity to our files")
    args = parser.parse_args()
    fetch = not args.no_fetch
    if args.advance is not None:
        return advance(args.advance or None, fetch)
    if args.fork_point:
        return fork_point(fetch)
    return report(args.diff, args.log, fetch)


if __name__ == "__main__":
    sys.exit(main())
