#!/usr/bin/env python3
"""Summarise root modules and per-crate Rust file and line totals."""
import os, re, collections

root = "src"
files = []
for d, _, fs in os.walk(root):
    for f in fs:
        if f.endswith(".rs"):
            p = os.path.join(d, f)
            with open(p) as fh:
                files.append((p, fh.read()))

def top(p):
    rel = os.path.relpath(p, root).split(os.sep)
    return rel[0].removesuffix(".rs")

mods = collections.defaultdict(lambda: [0, 0, 0])
for p, t in files:
    n = t.count("\n")
    m = mods[top(p)]
    m[0] += 1; m[1] += n
    if n < 100: m[2] += 1

total = sum(t.count("\n") for _, t in files)
tiny = [(p, t.count("\n")) for p, t in files if t.count("\n") < 100]
print(f"files={len(files)} lines={total} under100={len(tiny)} under50={sum(1 for _,n in tiny if n<50)}")
print("\nmodule          files  lines  <100")
for k, (c, l, s) in sorted(mods.items(), key=lambda x: -x[1][1]):
    print(f"{k:15} {c:5} {l:6} {s:5}")

edges = collections.Counter()
for p, t in files:
    a = top(p)
    for b in set(re.findall(r"crate::(\w+)", t)):
        if b != a:
            edges[(a, b)] += 1
print("\nmodule -> modules it imports (files importing)")
out = collections.defaultdict(list)
for (a, b), n in edges.items():
    out[a].append(f"{b}:{n}")
for a in sorted(out):
    print(f"{a:15} {' '.join(sorted(out[a]))}")

print("\ncrate                 files   lines")
for crate in sorted(os.listdir("crates")):
    source = os.path.join("crates", crate, "src")
    if not os.path.isdir(source):
        continue
    paths = (os.path.join(d, f) for d, _, fs in os.walk(source) for f in fs if f.endswith(".rs"))
    counts = [open(p).read().count("\n") for p in paths]
    print(f"{crate:21} {len(counts):5} {sum(counts):7}")
