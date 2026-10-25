#!/usr/bin/env python3
"""Add a hold at the end of an animated WebP by editing its frame table.

The pause belongs in the asset, not in JavaScript: an <img> of an animated image cannot be
paused, restarted on a frame, or held by CSS. The WebP container stores the duration of
every frame in the ANMF chunk that precedes its image data, at the three bytes the
decoders read (offset 12 of the chunk payload — verified against a file written by
libwebp with known durations, and against what ImageMagick and WebKitGTK both play). So
the hold is a three-byte edit: the last frame's 42 ms becomes 42 ms + the pause. No frame
is re-encoded, so the artwork is bit-for-bit what the artist exported.
"""
import struct
import subprocess
import sys

src, dst, pause = sys.argv[1], sys.argv[2], int(sys.argv[3])
b = bytearray(open(src, "rb").read())
if b[0:4] != b"RIFF" or b[8:12] != b"WEBP":
    sys.exit("not a WebP file")

frames = []  # (payload_start, chunk_size)
i = 12
while i + 8 <= len(b):
    fourcc = bytes(b[i : i + 4])
    size = struct.unpack("<I", b[i + 4 : i + 8])[0]
    if fourcc == b"ANMF":
        frames.append((i + 8, size))
    i += 8 + size + (size & 1)
if not frames:
    sys.exit("no ANMF chunks: this is not an animation")

def read_dur(off):
    return struct.unpack("<I", b[off + 12 : off + 15] + b"\0")[0]

def image_fourcc(off):
    return bytes(b[off + 16 : off + 20])

for off, size in frames:
    tag = image_fourcc(off)
    if tag not in (b"ALPH", b"VP8 ", b"VP8L"):
        sys.exit(f"unexpected ANMF layout at {off}: image data starts with {tag!r}, not a WebP image chunk")

durs = [read_dur(off) for off, _ in frames]
off, size = frames[-1]
new = read_dur(off) + pause
if new > 0xFFFFFF:
    sys.exit("pause does not fit in the 24-bit duration field")
b[off + 12 : off + 15] = struct.pack("<I", new)[:3]
open(dst, "wb").write(bytes(b))

print(f"frames          : {len(frames)}")
print(f"unchanged frames: {sorted(set(durs))} ms")
print(f"last frame      : {durs[-1]} ms -> {new} ms   (bytes at chunk offset {off + 12})")
print(f"loop            : {sum(durs) / 1000:.3f} s -> {(sum(durs) + pause) / 1000:.3f} s")
print(f"bytes           : {len(open(src,'rb').read())} -> {len(b)}")
out = subprocess.run(["identify", "-format", "%T ", src], capture_output=True, text=True).stdout
out2 = subprocess.run(["identify", "-format", "%T ", dst], capture_output=True, text=True).stdout
print(f"identify (cs)   : {out.strip()}")
print(f"identify patched: {out2.strip()}")
