#!/usr/bin/env python3
"""Hold the desktop's own idle blanking off for the length of a test run, then put it back.

Why this exists at all: the page-check pass in the desktop app paces itself on
requestAnimationFrame. When the session goes idle the screen blanks, the compositor stops
painting that window, rAF goes silent, and every frame-paced check falls back to its timeout
floor - a run that is doing everything right looks like one that hung, and the checks that
need a real paint skip instead of asserting.

Why this script does nothing but change one setting: the first version of it was an X client.
It faked a pointer move through XTEST, reset the X screen saver, and raised the app window. On
this machine that was wrong twice over. The session is Wayland, so `:0` is Xwayland, and an X
client injecting input makes mutter ask the desktop for a virtual input device through the
Remote Desktop portal - which answers by putting a "Remote Desktop / Allow Remote Interaction"
dialog in front of the user, once per keeper pass, stealing focus from the very window the
script was trying to keep painted. And since nobody consents to that, the injected input never
arrived: the keeper was generating a popup and achieving nothing. Raising another client's
window is not available to a script on Wayland either.

So: the desktop's own switch, through the desktop's own API, with the same restore discipline
as before (a state file, so a killed keeper's setting does not outlive it). `idle-delay=0` is
what "never blank" means to GNOME; nothing here touches DPMS, the lock screen, or input.

Run it, and if it says there is nothing to keep, that is the answer for this box: the session
already never blanks, and no helper needs to run.
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time

SCHEMA = "org.gnome.desktop.session"
KEY = "idle-delay"
# GNOME Shell's own blank/lock monitor, spoken to over the session bus. This is the desktop's API,
# not an X client: no input is injected, no window is raised, and mutter asks for no portal consent.
SS_DEST = "org.gnome.ScreenSaver"
SS_PATH = "/org/gnome/ScreenSaver"
SS_IFACE = "org.gnome.ScreenSaver"


def gsettings(*argv):
    return subprocess.run(["gsettings", *argv], capture_output=True, text=True)


def gdbus_ss(method, *extra):
    """A call on GNOME Shell's screensaver interface, or None if this shell does not have one."""
    try:
        out = subprocess.run(
            ["gdbus", "call", "--session", "--dest", SS_DEST, "--object-path", SS_PATH, "--method", f"{SS_IFACE}.{method}", *extra],
            capture_output=True,
            text=True,
        )
    except OSError:
        return None
    if out.returncode != 0:
        return None
    return out.stdout.strip()


def saver_active():
    """True / False if the shell will say, None if it will not. A blank screen is the thing that
    actually kills a page-check run - the compositor stops painting the window, requestAnimationFrame
    goes silent, and a run that is behaving perfectly looks like one that hung - and `idle-delay=0`
    alone does not always prevent it on this shell."""
    out = gdbus_ss("GetActive")
    if out is None:
        return None
    return "true" in out.lower()


def get_delay():
    """The session's idle-delay in seconds, or None if this desktop has no such setting."""
    out = gsettings("get", SCHEMA, KEY)
    if out.returncode != 0:
        return None
    # printed as `uint32 300`, or as `0` on a desktop that stores it plain
    for tok in out.stdout.split():
        if tok.isdigit():
            return int(tok)
    return None


def keepers():
    """Other running copies of this script, by pid.

    Matching is done on argv rather than on a command-line pattern: the shell that launched
    this script keeps the script's own text in its `bash -c` wrapper, and a `pgrep -f` finds
    that too. And any argument may be the script (`python3 -u scripts/idlekeep.py`), which is
    how two keepers once ran at once.
    """
    out = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or int(entry) == os.getpid():
            continue
        try:
            argv = open(f"/proc/{entry}/cmdline", "rb").read().split(b"\0")
        except OSError:
            continue
        if len(argv) < 2:
            continue
        exe = argv[0].decode(errors="replace")
        if "python" in exe and any(a.decode(errors="replace").endswith("scripts/idlekeep.py") for a in argv[1:]):
            out.append(int(entry))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--force", action="store_true", help="start even if another copy is running (not advised)")
    ap.add_argument("--duration", type=float, default=0, help="exit by itself after this many seconds")
    ap.add_argument("--check", action="store_true", help="report the current state and exit")
    args = ap.parse_args()

    have = get_delay()
    if have is None:
        sys.exit(f"this desktop has no {SCHEMA} {KEY}; nothing to keep, and no way to blank it from here")
    if args.check:
        others = keepers()
        act = saver_active()
        print(
            f"{KEY}={have}s; screensaver active: {'unknown' if act is None else act}; keepers running: {others or 'none'}"
        )
        return 0

    others = keepers()
    if others and not args.force:
        sys.exit(f"another idlekeep.py is already running (pid {' '.join(map(str, others))}); end it first, or pass --force")

    # The pid is printed so the caller ends exactly this process rather than pattern-killing by
    # name - a pattern would take out the launching shell too, which has this script's text in it.
    print(f"pid {os.getpid()}", flush=True)

    state = f"/tmp/idlekeep-saved-{os.getuid()}.json"
    # A keeper that is SIGKILLed (or that loses the machine) never runs its restore, which is how
    # a session can be left quietly never-blanking. Whatever a dead keeper took down goes back
    # before this one does anything of its own. A file in /tmp keyed by uid: a keeper is a
    # per-session helper, not application state.
    if os.path.exists(state):
        try:
            left = int(json.load(open(state)))
            gsettings("set", SCHEMA, KEY, str(left))
            print(f"restoring {KEY}={left}s that a killed keeper left behind", flush=True)
            # The value the dead keeper took down is the real original, so it is also what this
            # keeper owes the session when it exits.
            have = left
        except Exception as e:
            print(f"could not read {state}: {e}", flush=True)
        os.remove(state)

    if have == 0:
        print(f"{KEY} is already 0: this session never blanks, so there is nothing to keep and no reason to run", flush=True)
        return 0

    json.dump(have, open(state, "w"))
    gsettings("set", SCHEMA, KEY, "0")
    print(f"blanking off for this run ({KEY} was {have}s)", flush=True)

    def restore(*_):
        gsettings("set", SCHEMA, KEY, str(have))
        if os.path.exists(state):
            os.remove(state)
        print(f"blanking restored ({KEY}={have}s)", flush=True)
        sys.exit(0)

    signal.signal(signal.SIGTERM, restore)
    signal.signal(signal.SIGINT, restore)

    if args.duration:
        signal.signal(signal.SIGALRM, restore)
        signal.alarm(int(args.duration))

    # Not `signal.pause()`: the setting alone is not enough on this shell. A run this morning set
    # idle-delay=0 and still ended with 96 frame-clock stalls and 17 skipped geometry checks, and the
    # shell answered `GetActive` with `true` afterwards - the blank monitor had engaged anyway. So the
    # keeper keeps the setting down *and* asks the shell's own screensaver interface to stand down, once
    # every 20 seconds. Both are desktop API calls: no input is injected, no window is raised, and no
    # portal consent is requested. `time.sleep` rather than `pause`, because a signal handler still
    # interrupts sleep, so ending this process by its pid stays immediate.
    LAP = 20
    said_no_api = False
    while True:
        try:
            act = saver_active()
            if act is True:
                gdbus_ss("SetActive", "false")
                after = saver_active()
                print(
                    "screensaver was active; asked the shell to stand down (now "
                    f"{'unknown' if after is None else after})",
                    flush=True,
                )
            if act is None and not said_no_api:
                print("this shell exposes no org.gnome.ScreenSaver; keeping idle-delay down only", flush=True)
                said_no_api = True
            # Something can put idle-delay back (a settings daemon restart, a policy reload). The
            # keeper's promise covers the whole run, so it re-asserts instead of trusting one write.
            if get_delay() != 0:
                gsettings("set", SCHEMA, KEY, "0")
                print(f"{KEY} was put back; set to 0 again", flush=True)
        except Exception as e:  # a keeper that dies takes the blanking with it; a noisy one survives
            print(f"keeper lap failed: {e}", flush=True)
        time.sleep(LAP)


if __name__ == "__main__":
    main()
