#!/usr/bin/env python3
"""Generate every icon Cwoffee ships, from one hand-drawn source.

    assets/brand/cwoffee-source.png   ->  crates/tauri-app/icons/*
                                          ui/public/icon.png

Nothing in here draws the logo. The artwork is drawn by hand at 1024x1024 on a
transparent background; this script only crops it to its own bounds, pads it back
to a square, and resamples it down. If an icon looks wrong at some size, the fix is
the source file or the crop rule below - never a redraw inside the script, because
then the file on disk and the file that produced it stop agreeing.

Sizes, and who consumes them:
  16/32/48/64/128/256/512 .png   scripts/install-desktop.sh (hicolor theme),
                                 and the window icon at runtime (64x64 is embedded
                                 into the binary by include_image! in main.rs)
  32x32, 128x128, icon.png       crates/tauri-app/tauri.conf.json bundle list
  icon.ico                       Windows packaging
  ui/public/icon.png             the titlebar mark and the empty-screen cup

Requires Pillow:  python3 -c "import PIL" || pip install pillow
"""
from __future__ import annotations

import sys
from pathlib import Path

try:
    from PIL import Image, ImageFilter, ImageOps
except ImportError:  # pragma: no cover
    sys.exit("Pillow is missing. Install it:  python3 -m pip install pillow")

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "brand" / "cwoffee-source.png"
TAURI_ICONS = ROOT / "crates" / "tauri-app" / "icons"
UI_ICON = ROOT / "ui" / "public" / "icon.png"

# Square sizes for the tauri icons directory.
SQUARES = [16, 32, 48, 64, 128, 256, 512]
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]

# Fraction of the artwork size to leave transparent around it. App icons want air,
# and a hand-drawn mark cropped edge-to-edge reads as a sticker pasted onto the
# launcher tile. Small sizes get almost none: at 16px the artwork is the whole
# point, and margin there is lost detail.
MARGIN_LARGE = 0.07
MARGIN_SMALL = 0.01
SMALL_BELOW = 64


def crop_art(img: Image.Image) -> tuple[Image.Image, int]:
    """Trim the transparent border; return the artwork and its longest edge.

    The source artwork is not square - the cup is wider than it is tall - and the
    padding in the original file is not centred on the art. Cropping first is what
    makes the mark sit in the middle of the tile instead of hanging low in it.
    """
    bbox = img.getchannel("A").getbbox()
    if bbox is None:
        sys.exit(f"{SRC} is completely transparent - there is no artwork to use")
    art = img.crop(bbox)
    # The longest edge gets the margin, so the plate is art * (1 + 2*margin).
    return art, max(art.size)


def plate_at(art: Image.Image, side: int, size: int) -> Image.Image:
    margin = MARGIN_SMALL if size < SMALL_BELOW else MARGIN_LARGE
    plate = int(round(side * (1.0 + 2 * margin)))
    canvas = Image.new("RGBA", (plate, plate), (0, 0, 0, 0))
    ox = (plate - art.width) // 2
    oy = (plate - art.height) // 2
    canvas.paste(art, (ox, oy), art)
    canvas = canvas.resize((size, size), Image.LANCZOS)
    if size <= 64:
        # Downscaling thin hand-drawn strokes to 16-48px makes them grey and soft.
        # A light unsharp on the alpha edges is what keeps the face readable at the
        # size the dock actually draws it at.
        canvas = canvas.filter(ImageFilter.UnsharpMask(radius=0.6, percent=80, threshold=2))
    return canvas


def main() -> int:
    if not SRC.exists():
        sys.exit(f"missing source icon: {SRC}")
    src = Image.open(SRC).convert("RGBA")
    art, side = crop_art(src)
    print(f"source {SRC.relative_to(ROOT)}: {src.width}x{src.height}, "
          f"artwork {art.width}x{art.height}")

    TAURI_ICONS.mkdir(parents=True, exist_ok=True)
    UI_ICON.parent.mkdir(parents=True, exist_ok=True)

    written: list[tuple[str, int, int]] = []

    def emit(path: Path, img: Image.Image) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        img.save(path, "PNG", optimize=True)
        written.append((str(path.relative_to(ROOT)), img.width, path.stat().st_size))

    for n in SQUARES:
        emit(TAURI_ICONS / f"{n}x{n}.png", plate_at(art, side, n))

    # tauri.conf.json references icon.png; 512 keeps it usable as the largest tile.
    big = plate_at(art, side, 512)
    emit(TAURI_ICONS / "icon.png", big)

    # Windows: one .ico carrying the ladder, so a 16px taskbar button and a 256px
    # explorer tile both get a native resolution rather than a resample.
    ico = TAURI_ICONS / "icon.ico"
    plate_at(art, side, 256).save(ico, "ICO", sizes=ICO_SIZES)
    written.append((str(ico.relative_to(ROOT)), 0, ico.stat().st_size))

    # The in-app mark is drawn at ~20px in the titlebar and ~120px on the empty
    # screen, so 256 is the smallest file that stays crisp in both.
    emit(UI_ICON, plate_at(art, side, 256))

    for name, w, size in written:
        print(f"  {name:<40} {f'{w}px' if w else 'multi':>8} {size / 1024:7.1f} KB")
    print(f"\n{len(written)} files written from {SRC.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
