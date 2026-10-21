#!/usr/bin/env python3
"""Read a screen region with Tesseract and print the text.

StarCraft draws its UI in a small pixel font, so the region is upscaled 4x and flattened
to greyscale before OCR.

    ocr_text.py <image> <x> <y> <width> <height> [psm]
"""

import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    if len(sys.argv) < 6:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    image = Path(sys.argv[1])
    x, y, w, h = (int(v) for v in sys.argv[2:6])
    psm = sys.argv[6] if len(sys.argv) > 6 else "6"

    with tempfile.TemporaryDirectory() as tmp:
        crop = Path(tmp) / "crop.png"
        base = Path(tmp) / "crop"
        subprocess.run(
            [
                "magick", str(image),
                "-crop", f"{w}x{h}+{x}+{y}", "+repage",
                "-resize", "400%",
                "-colorspace", "Gray",
                "-normalize",
                str(crop),
            ],
            check=True,
            capture_output=True,
        )
        result = subprocess.run(
            ["tesseract", str(crop), "stdout", "--psm", psm],
            check=True,
            capture_output=True,
            text=True,
        )
        print(" ".join(result.stdout.split()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
