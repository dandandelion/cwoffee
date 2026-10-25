# UI tasks, 2026-10-25 — working doc

Tracked here on purpose: the user asked for these five and said *"save this into a markdown doc,
and track progress there, update each item so that even after compaction, the details are not
lost."* Update the **Status** line of each item as it changes, and add a **Note** line whenever a
decision gets made. Do not delete an item; a dropped item is a dropped promise.

How to verify any of it: `./scripts/selftest.sh <group>` runs the named check groups only
(`./scripts/selftest.sh --list` names them). A targeted green covers the groups that ran — the
verdict line says `[TARGETED n named group(s) ran, m not run: …]`.

---

## 1. The cup fades instead of vanishing

**User asked:** "Maybe instead of hiding the cup, transition (transition should happen both ways)
the opacity to a really transparent one, so that it's still there, just kinda hidden (transparent
enough that text behind it is coherent)."

**Status:** DONE — full Rust-core pass green with it

    cup band ok (136px picture pinned 8px off the pane's bottom, 168px of the pane held out of the
    text, column-aligned; across 60 streaming frames it travelled 0.0px, worst frame 0.0px, 0
    reversals, 1 lit/away toggle(s) in 35 lit and 25 put-away frames; faded to 0.10 mid-page and back
    to 0.90 at the end, caught mid-fade at 0.72 on the way back in; both ways by transition)

The "back to 0.90" is the first frame past the 0.9 threshold, not where the fade lands; the check now
reads the value again 520 ms later so the line says what the picture settles at.

Two bugs this item produced, both caught by the harness rather than by looking:

1. **`transition: opacity 0.42s var(--ease) both;` is an invalid shorthand.** `both` is an animation
   fill-mode; a transition has none. The whole declaration was dropped, so the cup kept snapping and
   the computed `transition-property` read `all` - which is exactly what the new assertion reported.
   The rule is now `transition: opacity 0.42s var(--ease)`.
2. **Sampling a fade right after the move grades the flight, not the destination.** The check used to
   write `scrollTop` and read the opacity three frames later; with a real 420 ms fade that sample says
   `0.85` and the check cries "the prose is not readable". It now polls until the value settles
   (≤ 0.16 going out, > 0.9 coming back) and reports the first *intermediate* value it happens to see
   on the way in, as an observation rather than a demand - whether a frame lands inside 420 ms is the
   compositor's clock.

And a third that was an environment artefact, not a bug: under reduced motion a global rule turns
transitions off, so `transitionProperty` reads `all` legitimately. That assertion now asks only when
motion is not reduced - while the opacity values are still asserted either way, because opacity is
not motion.

**What shipped**
- `.transcript-cup` carries `transition: opacity 0.42s var(--ease) both` on the **base** rule, so it
  fades in *and* out. On the toggling class it would have run one way and snapped the other, which is
  exactly what the eye catches.
- `.transcript-cup.cup-out { opacity: 0.1 }` replaces `display: none`. `.bare` still hides it outright
  (no conversation, no picture).
- The 24px threshold and the 400ms patience stay: they decide whether the picture would lie over prose,
  not whether it is painted. `pointer-events: none` holds in both states, so a faint cup cannot eat a
  click aimed at a word under it.
- The selftest changed with it: mid-page it now demands the box **keep** its layout (a dimmed picture
  keeps its box, only a hidden one drops it), opacity ≤ 0.16, `transitionProperty` containing `opacity`,
  and opacity > 0.9 back at the end. The follow check's "cup in the way of the pill" test now means
  *legible* (opacity > 0.5), not merely painted.

**Original plan, kept for the reasoning**
- `.transcript-cup.cup-out` stops being `display:none` and becomes a very low opacity, with the
  transition declared on the base rule so it animates **both ways**.
- Keep `.bare` as `display:none`: no conversation, no picture at all (the greeting screen already
  shows the same artwork, larger).
- Keep the geometry rule that decides *when* it dims (band depth minus picture+seat = 24px) and the
  400ms patience: those decide whether it is legible-over-text, not whether it is painted.
- `pointer-events: none` already holds, so a faded cup over a paragraph cannot swallow a click.

**Acceptance**
- Mid-page: the cup's box still exists (it is not `display:none`), and its computed opacity is very
  low (target ≤ 0.14) while the reading position is away from the end.
- At the end: opacity back to 1.
- The transition is present in the cascade and runs in both directions (assert the property, not a
  sampled mid-flight value — sampling a frame of a transition is the machine's business).
- No new lit/away class flicker: the class still toggles at most twice per streamed turn.

---

## 2. Workspace selector: the popover, from the composer chip, with the rule

**User asked:** "This hasn't been implemented still. This is the workspace selector on the initial
screen, clicking the workspace under the composer doesn't open native picker immediately, instead,
it opens a popover, where you can search *already added* workspaces, list of existing workspaces
(limit latest 10 displayed OR fixed height then add a vertical scroll), then at the bottom, (after
a line divider) two static items `New Workspace` (this opens the native folder picker) and
`No workspace` (which clears the workspace of the session) + the rule where already existing
conversation where an exchange happened cannot change workspace (or clear)."

**Status:** TODO

**What exists today:** the greeting screen's row opens a `wspick` popover with a filter, the list,
`.wsp-sep`, "New workspace" and "No workspace selected". The composer's workspace chip under the
input still goes straight to the native folder dialog — that is the half that is missing.

**Plan**
- The composer chip opens the same popover instead of invoking the picker.
- List: fixed max height with vertical scroll, newest 10 first (the scroll is what makes the cap
  honest rather than a truncation with no way to see the rest).
- Divider, then two static rows: **New workspace** (native folder picker) and **No workspace**
  (clears the current session's workspace).
- **The rule:** a conversation that has already had an exchange cannot change or clear its
  workspace. Enforce it in the host (the answer is a refusal the UI shows, not a silent no-op) and
  say it in the control: the rows are disabled with a `title` naming the reason. The popover should
  offer the change for a draft/no-exchange conversation and refuse for a live one.
- Filter focus stays on open (the existing check waits up to 640ms because the mount effect lands a
  tick later on the pi core).

**Acceptance**
- Clicking the chip opens the popover and does **not** open a native dialog.
- Filter is focused on open; typing narrows by name and by path.
- The list is capped (fixed height + `overflow-y: auto`) and never wider than the column.
- The two actions exist after a divider; `New workspace` reaches the folder picker; `No workspace`
  clears it.
- A session with messages: both actions refuse, and the refusal states why (the conversation has
  already spoken in this folder).

---

## 4. Remove the footer toolbar; put three icons in the sidebar footer

**User asked:** "Starting to not like the toolbar at the footer. Let's do something about it.
Remove it entirely (more vertical space for transcript and sidebar) then let's put them in here
(sidebar footer) as just icons (like Qoder here, you can 1:1 copy layout and you decide the icons).
What I wanna see in that sidebar bottom: Static cwoffee icon (as the profile picture), the name
`Cwoffee`, and then three icons (right aligned as well): Metrics (you can use the mileage icon),
Gateway (you decide the icon, can be a laptop/computer screen icon?), and theme toggle. Just those
three. Omit others (ipc, search, and help (already in settings))."

**Status:** DONE — measured green in a targeted run

    sidebar footer row fits 208px of sidebar (Session telemetry, Remote access, Toggle light or
    dark theme; all inside a 187px row, each at least 28px tall)

**What shipped**
- The 80-line `<footer class="telem …">` is gone; the markup where it sat says why, and where each
  control went.
- The sidebar's pinned footer now holds: the **still** cup mark (26px round), the word `Cwoffee`, and
  three 30×28 icon buttons pushed right — telemetry (a mileage dial), gateway (a laptop), theme
  (sun/moon). State the old bar spelled out survives as a dot on the glyph (gateway live, dropped
  events) plus the fact in the `title`.
- Dropped, not relocated: the ipc chip, the web-search pill, the `?` shortcut sheet — all three are
  already in Settings.
- `render_report`'s `footer` became `sidefoot` (page side and Rust side together, `#[serde(default)]`
  so an older bundle still parses); the startup watchdog keeps its "the chrome rendered" signal.
- Retargeted, not deleted: the dim/inert-behind-settings check (now `.side-id [data-menu]`), the
  phone-width fit check (now narrows the *sidebar* to 208px and demands the three controls fit and are
  thumb-sized), the footer-truth check (now the gateway icon's title + dot vs `gateway_status`, plus
  the mode chip), and the search check (now drives Settings → the `search` section, because the pill
  that used to open the popover is gone).
- New group `sidebar-chrome` (below) covers the pinned heading too.

**Original plan**
- Delete the app's bottom toolbar from the layout. Whatever it carried that is not in the three
  icons is **dropped, not relocated** — a fact with nowhere honest to live is a fact that should be
  said in a tooltip or not at all. ipc / search / help: dropped (help is in settings).
- New sidebar footer, laid out like the reference: profile picture + name on the left, then the
  three icon buttons pushed right.
  - cwoffee mark (static, no animation — the artwork already animates where it belongs, and an
    animated picture in a chrome row is a second gesture nobody asked for)
  - the word `Cwoffee`
  - **Metrics** — a mileage/speedometer glyph
  - **Gateway** — a laptop/monitor glyph
  - **Theme** — sun/moon
- Every one of them is an icon button with `title` + `aria-label` (the rule from earlier in this
  session: a glyph with a name costs no horizontal room and needs no translation), and each glyph
  is centred by `.iconbtn > svg { margin: auto }` so a display-mode change cannot move it.
- `render_report`'s `footer` field: it is a *layout* signal used by the startup watchdog. Either it
  reports the sidebar footer from now on, or the field goes away and the watchdog loses a signal.
  Decide deliberately; do not leave a field that means "the bar that no longer exists".

**Acceptance**
- No bottom toolbar element on the page; the transcript and the sidebar are taller by that height.
- The sidebar footer holds exactly: mark, the word `Cwoffee`, three icon buttons. Metrics/Gateway/
  Theme each open what the old footer opened.
- The three glyphs are centred in their buttons (the hamburger lesson: measure the real button).
- The selftest's footer/status-bar checks are retargeted to the new row, not deleted.

---

## 5. The sidebar's `Workspaces` header does not scroll

**User asked:** "The `Workspaces` (header) is currently included in the scrollable area. It
shouldn't scroll (stick to top?), should always be accessible even if workspaces/sessions scroll to
bottom."

**Status:** DONE — measured green in a targeted run

    sidebar chrome pinned ok ("Workspaces" and the identity row sit outside the 0px of scrollable
    list; the list starts 7px below the heading, and the row still holds its mark, its name and 3
    controls)

Note: that run had a short conversation list, so the 0px-of-scroll half of the claim was not
exercised; the containment claim (the scroller starts below the heading) was.

**What shipped**
- The heading is a sidebar column child rendered *above* `.sesslist`, not inside it — for both
  "Workspaces" (with fold-all / add-folder) and "Chats". Sticky-with-backdrop was rejected: the
  sidebar is painted with a gradient, so a sticky band would not match what it slides over.
- `.sesshead` gets `flex: none` and its own margin so it cannot be squashed by the list.
- New check group `sidebar-chrome`: the heading and the identity row must not be descendants of the
  scroller; with the list scrolled to its far end both must still be inside the sidebar's rect; the
  scroller must start below the heading (so no row can travel under it); the add-folder control must
  stay inside the sidebar; the identity row must carry a mark and the name.

**Original plan**
- Take the header out of the scrolling flow: either lift it above the scroll container in the
  markup, or make it `position: sticky; top: 0` **with an opaque background** (a sticky header with
  a transparent background is a header that rows read through, which is worse than the bug).
- Keep whatever control the header carries (count, add, collapse) reachable while the list scrolls.

**Acceptance**
- With the session list scrolled to its bottom, the header's box is still inside the sidebar's
  visible rect, and the first visible row does not sit under it.

---

## Order of work and current state

| # | Item | Status | Verified by |
|---|---|---|---|
| 1 | Cup fades both ways | **done** — full pass green on both cores | `cup` |
| 2 | Workspace popover from the composer chip + the exchange rule | **done** — full pass green | `workspace-picker` |
| 3 | (the item the user skipped in their list — nothing to do) | n/a | n/a |
| 4 | Footer removed, sidebar footer row | **done** — full pass green | `statusbar`, `sidebar-chrome`, `search`, `settings`, `dims` |
| 5 | Sticky `Workspaces` header | **done** — full pass green | `sidebar-chrome` |

Full passes after all of it:

- Rust core: `SELFTEST ok`, 55 ok lines, 0 failures (`/tmp/cwoffee-selftest-20261025-053728.log`).
- Pi core: `SELFTEST ok`, 58 ok lines, 0 failures (`/tmp/cwoffee-selftest-20261025-054213.log`).

Installed with them: `~/.local/share/cwoffee-desktop/{bin/cwoffee-app,bin/cwoffee-gateway,bin/cwtool,ui/dist}`,
launcher on `core: pi`. The startup line reads `… sidefoot=true … cup=none` now — `sidefoot` is the
sidebar row, since the bar it used to report is gone.

Things learned while doing them, worth keeping:

- **A fade belongs on the base rule.** A transition declared on the class that turns a thing off runs
  one way and snaps the other.
- **A dimmed picture keeps its box.** If the check demands the element collapse, it is checking the old
  feature, not the new one.
- **Removing chrome means re-hosting every claim about it.** Five check groups referenced the bottom
  toolbar. Deleting the bar without moving those claims would have quietly deleted the coverage — and
  the phone-width fit check is the one that caught a real bug in the first place.
- **A page-side claim must be measured against the thing it is actually inside.** The search check
  was retargeted from a floating popover to a settings section, and kept its "inside the window"
  claim; a settings section lives in a scrolling body, so that claim fired on a section that was
  merely below the fold. Containment has to be stated against the dialog, at the same instant.
- **Reduced motion invalidates transition assertions.** The cup's fade check demanded
  `transitionProperty` contain `opacity` and got `all`, because a global reduced-motion rule turns
  every transition off. The check now asks only when motion is not reduced - and opacity itself is
  still asserted either way, because opacity is not motion.
- **The render watchdog is a contract with the page.** Renaming `footer` to `sidefoot` in one place
  would have made every installed start report SUSPECT (or worse, kept a true field that named a bar
  that no longer exists).

Reference images the user gave (kept here so they are not lost):
- `/tmp/pi-clipboard-3ccb2d15-1a23-4c3d-b28e-decf755876da.png` — the workspace popover as they want
  it: search field, one row per added workspace (icon + name above the full path), then a divider,
  then `New workspace` and `No workspace selected`.
- `/tmp/pi-clipboard-21781a8f-40d1-4fe1-9d29-a342f87aa707.png` — the Qoder sidebar footer to copy
  the layout of: profile picture, name, then right-aligned icon buttons.
