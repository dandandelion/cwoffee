#!/usr/bin/env python3
"""Give every self-test block a name, so a run can ask for a few of them.

    ./scripts/selftest.sh --list          what the groups are called
    ./scripts/selftest.sh cup code        only the groups whose name or label matches

A self-test block is a bare `{ ... }` at the top level of the page-side check chain, which is how
the chain is already written: each check opens its own scope so its `const` cannot collide with the
next one's. Turning `      {` into `      if (want('cup-travel', 'the cup does not travel ...')) {`
keeps the braces identical - it is the one edit that cannot break the chain's structure - and gives
the block two names: a short id to type, and the human label the section comment already carries,
which is what the filter matches against so you can type a word you remember rather than a name you
have to look up.

Blocks with no `// --- ...` comment above them are left alone on purpose: those are setup and
carrying state, and skipping them is how a targeted run ends up failing for the wrong reason.

Run it again after adding a check; it is idempotent (a guarded line no longer looks like a bare
block) and it prints every group it knows about.
"""

import re
import sys
from pathlib import Path

APP = Path(__file__).resolve().parent.parent / "ui" / "src" / "App.svelte"

MARKER = re.compile(r"^      // --- (.*)$")
BLOCK = re.compile(r"^      \{ *$")
GUARD = re.compile(r"^      if \(want\(")


def slug(text: str) -> str:
    words = re.sub(r"[^a-z0-9 ]", " ", text.lower()).split()
    keep = [w for w in words if len(w) > 2][:3]
    return "-".join(keep) or "block"


def clean(text: str) -> str:
    """A label goes into a JS string literal, so the punctuation that would end it comes out."""
    text = text.replace("`", " ").replace("'", " ").replace('"', " ")
    text = re.sub(r"[-–—]{2,}.*$", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text[:78]


def main() -> int:
    lines = APP.read_text().splitlines(keepends=True)
    # Pair each bare block with the section comment that introduces it, if one is close above.
    marker: str | None = None
    marker_age = 0
    made = 0
    out = []
    seen: dict[str, int] = {}
    for line in lines:
        m = MARKER.match(line.rstrip("\n"))
        if m:
            marker = clean(m.group(1))
            marker_age = 0
        elif line.strip():
            marker_age += 1
        if BLOCK.match(line.rstrip("\n")) and marker is not None and marker_age <= 14:
            base = slug(marker)
            n = seen.get(base, 0)
            seen[base] = n + 1
            gid = base if n == 0 else f"{base}-{n + 1}"
            out.append(f"      if (want('{gid}', '{marker}')) {{\n")
            made += 1
            marker = None
        else:
            out.append(line)
    if made:
        APP.write_text("".join(out))
    print(f"guarded {made} block(s)")
    for gid, label in re.findall(r"want\('([^']+)', '([^']+)'\)", APP.read_text()):
        print(f"  {gid:<28} {label}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
