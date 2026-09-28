#!/usr/bin/env python3
"""Collapse duplicated hygiene entries into one-line pointers.

Each entry in MERGES keeps its heading (IDs are stable and cited elsewhere) and
has its body replaced by a pointer to the entry that carries the full finding.
Run from the repository root.
"""

import re
import sys
from pathlib import Path

FILES = {
    "HYGC": "notes/hygiene-channels.md",
    "HYGG": "notes/hygiene-guards.md",
    "HYGP": "notes/hygiene-policy.md",
    "HYGV": "notes/hygiene-values.md",
    "BUG-": "notes/bugs.md",
}

# duplicate -> entry (or entries, joined by " and ") that carries the finding.
# Live defects are carried by notes/bugs.md.
MERGES = {
    "HYGC-007": "BUG-025",
    "HYGC-008": "BUG-044",
    "HYGC-017": "BUG-060",
    "HYGC-020": "BUG-029",
    "HYGC-028": "BUG-043",
    "HYGC-031": "BUG-065",
    "HYGC-034": "BUG-027",
    "HYGG-003": "BUG-047",
    "HYGG-007": "BUG-073",
    "HYGG-009": "BUG-073",
    "HYGG-030": "BUG-016",
    "HYGG-031": "BUG-030",
    "HYGG-038": "BUG-036",
    "HYGG-043": "BUG-076",
    "HYGG-047": "BUG-059",
    "HYGG-048": "BUG-070",
    "HYGG-054": "BUG-021",
    "HYGG-056": "BUG-042",
    "HYGG-062": "BUG-019",
    "HYGG-069": "BUG-028",
    "HYGG-073": "BUG-031",
    "HYGG-082": "BUG-035",
    "HYGG-096": "BUG-056",
    "HYGG-097": "BUG-058",
    "HYGG-098": "BUG-058",
    "HYGG-105": "BUG-015",
    "HYGP-012": "BUG-014",
    "HYGP-016": "BUG-032",
    "HYGP-019": "BUG-066 and BUG-065",
    "HYGP-025": "BUG-061",
    "HYGV-022": "BUG-018",
    "HYGV-030": "BUG-069",
    "HYGV-071": "BUG-063 and BUG-064",
    "HYGV-080": "BUG-017",
    "HYGV-100": "BUG-038",
    "HYGV-011": "BUG-044",
    "HYGV-017": "HYGC-025",
    "HYGV-023": "HYGG-102",
    "HYGV-026": "HYGG-024",
    "HYGV-057": "HYGP-020",
    "HYGV-088": "HYGP-001",
    "HYGV-097": "HYGG-080",
    "HYGV-101": "HYGP-023",
    "HYGV-105": "HYGP-007",
    "HYGV-106": "HYGP-056",
    "HYGV-108": "BUG-066",
    "HYGG-013": "HYGP-036",
    "HYGG-014": "HYGP-036",
    "HYGG-020": "HYGV-043",
    "HYGG-040": "HYGP-041",
    "HYGG-044": "HYGP-031",
    "HYGG-060": "BUG-017",
    "HYGG-063": "HYGV-075",
    "HYGG-070": "HYGV-019",
    "HYGG-071": "HYGV-019",
    "HYGG-079": "HYGP-039",
    "HYGG-083": "HYGV-037",
    "HYGG-084": "HYGP-038",
    "HYGG-101": "HYGV-028",
    "HYGG-106": "BUG-018",
    "HYGG-107": "HYGV-066",
    "HYGG-108": "HYGV-065",
    "HYGG-109": "BUG-024",
    "HYGG-110": "HYGV-005",
    "HYGG-114": "HYGP-036",
    "HYGG-120": "HYGV-063",
    "HYGG-121": "HYGV-060",
    "HYGG-122": "HYGV-047",
    "HYGG-123": "HYGV-014",
    "HYGP-003": "HYGV-087",
    "HYGP-026": "HYGV-096",
    "HYGP-047": "HYGV-059",
    "HYGP-051": "BUG-056",
}

HEADING = re.compile(r"^## (HYG[CGPV]-\d{3}) ")


def stub(targets: str) -> str:
    refs = " and ".join(
        f"{target} (`{FILES[target[:4]]}`)" for target in targets.split(" and ")
    )
    verb = "carry" if " and " in targets else "carries"
    return f"\nMerged into {refs}, which {verb} the full finding.\n\n"


def main() -> int:
    ids_seen = set()
    for prefix, path in FILES.items():
        if prefix == "BUG-":
            continue
        lines = Path(path).read_text().splitlines(keepends=True)
        out = []
        skipping = False
        for line in lines:
            match = HEADING.match(line)
            if match or line.startswith("---"):
                skipping = False
            if skipping:
                continue
            out.append(line)
            if match and match.group(1) in MERGES:
                entry = match.group(1)
                ids_seen.add(entry)
                out.append(stub(MERGES[entry]))
                skipping = True
        Path(path).write_text("".join(out))
    missing = set(MERGES) - ids_seen
    if missing:
        print(f"entries not found: {sorted(missing)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
