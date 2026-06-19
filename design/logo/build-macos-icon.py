#!/usr/bin/env python3
"""Generate Selene's macOS app icon following Apple's icon-grid template.

macOS (unlike iOS) does NOT round app icons automatically: the artwork must
already sit inside the rounded "squircle" with transparent margin around it,
otherwise the Dock shows a hard-edged square. This script renders the Selene
"filled-S" mark onto a navy squircle tile sized to the documented macOS grid
(824x824 body, continuous corners, centred in a 1024x1024 canvas) and emits the
full .icns iconset plus the PNGs referenced by tauri.conf.json.

Reproducible: re-run after the mark changes. Requires `rsvg-convert` (librsvg)
and macOS `iconutil`/`sips`.

    python3 design/logo/build-macos-icon.py
"""

import math
import shutil
import subprocess
import tempfile
from pathlib import Path

# --- macOS icon-grid template (per Apple's 1024px grid) ---------------------
CANVAS = 1024          # full icon canvas
BODY = 824             # rounded-rect body (leaves a 100px transparent margin)
SUPERELLIPSE_N = 5.0   # exponent: ~5 approximates Apple's continuous corners
NAVY = "#0D1117"       # tile background (matches the existing brand mark)

# --- mark placement ---------------------------------------------------------
# The "filled-S" path lives in a 100x100 viewBox, glyph centred at (50,50),
# spanning y=[10,90] (height 80). Scale it so the glyph stands ~63% of the body
# height, leaving comfortable padding inside the tile.
MARK_HEIGHT = 520.0
MARK_SCALE = MARK_HEIGHT / 80.0
MARK_PATH = "M 50 10 A 20 20 0 0 0 50 50 A 20 20 0 0 1 50 90 L 50 10 Z"

ICONS_DIR = Path(__file__).resolve().parents[2] / "src-tauri" / "icons"
MASTER_OUT = Path(__file__).resolve().parent / "marks" / "macos-icon-1024.png"


def squircle_path(size: int, n: float) -> str:
    """Superellipse (squircle) path centred in a `size` x `size` box."""
    cx = cy = size / 2.0
    a = BODY / 2.0
    pts = []
    steps = 720
    for i in range(steps):
        t = 2.0 * math.pi * i / steps
        ct, st = math.cos(t), math.sin(t)
        x = cx + a * math.copysign(abs(ct) ** (2.0 / n), ct)
        y = cy + a * math.copysign(abs(st) ** (2.0 / n), st)
        pts.append(f"{x:.3f} {y:.3f}")
    return "M " + " L ".join(pts) + " Z"


def master_svg() -> str:
    body = squircle_path(CANVAS, SUPERELLIPSE_N)
    # Map the mark's 100-viewBox coords into the canvas, centred.
    tx = f"translate({CANVAS/2} {CANVAS/2}) scale({MARK_SCALE}) translate(-50 -50)"
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS}" height="{CANVAS}" viewBox="0 0 {CANVAS} {CANVAS}">
  <defs>
    <linearGradient id="acc" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#4493f8"/>
      <stop offset="1" stop-color="#58a6ff"/>
    </linearGradient>
  </defs>
  <path d="{body}" fill="{NAVY}"/>
  <path d="{MARK_PATH}" fill="url(#acc)" transform="{tx}"/>
</svg>
"""


def run(*args):
    subprocess.run(args, check=True)


def main():
    if not shutil.which("rsvg-convert"):
        raise SystemExit("rsvg-convert not found (brew install librsvg)")

    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        svg = tmp / "master.svg"
        svg.write_text(master_svg())

        # 1024 master, then downscale everything from it for crisp edges.
        run("rsvg-convert", "-w", str(CANVAS), "-h", str(CANVAS),
            str(svg), "-o", str(MASTER_OUT))
        print(f"wrote {MASTER_OUT.relative_to(Path.cwd())}")

        # Build the .icns iconset.
        iconset = tmp / "icon.iconset"
        iconset.mkdir()
        icns_sizes = {
            "icon_16x16.png": 16, "icon_16x16@2x.png": 32,
            "icon_32x32.png": 32, "icon_32x32@2x.png": 64,
            "icon_128x128.png": 128, "icon_128x128@2x.png": 256,
            "icon_256x256.png": 256, "icon_256x256@2x.png": 512,
            "icon_512x512.png": 512, "icon_512x512@2x.png": 1024,
        }
        for name, px in icns_sizes.items():
            run("sips", "-z", str(px), str(px), str(MASTER_OUT),
                "--out", str(iconset / name))
        run("iconutil", "-c", "icns", str(iconset),
            "-o", str(ICONS_DIR / "icon.icns"))
        print("wrote src-tauri/icons/icon.icns")

        # Regenerate the PNGs referenced by tauri.conf.json + the root master.
        png_sizes = {
            "icon.png": 512, "32x32.png": 32, "64x64.png": 64,
            "128x128.png": 128, "128x128@2x.png": 256,
        }
        for name, px in png_sizes.items():
            run("sips", "-z", str(px), str(px), str(MASTER_OUT),
                "--out", str(ICONS_DIR / name))
            print(f"wrote src-tauri/icons/{name}")


if __name__ == "__main__":
    main()
