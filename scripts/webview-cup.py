#!/usr/bin/env python3
"""Measure the opening screen's cup in the app's own engine, on a timer.

Why this exists: the cup's pause is not created by the page. The shipped file carries a baked hold,
and the app then re-arms the picture every lap by rewriting that duration in a copy of its own
bytes (see `ui/src/lib/anipause.ts`), drawing a fresh 4-8s from `IDLE_HOLD`. Nothing in CSS or JS
can hold an `<img>` of an animated image, so the only way to know what pause the user actually
sees is to sample the pixels a renderer produces. `get_snapshot` on a GLib timer does exactly that.

What it reports:
  * every quiet stretch of the cup's own face, not just the longest one. The pause is deliberately
    random per lap, so one number would hide the thing being measured; the spread is the result.
    A rectangle inside the artwork, so a quiet stretch there is the picture holding still;
  * a filmstrip of the cup and the space above the rim, for judging by eye that nothing is drawn
    over the picture. There used to be an animated SVG wisp of steam there, and a strip like this
    is how it was caught: the picture looped and something else looped over it.

The two rectangles are page coordinates, so they have to be re-pointed if the opening screen's
layout moves. `scripts/webview-rect.py` prints the element rects to aim them with. Sample long
enough for several laps (each is the 1.58s of movement plus a 4-8s rest, so 30s sees four or five):

    python3 scripts/webview-cup.py http://127.0.0.1:8199/ 32 200

Serve the built UI first, e.g. `python3 -m http.server 8199 -d ui/dist` - the browser build of
the UI boots with no sessions, which is the state the opening screen exists for. No endpoint,
no session and no user config is touched.
"""
import io
import sys
import time
import hashlib

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import Gtk, WebKit2, GLib  # noqa: E402
import cairo  # noqa: E402
from PIL import Image  # noqa: E402

url = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8199/"
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 9.0
step = int(sys.argv[3]) if len(sys.argv) > 3 else 200
# device px (the snapshot is at the page's dpr), as (x, y, w, h)
FACE = (1380, 400, 110, 60)     # the face: inside the artwork
WHOLE = (1300, 200, 330, 380)   # the cup, plus the space above the rim where nothing should be

win = Gtk.Window()
win.set_default_size(1180, 760)
wv = WebKit2.WebView()
win.add(wv)
win.show_all()
wv.load_uri(url)

frames, pending, t0 = [], [False], time.monotonic()


def crop(surf, rect):
    x, y, w, h = rect
    out = cairo.ImageSurface(cairo.FORMAT_ARGB32, w, h)
    cr = cairo.Context(out)
    cr.set_source_surface(surf, -x, -y)
    cr.paint()
    return out


def on_snap(view, res, _user):
    pending[0] = False
    try:
        surf = view.get_snapshot_finish(res)
    except Exception as e:  # a snapshot can fail on a blank page; that is a measurement, not a crash
        print("snapshot failed:", e, flush=True)
        return
    face = io.BytesIO()
    crop(surf, FACE).write_to_png(face)
    whole = io.BytesIO()
    crop(surf, WHOLE).write_to_png(whole)
    frames.append((round((time.monotonic() - t0) * 1000), hashlib.sha1(face.getvalue()).hexdigest()[:10], whole.getvalue()))


def snap():
    if pending[0]:
        return
    pending[0] = True
    wv.get_snapshot(WebKit2.SnapshotRegion.FULL_DOCUMENT, WebKit2.SnapshotOptions(0), None, on_snap, None)


def stop():
    if not frames:
        print("no frames: the page never painted", flush=True)
        Gtk.main_quit()
        return False
    changes = [frames[0][0]]
    for i in range(1, len(frames)):
        if frames[i][1] != frames[i - 1][1]:
            changes.append(frames[i][0])
    print(f"frames sampled: {len(frames)}  face-region changes: {len(changes)}", flush=True)
    if len(changes) > 1:
        gaps = [(changes[i] - changes[i - 1], changes[i - 1]) for i in range(1, len(changes))]
        quiet = max(gaps)
        print(f"longest the cup's own face stayed bit-identical: {quiet[0]}ms (from t={quiet[1]}ms)", flush=True)
        # Each lap is a different length now: the pause is drawn per cycle from IDLE_HOLD, so a
        # single number would hide the thing being measured. Report the rests, and their spread.
        rests = sorted(g[0] for g in gaps if g[0] >= 2000)
        if rests:
            print(f"rests between laps (quiet stretches >=2s): {rests}ms", flush=True)
            print(
                f"spread over {len(rests)} laps: shortest {rests[0]}ms, longest {rests[-1]}ms, "
                f"median {rests[len(rests) // 2]}ms",
                flush=True,
            )
        print("face change times (ms):", changes, flush=True)
    # a hold that runs to the end of the sampling window has no closing change, so the gap after
    # the last change counts too - as a floor, because sampling may have stopped mid-hold
    if changes and frames[-1][0] > changes[-1]:
        tail = frames[-1][0] - changes[-1]
        note = "" if tail <= 1000 else "  (floor: sampling ended, so the hold may run longer)"
        print(f"quiet from the last change to the last sample: {tail}ms (from t={changes[-1]}ms){note}", flush=True)
    tiles = [Image.open(io.BytesIO(f[2])).convert("RGB") for f in frames[:24]]
    cols, w, h = 6, tiles[0].width, tiles[0].height
    rows = (len(tiles) + cols - 1) // cols
    sheet = Image.new("RGB", (cols * w, rows * h), (253, 252, 249))
    for k, t in enumerate(tiles):
        sheet.paste(t, ((k % cols) * w, (k // cols) * h))
    sheet.save("/tmp/cup-filmstrip.png")
    print(f"filmstrip: {len(tiles)} frames, every {step}ms -> /tmp/cup-filmstrip.png", flush=True)
    Gtk.main_quit()
    return False


GLib.timeout_add(step, lambda: (snap(), time.monotonic() - t0 < secs)[1])
GLib.timeout_add(int(secs * 1000) + 2500, stop)
Gtk.main()
