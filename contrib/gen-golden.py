#!/usr/bin/env python3
"""Generate golden images for MSDF visual regression tests.

Uses Pillow (FreeType backend) to render text at known-good quality.
These serve as ground-truth references for SSIM comparison against
the Bevy MSDF renderer.

Usage:
    python3 contrib/gen-golden.py              # generate all
    python3 contrib/gen-golden.py --preview    # show in terminal via kitty/sixel
    python3 contrib/gen-golden.py --list       # list what would be generated

Fonts: Uses fontconfig defaults (fc-match monospace / fc-match serif).
       On Arch: Noto Sans Mono + Noto Serif.
"""

import argparse
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parent
GOLDEN_DIR = PROJECT_ROOT / "assets" / "test" / "golden"


def find_font(family: str) -> str:
    """Resolve a font family name to a file path via fontconfig."""
    result = subprocess.run(
        ["fc-match", family, "--format=%{file}"],
        capture_output=True, text=True, check=True,
    )
    path = result.stdout.strip()
    if not Path(path).exists():
        print(f"Warning: fc-match returned {path} but file doesn't exist", file=sys.stderr)
    return path


MONO_FONT = find_font("monospace")
SERIF_FONT = find_font("serif")

print(f"Monospace font: {MONO_FONT}")
print(f"Serif font:     {SERIF_FONT}")


@dataclass
class GoldenSpec:
    """Specification for a golden image, mirroring TestConfig in tests.rs."""
    name: str
    text: str
    font_size: int  # px
    width: int
    height: int
    font_path: str
    left: int = 10
    top: int = 10


# These MUST match the TestConfig values in tests.rs exactly.
SPECS: list[GoldenSpec] = [
    GoldenSpec(
        name="golden_document_22px_mono",
        text="document",
        font_size=22,
        width=250,
        height=60,
        font_path=MONO_FONT,
    ),
    GoldenSpec(
        name="golden_mm_22px_mono",
        text="mm",
        font_size=22,
        width=100,
        height=50,
        font_path=MONO_FONT,
    ),
    GoldenSpec(
        name="golden_hello_15px_mono",
        text="Hello, World!",
        font_size=15,
        width=200,
        height=40,
        font_path=MONO_FONT,
    ),
    GoldenSpec(
        name="golden_av_22px_serif",
        text="AV",
        font_size=22,
        width=100,
        height=50,
        font_path=SERIF_FONT,
    ),
    GoldenSpec(
        name="golden_code_15px_mono",
        text="fn main() {",
        font_size=15,
        width=200,
        height=40,
        font_path=MONO_FONT,
    ),
]


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

def render_golden(spec: GoldenSpec) -> Image.Image:
    """Render a golden image using Pillow's FreeType backend.

    - Black background, white text (matches test harness ClearColor + default)
    - Antialiased via FreeType's built-in hinting + grayscale AA
    - Kerning enabled by default in Pillow's text rendering
    """
    img = Image.new("RGBA", (spec.width, spec.height), (0, 0, 0, 255))
    draw = ImageDraw.Draw(img)

    font = ImageFont.truetype(spec.font_path, spec.font_size)
    draw.text(
        (spec.left, spec.top),
        spec.text,
        font=font,
        fill=(255, 255, 255, 255),
    )

    return img


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preview", action="store_true", help="Print paths only, don't overwrite")
    parser.add_argument("--list", action="store_true", help="List specs without generating")
    parser.add_argument("names", nargs="*", help="Generate only these (substring match)")
    args = parser.parse_args()

    GOLDEN_DIR.mkdir(parents=True, exist_ok=True)

    specs = SPECS
    if args.names:
        specs = [s for s in SPECS if any(n in s.name for n in args.names)]

    if args.list:
        for spec in specs:
            path = GOLDEN_DIR / f"{spec.name}.png"
            exists = "exists" if path.exists() else "missing"
            print(f"  {spec.name}: \"{spec.text}\" {spec.font_size}px {spec.width}x{spec.height} [{exists}]")
        return

    for spec in specs:
        img = render_golden(spec)
        path = GOLDEN_DIR / f"{spec.name}.png"
        img.save(path)
        print(f"  {path.relative_to(PROJECT_ROOT)}  ({spec.text!r} {spec.font_size}px)")

        if args.preview:
            # Also save to /tmp for quick viewing
            tmp_path = Path("/tmp/msdf_tests") / f"{spec.name}_freetype.png"
            tmp_path.parent.mkdir(parents=True, exist_ok=True)
            img.save(tmp_path)
            print(f"    preview: {tmp_path}")

    print(f"\n{len(specs)} golden images written to {GOLDEN_DIR.relative_to(PROJECT_ROOT)}/")


if __name__ == "__main__":
    main()
