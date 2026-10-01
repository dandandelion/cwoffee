#!/usr/bin/env python3
"""Generate placeholder Cowry app icons.

This is a PLACEHOLDER so the Tauri bundle step has real assets and the dev
window has an identity. The hand-drawn watercolour cowry-shell face (PLAN.md
§11) replaces this later — the one hard constraint carried forward is that the
face must still read at 16px.

usage: python3 scripts/make-icon.py
"""
from PIL import Image, ImageDraw
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "tauri-app", "icons")
os.makedirs(OUT, exist_ok=True)

S = 512


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def make(size: int) -> Image.Image:
    s = size
    k = s / S
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # --- soft blue wash (concentric ellipses standing in for a watercolour) ---
    inner = (214, 232, 248)
    outer = (168, 205, 236)
    rings = 26
    for i in range(rings, 0, -1):
        t = i / rings
        col = lerp(inner, outer, t) + (int(90 + 90 * t),)
        r = (150 + 60 * t) * k
        d.ellipse(
            [(256 - r) * k, (250 - r * 0.92) * k, (256 + r) * k, (250 + r * 0.92) * k],
            fill=col,
        )

    # --- shell body: cream oval, slightly asymmetric ---
    body = (247, 238, 219)
    edge = (176, 148, 106)
    d.ellipse([170 * k, 150 * k, 342 * k, 386 * k], fill=body, outline=edge, width=max(1, int(3 * k)))

    # --- the cowrie's toothed ridge ---
    ridge = (196, 168, 124)
    d.line([(256 * k, 172 * k), (256 * k, 364 * k)], fill=ridge, width=max(1, int(10 * k)))
    teeth = 9
    for i in range(teeth):
        y = 180 + i * 21
        d.line([(244 * k, y * k), (268 * k, y * k)], fill=edge, width=max(1, int(2 * k)))

    # --- face: must read at 16px, so keep it to two dots and an arc ---
    eye = (74, 60, 48)
    er = 9 * k
    for cx in (228, 284):
        d.ellipse([(cx - 9) * k, (222 - 9) * k, (cx + 9) * k, (222 + 9) * k], fill=eye)
    d.arc([226 * k, 246 * k, 286 * k, 292 * k], start=20, end=160, fill=eye, width=max(1, int(5 * k)))

    return img.resize((size, size), Image.LANCZOS) if size != S else img


def main():
    sizes = [16, 32, 48, 64, 128, 256, 512]
    for n in sizes:
        make(n).save(os.path.join(OUT, f"{n}x{n}.png"))
    big = make(512)
    big.save(os.path.join(OUT, "icon.png"))
    # .ico / .icns for the other platforms
    big.save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    print(f"wrote {len(sizes) + 2} icons to {OUT}")


if __name__ == "__main__":
    main()
