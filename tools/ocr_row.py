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
        # Tesseract is flaky on small/degenerate crops: retry a few times and treat a
        # persistent failure as "no match" instead of raising.
        ok = False
        for _ in range(3):
            try:
                subprocess.run(
                    ["tesseract", str(crop), str(base), "tsv", "--psm", "6"],
                    check=True,
                    capture_output=True,
                )
                ok = True
                break
            except subprocess.CalledProcessError:
                continue
        if not ok:
            return 1

        with open(f"{base}.tsv", newline="", encoding="utf-8") as file:
            rows = list(csv.DictReader(file, delimiter="\t"))

        for level in (LINE, WORD):
            for row in rows:
                if row.get("level") != level:
                    continue
                text = (row.get("text") or "").strip().lower()
                if want not in text:
                    continue
                try:
                    top = int(row["top"])
                    height = int(row["height"])
                    left = int(row["left"])
                    width = int(row["width"])
                except (KeyError, TypeError, ValueError):
                    continue
                print(CROP_X + (left + width // 2) // SCALE, CROP_Y + (top + height // 2) // SCALE)
                return 0
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
