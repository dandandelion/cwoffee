#!/usr/bin/env python3
"""Resize an animated WebP mark without letting the frame table drift.

The app ships artwork sized for the CSS box it will be drawn in at this display's DPR (a 20px
mark on a DPR-2 screen wants a 40px file), while the artwork itself is kept at its authored
size under `assets/` so a later size decision does not need the artist's file again. Resizing
is a pixel operation, and Pillow does that losslessly. What it does not do reliably is carry
the frame table back out: the durations an encoder wrote come back through `ImageSequence` as
`None` in this Pillow, so a build that re-times the animation from whatever it has to hand
silently changes how fast the drawing moves — and a mark that runs six times too slowly is
invisible in a screenshot and obvious on a bench.

So the durations are read from the container here. A frame's duration sits at offset 12 of the
ANMF payload, with the frame's image chunk starting at +16: that is the offset and the
structural check `webp-pause.py` records (verified there against a file libwebp wrote with
known durations, and against what ImageMagick and WebKitGTK play). Read offset 6 instead and
this artwork answers 198 where its own table says 33 — a six-fold drift, which is exactly the
mistake this script exists to make impossible. Both halves are printed, and the written file is
re-read: the check is the point.

usage: mark-resize.py SOURCE.webp LOOP_DEST.webp SIZE STILL_DEST.webp

The still is written from the same resized frames rather than by carving frame 0 out of the
loop with webp-still.py: this build stores delta frames (a frame's image data covers 23x28 of
the 40-unit canvas at an offset), and copying those bytes into a full-canvas container produces
a file decoders call corrupt. One source, one resample, one size decision for both files.
"""
import struct
import subprocess
import sys

from PIL import Image, ImageSequence

if len(sys.argv) != 5:
    sys.exit("usage: mark-resize.py SOURCE.webp LOOP_DEST.webp SIZE STILL_DEST.webp")
src, dst, size, still = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]


def walk(buf):
    """Payload start of every ANMF chunk, by walking the container.

    A byte scan for the fourcc is not enough: compressed image data holds byte sequences that
    spell ANMF, and an early version of this script read a frame duration out of one of them
    and reported a lap of 587,748,766ms.
    """
    found, i = [], 12
    while i + 8 <= len(buf):
        fourcc = bytes(buf[i : i + 4])
        chunk = struct.unpack("<I", buf[i + 4 : i + 8])[0]
        if fourcc == b"ANMF":
            found.append(i + 8)
        i += 8 + chunk + (chunk & 1)
    return found


def dur_at(buf, off):
    return struct.unpack("<I", buf[off + 12 : off + 15] + b"\0")[0]


def image_fourcc(buf, off):
    return bytes(buf[off + 16 : off + 20])


def table(buf, what):
    """The frame table of an animated WebP, or a refusal naming what was wrong."""
    if buf[0:4] != b"RIFF" or buf[8:12] != b"WEBP":
        sys.exit(f"{what}: not a WebP file")
    offs = walk(buf)
    if not offs:
        sys.exit(f"{what}: no ANMF chunks, so not an animation")
    for off in offs:
        if image_fourcc(buf, off) not in (b"ALPH", b"VP8 ", b"VP8L"):
            sys.exit(
                f"{what}: unexpected ANMF layout at {off}: image data starts with "
                f"{image_fourcc(buf, off)!r}, so this file's frame table is not the layout "
                "this script reads — find the offset before resizing it"
            )
    return offs, [dur_at(buf, off) for off in offs]


b = open(src, "rb").read()
offs, durs = table(b, "source")
anim = b.find(b"ANIM")
loop = struct.unpack("<I", b[anim + 8 : anim + 12])[0] if anim >= 0 else 0
print(f"read: {len(durs)} frame(s), durations {sorted(set(durs))}, lap {sum(durs)}ms, loop {hex(loop)}")

im = Image.open(src)
out = [fr.convert("RGBA").resize((size, size), Image.LANCZOS) for fr in ImageSequence.Iterator(im)]
# disposal=2 (clear the canvas) because every resized frame is full-canvas: the source stores
# delta frames and ImageSequence hands back the composited canvas, which must not accumulate.
out[0].save(dst, save_all=True, append_images=out[1:], duration=durs, loop=0, lossless=True, disposal=2)

out[0].save(still, format="WEBP", lossless=True)

_, wdur = table(open(dst, "rb").read(), "written")
if wdur != durs:
    sys.exit(f"frame table drifted: asked for {durs[:4]}… ({sum(durs)}ms), the file says {wdur[:4]}… ({sum(wdur)}ms)")
print(f"wrote: {dst} {size}x{size}, {len(wdur)} frame(s), lap {sum(wdur)}ms, {len(open(dst,'rb').read())} bytes")

# Fidelity: the shipped pixels against the source resized at the same size. Not a comparison
# against the source's own bytes — a resize may differ from what the artist's encoder did, but
# not by more than the resampler's rounding.
worst, worst_i = 0.0, -1
sa = list(ImageSequence.Iterator(Image.open(src)))
sb = list(ImageSequence.Iterator(Image.open(dst)))
if len(sb) != len(durs):
    sys.exit(f"frame count drifted: {len(durs)} read, {len(sb)} written")
for n, (f1, f2) in enumerate(zip(sa, sb)):
    p1 = f1.convert("RGBA").resize((size, size), Image.LANCZOS).load()
    p2 = f2.convert("RGBA").load()
    s = 0
    for y in range(size):
        for x in range(size):
            for k in range(4):
                s += abs(p1[x, y][k] - p2[x, y][k])
    m = s / (size * size * 4)
    if m > worst:
        worst, worst_i = m, n
print(f"pixel check: worst per-frame mean channel error {worst:.5f}/255 (frame {worst_i})")

# A WebP that only Pillow's own reader likes is not shipped: the app decodes these in WebKitGTK,
# and the only still in this pair is what `prefers-reduced-motion` shows. `identify` is a second,
# independent decoder, and it is the one that caught a container this script had corrupted.
for f in (dst, still):
    r = subprocess.run(["identify", f], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"{f} is not readable by an independent decoder: {r.stderr.strip()}")
    print(f"decoder check: {f} -> {r.stdout.strip().splitlines()[0]}")
