# UI tasks, 2026-10-25 — working doc

Two batches ran today. Items 1-5 are the morning's; the six that came after those were installed
are tracked under their own heading further down ("Second batch"), lettered A-F because the
morning's numbering — which the user's own list skipped around — does not continue. Do not merge
the two status tables: each batch was verified against its own passes.

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

---

# Second batch, same day — the six that came after the install

Asked after items 1-5 were installed and in use. Same rule as above: each item says what it turned
out to be, not just what was typed.

| # | Item | Status | Verified by |
|---|---|---|---|
| A | Telemetry panel trimmed to the two rates a person can act on | **done** — both cores | `telemetry-pair` |
| B | Copy and Edit on my own messages; "save and send again" resets the conversation from that message | **done** — both cores, end to end | `user-message` (the control), `edit-resend` (the whole path through the host), 3 tests in `engine::edit`, 2 in `store`, 5 in `bridge::rewind` |
| C | The dark/light icon snaps; it should turn | **done** | `theme-icon` |
| D | Settings opens in the bottom-right corner, not centred | **done** | `settings-centred` |
| E | A setting's description and its selector were on separate lines | **done** | `settings-rows` |
| F | Keyboard shortcuts move to a "Help" settings page, drawn as key caps | **done** | `help-keys` |

## A. The telemetry panel answers two questions, and stops

It held ten readouts, a frame ledger and a GPU line. Two of them were questions anyone could act
on — *how fast does this endpoint answer*, *how fast does it read what I sent* — and both are the
endpoint's own token counts averaged over the conversation's recent turns (up to `ROLLING_TURNS`,
ten). Everything else was either a second, noisier answer to the same two questions (a live decode
rate, "effective" and "raw" prefill, time-to-first-token, the turn's wall clock, the
speculative-accept rate, prompt/completion counts, compactions) or diagnostics for this app's own
rendering and host GPU. The diagnostics are still in the log the board reads; the panel is where a
person looks while deciding whether an endpoint is too slow, so it now says two numbers and states
on its own face what it averaged. `GpuReport` and its boot fetch left the UI with it.

## B. Correcting a message you already sent

The part that is not a text box: **the conversation lives in two places**, and both have to change.
The cwoffee SQLite rows are the transcript every screen reads. The model's memory is separate —
the Rust core's in-process agent history, the pi core's JSONL session file — and neither one knows
about the other. Editing only the record produces a conversation whose next answer refers to
messages nobody can see.

So one verb, one order, both transports (`crates/engine/src/edit.rs`, called by the desktop command
and the gateway arm alike):

1. **Check it.** The caller has to *prove which message it means*: a stored `message_id`, or a
   position counted back from the newest of the person's own messages, plus `shown` — the text that
   window is looking at. If `shown` does not match the row now, nothing changes and the error says
   what the row says instead. A second window, or a phone tab that drifted, cannot rewrite a message
   it never saw.
2. **Cut the record.** `Store::truncate_from(session, id)` drops that row and everything after it —
   that row included, because the ask is re-sent and a send writes its own row; leaving the original
   standing with a copy of it one row lower is the bug that choice prevents. The search index goes
   with it; the attachments are read *before* the cut, because their rows cascade away with it.
3. **Hand the host a rewind** (`HostCmd::Rewind`), before anything is sent. The Rust core re-seeds
   the agent's history from the trimmed store — the same path a reopened conversation takes. The pi
   core has no rewind RPC, so the bridge rewrites pi's session file as a prefix (keep the header and
   scaffolding, keep entries up to the *n*th user entry, where *n* is the user rows the store still
   holds), writes to a temp name and renames, then `switch_session` on the same path, re-absorbs
   state and puts the conversation's thinking level back — the entry that recorded it may have been
   in the stretch that went.
4. **Put the corrected words through the ordinary send path.** Re-sending before the rewind would
   answer from the conversation that was just deleted, which is the whole reason these are one verb
   instead of two round trips another device can answer in between.

The verb answers `{ note, dropped, resending, visible }`, and `visible` is there because the client
reloads the conversation from the record after a change that worked: if the re-send's row does not
exist yet, that reload paints an answer with no question under it. Refused outright while a turn is
running — the cut and the re-send are not atomic against a turn in flight.

Written down where the two transports and the two cores have to agree: `docs/GATEWAY.md` (the verb
and its arguments) and `docs/PI-CORE-PROTOCOL.md` (§4, "the record and pi's memory are two
ledgers", including what the prune depends on if pi sessions ever branch).

On screen: a quiet Copy / Edit row under each of my own messages. Edit replaces the bubble in place.
Two buttons say what they do — **Save** changes only that message and says plainly that the model is
not being told, because it already answered the words that were there; **Save and send again** takes
the conversation back to that message. Escape cancels and does not reach the host at all.

## C. The theme icon turns instead of swapping

`{#if}` was the problem: the old glyph left the DOM and the new one arrived at full opacity in the
same frame, so there was nothing to animate. Both faces are in the page now; `[data-theme]` decides
which is lit, and the swap is a transition on opacity and rotation. Reduced motion keeps the state
and drops the turn.

## D. Settings opened in the bottom-right corner

`left: 50%; top: 50%; transform: translate(-50%, -50%)` centres a panel, and `cwoffee-pop` animates
`transform` — and an animation owns the property it animates for its whole length, so for 200ms the
centring did not exist and the panel's *top-left corner* sat at 50%/50%. Now it is `inset: 0;
margin: auto`, which centres a box with a definite size without touching `transform`, so the two
mechanisms that could each centre it are one mechanism.

## E. A setting's sentence and its control are one row

The default thinking effort had the dial on one line and the sentence explaining it underneath:
reading it and clicking it were two places. Every other row on that screen puts the words left and
the control right, flush with the panel edge, so it now does the same.

## F. The shortcuts are a page, and they are drawn as keys

They were the last section on Appearance, behind a footer icon that opened a popover: two ways to
reach a list nobody could find, and a page about colours ending in a list of keystrokes. There is a
Help entry in the settings rail now (life-buoy), and each shortcut is a row of key caps — `Ctrl`
`Enter` as two caps with the plus between them, not a sentence that says "Ctrl Enter". The older
names (`keys`, `shortcuts`) still land there, including the one that used to open the popover.

## Full passes after all of it

- Rust core: `SELFTEST ok`, 65 ok lines, 0 failures (`/tmp/cwoffee-selftest-20261025-200833.log`).
- Pi core: `SELFTEST ok`, 67 ok lines, 0 failures (`/tmp/cwoffee-selftest-20261025-201540.log`).
  Both cores print their own `correction ok` line for B — the Rust core says "the model is back to
  0 messages", the pi core says "pi's thread for conversation 35 is back to 0 of your messages; 4 of
  its entries went". That is the parity claim, measured rather than asserted.
- `cargo test --workspace --lib`: engine 510 passed, bridge 35, gateway 38, 0 failed.
- `node scripts/wire-names.mjs`: ok (every argument a host demands is a name the client sends).
- `npx svelte-check`: 0 errors, 0 warnings.

Installed with them: `~/.local/share/cwoffee-desktop/{bin/cwoffee-app,bin/cwoffee-gateway,bin/cwtool,ui/dist}`,
launcher on `core: pi` as before. `--check-only` after the copy reports
`render OK: … sidefoot=true … cup=none`, i.e. the installed entry opens a window rather than
installing beautifully and refusing to map one.

## Things learned doing them, worth keeping

- **An animation owns the property it animates.** Centring by `transform` and entering by
  `transform` are one property fighting itself. Check D measures *during* the entrance, because a
  check measured after it would have passed on the broken code.
- **A check written against a bug can pass on the fixed code, and then fail when the bug goes.**
  The dim check aimed a synthetic click at the sidebar footer control and expected the scrim. It
  passed only because the dialog sat in the bottom-right corner for the first 200ms of its entrance
  — the very bug D fixes. Once the dialog was centred from the first frame, the click legitimately
  landed on the dialog. The claim was about the dim, so the point moved to the strip of window
  beside the dialog. Same lesson as the footer deletion in the first batch: removing or fixing
  chrome means re-hosting every claim about it.
- **Serialising a `Result` instead of propagating it turns a refusal into a success.** Both
  transports did `serde_json::to_value(apply_and_send(...))`, which hands the client `{"Err": "…"}`
  where it reads `{note, dropped, resending, visible}` — so an edit that changed nothing was
  reported as `undefined` fields, and the UI's `out?.note ?? 'saved'` said "saved". The new check
  caught it on its first run; both transports now map the error out.
- **Two selftest runs must not overlap.** They share the mock endpoint on 8190; the second reuses
  the first's ("CURRENT scenarios.json — reusing it") and the first's cleanup then takes it away.
  The second run reports `list_models returned nothing` and, one run, a transcript that never
  received its rows. Two confusing failures, one cause, no code at fault.
- **A motion claim needs paints.** Which face of the theme glyph is lit is a transition that has
  run, and a transition runs on the frame clock: an occluded window gets no frames, and the glyphs
  sit where they started. The suite already has `unpainted()` for exactly this; the theme check asks
  it, and asserts the DOM and the state flip either way.
- **A harness-side "refusal" has to be aimed at a live turn.** A mock that answers "ok" finishes
  before the harness can sample `busy`, so the guard looked broken. The filler turn now counts to
  fifteen.

Reference images from this batch (kept here so they are not lost):
- `/tmp/pi-clipboard-8b0d8078-cb85-4c2e-806d-1991c29cdef9.png` — the telemetry panel as asked for:
  two readouts, `decode/s session avg` and `prefill/s session avg`, nothing below them.
- `/tmp/pi-clipboard-fe7705dc-04bc-4626-af36-d0b1e7010e26.png` — the alignment complaint (E): the
  sentence explaining a setting and the control it explains, stacked instead of on one line.
- `/tmp/pi-clipboard-aafeb2b2-c053-4b85-bbd3-3a77edd4bbb8.png`,
  `/tmp/pi-clipboard-2de6e9aa-f6b7-4216-8a4e-8f92959cad17.png` — the shortcuts page as it now
  renders (F): the section headed and described, then one row per shortcut with the keys as caps —
  `Ctrl` `Enter`, and `y` `/` `a` `/` `n` for the three answers an approval card takes.

---

# Third batch, same day — "the image I pasted is gone", and the flat look

Two asks, and they turned out to need different kinds of proof.

> "when I paste an image into the composer it seems like the file is attached, but when you send,
> there's no indication that it got sent" — and the model did not mention it either.

> "can you copy the layout of this image" — `/tmp/pi-clipboard-59149cb9-f573-4386-a80a-878588229902.png`:
> almost no divider lines, and the conversation area as a white rounded card.

## G. The picture was sent. Nothing on screen said so.

The send path was sound, and it is now proven rather than argued: the `paste-send` group stages a
PNG on the desktop clipboard, drives the real paste handler, sends, and then reads three receipts —
the bubble this window painted, the scripted endpoint's own echo (`Attachment seen: 1 image(s):
image/png`), and the stored row after a reload. Both cores agree.

The complaint was still true, from the reader's side. The composer had shown a 44px thumbnail of
that exact picture seconds earlier; once sent, the message carried a dashed 11px pill of muted ink
saying `clipboard-221045.png · 412 kB`. A caption where a thumbnail belonged reads as a lost
attachment. So this was a presentation bug with a real pipeline behind it, which is the worst kind
to be told about, because the answer "it actually worked" is useless to the person looking at it.

What changed:

- An image attachment in your own message is a chip with the picture in it, when this window holds
  the bytes — which it does for anything you just sent.
- When it does not — a conversation reopened from the store, an hour later — the chip is a square
  with an image glyph and the name, and pressing it fetches that one row's payload and opens the
  picture at reading size. Escape or a click outside gives the transcript back. The chip is filled
  in once fetched, so a second press is free.
- The transcript page still carries name and size only. That is kept deliberately: reading a
  conversation should not pull every attached megabyte across IPC for pictures nobody is looking
  at. The new `message_attachments` verb (both transports, watch-role like the text of the
  conversation it belongs to) is what makes that cheap *and* honest instead of cheap and lossy.
- A model marked text-only now says so in the composer, in the warning palette rather than the
  refusal red: the file is fine and it will travel. On the pi core pi simply omits the image blocks
  without a word, which is exactly where "did the app lose my image?" comes from. The line appears
  only when the endpoint has actually declared its modalities — a warning invented from nothing is
  the sort of thing that trains people to ignore warnings.

The check (`paste-send`) walks the whole chain, and it caught its own lie while being written: the
first version asserted the chip existed, not that it could be opened, and it passed while pressing
the chip fetched nothing. See H about why.

One older check was re-hosted: it looked for `.atttag` by class, which is an image *shape* choice,
not the claim. The claim is "the message says what it carried, by name", so it now reads
`.atttag, .tatt` and asserts the name.

## H. Two silent holes in `scripts/wire-names.mjs`, and one real bug through them

`message_attachments` reached a person's screen broken for a full day: the desktop command was
declared plain `#[tauri::command]`, so Tauri demanded `messageId` while the client sent
`message_id`. The wire check exists for precisely this, ran on every pass, and said nothing —
because of two independent holes in it:

- **A call site with a payload-typed generic was invisible.** The pattern for the client stopped a
  generic at the first `;`, and `invoke<{ mime: string; data_b64: string }[]>('...', {...})` puts a
  `;` in there. The call simply did not exist as far as the check was concerned.
- **A Tauri command with a generic in its first parameter demanded no arguments at all.** The
  parameter splitter opened `<` and never closed it — `>` was not in its close set — so
  `state: tauri::State<'_, AppState>, message_id: i64` parsed as one fragment named `state`, which
  is skipped, and the command's required set came back empty. Every command after `state` was in
  this state, which is also why the earlier "`Result` serialized as a success" bug was invisible to
  it.

Both are closed: the client walk now crosses a generic to the first `(` at angle-depth 0, and the
splitter closes angle brackets. The script also counts call sites it cannot read and says so out
loud, so a clean verdict cannot rest on a silent gap — it reports 334 sites, 2 unreadable. Proof
the holes are closed: removing `rename_all` from that command again makes the script fail with the
sentence this bug needed ("needs `messageId` and the client spells it `message_id` — one host reads
it, this one answers 'missing required key messageId'").

## I. The conversation sits on a card, and the lines between things went away

The reference has no divider lines and one obvious surface. Three rules carried the app's structure
before: a hairline under the titlebar, one right of the sidebar, and the transcript flat on the same
page as everything else.

- Both hairlines are gone. The sidebar already wears its own wash and the titlebar its own surface;
  the lines were saying what those surfaces already say.
- One token does the separating: `--color-pane`, one notch under the paper in the light theme and
  one notch deeper in the dark. The card stays exactly `--color-paper`, because every answer, code
  block and table colour in this app was tuned against paper — tinting the card would have moved
  all of them, and the ask was about the frame, not about the text.
- The scroller *is* the card: `background: var(--color-paper)` and a 16px radius. The gutter that
  floats it (10/16/14) lives on `.scrollerwrap`, not as a margin on the scroller, so the scroller
  keeps the box the geometry checks measure — that distinction has bitten once before, when a box
  changed identity mid-layout.
- The composer row was already padded 16px, so its card and the transcript card line up without a
  new rule.
- `--cup-left` gains the gutter (80→96, and 24→40 below 860px). The centred branch does not move:
  the gutter takes the same off both sides, so the column's centre is unchanged.

**What it cost, measured by the app's own check.** The first version edged the card with
`box-shadow: inset 0 0 0 1px`. On a surface this size that is not free: `a full transcript makes
the turn 32.0ms/frame against 13.0ms idle on the same page (x2.5)` — the ring is repainted with
every line that scrolls under it. And while the page was that starved, an unrelated group failed
too (`drafts`: the view not leaving the empty state after a send), because Svelte's state updates
ride the frame clock: `waitFor(...)` gave up on a view that had not been given a frame. Neither
failure was about the feature being checked. Tone alone is the edge now, which is also how the
reference does it, and both cadence checks pass again.

## Full passes after all of it

- Rust core, whole suite: `/tmp/cwoffee-selftest-20261026-015909.log` — `SELFTEST ok`, 73 claims
  logged, including `paste ok`, `correction ok`, `settings placement ok`, `settings row ok`,
  `telemetry panel ok`, `theme icon ok`, `help page ok`, `user message ok`, `attachment reaches the
  model`.
- pi core, whole suite: `/tmp/cwoffee-selftest-20261026-020925.log` — `SELFTEST ok`, 75 claims.
- The paste chain on its own, before the layout work: rust `/tmp/cwoffee-selftest-20261026-005201.log`,
  pi `/tmp/cwoffee-selftest-20261026-010037.log`.

## Things learned doing them, worth keeping

- **"It works" is not an answer to "I can't see it."** The image reached the model on both cores.
  The person was right anyway: the receipt was a caption in a place they had stopped looking. Fix
  the receipt, then prove the pipeline, in that order.
- **A check that asserts presence instead of function passes on broken code.** `paste-send` first
  asserted that a chip named the picture; pressing it fetched nothing, and the check stayed green.
  It now presses it and reads PNG magic bytes out of what came back.
- **A check can be reading the previous turn's DOM for weeks.** The markdown-table check asserted
  "no thinking line in the turn" and passed by finding a thinking block an earlier scenario had
  left on screen; run on its own it failed. The first fix scoped it to `.blk-run` — and an answer is
  a *sibling* of the run, never inside it, so the scope fell back to the whole document and the
  check still failed, now with a message that read like a product bug. `.turn` is the element that
  means "this turn". The positive claims about what a thinking line looks like moved to
  `thought-line`, whose own mock script is what puts a thought there; the table scenario now asserts
  the opposite of what it used to, because its script sends no reasoning at all: a turn with no
  reasoning must not grow a thinking line.
- **A starved page breaks checks that have nothing to do with painting.** Frame-pinned state updates
  time out, and the failure lands in whichever group happens to run next. When two unrelated groups
  fail in one run, read the cadence lines first.
- **A guard script is only as wide as its parser.** "The wire check passed" meant nothing here twice
  over, because the call site and the command's parameters were both unreadable to it. The fix is
  not only the parser: the script now prints how many call sites it could not read, so silence is
  no longer mistaken for clean.

---

# Beta pass — a first run that admits it, a chip that is a promise, and a folder that is a folder

Reported as: "before this ships, a fresh install should not point itself at a port I never set;
Chat mode should behave like Chat mode; and Settings should tell me what this thing is."

## J. The app invented a port, and a first run hung on it

`Config::default()` named `http://127.0.0.1:8090/v1`. On the author's machine that is true, so every
test passed. On a fresh install it was one of two bad days: nothing listened there and every ask hung
until timeout — including the invisible titling ask, measured at ~8 s per conversation before the
transcript showed anything — or something *did* listen there, and the app was suddenly in a
conversation with a model nobody had chosen. "Not configured" and "configured to 8090" looked
identical on screen.

The default is now empty, and empty is a state the whole app has to speak about:

* `Config::build_provider()` refuses with one sentence, `NO_ENDPOINT`, and a test asserts the config
  loader, the provider factory and `ProviderError::NoEndpoint` all say *that* sentence — three doors,
  one answer.
* `NullProvider` lets the app boot with nothing configured; asking it to stream or list models is what
  fails, and it fails in words.
* The pi bridge refuses **before** spawning a child, because a pi pointed at no endpoint dies with a
  "model not found" that describes the symptom and not the cause.
* `endpoint_configured` reads `load_effective` (file *plus* `CWOFFEE_URL`), not the file: a scripted
  run that is busy answering through the environment must not be told it has no endpoint.
* The composer's model chip says `no model`, marked, and sending is refused *before* a row is created
  — a conversation opened against nothing is a dead row in the sidebar forever.
* A setup flow opens on its own when nothing answers: address (with API key and kind), then pick a
  model. Two steps on purpose: an endpoint with no model chosen still cannot answer, and one Save
  button would have to guess which of its models the person means.

## K. A chip that changes what the model can do

The Chat composer's `Web` switch is not a filter on the answer; it decides which tools the model is
mounted with and which prompt it is told. So the grant lives on the conversation (`sessions.web_on`),
not in the window: a phone and a desktop opening the same chat have to see one answer, and leaving a
chat and coming back has to bring the same tools with it. Both transports call the same
`set_conversation_web`, so both refuse a mid-turn flip and a cowork conversation in the same words.

The tips had to follow the switch. "Summarise my workspace" was offered to a Chat that owns no
folder, and "look it up and cite" is a tool claim: it appears only while Web is on. A suggestion is a
promise, and the cup is where the app makes promises.

## L. A folder is a folder on every screen

Folder labels and colours moved out of one window's `localStorage` into the workspace row, so the
arrangement is a record rather than a decoration of one display. The chevron moved to the right end of
the row and a folder glyph took its place: the leading mark should say what a row *is*, and folded/not
folded is not what a folder is. The mode tabs got glyphs for the same reason — two people for the
party that does things, a bubble with dots for the one that only talks.

Colour is validated twice: `#rrggbb` or nothing at the store, and again in the client before it can
reach a `style` attribute. A record written by an older build, or hand-edited by whoever owns the
file, cannot be allowed to put text into another machine's inline style.

## M. "Which build is this, and where are my files?"

An About page in Settings: version, which core is *answering* (asked from the host, not from
`CWOFFEE_CORE`, which only says what was requested), the model and endpoint the **effective** config
answers from, how many conversations and messages are in the record, and the paths of the config, the
SQLite file and — on the pi core — the directory of pi's own thread files, which is the first place
anyone debugging a rewind bug needs to look. No gateway token, no endpoint API keys: it is a
watch-role read, and "which build am I running" must not quietly become a way to read a secret.
Fields the host could not fill are skipped rather than shown blank: "Model thread files: —" reads
like a broken app when the truth is that the Rust core keeps no second copy of the conversation.

## N. What the beta pass found on the way, in the order it was found

1. **`--no-tools` and a stale tool list.** The receipt on the Web chip read `-read, -bash, -edit,
   -write` for a chat that had been switched *off*. The web-off branch of a chat launch set
   `no_tools = true` and left the cloned cowork denylist in `launch.tools`. The launch was correct —
   pi was given `--no-tools` — but every later reader of that field was reading a fiction. A field
   that disagrees with the flag next to it is a bug that waits for an innocent reader.
2. **A test that leaves the world changed teaches the next tests to lie.** The web group left the app
   in Chat persona; the sidebar-grouping invariant then reported `grouping holds 0 of 2`. Nothing was
   wrong with grouping: in Chat persona the sidebar is a flat Chats list *by design*, and chat rows
   own no folder. The invariant now counts the population the tree is built from, and the group puts
   the persona back whether its claims passed or not.
3. **Setup called a ping where it needed a probe.** `provider_test` answers "is something there?" —
   it does not name models, so the second step opened with nothing in it and the check said *"the
   setup asks for a model and then shows no model to pick"*. The check was right and the flow was
   wrong.
4. **Three checks that measured their own assumptions.** The About check read `.set-rail` while
   settings was closed and called the empty list a missing entry; the model-chip check read the label
   in the same tick it changed and saw the old name; the Save-button check asserted "disabled before
   an address is typed" in a run where the address field was legitimately prefilled with the endpoint
   the run was answering from. The honest version of that assertion is the rule it encodes —
   `disabled === !pvUrl.trim()` — which holds on a real first run too.
5. **A pre-existing test was quietly red on `HEAD`.** `a_turn_parked_on_a_human…` expected 40 tok/s
   and got 80. It was failing before any of this work: the decode window stopped counting the wait for
   the first token (which the engine's own test insists on) and the fixture's expectation never moved.
   The expectation was wrong, not the code — 20 tokens over 250 ms of decoding is 80.
6. **`ALTER TABLE` runs in textual order.** The v13 `workspaces.color` column has to be added *after*
   the v10 block that creates `workspaces`; on a fresh database the migration ran against a table that
   did not exist yet, and `add_column_if_missing` cannot tell a missing table from a missing column —
   its `PRAGMA` reads "no columns".

## Full passes after the beta pass

- Rust core, whole suite: `/tmp/cwoffee-selftest-20261026-131726.log` — `SELFTEST ok`, 69 claims
  logged, including `about page ok`, `first run ok` and `web switch ok` (the rust core refuses chat
  mode in words, and the check asserts no Web chip appears where there is no chat to grant anything to).
- pi core, whole suite: `/tmp/cwoffee-selftest-20261026-132155.log` — `SELFTEST ok`, 71 claims,
  including `about page ok`, `first run ok`, `web grant ok`, `correction ok`, `paste ok`.
- The same groups on their own, before the full runs: rust `first-run`+`about-page`
  (`/tmp/cwoffee-selftest-20261026-130717.log`), pi `first-run`+`about-page`+`web-grant`
  (`/tmp/cwoffee-selftest-20261026-131029.log`). The pi log carries the process lines that are the
  actual evidence of the tool mount — at lines 2115/2125/2145: `persona chat, 1 extension(s), tools
  web_search,web_fetch` with the chip on, `persona chat, 0 extension(s), tools none (--no-tools)`
  with it off, and the pair again when the switch is thrown mid-conversation.
- `cargo test --workspace`: 648 passing, 0 failing — including `mock_endpoint` and `pi_fixtures`
  integration tests that were already red on `HEAD`.
- `node scripts/wire-names.mjs`: 114 verb-host pairs graded across 359 call sites; two call sites
  still too dynamic to read, and the script says so.
- `svelte-check`: 0 errors, 0 warnings. Production `vite build`: clean.

## Things learned doing them, worth keeping

- **A default is a claim about someone else's machine.** `127.0.0.1:8090` was a true statement about
  one laptop and a wrong one everywhere else. When the honest state is "unknown", the app should say
  unknown and say what would make it known — and the tests that depended on the guess have to count
  the scenario, not the request: a conversation that names itself makes two round trips, and treating
  the titling ask as an unexpected extra call made two good tests look broken.
- **A switch is a promise; a receipt is what makes it checkable.** `HostState.web_tools` exists so
  the chip can be held against the argv the model process actually got. Without it, every claim in
  the web group would have been about a CSS class.
- **Put the world back, passed or not.** Persona, theme, selection: a group that changes shared state
  restores it in its own tail, or the next group's failure message describes yesterday's test.
- **Scope an invariant to the population it was written about.** "Every session the sidebar shows is
  in a folder group" is a claim about the folder tree. Run it while the sidebar is a chat list and it
  reports a bug that does not exist — which costs the same debugging time as a real one.
- **Two hosts, one sentence.** Busy refusals, missing-endpoint refusals and web-grant refusals live in
  engine functions both transports call. The desktop and the phone disagreeing about *wording* is the
  first way a user learns there are two implementations.


---

# Batch four — the card is the whole right side, and the notices pick their corner

## O. The card stopped halfway down the page

The card from the last batch was the transcript and nothing else: the composer sat on the strip
below it, the folder chip below that, and the opening screen had no card at all — the greeting
floated on the surround with a composer under it. That is two surfaces pretending to be one. What
was asked for is the reading surface for the whole right side: conversation, composer and folder
line together, and the same card before there is a conversation.

`.panecard` is that surface now, and it is one box in `main`, guttered inside the pane so the
surround shows around it on all four sides:

```
┌─ panecard ─────────────────────────────┐
│  conversation (scrolls)                │
│  approval / web note / picker          │
│  composer                              │
│  folder line        tokens  ceiling    │
└────────────────────────────────────────┘
```

The transcript keeps its own scroll box (`.scroller`) but no longer paints anything: it is a
window onto the card, not a second sheet. The composer's old strip — `px-4 pb-2 pt-2` in App
markup, with its own top hairline in CSS — became a class, `.composerbar`, so the padding rule and
the (now removed) divider lived in one place instead of two files; a strip whose padding is in the
markup and whose border is in the stylesheet is how a divider comes back after someone deletes it.

`main.empty-mode` still centres the greeting, but now inside the card: `.empty-mode .panecard`
shrinks to its content and centres it, and the cup measures against the card's content box (its
`::before`), which is the box the greeting is centred in. The cup's `--cup-left` moved to 16px at
the narrow breakpoint because the gutter now belongs to the card, not to the scroller.

The connection banner lives outside the card — it is about the app, not about the conversation —
so its margins were set to the card's gutter so it lines up with the card's edge instead of the
window's.

One thing the change had to survive: the checks that measure the reading column. `.scroller` used
to *be* the surface; now it is a transparent scroll window inside one, and every geometry claim
about "the transcript" still holds because the scroller's box did not move — the card grew around
it. The new `panecard` check asserts the containment rather than the look: the transcript, the
composer and the folder line are all inside the card's box, the card is guttered inside the pane
and rounded, and the same card is on screen at the opening (with the greeting and the composer
inside it) — measured by opening a session, then a draft, then reopening the session.

## P. Six corners for the notices, and the preference that says which

Notifications used to be welded to the bottom centre. The ask: top-right by default, and
configurable — `top`, `bottom`, `top-left`, `top-right`, `bottom-left`, `bottom-right`.

The choice is a host preference, not `localStorage`: `notif_place` in `crates/engine/src/prefs.rs`,
group `appearance`, default `top-right`, with the vocabulary validated at the store (`test_pref`
rejects nonsense) so a phone and a desktop window read the same answer. The page falls back to
`top-right` for a value it does not recognise, because a store written by a newer build is not a
reason to position a notice by `undefined`.

Appearance grew a **Notifications** section that renders that preference, and the control is six
little rectangles with a dot where the notices would land — the setting shows its own meaning. A
plain `Segmented` of the six words would have worked; it would also have made someone read six
words to find a corner.

CSS is six placement rules on one container, and the entrance animation had to move off
`transform`: `.toast` used to sit at the bottom centre with `transform: translateX(-50%)`, while
the entrance keyframes (`cwoffee-rise`) animate `transform: none` with a fill — so the centring
was discarded by the animation that was supposed to be decorative, and the toast's final position
was silently whatever the keyframes said. The centring now rides the `translate` property and the
animation animates `translate`, so both are the same property and neither can clobber the other.

`.ctl-note` — the quiet explanation under a control ("this chat can no longer reach the web",
"this conversation has no folder") — is deliberately **not** promoted into a corner notice. The
`notif-place` check asserts that: it reads notes whose class list is exactly `ctl-note` and fails
if one of them is standing in the fixed stack. Control-local feedback belongs under the control it
explains; only app-level toasts travel.

The check drives the real setting (`setPref` → `prefs_set` → reload), fires a toast for each of the
six values, measures the toast's bounding box against the viewport for each, and restores the
original value — reporting it rather than leaving the window in the last tested state:

```
note place ok (six places land where they say — default top-right, this window read
top-right from the host; last measured bottom-right@723/14/14/956)
```

## Q. Three checks that had been blaming the app for the harness

The full suite went red in new places after this batch, and each of the three turned out to be a
check grading its own scaffolding rather than the product.

1. **`titling`: "no caret was ever painted".** The poll counts polls in which the caret was on
   screen, and fails when that count is zero — which conflates "the name swapped in" (a real bug)
   with "this window never drew the caret between two frames" (the window's frame budget). Two new
   counters tell them apart: polls where the row was on screen, and polls where the app was holding
   an in-flight name for this conversation (`namingId === sid && namingText`). If the state was
   held and the pixels never landed, the run says so and keeps the claims it can make.

2. **`titling`: "no clock for the ordering claim".** The comparison used `turnStartTs` — the app's
   own memory of when the turn started — read after the turn. The app is entitled to let that go:
   a host-state sync that says "nothing is running" clears it, and the sync usually wins. The group
   had already been recording its own clock, `busyFirst`, and never using it. The claim is now
   graded against the clock the check owns, with the poll's own 10ms error bar stated in the
   constant rather than hidden: the pi core lost the old claim on the first try, the rust core lost
   it on the second.

3. **`drafts`: "the telemetry panel shows no context row"** (a downstream check). The telemetry
   panel is the footer popover, and the popover is a *toggle*. A check that ran while a popover was
   already standing clicked it shut and then reported a missing row. The check now puts the screen
   to rights first (close the menu, close Settings), waits for the row instead of assuming a
   quarter of a second, and quotes what the popover *did* say when the row is still missing.

The lesson is the same one the suite keeps relearning: **a claim needs a clock and a scope that the
check owns**, and a failure message has to name which side of the seam broke. Both `titling`
messages and the `drafts` one now do.

## R. A view wiped by its own refresh — the flake that was a bug

`view stayed empty after sending (viewId=16 msgCount=0 busy=false)` failed two of four full runs
and looked like a layout regression. It was not, and the old message could not say what it was, so
the first change was to make the message tell: read the record for the conversation that was just
sent before failing.

```
view stayed empty after sending (viewing 16, the conversation just sent is 16, the record
holds 2 row(s) for it, msgCount=0, busy=false)
```

Record has it, screen has nothing: the view was wiped by a refresh, not by a missing write. The
rust core persists a turn **when it ends**, so a `resyncView` whose read goes out during a turn can
hand back an empty page *after* that turn finished. `resyncView` clears the transcript before it
loads, so painting that stale page removes a conversation that exists — and nothing repaints it.
The code already knew about the mid-turn case (`if (turnSessionId === id)` replays the live buffer)
and about the duplicated-question case (`storeHoldsSentDraft`), but nothing covered a read that
landed just after the end.

The fix is one more read whenever a page comes back with nothing:

```ts
if (rows.length === 0) {
  // A page that comes back empty is the one page a turn in flight can invalidate …
  again = await loadTranscript(id, keepRows)
  if (again.length)
    pageLog(`resync: conversation ${id} looked empty and was not — …`)
  rows = again
}
```

The cheaper witness would have been `Session.message_count` on the sidebar row, and the first
version used it — which is why the first fix did nothing: the conversation being loaded was created
by the very send now in flight, so it is not in the list this window holds yet. Believing a counter
that has not caught up is the same mistake as believing a page that has not caught up.

It is not exotic: one full rust-core run logged the retry **four times** (`16`, `17`, `35`, `36`),
each time a conversation the person would have seen go blank. The pi core logs none, for the reason
already written down in `storeHoldsSentDraft`: the bridge writes the user's row *before* asking the
model, so there is no window where the record is empty behind a finished turn. The race is shaped
by which core persists when — which is also why it survived as a flake instead of a bug report.

## What this batch changed

- `main` wraps its right-hand content in `.panecard`; the transcript surface, the composer strip and
  the folder line are one card, on the opening screen too. `.composerbar` is a class now;
  `.scroller` paints nothing.
- `notif_place` (host preference, default `top-right`, six values), a Notifications section in
  Appearance with six picture-of-the-corner controls, and six placement rules in CSS. The toast
  entrance animates `translate` so it can no longer eat its own centring.
- `resyncView` re-reads a conversation that hands back an empty page.
- Three selftest claims re-pointed at clocks and scopes they own; two failure messages rewritten to
  name the side of the seam that broke.
- New checks: `panecard` (containment, gutter, rounded edge, and the same card on the greeting
  screen) and `notif-place` (six placements measured from the toast's own box, `.ctl-note` rows kept
  out of the fixed stack, the setting restored afterwards).

## Full pass, both cores

```
rust core: SELFTEST ok — 71 claims, including
  card ok (one surface 900x709 inside a 932px pane, holding conversation + composer + folder row;
    16px of gutter; the same card on the opening screen)
  note place ok (six places land where they say — default top-right, this window read top-right
    from the host; last measured bottom-right@723/14/14/956)
  titling ok … first name paint 37117ms against the page learning the turn was running at 37250ms
  resync: conversation 16 looked empty and was not — 2 row(s) came back when the page was read a
    second time   (×4 in one run)
pi core:   SELFTEST ok — 73 claims, same two groups, no resync retries needed
cargo test --workspace: ok, 0 failing (engine 519, gateway 38, bridge 35, pi fixtures 16, mock 10)
svelte-check: 0 errors, 0 warnings · vite build: built
wire-names: OK (unchanged: three desktop/gateway-only verbs reported as notes)
```

## Lessons

1. **A card that ends where the transcript ends is not a card.** The right-hand surface has to cover
   everything a person reads and types on that side, including before there is anything to read.
2. **An animation that animates `transform` will eat a centring that also lives in `transform`.**
   Put the static offset and the motion on one property, or animate `translate` and keep
   `transform` for something else.
3. **A failure message is a debugging tool, not a verdict.** `view stayed empty after sending` cost
   four runs; `the record holds 2 row(s) for it, msgCount=0` found the bug in one.
4. **A flake that repeats is a bug with a timing profile.** Two of four full runs, four hits in a
   single run: the retry is not a workaround, it is the fix for a read that can be older than the
   write it is waiting for.
5. **Do not grade motion or ordering on a clock the app owns.** The app is free to clear its memory
   of a turn; the check has to keep its own.
6. **A counter that describes a page can be as stale as the page.** The sidebar's `message_count`
   does not know about the conversation this send just created.

---

# Batch five — the arrow moves to the end of the row, the header learns one fold button, and the window becomes one surface (twice; one of those attempts broke it)

## O. The folder row's reading order

The arrow that says how a folder is arranged was at the right end of the *label*, which put it
between the conversation count and the row's actions — so the last thing a row offered was neither
the last thing you press nor the least actionable thing it says. Now the row reads in the order it
gets read: folder, name, count, then the actions (`+`, `⋯`), then the fold state, last.

The arrow also stopped being a mark inside the label button and became a control in its own right —
it names itself (`Fold cwoffee-workspace` / `Unfold cwoffee-workspace`) and reports
`aria-expanded`, which a decorative `<svg>` inside someone else's button cannot do. The label still
folds on click, because the row is the thumb target and folding a folder is a navigation act; in
selection mode the arrow stays in place so the row does not change shape mid-pick, and does nothing
(`disabled`).

## P. One fold button, and it says which way it will go

The `Workspaces` heading carried three buttons: fold all, unfold all, add a folder. Two of them were
a question about the state of six folders — one of the two was always inert, and both were chevrons
pointing at nothing in particular.

One button now, and it takes the side of the list it is looking at: while anything is open it folds,
and only once the list is shut does it open.

```ts
const anyWsOpen = $derived(wsGroups.some((g) => !wsCollapsed.has(g.path)))
```

The mark is two sheets stacked — the stack is the scope, it acts on every folder — and the square in
front carries the sign of the action: `−` while it is about to fold, `+` once it is about to unfold.
A chevron would have said "there is more", which is a fact about the list rather than a promise
about the press.

The `delete` check had been clicking `.iconbtn.ws-fold[aria-label="Unfold every folder"]`. That
selector describes a button that no longer exists, and worse, it names the half rather than the
control — so it now presses the one button *until the list is open*, which is what the check
actually needs and what survives the next change to the control.

## Q. The opening screen's "pair" was two things

Last batch's card was supposed to centre the greeting and the composer as one subject. It did not,
and the CSS believed it had: `.empty-mode .panecard { justify-content: center }` was centring a
column whose middle was occupied by the transcript's box, which is `flex: 1` and keeps swallowing
the free space even while the transcript inside it is `display: none`. Cup at the top of the card,
composer at the bottom, 600px of nothing in between.

```css
/* The wrap has to go as well. It is `flex: 1`, so it keeps swallowing the free space even when
   the thing inside it is hidden … */
.empty-mode .scrollerwrap { display: none; }
```

Hidden, not unmounted — the renderer holds a reference to its root node.

The new claim measures the pair rather than admiring it:

```
opening screen ok (greeting and composer 32px apart, the pair 24px off the card's middle,
the pair spans 608px of a 709px card)
```

## R. One surface across the window — and two attempts that cost more than they gave

The ask: the titlebar, the sidebar and the band behind the card should be one colour, with no
dividers, and the whole window should get rounded corners and a hairline.

The first half shipped. The window used to be three tones — `paper-2` across the top, a
cream-to-olive wash down the left, `pane` behind the card — which is what made one container read as
three containers with lines between them even in the places where the line had already been deleted.
Now `.app-shell` carries the surface and everything inside it is transparent; the sidebar's inline
gradient and its three `--color-wash-*` tokens are gone; `main` lost its `bg-pane`; the titlebar lost
its `bg-paper-2`. The card stays `paper`, and it is now the only step in tone on the screen — which
is the entire job of a card.

The gradient was tried, because the ask said to try it. Measured, on the same page in the same run:

| desk | big transcript, median frame | frame cadence, median |
| --- | --- | --- |
| gradient on the shell box | 43–46 ms | 34–36 ms |
| gradient on a fixed layer behind the content | (see below) | — |
| flat `--color-pane` | 18–30 ms | 12–23 ms |

A scrolled transcript repaints the surface behind it, and a gradient is not a free fill. The flat
colour is what holds. (Removing the sidebar's wash gradient also *recovered* frame time: the
big-transcript median is now 18ms, better than the 24–25ms this batch started at.)

The second half did not ship, and the reason is in the stylesheet where someone would next try it:
`"transparent": true` in `tauri.conf.json`, a 14px radius and an inset hairline on `.app-shell`, and
a page made transparent behind it, made the webview stop painting the shell's own surface on this
WebKitGTK/GNOME session. The app went part translucent: the terminal behind it showed through the
desk, the settings dialog floated over someone else's window text, and the screen flashed as the
compositor decided what to keep. That is the report that ended the experiment. A frameless window on
Linux is a rectangle the compositor clips; corners would have to come from the window manager or a
real CSD implementation, not from making the page see-through. The modernising is done with tone
instead: one surface, no dividers, a rounded card.

Three things were kept from the failed attempt, because they were good independent of the rounding:

- `desk_bg` is now part of the startup render report, and the watchdog's "did the webview paint?"
  test reads the app's own surface instead of `document.body`. The old test treated
  `rgba(0, 0, 0, 0)` as the symptom of a blank window, which is exactly what a *correctly*
  transparent window looks like — so the signal was about to become a lie either way.
  `a_transparent_body_with_a_painted_shell_is_not_a_blank_window` keeps it honest.
- The `panecard` check had been comparing the card's tone with `main`'s, which is transparent now:
  a build that painted nothing would have passed. It compares the card with what the window paints.
- The `desk` check asserts the page paints an opaque surface behind the app, with the flashing bug
  written into the comment. If anyone re-lands a transparent window, the run says what it broke.

## What this batch changed

- Folder row: the fold arrow is a named control at the far right, past the actions; the label still
  folds; the arrow is disabled during selection.
- Heading: one fold control, signed `−` open / `+` shut, two stacked sheets; `delete` presses it
  until the list is open instead of naming a button.
- Opening screen: the transcript's box leaves the flex flow, so the greeting and composer centre as
  the pair the CSS always claimed they were.
- Window: one surface (`.app-shell` carries `--color-pane`); titlebar, sidebar and `main` no longer
  paint their own; the three `--color-wash-*` tokens are deleted; no divider anywhere.
- Startup render report: `desk=` added, and the "painted?" test moved to the shell.

## Full pass, both cores

```
rust core: SELFTEST ok — 73 claims, including
  fold controls ok (one header button, signed − open and + shut; the arrow is the last thing on a
    folder row and folds that folder alone; the label still folds too; 2 folder row(s))
  window surface ok (one desk under the titlebar, the sidebar and the band around the card; the card
    is the only step up in tone; no divider between the parts; the page paints opaque behind the app)
  opening screen ok (greeting and composer 32px apart, the pair 24px off the card's middle, the pair
    spans 608px of a 709px card)
  big transcript cadence: 168 blocks / 635096 characters; median 18.0ms, p95 44.0ms
pi core:   SELFTEST ok — 76 claims, same three groups
cargo test --workspace: 0 failing (app 25, engine 519, gateway 38, bridge 35, pi fixtures 16, mock 10)
svelte-check: 0 errors, 0 warnings · vite build: built · wire-names: OK
installed (--core pi), launch check OK:
  render OK: … bg=rgb(253, 252, 249) desk=rgb(242, 239, 231) …
```

## Lessons

1. **A flex child does not stop eating space because its child is hidden.** `display: none` on the
   transcript left its `flex: 1` wrap taking the middle of the card, and the centring rule that
   looked correct was centring a column with a hole in it.
2. **Measure the gradient before shipping the gradient.** It was asked for, it looked fine in a
   screenshot, and it cost ~15ms a frame on a large transcript. Numbers ended the argument.
3. **A transparent window is not a corner radius.** On this WebKitGTK/GNOME session it removed the
   webview's own paint: the app went translucent and the desktop showed through. The failed attempt
   is written in the stylesheet where it would be attempted again, with what it did and what it cost.
4. **A watchdog signal has to describe the thing it claims.** `body` being empty stopped meaning
   "nothing painted" the moment the window was allowed to be transparent — the surface moved to the
   app's own shell, and the check moved with it or it would have been lying in both directions.
5. **When a control's two halves merge, the checks that named each half have to name the state
   instead.** "Press the unfold button" became "press the fold control until the list is open".
