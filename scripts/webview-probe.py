#!/usr/bin/env python3
"""Open a URL in the same WebKitGTK build the desktop app links, print what the page
posts to its host bridge, and exit.

Why not a browser: the report this exists for is about scroll cost in the app's webview.
`ldd target/release/cwoffee-app` says libwebkit2gtk-4.1, and this script uses exactly
that library, so the numbers are the engine's, not some other engine's.

The window is shown on purpose. An occluded or unmapped webview stops animating
(rAF goes silent), which is how one probe run once reported a 7.9 s stall that was
a locked screen — measuring window management instead of the page.
"""
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2  # noqa: E402

url = sys.argv[1]
timeout = int(sys.argv[2]) if len(sys.argv) > 2 else 90

done = {"quit": False}


def report(msg):
    if done["quit"]:
        return
    done["quit"] = True
    print(msg, flush=True)
    Gtk.main_quit()


def on_message(_ucm, result):
    try:
        value = result.get_js_value().to_string()
    except Exception as exc:  # noqa: BLE001
        report(f"PROBE: unreadable host message: {exc}")
        return
    # Progress lines stream past; the report is the JSON object (or a PROBE failure).
    head = value.lstrip()
    if head.startswith("{") or head.startswith("PROBE"):
        report(value)
    else:
        print(f"[page] {value}", file=sys.stderr, flush=True)


def on_failed(_view, event, uri, error, *args):
    if event == WebKit2.LoadEvent.FAILED:
        print(f"[webview-probe] load failed for {uri}: {error.get_message()}", file=sys.stderr, flush=True)


win = Gtk.Window(title="cwoffee scroll probe")
win.set_default_size(1180, 880)
win.set_resizable(True)

ucm = WebKit2.UserContentManager()
ucm.register_script_message_handler("probe")
ucm.connect("script-message-received::probe", on_message)

view = WebKit2.WebView.new_with_user_content_manager(ucm)
s = view.get_settings()
s.set_property("enable-developer-extras", True)
try:
    # The name and existence of this setting have changed across WebKitGTK releases; the
    # probe posts to the host bridge anyway, so console capture is a convenience, not a
    # dependency. Try both spellings GObject accepts.
    s.set_property("enable-write-console-messages-to-stdout", True)
except Exception:  # noqa: BLE001
    pass
view.connect("load-failed", on_failed)

win.add(view)
win.connect("destroy", lambda *a: report("PROBE: window closed before the page reported"))
win.show_all()
view.load_uri(url)

def on_timeout():
    report(f"PROBE TIMEOUT after {timeout}s (the page never posted to its host bridge)")
    return False

GLib.timeout_add_seconds(timeout, on_timeout)
Gtk.main()
