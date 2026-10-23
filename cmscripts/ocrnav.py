#!/usr/bin/env python3
"""OCR-driven menu navigation for the Cosmonarchy farm.

The patched title menu changes between releases, so the farm must not depend on
fixed click coordinates. This tool OCRs a screenshot and locates words by
label, returning their center so a driver can click "Single Player", "Ok",
a map name, etc. by text.

Usage:
  ocrnav.py find <image> <label...>   # print "x y score" of the best match
  ocrnav.py words <image>             # print "word x y" for every OCR word
  ocrnav.py click <display> <image> <label...>   # locate + xdotool click

Matching is fuzzy: OCR output is folded to alphanumerics, lowercased, and a
label matches if its folded form is a subsequence of the OCR word (or vice
versa for long words), scored by length ratio.
"""
import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path


def ocr_words(image: Path) -> list[tuple[str, int, int, int, int]]:
    """[(word, left, top, width, height)] from tesseract TSV."""
    with tempfile.TemporaryDirectory() as tmp:
        up = Path(tmp) / "up.png"
        subprocess.run(
            [
                "magick", str(image),
                "-resize", "200%",
                "-colorspace", "Gray",
                "-normalize",
                str(up),
            ],
            check=True, capture_output=True,
        )
        out = subprocess.run(
            ["tesseract", str(up), "stdout", "--psm", "11", "tsv"],
            check=True, capture_output=True, text=True,
        ).stdout
    words = []
    for line in out.splitlines()[1:]:
        f = line.split("\t")
        if len(f) < 12 or not f[11].strip():
            continue
        words.append((f[11], int(f[6]), int(f[7]), int(f[8]), int(f[9])))
    # Coordinates were scaled 2x; map back.
    return [(w, x // 2, y // 2, wd // 2, ht // 2) for (w, x, y, wd, ht) in words]


def fold(s: str) -> str:
    return re.sub(r"[^a-z0-9]", "", s.lower())


def match_score(ocr_word: str, label: str) -> float:
    w, l = fold(ocr_word), fold(label)
    if not w or not l:
        return 0.0
    if l in w or w in l:
        return len(l) / len(w) if len(w) >= len(l) else len(w) / len(l)
    # subsequence match
    def subseq(needle, hay):
        it = iter(hay)
        return all(c in it for c in needle)
    if subseq(l, w):
        return 0.8 * len(l) / len(w)
    return 0.0


def find_label(image: Path, label: str, region=None):
    x0, y0, x1, y1 = region or (0, 0, 10**9, 10**9)
    best = None
    for (w, x, y, wd, ht) in ocr_words(image):
        if not (x0 <= x <= x1 and y0 <= y <= y1):
            continue
        score = match_score(w, label)
        if score > 0.35 and (best is None or score > best[0]):
            best = (score, x + wd // 2, y + ht // 2, w)
    return best


def region_args(p):
    p.add_argument("--x0", type=int, default=0)
    p.add_argument("--y0", type=int, default=0)
    p.add_argument("--x1", type=int, default=10**9)
    p.add_argument("--y1", type=int, default=10**9)


def region_of(args):
    return (args.x0, args.y0, args.x1, args.y1)


def cmd_find(args):
    for label in args.label:
        hit = find_label(Path(args.image), label, region_of(args))
        if hit:
            print(f"{hit[1]} {hit[2]} {hit[0]:.2f} ({hit[3]!r} ~ {label})")
        else:
            print(f"- - 0.00 (no match for {label})")


def cmd_words(args):
    for (w, x, y, wd, ht) in ocr_words(Path(args.image)):
        print(f"{w} {x + wd // 2} {y + ht // 2}")


def cmd_click(args):
    hit = find_label(Path(args.image), " ".join(args.label), region_of(args))
    if not hit:
        sys.exit(f"no match for {' '.join(args.label)!r}")
    subprocess.run(
        ["xdotool", "mousemove", str(hit[1]), str(hit[2]), "click", "1"],
        env={"DISPLAY": args.display, "PATH": "/usr/bin:/bin"},
        check=False,
    )
    print(f"clicked {hit[1]},{hit[2]} for {' '.join(args.label)!r}")


def cmd_dclick(args):
    hit = find_label(Path(args.image), " ".join(args.label), region_of(args))
    if not hit:
        sys.exit(f"no match for {' '.join(args.label)!r}")
    subprocess.run(
        [
            "xdotool", "mousemove", str(hit[1]), str(hit[2]),
            "click", "--repeat", "2", "--delay", "120", "1",
        ],
        env={"DISPLAY": args.display, "PATH": "/usr/bin:/bin"},
        check=False,
    )
    print(f"double-clicked {hit[1]},{hit[2]} for {' '.join(args.label)!r}")


def cmd_firstlist(args):
    """Click the first OCR word in the dialog list region (profile selector)."""
    x0, y0, x1, y1 = args.x0, args.y0, args.x1, args.y1
    best = None
    for (w, x, y, wd, ht) in ocr_words(Path(args.image)):
        if not (x0 <= x <= x1 and y0 <= y <= y1):
            continue
        if w in {"Ok", "Cancel", "New", "Delete"}:
            continue
        if best is None or (y, x) < best[0]:
            best = ((y, x), x + wd // 2, y + ht // 2, w)
    if not best:
        sys.exit("no list entry in region")
    subprocess.run(
        ["xdotool", "mousemove", str(best[1]), str(best[2]), "click", "1"],
        env={"DISPLAY": args.display, "PATH": "/usr/bin:/bin"},
        check=False,
    )
    print(f"clicked list entry {best[1]},{best[2]} ({best[3]!r})")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("find")
    p.add_argument("image")
    p.add_argument("label", nargs="+")
    region_args(p)
    p.set_defaults(fn=cmd_find)
    p = sub.add_parser("words")
    p.add_argument("image")
    p.set_defaults(fn=cmd_words)
    p = sub.add_parser("click")
    p.add_argument("display")
    p.add_argument("image")
    p.add_argument("label", nargs="+")
    region_args(p)
    p.set_defaults(fn=cmd_click)
    p = sub.add_parser("dclick")
    p.add_argument("display")
    p.add_argument("image")
    p.add_argument("label", nargs="+")
    region_args(p)
    p.set_defaults(fn=cmd_dclick)
    p = sub.add_parser("firstlist")
    p.add_argument("display")
    p.add_argument("image")
    p.add_argument("--x0", type=int, default=280)
    p.add_argument("--y0", type=int, default=150)
    p.add_argument("--x1", type=int, default=700)
    p.add_argument("--y1", type=int, default=340)
    p.set_defaults(fn=cmd_firstlist)
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
