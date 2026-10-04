#!/usr/bin/env python3
"""Generate the Cwoffee app icons: a hand-drawn coffee cup with a face.

The one hard constraint carried over from the old shell design: the face has to
still read at 16px. That is why it is two dots and an arc, and why the steam is
drawn thin enough to disappear gracefully rather than turn into mud.

usage: python3 scripts/make-icon.py
"""
from PIL import Image, ImageDraw
import math
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "tauri-app", "icons")
os.makedirs(OUT, exist_ok=True)

S = 512


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def cup_outline(cx, top_y, bot_y, half_top, half_bot, r):
    """A tapered cup with a genuinely rounded base.

    The corners are swept as explicit arcs about their own centres. The first
    attempt here approximated the base with a half-ellipse offset from the side
    walls, which is how a cup grew a tongue.
    """
    pts = []
    # rim, left to right, bowing up slightly so it reads as an opening
    steps = 20
    for i in range(steps + 1):
        t = i / steps
        x = (cx - half_top) + 2 * half_top * t
        y = top_y - 7 * math.sin(math.pi * t)
        pts.append((x, y))
    # right wall down to the corner start
    pts.append((cx + half_bot, bot_y - r))
    # bottom-right corner: right side -> bottom
    cr = (cx + half_bot - r, bot_y - r)
    for i in range(1, 9):
        a = (math.pi / 2) * (i / 8)
        pts.append((cr[0] + r * math.cos(a), cr[1] + r * math.sin(a)))
    # base, right to left
    pts.append((cx - half_bot + r, bot_y))
    # bottom-left corner: bottom -> left side
    cl = (cx - half_bot + r, bot_y - r)
    for i in range(1, 9):
        a = math.pi / 2 + (math.pi / 2) * (i / 8)
        pts.append((cl[0] + r * math.cos(a), cl[1] + r * math.sin(a)))
    # left wall back up to the rim
    pts.append((cx - half_top, top_y))
    return pts


def wavy(d, x, y0, y1, amp, k, color, width):
    """Steam: a sine stroke, thin enough to vanish at icon sizes."""
    steps = 26
    pts = []
    for i in range(steps + 1):
        t = i / steps
        y = y0 + (y1 - y0) * t
        x_i = x + amp * math.sin(t * math.pi * 1.9) * (1.0 - 0.45 * t)
        pts.append((x_i, y))
    d.line(pts, fill=color, width=max(1, int(width * k)), joint="curve")


def make(size: int) -> Image.Image:
    s = size
    k = s / S
    # Below ~64px the cup is about 9px wide. Steam, crema, saucer and cheeks at
    # that size do not degrade, they merge into a brown blob - and the blob was
    # the first version of this file. Small sizes get a simplified drawing with
    # proportionally larger features instead of a shrunken detailed one.
    detail = size >= 64
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # --- warm latte wash behind the cup -------------------------------------
    inner = (250, 240, 224)
    outer = (214, 186, 150)
    rings = 26
    for i in range(rings, 0, -1):
        t = i / rings
        col = lerp(inner, outer, t) + (int(90 + 90 * t),)
        r = (150 + 58 * t) * k
        d.ellipse(
            [(256 - r) * k, (258 - r * 0.94) * k, (256 + r) * k, (258 + r * 0.94) * k],
            fill=col,
        )

    body = (252, 246, 235)
    edge = (122, 92, 62)
    foam = (236, 216, 186)
    steam = (168, 135, 92)

    if detail:
        hx, hy, hr = 350, 272, 62
        top_y, bot_y, half_top, half_bot, corner = 192, 398, 102, 74, 28
        lw = max(1, int(6 * k))
        eye_r, eye_y, eye_dx, mouth = 9, 288, 32, [222, 302, 290, 350]
    else:
        # fill the frame, and make the face a third of the cup's width
        hx, hy, hr = 372, 268, 86
        top_y, bot_y, half_top, half_bot, corner = 150, 424, 148, 112, 34
        lw = max(1, int(11 * k))
        eye_r, eye_y, eye_dx, mouth = 26, 300, 62, [182, 296, 330, 392]

    # --- steam first, so the cup overlaps it cleanly -------------------------
    if detail:
        wavy(d, 222, 118, 186, 16, k, steam + (215,), 6)
        wavy(d, 288, 108, 182, 14, k, steam + (185,), 5)

    # --- handle: two arcs with the body colour between them ------------------
    for box, col, w in (
        ([(hx - hr) * k, (hy - hr) * k, (hx + hr) * k, (hy + hr) * k], edge, lw),
        ([(hx - hr + 26) * k, (hy - hr + 26) * k, (hx + hr - 26) * k, (hy + hr - 26) * k], edge, lw),
        ([(hx - hr + 13) * k, (hy - hr + 13) * k, (hx + hr - 13) * k, (hy + hr - 13) * k], body, max(1, int(26 * k))),
    ):
        d.arc(box, -78, 78, fill=col, width=w)

    # --- saucer ---------------------------------------------------------------
    if detail:
        d.ellipse([142 * k, 386 * k, 370 * k, 424 * k], fill=foam, outline=edge, width=max(1, int(5 * k)))

    # --- cup body -------------------------------------------------------------
    pts = cup_outline(cx=256, top_y=top_y, bot_y=bot_y, half_top=half_top, half_bot=half_bot, r=corner)
    poly = [(x * k, y * k) for x, y in pts]
    d.polygon(poly, fill=body)
    d.line(poly + [poly[0]], fill=edge, width=lw, joint="curve")

    # --- crema line just under the rim ---------------------------------------
    if detail:
        d.arc([160 * k, 172 * k, 352 * k, 240 * k], start=14, end=166, fill=foam, width=max(1, int(15 * k)))
        d.arc([160 * k, 172 * k, 352 * k, 240 * k], start=14, end=166, fill=edge, width=max(1, int(2 * k)))

    # --- face ----------------------------------------------------------------
    eye = (74, 58, 44)
    er = eye_r * k
    for cx in (256 - eye_dx, 256 + eye_dx):
        d.ellipse(
            [(cx - eye_r) * k, (eye_y - eye_r) * k, (cx + eye_r) * k, (eye_y + eye_r) * k],
            fill=eye,
        )
    d.arc([mouth[0] * k, mouth[1] * k, mouth[2] * k, mouth[3] * k],
          start=18, end=162, fill=eye, width=max(1, int((6 if detail else 11) * k)))

    # cheeks: soft, and dropped entirely at small sizes where they would blur
    if size >= 128:
        blush = (226, 176, 150, 150)
        for cx in (196, 316):
            d.ellipse([(cx - 15) * k, (300 - 9) * k, (cx + 15) * k, (300 + 9) * k], fill=blush)

    return img.resize((size, size), Image.LANCZOS) if size != S else img


def main():
    sizes = [16, 32, 48, 64, 128, 256, 512]
    for n in sizes:
        make(n).save(os.path.join(OUT, f"{n}x{n}.png"))
    big = make(512)
    big.save(os.path.join(OUT, "icon.png"))
    big.save(
        os.path.join(OUT, "icon.ico"),
        sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print(f"wrote {len(sizes) + 2} icons to {OUT}")

    # The titlebar renders ui/public/icon.png. This script used to write only
    # the bundle icons, so the titlebar kept showing the previous logo forever -
    # the app was renamed and rebranded and the little mark beside the title was
    # still the old one.
    public = os.path.join(os.path.dirname(__file__), "..", "ui", "public")
    os.makedirs(public, exist_ok=True)
    make(64).save(os.path.join(public, "icon.png"))
    print(f"wrote ui/public/icon.png")


if __name__ == "__main__":
    main()
