#!/usr/bin/env python3
"""Report what changed in upstream herdr since the commit shepr last caught up to.

shepr is a hard fork of https://github.com/herdrdev/herdr. This script keeps a
blobless bare mirror of upstream in `research/herdr-upstream/` (gitignored),
fetches it, and reports, per watched path, the files added, removed and modified
between the recorded baseline (`scripts/upstream_baseline.txt`) and upstream
HEAD, each with the shepr file it concerns. The mirror has no working tree: it
is only ever read through git objects, never through checked-out files.

Watched: the integration assets (hooks, plugins and their bun tests), the
detection manifests both as upstream bundles them and as it publishes them for
over-the-air updates (a published manifest can be newer than the bundled one,
and shepr, which has no over-the-air updates, ports it as a detection fix), the
manifest check tooling, the detection and hook wiring with its tests, and host
terminal input framing. The shepr side of every mapping is also listed in
AGENTS.md ("Upstream tracking"); `scripts/check_upstream_watch_paths.py` keeps
the two lists equal.

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
    # Upstream's published copies of the manifests, one file per agent under
    # the same names as the bundled ones.
    ("distribution/agent-detection/", "crates/shepr-agent/src/detect/manifests/"),
    ("src/pane/agent_detection.rs", "crates/shepr-mux/src/pane/agent_detection.rs"),
    ("src/server/autodetect.rs", "src/autodetect.rs"),
    # Host terminal input framing: escape disambiguation, split mouse reports,
    # held host replies.
    ("src/raw_input.rs", "crates/shepr-termio/src/input/raw_input.rs"),
    ("src/client/input.rs", "crates/shepr-client/src/input.rs"),
]

# (upstream path, shepr path or None, note). Watched upstream paths with no
# same-named shepr counterpart: where the agent list, resume definitions, hook
# wiring and the tests that exercise them live upstream. A change here is
# reported with the shepr area to compare by hand; None means shepr has no
# counterpart at all.
LOOSE = [
    ("distribution/agent-detection/index.toml", None, "upstream's publish catalog; shepr publishes nothing"),
    ("scripts/agent_detection_manifest_check.py", "crates/shepr-agent/src/detect/", "manifest validation and its tests"),
    ("scripts/test_agent_detection_manifest_check.py", "crates/shepr-agent/src/detect/", "manifest validation and its tests"),
    ("scripts/test_hermes_integration_asset.py", "crates/shepr-agent/src/integration/", "asset tests"),
    ("tests/auto_detect.rs", "crates/shepr-agent/src/detect/", "detection tests"),
    ("src/detect.rs", "crates/shepr-agent/src/detect/", "detection entry point"),
    ("src/integration.rs", "crates/shepr-agent/src/integration/", "integration entry point"),
    ("src/agent", "crates/shepr-agent/src/agent/", "agent list, resume definitions"),
    ("src/terminal/state.rs", "crates/shepr-mux/src/terminal/state/", "hook authority, sessions"),
    ("src/app/actions.rs", "crates/shepr-server/src/app/", "hook-lifecycle tests"),
]

WATCHED = sorted({p for p, _ in MAPPING} | {p for p, _, _ in LOOSE})


def shepr_paths() -> list[str]:
    """Every shepr file or directory a watched upstream path maps to: the list
    AGENTS.md repeats."""
    return sorted({mine for _, mine in MAPPING} | {mine for _, mine, _ in LOOSE if mine is not None})


def git(*args: str, cwd=CLONE, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )
    if check and result.returncode != 0:
        sys.exit(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def ensure_clone(fetch: bool) -> None:
    if not (CLONE / "HEAD").exists():
        if CLONE.exists():
            sys.exit(
                f"{CLONE.relative_to(ROOT)} exists but is not the bare mirror this script keeps; "
                "remove it and run again"
            )
        CLONE.parent.mkdir(parents=True, exist_ok=True)
        print(f"mirroring {UPSTREAM_URL} (blobless) into {CLONE.relative_to(ROOT)}", file=sys.stderr)
        subprocess.run(
            ["git", "clone", "--mirror", "--filter=blob:none", UPSTREAM_URL, str(CLONE)],
            check=True,
        )
    elif fetch:
        git("fetch", "--quiet", "--prune", "origin")


def head_rev() -> str:
    return git("rev-parse", "HEAD").strip()


def baseline() -> str:
    text = BASELINE.read_text().strip()
    if not text:
        sys.exit(f"{BASELINE.relative_to(ROOT)} is empty")
    return git("rev-parse", "--verify", f"{text}^{{commit}}").strip()


def ours(path: str) -> str:
    """The shepr file or area an upstream path concerns: the longest matching
    prefix across both tables, so a LOOSE entry nested under a MAPPING prefix
    wins for its own path."""
    candidates = [(up, mine, None) for up, mine in MAPPING] + list(LOOSE)
    best = max((c for c in candidates if path.startswith(c[0])), key=lambda c: len(c[0]), default=None)
    if best is None:
        return "(no mapping)"
    up, mine, note = best
    if note is None:
        return mine + path[len(up):]
    return f"{mine} ({note})" if mine is not None else f"({note})"


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
    target = git("rev-parse", "--verify", f"{rev or 'HEAD'}^{{commit}}").strip()
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
    revs = git("rev-list", f"--max-count={limit}", "HEAD", "--", manifests).split()
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
