#!/usr/bin/env python3
"""Cluster farm desyncs by divergence signature and summarize the farm.

Signature: at the divergence frame, the per-range fingerprints the two clients
logged (`live desync f<frame> range <name> <hash>` lines in slot-a.log /
slot-b.log) are diffed; the signature is the sorted set of range names whose
hashes differ. Runs without both sides fall back to the reporting side's ranges
plus a coarse frame bucket, so they still cluster together.

Usage:
  triage.py [--dir ~/Games/cosmo-farm] [--json] [--top N]
"""
import argparse
import json
import os
import re
import sys
from collections import Counter, defaultdict

DESYNC_LINE = re.compile(r"live desync f(\d+) range (\S+) ([0-9a-fA-F]{16})")
VOID_LINE = re.compile(r"voiding the game \(no hash majority at frame (\d+)\)")


def log_fps(path):
    """{frame: {range: hash}} from `live desync` lines (either side)."""
    out = defaultdict(dict)
    if not os.path.exists(path):
        return out
    with open(path, errors="replace") as f:
        for line in f:
            m = DESYNC_LINE.search(line)
            if m:
                out[int(m.group(1))][m.group(2)] = m.group(3)
    return out


def void_frame(path):
    if not os.path.exists(path):
        return None
    with open(path, errors="replace") as f:
        for line in f:
            m = VOID_LINE.search(line)
            if m:
                return int(m.group(1))
    return None


def signature(a_path, b_path):
    """Sorted differing range names, or a fallback key."""
    fa = log_fps(a_path)
    fb = log_fps(b_path)
    common = set(fa) & set(fb)
    if common:
        frame = sorted(common)[0]
        sig = sorted(
            name
            for name in set(fa[frame]) | set(fb[frame])
            if fa[frame].get(name) != fb[frame].get(name)
        )
        if sig:
            return ("ranges", frame, tuple(sig))
    # Fallback: reporting side + coarse frame bucket (signature is less precise
    # but still clusters identical causes).
    frames = sorted(set(fa) | set(fb))
    if not frames:
        return ("void-only", None, ())
    f = frames[0]
    names = tuple(sorted(set(fa.get(f, {})) | set(fb.get(f, {}))))
    return ("fallback", f // 1000 * 1000, names)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=os.path.expanduser("~/Games/cosmo-farm"))
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--top", type=int, default=15)
    args = ap.parse_args()

    art = os.path.join(args.dir, "artifacts")
    stats_path = os.path.join(args.dir, "stats.json")
    stats = json.load(open(stats_path)) if os.path.exists(stats_path) else {}
    clusters = defaultdict(list)
    examples = {}
    if os.path.isdir(art):
        for run in sorted(os.listdir(art)):
            d = os.path.join(art, run)
            if not os.path.isdir(d):
                continue
            sig = signature(os.path.join(d, "slot-a.log"), os.path.join(d, "slot-b.log"))
            clusters[sig].append(run)
            examples.setdefault(sig, run)

    rows = []
    for sig, runs in clusters.items():
        kind, frame, names = sig
        rows.append(
            {
                "signature": "|".join(names) if names else ("void-only" if kind == "void-only" else "unknown"),
                "frame": frame,
                "count": len(runs),
                "latest": runs[-1],
                "example": examples[sig],
            }
        )
    rows.sort(key=lambda r: -r["count"])

    if args.json:
        json.dump(
            {"stats": stats, "clusters": rows[: args.top]},
            sys.stdout,
            indent=2,
        )
        return

    total = sum(s.get("runs", 0) for s in stats.values() if isinstance(s, dict))
    desync = sum(s.get("desync", 0) for s in stats.values() if isinstance(s, dict))
    clean = sum(s.get("clean", 0) for s in stats.values() if isinstance(s, dict))
    print(f"== farm {args.dir}")
    print(f"   runs {total}   clean {clean}   desync {desync}   unique signatures {len(clusters)}")
    if stats.get("updated"):
        print(f"   updated {stats['updated']}")
    print()
    print(f"== top {args.top} divergence signatures (count | frame | ranges)")
    for r in rows[: args.top]:
        print(f"   {r['count']:>4}  f{r['frame'] if r['frame'] is not None else '?':>7}  {r['signature'] or '-'}")
        print(f"        example: {r['example']}")
    if not rows:
        print("   (no desync artifacts yet — runs are clean so far)")


if __name__ == "__main__":
    main()
