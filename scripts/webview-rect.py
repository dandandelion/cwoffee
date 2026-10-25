#!/usr/bin/env python3
"""Ask the running page where its elements are, in CSS px, in the app's own engine.

The page-check pass measures the same thing from inside the app; this measures it from
outside, which is what you want when a layout claim in a CSS comment has to be checked
against the real renderer, or when a switch driven by a media query has to be proven to
flip. `--reduced-motion` sets the GTK setting WebKitGTK reads for `prefers-reduced-motion`,
so the same page can be read under both motion settings.

    python3 scripts/webview-rect.py http://127.0.0.1:8199/
    python3 scripts/webview-rect.py http://127.0.0.1:8199/ --reduced-motion
    python3 scripts/webview-rect.py URL --expr "JSON.stringify(getComputedStyle(document.querySelector('.cupwrap')).width)"

The default expression reports the opening screen's cup: the box it is drawn in, the box the
picture itself paints, which of the two files the motion setting picked, and how many elements sit
inside the frame - the frame should hold the picture and nothing else, because an animated SVG wisp
of steam used to be drawn on top of the artwork and read as unrelated motion. Serving the built UI
is enough (`python3 -m http.server 8199 -d ui/dist`); the browser build boots with no sessions, so
the opening screen is what renders.
"""
import json
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import Gtk, WebKit2, GLib  # noqa: E402

DEFAULT_EXPR = """JSON.stringify((() => {
  const q = (s) => { const e = document.querySelector(s); if (!e) return null;
    const r = e.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; };
  const wrap = document.querySelector('.cupwrap');
  const cup = document.querySelector('.cupwrap .cup');
  return { wrap: q('.cupwrap'), img: q('.cupwrap .cup'),
           inFrame: wrap ? Array.from(wrap.children).map((c) => c.tagName.toLowerCase()) : null,
           src: cup && cup.getAttribute('src'),
           reduced: matchMedia('(prefers-reduced-motion: reduce)').matches,
           imgAnimation: cup && [getComputedStyle(cup).animationName, getComputedStyle(cup).animationDuration] };
})())"""

args = [a for a in sys.argv[1:] if not a.startswith("--")]
url = args[0] if args else "http://127.0.0.1:8199/"
expr = DEFAULT_EXPR
if "--expr" in sys.argv:
    expr = sys.argv[sys.argv.index("--expr") + 1]

if "--reduced-motion" in sys.argv:
    Gtk.Settings.get_default().set_property("gtk-enable-animations", False)
    print("reduced motion requested through the GTK setting", flush=True)

win = Gtk.Window()
win.set_default_size(1180, 760)
wv = WebKit2.WebView()
win.add(wv)
win.show_all()
wv.load_uri(url)


def done(view, res, _user):
    try:
        val = view.run_javascript_finish(res).get_js_value()
        try:
            txt = val.to_json()
        except Exception:
            txt = val.to_string()
        print(txt, flush=True)
        parsed = json.loads(txt)
        if isinstance(parsed, str):  # the expression returned a string containing JSON
            parsed = json.loads(parsed)
        print(json.dumps(parsed, indent=2, sort_keys=True), flush=True)
    except Exception as e:
        print("evaluation failed:", e, flush=True)
    Gtk.main_quit()


def fire():
    wv.run_javascript(expr, None, done, None)


GLib.timeout_add_seconds(4, fire)
GLib.timeout_add_seconds(25, lambda: (print("timeout waiting for the page"), Gtk.main_quit())[1])
Gtk.main()
