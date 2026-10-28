#!/usr/bin/env python3
"""Click target for a named row in a StarCraft list box.

The UI is drawn in a small pixel font that Tesseract cannot read at 1x, so the region is
cropped, upscaled 4x and flattened to greyscale before OCR. Rows are matched by their whole
line first ("(2)Potivector 1.1.scx") and by single words second ("1/8 alpha Pagan Imperium"
-> "alpha"), because the CrownLink games list and the map browser format rows differently.

    ocr_row.py <screenshot.png> <text> [x y width height]

Defaults to the games list box of a 1280x720 screenshot. Prints "x y" (root-screen
coordinates) on success; exits 1 when nothing matches.
"""

import csv
import subprocess
import sys
import tempfile
from pathlib import Path

# Games list box in a 1280x720 screenshot (the game renders 640x480 centred).
CROP_X, CROP_Y, CROP_W, CROP_H = 250, 140, 470, 130
SCALE = 4

# tesseract TSV page layout: 1 page, 2 block, 3 paragraph, 4 line, 5 word.
LINE, WORD = "4", "5"


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    image, want = Path(sys.argv[1]), sys.argv[2].lower()
    global CROP_X, CROP_Y, CROP_W, CROP_H
    if len(sys.argv) >= 7:
        CROP_X, CROP_Y, CROP_W, CROP_H = (int(v) for v in sys.argv[3:7])

    with tempfile.TemporaryDirectory() as tmp:
        crop = Path(tmp) / "crop.png"
        base = Path(tmp) / "crop"
        try:
            subprocess.run(
                [
                    "magick", str(image),
                    "-crop", f"{CROP_W}x{CROP_H}+{CROP_X}+{CROP_Y}", "+repage",
                    "-resize", f"{SCALE * 100}%",
                    "-colorspace", "Gray",
                    "-normalize",
                    str(crop),
                ],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError:
            return 1
        # Tesseract is flaky on this pixel font: the same image OCRs differently run to
        # run (rows drop out or gain junk glyphs). Retry several times and match against
        # the UNION of all runs' rows, so a row that read cleanly in any single pass is
        # found. A persistent tesseract failure still means "no match".
        ok = False
        matches = []
        for _ in range(6):
            try:
                subprocess.run(
                    ["tesseract", str(crop), str(base), "--psm", "6", "tsv"],
                    check=True,
                    capture_output=True,
                )
                ok = True
            except subprocess.CalledProcessError:
                continue
            with open(f"{base}.tsv", newline="", encoding="utf-8") as file:
                rows = list(csv.DictReader(file, delimiter="\t"))
            # Multi-word queries ("Class 0 - Testing Grounds") never match a single
            # word: the line rows carry no text and tesseract splits the row across
            # words. Reconstruct each line by joining its words (grouped by block,
            # paragraph and line) and match the query against the joined line, then
            # against each word as a fallback for single-word queries.
            lines = {}
            for row in rows:
                if row.get("level") != WORD:
                    continue
                key = (row.get("block_num"), row.get("par_num"), row.get("line_num"))
                word = (row.get("text") or "").strip().lower()
                try:
                    top = int(row["top"])
                    height = int(row["height"])
                    left = int(row["left"])
                    width = int(row["width"])
                except (KeyError, TypeError, ValueError):
                    continue
                if word:
                    entry = lines.setdefault(key, {"words": [], "top": top, "height": height, "left": left, "right": left + width})
                    entry["words"].append(word)
                    entry["top"] = min(entry["top"], top)
                    entry["height"] = max(entry["height"], height)
                    entry["left"] = min(entry["left"], left)
                    entry["right"] = max(entry["right"], left + width)
            for entry in lines.values():
                joined = " ".join(entry["words"])
                if want not in joined:
                    continue
                point = (
                    CROP_X + ((entry["left"] + entry["right"]) // 2) // SCALE,
                    CROP_Y + (entry["top"] + entry["height"] // 2) // SCALE,
                )
                if point not in matches:
                    matches.append(point)
        if not ok:
            return 1
        if not matches:
            return 1
        print(matches[0][0], matches[0][1])
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
