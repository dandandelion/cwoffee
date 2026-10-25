#!/usr/bin/env python3
"""Pull frame 0 out of an animated WebP as a single-frame WebP, without re-encoding.

The reduced-motion version of an animated picture is the same picture with the motion taken
out. A media query can stop a CSS keyframe but it cannot pause a raster loop, so the app
ships a second file for that case (see `idleArt` in ui/src/App.svelte).

The ANMF chunk holds a frame's image data as ordinary `ALPH` + `VP8 `/`VP8L` chunks, so the
still is those same bytes in a shorter container: no decode, no second generation of
artefacts, no quality decision to argue about.

One thing the container insists on: a file with an `ALPH` chunk must be in the *extended*
format, so a `VP8X` chunk declaring the canvas and the alpha flag has to come first. Copying
the frame's chunks on their own produces a file that ImageMagick calls `corrupt image`.
"""
import struct
import subprocess
import sys

src, dst = sys.argv[1], sys.argv[2]
b = open(src, "rb").read()
if b[0:4] != b"RIFF" or b[8:12] != b"WEBP":
    sys.exit("not a WebP file")

# canvas size, from the VP8X chunk the animation already carries
W = H = None
i, payload = 12, None
while i + 8 <= len(b):
    fourcc, size = bytes(b[i:i + 4]), struct.unpack("<I", b[i + 4:i + 8])[0]
    if fourcc == b"VP8X" and payload is None:
        # the chunk payload starts after its 8-byte header, and the first 4 bytes of it are flags
        dims = b[i + 12 : i + 18]
        W = int.from_bytes(dims[0:3], "little") + 1
        H = int.from_bytes(dims[3:6], "little") + 1
    if fourcc == b"ANMF" and payload is None:
        payload = b[i + 8 : i + 8 + size][16:]  # past X/Y/duration/flags: the frame's own chunks
    i += 8 + size + (size & 1)
if payload is None:
    sys.exit("no ANMF chunk")
if W is None:
    sys.exit("no VP8X chunk to read the canvas from")

body = b""
j = 0
while j + 8 <= len(payload):
    fourcc, size = bytes(payload[j:j + 4]), struct.unpack("<I", payload[j + 4:j + 8])[0]
    if fourcc not in (b"ALPH", b"VP8 ", b"VP8L"):
        break
    body += payload[j:j + 8 + size] + (b"\0" if size & 1 else b"")
    j += 8 + size + (size & 1)
if not body:
    sys.exit("no image chunks in the first frame")

# 0x10 is the alpha flag in VP8X's flag field
vp8x = b"VP8X" + struct.pack("<I", 10) + struct.pack("<I", 0x10)
vp8x += struct.pack("<I", W - 1)[:3] + struct.pack("<I", H - 1)[:3]
out = b"RIFF" + struct.pack("<I", 4 + len(vp8x) + len(body)) + b"WEBP" + vp8x + body
open(dst, "wb").write(out)

print(f"{src} -> {dst}: {len(out)} bytes (VP8X + {len(body)} bytes of the frame's own image data)")
print(subprocess.run(["identify", "-format", "%wx%h channels=%[channels] frames=%n\n", dst],
                     capture_output=True, text=True).stdout.strip())
