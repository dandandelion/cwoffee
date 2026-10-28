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


## R-bis. What the one-surface change broke, six minutes after it shipped

The screenshot: at a narrow window the sidebar drawer was see-through — the greeting, the tips and
the composer readable *through* the session list.

The drawer never had its own surface. Below 768px the aside is a fixed overlay, and its only paint
came from an inline `style="background: linear-gradient(…)"` on the element, which the one-surface
change deleted along with the three `--color-wash-*` tokens. Docked, that deletion is the whole
point: the window's desk shows through and there is no second tone. Overlaying, it is a hole — the
drawer slides over the conversation, and with nothing painted the conversation shows through.

The fix is one declaration in the narrow-width block, on the same principle the docked column has
always had — one notch under the paper it sits on:

```css
.sidebar { /* @media (max-width: 767px) */
  background: var(--color-pane);
}
```

The new `drawer` group holds both halves of the invariant, because the two halves point in opposite
directions and either can be broken alone: the docked sidebar must paint *nothing* (so the window's
surface shows through), and the narrow-width rule must give the drawer a surface that *resolves* to
an opaque colour. Resolving matters: the check sets the declaration on a throwaway element and reads
the computed colour, because `var(--color-pane)` is only an opaque paint if the token says so — a
check that stopped at the string would pass on `var(--color-glass)`.

```
drawer surface ok (docked sidebar paints nothing and the window's surface shows through;
the narrow-width rule gives it var(--color-pane) → rgb(242, 239, 231))
```

Lesson: **when you delete a surface, ask what was overlaying something.** "Everything sits on one
background" is true of a docked column and false of a drawer, and the stylesheet says which one a
rule belongs to while the markup that carried the paint did not.

---

# Batch seven — a file said twice, a name that said nothing, and a scroll box that never noticed the window

Five reports at once, and the most interesting one was a bug I could not find by looking:

1. the same file appears twice in one turn — keep the dashed hand-over, lose the duplicate, stop the
   hover fills, align the row with the chat, and never let a file land in the middle of what the turn
   said;
2. remove the `cwoffee` label from the transcript;
3. the open conversation's marker: lose the left bar, make the fill readable in both palettes;
4. put the animated mark from `~/Downloads/loading-status.webp` where the label was — looping with no
   holds, sticky the way the label was, gone when the turn ends;
5. "resize to where the scroll bars appear, scroll to the very bottom, then fullscreen: the top of the
   conversation is cut off and stays cut off even after the scroll bars disappear."

## O. A file the model wrote and a file the model handed over are one word

The screenshot showed `machine-build.md` twice in one turn. It was not a duplicated render: the model
wrote the file in the workspace (a working chip) and then put a copy under `.cws/` to hand it over
(the dashed unit with the Save control). Two real records, one name — and a reader reads *names*, not
paths, so the turn looked like it had made the same file twice.

The dedupe happens where the reader reads: by name, with the hand-over copy winning, because it is the
one the model wrote *in order to give*. The working copy is not hidden from the record — `files` still
lists it and the Save control still hands over the delivered one.

The placement was wrong in two opposite directions, and both shapes are asserted:

- appended to the live turn, a stored transcript put the chip at the *top* of the conversation on
  reload (the old "the file was not shown at turn end" report);
- emitted per message, it landed *inside* an answer that was still talking about something else,
  because the write event arrives before the next paragraph does.

A turn's files are now accumulated and flushed at the end of the turn, above its
`Copy / Share as markdown / Branch` row — live (`pinFilesLast`, re-applied after every append) and
stored (`histFiles`, flushed at the turn boundary). The row starts where the sentences start rather
than being justified to the right edge: it is part of what the turn said, not a margin note.

### The hover fill was not where it looked

"Remove the hover background on the label and on the download control." The rule that looked
responsible — `.attachunit .filechip:hover` — was already gone. Deleting it again changed nothing,
because the fill came from a *generic* rule several hundred lines later:

```css
.filechip:hover { border-color: var(--color-shell); background: var(--color-tint); }
```

Same specificity as `.attachunit .filechip`, later in the file, so on hover it wins and the name half
lights up anyway. The unit now says otherwise for both halves, explicitly, and the stylesheet says why
the explicitness is load-bearing.

### A check that answers the cascade question, not the string question

The page cannot be made to answer `:hover` from JS, so the claim has to come from the rules — and it
has to be made the way the engine makes it. The first version failed if *any* matching hover rule
declared a background; that failed for the wrong reason, because a rule that loses the cascade is
harmless. What is graded now:

- among the rules that match this element and declare a background, find the winner **at rest** and
  the winner **on hover**, by specificity and source order;
- resolve both declarations through the engine (`var()` and `color-mix()` are not colours until they
  are resolved);
- fail only when the two resolve to different ink.

Verified from both sides — with the CSS override deleted, on a build that had the bug:

```text
SELFTEST FAIL: on hover hover resolves .filechip:hover (var(--color-tint) → color(srgb 0.947294 0.915765 0.868627))
over .attachunit .filechip (transparent → rgba(0, 0, 0, 0)) — the file name is a control inside a box
that is already a box
```

and with it in place:

```text
turn files ok (one chip for a file written and delivered, the hand-over copy kept with its Save
control, the row last in the turn and 0px from the text edge, no hover fill on either half)
```

## P. The word `cwoffee` said nothing, and the gutter was free

Everything in the transcript that is not your own bubble is cwoffee's. The pinned label restated what
the layout already announced, so it is gone — from the live marker, from the stored transcript (where
its container rule and its gap-payback went with it), and from the block heads that were already
quiet.

What stayed is the *row*: sticky, zero-height, bounded by the turn so it releases at the turn's end
instead of floating into the next one. The row carries a state now — the looping mark from the file
provided, `ui/public/cwoffee_turn_working_1.webp`, with frame zero kept as
`cwoffee_turn_working_still.webp` for `prefers-reduced-motion`.

*(Superseded one batch later: the pinned 56px gutter mark and the pinned name row are both gone, and
the mark is inline at 20px in the turn's header or after its last block. See "Batch eight" below.)*

The looping needed no code. The file's own `ANIM` header says `loop_count=0` (infinite) and every
`ANMF` frame says 198ms — a uniform lap with no holds, which is what a *state readout* wants. The
transcript's cups take random pauses because they are artwork that would otherwise be the only moving
thing on a still page; a readout that stops and starts on a schedule of its own is a riddle. So there
is no hold logic and no timer here: the picture is made once per turn (per-block creation would restart
the loop at frame zero every time the model paused between a tool call and a sentence) and removed by
`dropWorking()` at `turnEnd`, so a settled conversation contains no motion at all.

### A pinned mark must not be raised above the line that pins it

The word used to be *centred* on the header line of the turn's first block, which meant hanging ~22px
above the marker row. For a picture, that is a clipping hazard: the row sticks to the top of the pane,
the pane clips at its own edge, and a pinned mark whose top reaches above that line loses its top. The
first measurement of the new mark showed exactly that — mark at `32..88` against a wrap starting at
`y=54`, with the dump naming the culprit (`top=-22px`).

So it hangs *down* from the published line (`top: var(--who-top)`) and stays whole. Two claims hold
that: one measures the mark against the top of the box that clips it while a turn is mid-flight, one
does the same on a streaming answer.

The old alignment claim ("the name is 0.1px off the folded group's header line") could not be carried
over honestly: it only ever held while the row sat at its natural position, and a stuck row and a
header moving under it are two different things. What still has to be true is that the mark costs the
prose nothing and never enters it:

```text
turn marker ok: no generated marker on 10 labels, one cup 136px pinned in the pane's band, working
mark 66px clear of the text (--who-top 2px)
working mark ok (paragraph …px clear of the text, heading …px clear, and none at all once the turn has landed)
```

The geometry, measured on a real window: the card runs `264..1164`, the prose starts at `350`, the
marker row sits at `350`, and the mark occupies `284..340` — inside the card, 20px clear of its edge,
66px clear of the first word. That column is what the reservation *is* now, which is why the old
"the label is outdented from the text column" claim had to be re-hosted: the row is not outdented, the
mark hangs in the pane's own left column.

## Q. One mark for the open conversation, readable in both palettes

The active row wore a left bar *and* a fill for one fact. The bar drew a stroke that lined up with
nothing, and the fill under it was so close to the hover fill that a list of nine conversations could
not be read for which one was open. The bar is gone and the fill is stronger:

```css
.sessrow.active { background: color-mix(in srgb, var(--color-shell) 34%, var(--color-paper)); }
```

### A colour check that graded NaN

The first version parsed `getComputedStyle(...).backgroundColor` for `rgb(...)`, and this engine
answers a `color-mix()` as `color(srgb 0.922824 0.876235 0.809804)` in the light palette and
`oklab(0.909909 0.005904 0.025468)` in the dark one. The parse returned `NaN`, `NaN < 8` is false, and
the group reported:

```text
active row ok (no bar; fill NaN of luminance from a neighbour and NaN from the hover fill …)
```

Green, on a colour it had never read. The parser now handles `rgb()`, hex, `color(srgb …)` and
`oklab()`, converts to one quantity (oklab L × 255 — an sRGB luma and an oklab lightness are not the
same number), and anything it cannot read is a **failure naming the string**.

### Grading the file, not the running app's mood

The next version flipped `data-theme` at runtime and re-read the row. That measured the engine's colour
cache as much as the design: in a window the compositor had stopped painting, no frame arrives to drop
a cached resolution, and the check read the *light* palette's fill against the *dark* desk — 179
points apart, which is arithmetic rather than ink.

So the claim is made against the shipped rules: read the declaration out of CSSOM, resolve it inside a
throwaway element under an ancestor carrying `data-theme`, for both palettes; and separately assert
that the row on screen paints what the rule says, so a later rule of the same specificity cannot
quietly win. That also documents a build artefact: the transform emits a plain-colour fallback for
every `color-mix()` and keeps the real one inside `@supports (color:color-mix(in lab,red,red))` with
the same selector, so "the declared value" means the last one in source order.

```text
active row ok (one mark, no bar, and the row on screen paints what the rule says — light:
color(srgb 0.922824 0.876235 0.809804) is 11 from the desk (rgb(242, 239, 231)) and 15 from a hovered
row (rgb(247, 245, 239)); dark: color(srgb 0.336314 0.289726 0.231216) is 53 from the desk
(rgb(25, 24, 22)) and 40 from a hovered row (rgb(38, 36, 31)))
```

The light palette is the tight one — 11 from the desk, 15 from hover — and the thresholds sit at 10 and
8: "noticeably apart", with a little room, and deliberately not "arithmetically distinct".

## R. Two ways a stylesheet scan can lie

### `if (rule.cssRules) continue` skips every rule on the page

Both new checks walked the sheet with what looked like the obvious grouping test. Since CSS nesting, a
**style** rule answers `cssRules` too — an empty list when it nests nothing — so that branch recursed
into every rule and examined none of them. One consequence was loud, one was silent:

```text
SELFTEST FAIL: the stylesheet declares no background for `.sessrow.active`, so the open row is marked by nothing
```

with the rule sitting in the same sheet; and the hover check passing, having found no fills at all.
The loops now examine the rule *and* look inside it. A scan that finds nothing has to be able to say
so — `row-active` fails when a declaration it needs is missing, and that is the only reason this
surfaced instead of shipping.

### My own splice put two copies of a group in the suite

One edit used `s[:i] + new + s[j:]` where `j < i`, duplicating the span between them. The tell was in
the harness's summary:

```text
TARGETED 2 named group(s) ran, 57 not run: ... files-end, files-end ...
```

The "these ran / these did not" list is a checksum on the suite itself, and it is worth reading even
when the run is green.

## S. A blank transcript is a seam, and the counter next to it names it

Fixing something else here uncovered the real version of an old symptom. `resyncView` re-reads a page
that comes back empty, because the Rust core writes a turn's rows at turn end and a read issued
mid-turn can land after it: painting that empty page wipes a conversation that exists, and nothing
repaints it. That is "I sent the first message and the screen stayed blank."

One extra read was not enough — a second read issued in the same breath as the first can come back with
the same answer. What bounds the polling honestly is the counter the row carries next to the
conversation's name:

```ts
const expected = sessions.find((x) => x.id === id)?.message_count ?? 0
const until = Date.now() + (expected > 0 ? Math.min(2400, 300 + expected * 120) : 150)
```

A conversation the sidebar says holds four messages gets a few hundred more milliseconds than one it
says holds none. The counter is a hint about *how long to wait*, never a substitute for the page: a
conversation created by the very send being loaded is not in this window's list at all. When the reads
still come back empty against a non-zero counter, the note says so, because that is exactly the seam a
blank transcript comes out of:

```text
resync: conversation 3 looked empty and was not — 2 row(s) came back on read 2
resync: conversation 4 still reads empty after 15 more read(s) while its row says 4 message(s);
showing it empty, and this is the seam a blank transcript comes out of
```

## T. The scroll box that never noticed the window

The transcript keeps its own picture of where the reader is: `pinned`, `away`, the way-back control's
`Npx below the bottom of the view`, the cup's band, the pending jump distance. All of it is recomputed
in one place — a `ResizeObserver` that watched **the transcript's own box**.

That observer fires when a model talks. It does not fire when a *window* changes shape: a resize or a
fullscreen changes the scroll box's `clientHeight` while the content's `scrollHeight` is unchanged. The
layout recovered on its own — the browser re-lays-out and clamps an out-of-range `scrollTop` — while
every number the app kept about the reader's position went on describing a box that no longer existed.

`watchHeight()` observes the scroll box as well:

```ts
const sc = this.hookScroller()
let lastBox = sc?.clientHeight ?? 0
const ro = new ResizeObserver(() => {
  const box = sc?.clientHeight ?? 0
  const boxChanged = box !== lastBox
  lastBox = box
  ...
  if (boxChanged && sc && this.pinned) this.follow('chrome')
  this.measureFollow()
})
ro.observe(this.root)
if (sc) ro.observe(sc)
```

The `pinned` branch carries the decision: a reader who was at the latest, and whose box then grew, is
no longer at the latest. What they chose was "watch the newest line", not "sit N px from the top of a
particular window", so they are followed back. A reader who had scrolled away is not moved.

**What the probe did and did not prove.** A group drove the real window
(`selftest_resize_window`, gated by `selftest_armed`) through shrink → bottom → fullscreen → restore.
It never reproduced "the top is cut off" as a layout fault: `scrollTop = 0` brought the top back every
time. What it did show — and what the permanent `scroller-shape` group asserts — is that the app's own
reading of the position was stale. Saying which of the two was broken matters: the report described a
visual symptom, the demonstrable fault was bookkeeping, and the fix addresses the bookkeeping. If the
symptom returns, the claim that would catch it is already in the suite, and it reports its own blind
spot rather than a number:

```text
selftest note: the resize-shape claim was not measured — the transcript box is not laid out (clientHeight 0)
```

## U. Claims that were grading the window manager

Three suite failures turned out to be measurements of the desktop, and each was re-hosted rather than
loosened:

- **A structure claim sampled the wrong block.** The table/heading check took
  `all[all.length - 1]` of `.blk-answer .blk-body` across the whole document. In a 168-block transcript
  that is whatever the paging happened to leave last, so it reported "the heading never became a
  heading" with a trace quoting an older answer. It now samples the turn the ask started.
- **A paint witness has to be the turn's, not the run's.** `unpainted()` counts the whole run, and a run
  can collect a dozen frames early and then go dark for the turn under test — the claim was graded
  against that darkness. The guard is the paints *this turn* delivered (`lastTurnFrames === 0`), which
  is what the neighbouring bullet check already used. When the structure exists at settle, the settled
  half is still asserted; only the mid-stream moment is written off, with a note that says so:

  ```text
  selftest note: skipped this turn's structure claims; it painted 0 frames from start to settle, and at
  settle the block held 0 heading(s), 0 table(s), 2 list item(s) - markdown is parsed on the frame
  clock, so a turn nobody composited has no clock to parse on. Keep the window visible and this bites.
  ```
- **"Opening a conversation repaints the transcript" needs frames too.** The prologue polls 3s for a
  text change; a page nothing composites applies mount effects late. The poll now counts frames in its
  own window, so the failure says whose answer it is (`… (70 frame(s) ran while waiting)`) and a zero
  frame window is a named skip rather than a bug report.

`reveal`'s word claims had the same flaw in miniature: they were skipped on "`frame()` returned true",
which means *at least one* paint. One stray frame in an un-composited window is enough to grade spans
frozen at their start values — `a settled word is still under a filter: blur(0.001965px)`,
`a word from an earlier arrival sits at opacity 0.972074`, `10 word spans are still half-drawn after
400ms`. Those are an animation that never advanced. The claims now need a clock seen to tick (a few
paints across the streaming loop; three paints in the ghost-tail window).

The desktop, meanwhile, took its own price: on Wayland/GNOME an application cannot take focus back —
`focused=false visible=true minimized=false` — and WebKitGTK stops painting an unfocused window. The
harness asks once, up front, says what it needs ("clicking the cwoffee window once does it"), and when
it does not get it, fails itself instead of reporting green:
`7 geometry check(s) were skipped … Keep the window visible and re-run.`

## Lessons

- **Two records, one word.** Dedupe where the reader reads. `report.md` written and `report.md` handed
  over are two facts in the store and one name on screen; keeping the hand-over copy is a product
  decision, not a rendering one.
- **A file is part of what the turn said.** Not a footnote to the sentence that mentioned it, and not a
  note against the right edge: end of turn, above the row that acts on it.
- **The rule responsible is rarely the rule with the matching name.** Cascade, specificity and source
  order are the question a check has to answer.
- **A guard has to be shown to bite.** The hover check was run against a build with the fix deleted
  before it was believed.
- **`NaN < 8` is false, so a broken parse is a green test.** Report the string you could not read.
- **One quantity, or the comparison grades arithmetic instead of ink.** The same token answers
  `color(srgb …)` in one palette and `oklab(…)` in the other.
- **Grade the file, not the running app's mood.** A runtime theme flip measures the engine's cache.
- **A style rule answers `cssRules` too.** "Recurse *or* examine" is wrong under CSS nesting; examine,
  then recurse — and make a scan that finds nothing fail.
- **The suite summary is a checksum on the suite.** `files-end, files-end` in the "not run" list was my
  own splice duplicating a group.
- **Scope a claim to the thing it asked about.** "The last answer block in the document" is a different
  answer every time the paging order changes.
- **A witness has to be taken from the same window as the claim.** A run-level paint count cannot
  witness a per-turn animation; a global "did anything paint" cannot witness "did *this* turn paint".
- **A counter beside a record is a bound, not a source of truth.** The sidebar's message count says how
  long to keep reading an empty page; it never says what to paint.
- **Watch the box, not only the content.** Anything computed from a scroll position is a claim about two
  boxes, and the outer one changes on its own.
- **"It never reproduced" is a result and has to be reported as one.** The layout was not broken; the
  bookkeeping was.

## Batch seven, second pass — the person's own window says "not rendered" and "cannot click"

Tested by hand at the desktop, the report was two items: the working mark is missing to the left of
the grouped actions ("not rendered at all, or not added yet?"), and a folder's colours cannot be
picked ("cannot change (cannot click the colors) colors of workspace"). Both were real, both were
invisible to the suite, and each had a different reason for being invisible.

### The mark was in the page at 0×56

The element existed, had `display: block`, had `width: 56px` declared, and measured **0 wide by 56
tall**. The page-wide reset `img, video { max-width: 100% }` resolves a percentage against the
containing block, and the mark's containing block is the marker row — deliberately zero wide, because
it is a gutter slot and not a column of text. 100% of nothing is nothing: the declared width lost to
the reset, and height, which has no percentage in it, came through untouched. The most diagnostic thing
about the box was that only half of it was missing.

The CSS comment beside the transcript's cup had *already described this exact trap* — "100% of the
containing block is 0px and the image vanishes while its height goes unhurt" — and had chosen to leave
the reset in place for the cup, which is right for an image in flow. I wrote the bug anyway, in the
new absolutely-positioned picture two rules away, in the same zero-wide row. A comment that explains a
trap is not a guard against writing it again.

Lifted for this element only: `.turn-who > .turn-working { max-width: none }`.

`mark-visible` is a new group, and its point is that a DOM query cannot answer "is it on the screen".
It grades the painted box against the thing beside it: `display` is not `none`, the rect is a real
56×56, it stays inside the card that clips it, it does not reach into the grouped actions next to it,
and its ink is readable against the paper behind it — the artwork's own pixels drawn to a canvas and
compared to the resolved background, on a linear-light scale, because a thin brown line drawing on a
dark paper is a state nobody can see. Measured: `ink rgb(159, 122, 82) against rgb(253, 252, 249)`,
0.75 apart in light, ~0.20 in dark.

### A field dropped at the desktop seam

The colour press worked on the store side and did nothing on the desktop. `crates/tauri-app` maps the
engine's `WorkspaceEntry` into its *own* `WorkspaceRow` struct, and that struct had `path`, `label`,
`sessions` — and no `color`. The gateway serializes the engine's entry directly, so the identical press
on a phone wrote the colour, read it back, and drew it. One record, two transports, one of them blind.

So the person's "cannot click the colors" was accurate as an experience and wrong about the cause: the
swatches were clickable (a hit-test says so: `document.elementFromPoint` at each swatch's centre answers
to the swatch), and the press wrote. What was missing was any consequence. A control whose effect the
window cannot see is an unclickable control as far as a person is concerned, and the check now grades
the consequence rather than the click.

`ws-color` (new group): open the folder's menu, count the swatches, hit-test each one, press one, read
the list the *store* answers with, then read the row's folder glyph and compare the colour it actually
paints with the hex the record holds — and put the folder's original colour back whether the claims
pass or fail. Two Rust tests on the seams: `a_folder_row_reaching_the_window_carries_every_name_the_window_reads`
(the desktop's wire row must carry `path`, `label`, `color`, `sessions`), and
`a_folder_colour_reads_back_on_the_sidebar_list` (gateway: set, read, clear, read).

### The mark was free while it was invisible

Once it painted, it cost. On the 168-block / 635,054-character history the `gutter-crowd` group builds,
a streaming turn measured **30.0ms and 37.0ms per frame** with the mark displayed and **21.0ms** with it
hidden — a new feature that pushed a long conversation under 30fps, which is the very report the mark
was commissioned to answer. `will-change: transform; transform: translateZ(0)` on the mark brought the
same measurement back to **19.0ms and 21.0ms**: the animated picture composites on its own layer instead
of repainting the transcript under it, and the cost becomes the cost of the history it sits in. The
element exists only while a turn is arriving (`workingImg` makes it, `dropWorking` removes it), which is
the one use `will-change` is for. `mark-visible` now fails if the promotion is removed, so a later
tidy-up cannot quietly give the frame drop back.

### Three selftest honesty fixes, all from the same run

- **`titling`'s ordering claim had a clock that could miss the event entirely.** The check sampled the
  app's `busy` flag every 10ms; a mock turn of 36 characters finished inside one sample gap
  (`frames=1`), and the failure read "no clock for the ordering claim: … this page first saw the turn
  running at 0ms" — a description of its own sampling. A check can now install a witness that the
  shipped `turnStart` signal fills (`edgeWatch`), so the fallback is the event rather than a poll; if
  *neither* caught it, the failure says the app ran a whole turn without ever telling this page it had
  started, which is a product fact worth failing on.
- **`tool-card`'s guard contradicted itself.** `the transcript is hidden or belongs elsewhere while the
  layout checks ran (viewId=3, want 3)` — the ids were equal. Two problems, one sentence, and the
  sentence named the wrong one. Now they are separate claims, and the "hidden" one names *why*:
  empty mode is the app being honest about a conversation the record says has no rows.
- **`mark-visible` needs its blind skip.** A 0×0 rect on a window nothing is compositing is an artifact
  of the run, so the group waits for a laid-out box and, if it never gets one, says so and counts a
  skip instead of grading nothing.

### The run's real limitation, recorded

This desktop loses the app's focus every ~20 seconds during a harness run, and an app cannot take focus
back on Wayland. The harness re-activates the window (30 attempts) and several geometry/paint claims
still went blind in the full run. Cadence and colour claims measured green in focused runs; `paste-send`
needs a focused run to prove its on-screen receipt, and the focus problem is why it has not been green
twice in a row here.

### What this pass taught

- **An element is not a picture.** "It is in the DOM, it has a width, it is displayed" can all be true
  of something that measures zero. Claims about what a person sees have to be about geometry and paint.
- **A percentage of nothing is nothing.** A reset that is correct for flow is a trap for something
  absolutely positioned in the same zero-wide parent, and a comment describing the trap does not stop
  you writing it again.
- **A field dropped at a transport seam is a feature that works on one device and not the other.** Two
  clients of one record disagreeing is the failure this project already treats as the important one; the
  fix was a field, the guard has to be on both wires.
- **Clickability is a consequence, not a hit target.** The swatches were hittable and the write landed;
  what made them feel unclickable was that the window could not see the result. Grade the consequence.
- **Measure the cost after the feature is visible.** The mark had been free while it was invisible, and
  the honest number only existed once it painted.
- **A claim that samples a flag can miss the event.** If the app has a signal, let the check witness the
  signal; keep the sample, and make "the app never told the page" a failure that means what it says.
- **A failure message that prints equal numbers is not describing a mismatch.** Say which of the two
  things you mean, and say when the run could not have known.

### The third bug: a 0-row page painted over the message you had just sent

`paste-send` kept failing with the picture missing from the bubble, and the failure text could not say
why. Once the failure described the page it had graded, it said the quiet part: *the on-screen
transcript had zero turns while this window's own send was in flight*. Then the transcript's own clear
trail — `Transcript.reportClears`, armed by the page harness only, naming every wipe — showed the
culprit in one line:

```
transcript clear: start screen              (the check's own newSession)
send: host is on 20, moving it to the viewed session 21
start_turn (desktop): session=Some(21) attachments=1
history: 0 row(s) for session 21
transcript clear: page paint               ← here
turn sent with 1 attachment(s)
```

The rust core writes a turn's rows **when the turn ends**, so a `session_messages` read issued mid-turn
honestly answers "nothing". The app was painting that nothing over the bubble it had just drawn,
thumbnail and all, for a second or two — a self-inflicted version of "I sent it and it isn't there",
caused by the app reloading its own view.

Two changes, both about not blanking a view that is not empty:

- `loadTranscript` holds an empty page when `turnSessionId === id`: keep what is on screen, log
  `conversation N reads empty while its turn is in flight; keeping what is on screen`, and let the
  caller's poll paint the rows when they exist. Painting nothing is only honest for a conversation that
  is actually empty, and "a turn of mine is running in it" is the app's own answer to that question.
- `resyncView` clears early only when it is *navigating to a different conversation*. A repaint of the
  conversation already on screen keeps its content while the page is read — `loadHistory` clears what
  it replaces anyway, so the early clear bought nothing but a gap. `loadTranscript` also stopped
  zeroing `viewMsgCount` on the way in, which was a second route to an empty screen while a page was
  merely being fetched.

Also worth keeping: the diagnostic that made this findable. A loaded session logs its page; a wipe
logged nothing, so "the transcript went blank" could only be inferred from which lines happened to sit
nearby. Naming every wipe, in a check run only, turned an inference into a log line.

### Two harness lessons from the same chase

- **A claim about a view has to ask for the view.** `scroller-shape`, `row-active` and `mark-visible`
  were skipping whenever an earlier group happened to leave the app on its opening screen — green in one
  run, blind in the next, with the skip note blaming the compositor. They now call
  `ensureTranscriptMounted()` and measure. A skip that is really "the setup didn't happen" is a skip
  that hides the claim, not one that reports it.
- **A blind failure needs the state it graded.** `the message on screen carries []` was technically
  true and useless: no bubble, a bubble with a text pill instead of a picture, a bubble repainted from
  the record, and a chip belonging to another conversation are four different bugs. The failure now
  prints the page (`blk-user`, `attlist`, `turns`, empty-mode, which conversation is open) alongside the
  record's own attachments for it.
## Batch eight — the working status went inline, and the cup at the end of the transcript left

Three asks: make the loader smaller and inline with the grouped action header, put the same loader in
the composer's send button without expanding it, and delete the coffee cup at the end of the transcript
because the loader now says "it's working" and two looping pictures in one window is one too many.

### The header is not a home for the commonest turn

"Put the loader in the run header" sounds complete: a turn with tool calls or thinking has a header, and
a header is a line the reader's eye is already on. It failed on the first fixture that mattered:

    SELFTEST FAIL: a heading answer has no working mark either

A plain answer — no tool calls, no reasoning — has no `.blk-run > .blk-head` at all, and a plain answer
is the commonest turn the app writes. So the mark has two homes and `syncWorking()` decides between them:
the header when there is one, otherwise a loose `.run-working.turn-tail` after the last block written,
placed before the turn's exits (`.attachline`, `.fileturn`, `.turn-acts`) so a file row never ends up
under it. `runWorking()` removes a loose mark the moment a real header appears: one loader per turn, in
whichever line is actually being written.

The tail mark needed `animation: none`. `.turn > *` runs `cwoffee-rise` with `both` fill on every child
of a turn, and a fill that holds the animation's end state overwrites a status mark's own declared
opacity and its layer promotion — a direct child of the turn silently loses the two properties that make
it cheap and legible.

### A mark inside a filled button has to be tested against that fill

The composer's send button is a 32×32 ink square in light mode and a paper square in dark. The loader
goes inside it at 20px, `position: absolute; inset: 0; margin: auto`, so the button's own metrics do not
move — the claim measures the button and the strip around it before and during a turn and fails on any
growth.

Contrast was the interesting part. Measuring the mark against the *paper* passed while the button's fill
made it unreadable, and measuring the artwork's mean ink passed while one of its two hues was invisible:
a slate hue and a terracotta hue average into a number that describes neither. Sampling per hue, against
the fill, before any filter, was 2.72:1 on the light button and 2.42:1 on the dark one — under WCAG's 3:1
for a graphic. `filter: brightness(1.25)` in light and `brightness(0.45)` in dark, and the selftest
applies the same scalar to the pixel samples it grades, so a filter removed from CSS cannot quietly take
the mark back under the threshold. Last run:

    send button loader contrast: worst 4.03:1 against rgb(43, 42, 39) (filter brightness(1.25),
      dark hue rgb(126, 139, 129) 4.03:1/35px; light hue rgb(238, 164, 90) 6.90:1/67px)
    send loader ok (32x32 button unchanged through the turn, 20px mark inside it over 178 painted
      frame(s), stop square still painted, labelled Stop while busy and Send once settled)

The two clocks in that second receipt are deliberate. A timer samples geometry even when the compositor
is occluded, and a `requestAnimationFrame` counter records whether the mark ever *reached the screen*;
when frames are zero the group logs a named note ("measured by geometry only … its painted state was not
witnessed") instead of claiming it saw the animation.

### The cup, and what its removal had to keep paying for

`CUP_ART`, `cupImg`, `placeCup`, `repaintCup`, `stopCup`, `--cup-band: 168px` and `--cup-left` are gone,
and the scroller's bottom padding went back to the ordinary `var(--scroller-pad-top)`: 168px of reading
surface came back per conversation. The follow, jump and file-ordering claims were retargeted rather than
deleted, and now fail if `.transcript-cup` comes back — a removed feature needs a claim that it stays
removed, or a later "restore the cup" patch looks like a clean revert.

### The still was corrupt, and only a second decoder noticed

The reduced-motion still used to be frame zero of the loop, carved out. The inline loop's frame zero is a
**delta at (4,3)**, not a full-canvas frame, and delta image data only means something inside its
animation container. Pillow happily wrote the file and happily read it back; `identify` said
`corrupt image`; the app's own engine said it did not decode. So:

- `scripts/mark-resize.py` takes `SOURCE LOOP SIZE STILL` and writes both files from one resized frame
  set — one resample, one size decision, and both outputs checked with an independent decoder before the
  script is allowed to say it succeeded.
- `scripts/webp-still.py` refuses a first frame with a non-zero offset:
  `frame 0 is a delta at (4,3); carving it out would corrupt the still`.

    loader art ok (/cwoffee_turn_working_inline_1.webp: 75 frame(s), lap 2500ms, 40px canvas for a 20px
      box at DPR 2.00, 64KB)
    loader still ok (/cwoffee_turn_working_inline_still.webp: one frame, 40px, 1KB)

`mark-art` grades the shipped bytes rather than the source artwork: what CSS box the loader actually
resolves to (which is how the `0x56` trap would be caught here as well), the loop's frame count and lap,
the canvas against the box at this window's DPR, and whether the still decodes. `assetBytes()` was added
to `anipause.ts` for exactly one reason: `aniBytes()` refuses a file whose frame table it cannot read,
which is right for re-arming a loop and wrong for verifying a still.

### Three claims that were grading something else

- `reveal` reported "only 0 words were ever mid-fade: the reveal is invisible" on a working reveal. It
  sampled once after `frame(150)`; a 130ms fade can be finished by then, and the poll's phase decided the
  verdict. It now runs a frame-clock sampler of its own and grades the peak number of simultaneously
  mid-fade words — and a blind skip needs *both* its deltas and its sampler to have seen zero paints.
- `thought-line` claimed the thinking header should outdent from the column it labels. A thinking line's
  header is not a gutter label; it is the line the thought is written on, inside the accordion. The
  outdent invariant moved to `turn-shape`, where a user bubble really does have a sticky label — and that
  claim was rewritten to the CSS's actual design (above the body, inside the scroller's clip box, sticky,
  with a readable band) rather than a horizontal outdent that was never true.
- `ui/perf/probe.ts` followed the cup. It follows the loader now, and says so plainly when no loader is
  on the page because every turn has landed.

## Batch nine — shorter chrome, no folder row in a chat, and a dot that would not go

### 44px of titlebar, and where the 8px went

The chrome row carried `h-11` in markup, which is 44px, for three things: a drawer toggle below 768px, a
wordmark, and the conversation's name. It is now `--titlebar-h: 36px` in the stylesheet and nothing in
markup, because four places measure that height — the row, the narrow-width drawer that starts under it,
the settings dialog that has to fit under it, and the check that grades all three. A fifth literal is how
those drift apart; the drawer's `top` and the dialog's `calc(100vh - var(--titlebar-h) - 16px)` are
resolved through the browser rather than string-matched.

The height only counts if something receives it, so the claim is about that:

    chrome ok (36.0px row from --titlebar-h 36px, its controls inside it, sidebar and pane starting on its
      bottom edge with 10px of card gutter and 0px spare below them; drawer top and the settings dialog
      resolve to the same height)

The first version of that claim failed with `the card starts 10px off the chrome row's bottom edge`, which
was the card's own gutter inside its pane — a different 10px, and not the chrome row's business. The pane
is what has to start on the row's bottom edge; the card is allowed its margin. And "0px spare below them"
is the actual point of the request: the freed band became room rather than a gap.

### A chat does not need a line explaining that it has no folder

Chat mode had replaced the workspace picker with a row that said "Chat has no folder, and no tools to use
one with". That was written to defend against a gap — "not hidden, because the space where the folder used
to be would otherwise read as a bug" — but a sentence under the composer is still a line of the card, and
what it explains is a thing the mode already says: a chat has no tools, so there is no folder to choose
and nothing to choose it for. The row is gone in chat mode, and the dead `.wsrow.ws-none` rule went with
it; a rule that styles nothing is a claim about a design that no longer exists.

The chat-mode check now rejects both shapes of the old build — a control that picks a folder, or a
sentence standing in one's place — and `panecard` requires the folder row only outside chat mode, so the
removal cannot be read as a broken card.

    chat list ok (1 chat row(s) drawn, no folder tree, no approvals control)

### The unread dot was marking the wrong conversation

"I cannot mark it as read even opening it and scrolling to the bottom, it doesn't go away." Nothing about
it was about scrolling. The dot compared a conversation's stored message count with where that
conversation was last read, and "last read" was keyed on `hostState.session_id` — which is not the
conversation on screen. Opening a conversation is a *view*, not a switch: `adoptHostSession` says so out
loud, because a watch-only client must not move the engine by loading a page. So the host could be inside
conversation 7 while conversation 12 was what the reader was looking at, and the marker said 7 had been
read. However much of 12 you opened, its dot stayed; some conversation you had not looked at got cleared
instead.

Three changes, in the order the bug needed them:

- `readHere = viewId ?? 0` — the conversation on screen is the one being read. No fallback to the host:
  on the start screen nothing is being read, and clearing a dot for a conversation nobody is looking at
  is the same mistake in the other direction.
- `markViewRead()` runs at the end of `resyncView()`. A poll tick is not an answer to a click; waiting
  for the next tick is what made the mark look broken from the chair.
- `loadSessions()` now calls `loadSeen()` first. The read marker is a record in storage, not a fact about
  this window, and boot used to be the only time it was read — so a window's copy drifted from the stored
  one for the rest of its life.

The check had to be built carefully, because every shortcut in it would have made it false:

- the marker is written through `SEEN_KEY`, the same key the app writes, rather than by poking component
  state — a claim about a record should set the record;
- the check *looks away first* (`showStartScreen()`), because it starts a turn to make the conversation,
  and a turn moves the view to what it grew — claiming "unread" while reading it would be the opposite
  bug;
- it waits for the dot to go rather than grading the class the instant `viewId` changes: the view name
  moves before the transcript is loaded, and the first version of this check failed on that race, not on
  the app;
- it puts the person's own marker and view back in `finally`, passed or failed — a suite that leaves a
  read marker moved tells the next start of their app that a conversation they never read had been read.

    unread dot ok (2 message(s) ahead of a marker at 0 drew the dot on "Selftest title, typed in";
      opening conversation 40 cleared it, wrote the marker at 2, and a refresh kept it clear)

### Lessons

- A status mark inside a filled button is graded against that button's fill, per hue. Paper contrast can
  pass while the mark is invisible, and an average of two hues describes neither.
- "Freed height" is only freed if something receives it. The number on the bar is the easy half; the
  claim is that the columns start on the bar's bottom edge and reach the window's bottom.
- When a row is deleted because it explained an absence, delete the rule that styled it too.
- The app's own record is the only honest way to set up a claim about a record. If the test cannot make
  the state through the record, the record is the thing that needs fixing.
- A view name changing is not the page landing. Do not grade a class on the click when the effect happens
  on the load.
- A selftest that mutates a person's state restores it in `finally`, passed or failed.

## Full pass, both cores (batch eight and nine)

Rust core, full suite: one failure, and it was the window, not the app —

    SELFTEST FAIL: 27 geometry checks were skipped: the window was off-screen for most of this run,
      so nothing about layout was verified. Keep the window visible and re-run.

and the substantive receipts in that same run:

    working mark ok (run header 29.0px clear of the text, plain answer's loose loader 36.6px below the
      first line and on the column, and none at all once the turn has landed)
    a thought is one line and the answer keeps its outdented label
    turn shape ok (2 answer blocks and 2 pieces of working in 1 accordion, no assistant label, no
      generated marker on 8 label(s), 1 name for 2 of your messages, your ask under the divider)
    opening screen ok (greeting and composer 26px apart, the pair 21px off the card's middle, the pair
      spans 581px of a 717px card)
    card ok (one surface 900x717 inside a 932px pane, holding conversation + composer + folder row;
      16px of gutter; the same card on the opening screen at 900x717)
    window surface ok · drawer surface ok · active row ok · turn files ok (two receipts)
    chrome ok (…) · unread dot ok (…) · loader art ok · loader still ok

Pi core, full suite, window composited: `SELFTEST ok`, with `mode toggle ok`, `chat list ok`,
`web grant ok`, `send loader ok (… 178 painted frame(s) …)`, and one geometry check skipped.

`cargo test --workspace`: 656 passed, 0 failed. `wire-names`: clean (two pre-existing notes about
desktop-only verbs). `svelte-check`: 0 errors, 0 warnings. `vite build`: `index-p1hvjwir.css` 90.64 kB,
`index-Ci2g5Spn.js` 306.76 kB. `install-desktop.sh --core pi` installed that bundle, and
`install-desktop.sh --check-only`:

    launch check: OK (gio launch ~/.local/share/applications/cwoffee.desktop)
      app says: [cwoffee] engine up: core=pi persona=chat url=http://127.0.0.1:8090/v1
        model=qwen3.8-flash-next-iq3_s workspace=… mode=Auto reasoning=XHigh
      app says: [cwoffee] render OK: titlebar=true sidebar=true sidefoot=true stream=true empty=false
        overlay=false text=4566 bg=rgb(253, 252, 249) desk=rgb(242, 239, 231) dpr=2.00 cup=none

## Batch ten — one message on the busy button, a loader in the gutter that stops resetting, and two lines that only existed in the installed bundle

The report was three screenshots of the app the desktop actually launches. That bundle was batch
nine's, so two of the three complaints ("a chat says it has no folder", "a chat says
`cwoffee-workspace`") had already been removed in the tree and were still being drawn by the running
window. The fix for those was a claim rather than a change: the chat check now reads the pane's
text, not just the composer's selectors, so either sentence anywhere in the pane — or in the second
composer the opening screen keeps — fails the run:

    SELFTEST FAIL: chat mode still says "Chat has no folder" in the pane — a folder line, or a copy
      of one, is still being drawn

The third complaint was real and visible in both: the send button was drawing the stop square *and*
the working orbit in the same 14px glyph, which is two sentences in the space of one — "this is
working" and "press to end it".

### A control with two states should show one of them

The busy button now shows the mark at rest and the square on `:hover`, with `:focus-visible` beside
the hover rule. The focus half is not decoration: a hover-only affordance is invisible to the person
who arrives with a keyboard, and the square is the only thing that says this control ends the turn.
Nothing in this suite can put a pointer on a button, so the runtime half measures the rest state
(mark lit, square at opacity 0) and the reveal half reads the shipped rules: a rest rule that hides
the square, and a rule per reveal that brings the square back *and* takes the mark away. Revealing
without dimming would restack the two for whoever hovers, which is the thing being objected to:

    send loader ok (32x32 button unchanged through the turn, 20px mark inside it at opacity 0.95 over
      170 painted frame(s), stop glyph held at opacity 0 until hover or keyboard focus, labelled Stop
      while busy and Send once settled)

### "It resets when something gets appended" was three bugs, not one

The obvious reading — the animation restarts — was true, and its cause was the box, not the file: as
a flex item in the header row the mark's box was re-laid-out every time the header's text changed, and
an animated raster whose box is rebuilt starts again at frame zero. `position: absolute` at a fixed
offset gives it a box that nothing in the turn can change. Then two more, both found by measuring:

- `.blk > .blk-head` exists to put a block's *label* in the gutter, and it carries
  `position: absolute; left: -66px; top: 2px; width: 58px`. The run header overrode the position and
  the display but not the geometry, so it kept a label's 58px box — a sentence clipped to 58px with an
  ellipsis. With `position: relative` it also inherited the -66px shift, which is what made the
  mark's offset unreadable from the CSS alone. Three resets, and a claim that measures the header
  against its own block's content column.
- This WebKit answers `translate: 0 -50%` by ignoring it, so the mark hung half its own height below
  the row's centre. `top: 50%` plus a negative `margin-top` is the oldest way to centre an absolutely
  placed box, and it composes with the `transform` that carries the layer promotion instead of
  competing with it.

Moving the mark into the gutter is arithmetic against the clip box, and the numbers came from a probe
rather than from the comments: the scroller reserves 80px and the card clips at its own edge, so
`left: -60px` puts the mark 26px clear of the block's rule and 40px inside the card. The first
attempt, `-54px`, put it outside the card and the claim said so.

    working loader ok (painted at its declared box, hung 26.0px clear of the block's rule and inside
      the card, on the header row's centre, the header spanning its block, one line row, the same
      element through the next step, and gone when the turn landed; the narrow-width offsets are
      graded as shipped rules rather than measured, because this suite cannot resize its own window)

Keeping the mark last is what lets it never move: `appendToTurn` and `addFiles` insert before it
rather than after, so nothing has to be re-parented. The claim that grades this appeared only after
it was written against something real — streamed prose continues the block that is already open and
adds no node at all, so a paragraph append proves nothing about order; files do add nodes, and that
claim caught `addFiles` dropping the strip *under* the loader:

    SELFTEST FAIL: 1 node(s) sit after the loose loader, first is <div class="filestrip"> — the
      loader is no longer the last thing in the turn's writing

### The web switch stopped narrating itself

Turning Web on drew a sentence under the composer saying the model would be given `web_search` and
`web_fetch`. The chip on the control that was just pressed already says `on`, and its tooltip already
names the tools. A switch that worked needs no prose; a switch that was *refused* does, because
nothing else on the screen says the press was refused — so the note is now only ever a refusal, and
the host's success receipt goes to the log where it can still be read.

### Lessons

- A screenshot of a running app is a screenshot of the installed bundle. Check which one you are
  looking at before changing code, or you will "fix" a line that is already gone.
- A geometry claim written from comments about the layout is a guess. Measure, then write the number.
- When a rule is inherited from a rule written for a different thing, resetting the position is not
  enough: the geometry travels with it.
- "The animation resets" is usually about the box. Ask what re-lays-it-out before touching the file.
- A selftest cannot hover. Grade the reveal against the shipped rules, and grade the rest state from
  pixels — and say which half each claim is.
- An append that updates an existing node cannot prove an ordering invariant. Pick the append that
  adds a node.
- A claim that nothing is drawn is a claim about text, not about selectors: a copy of the sentence
  under a new class passes a selector check.

### Receipts

Targeted (`chat web-grant reveal send-loader mark-visible panecard`), pi core, window frontmost:
`SELFTEST ok`, with `chat list ok (1 chat row(s) drawn, no folder tree, no approvals control)`,
`web grant ok (… a mid-turn flip is refused; and the grant comes back with the conversation)`,
`working mark ok (… the same element and still the last block's successor after an append, …)`,
`card ok`, and the two loader receipts above.

Falsification, five sabotages in one run, each caught by the claim written for it: the web note
back, the mark back inside the heading (`the loader's right edge is -28.0px from the block's rule
instead of clear of it`), the header mark rebuilt per step (`the loader is a different element after
a step landed: the mark is rebuilt…`), the rest rule for the square deleted (`the stop square is
painted at opacity 1 under the working mark…`), and `appendToTurn` appending back (the `filestrip`
failure above).

`cargo test --workspace`: 647 passed, 0 failed. `wire-names`: OK, same two desktop/gateway notes.
`svelte-check`: 0 errors, 0 warnings. Production bundle installed; `install-desktop.sh --check-only`:

    launch check: OK (gio launch ~/.local/share/applications/cwoffee.desktop)
      app says: [cwoffee] render OK: … cup=none

---

## Feature 1: Branch — a second path out of one message

The Branch control had been under every finished turn for months with a tooltip that read *Branch
this conversation — not built yet*. Honest, and useless: the feature a person reaches for when an
answer went somewhere they did not want it to go, without losing the path that got there.

### What it is, from the chair

- **Branch from one of your messages** (the control sits next to `Edit` under each of your bubbles):
  a new conversation carries everything **before** that message, and the message itself comes back
  into the composer so it can be worded differently. Nothing is written twice.
- **Branch from an answer**: the new conversation carries that answer and everything before it, and
  starts from there. Branching at the newest answer is a copy of the whole conversation.
- **The conversation you branched from keeps everything it has.** A branch is not a rewind, and the
  note the host answers with says so in the same breath as it names the new conversation — because
  "did I lose the original?" is the only question a person has when a second row appears.
- **The branch inherits the conversation it came from**: folder, model, persona, the chat's Web
  grant, the chosen thinking effort. A branch of a cowork conversation that suddenly has no folder,
  or a chat that suddenly cannot search, is a different kind of conversation wearing the same title.
- **The sidebar row says what it is**: *"the parent — branch of the parent"*. Two conversations whose
  first message is identical are indistinguishable otherwise, and a branch of a branch names the
  parent it was made from rather than its great-grandparent.
- **A turn that has not been stored yet cannot be branched**, and its control says so
  ("once it is stored — a moment after the turn ends") rather than branching a guess: a branch names
  the row it means, and a live turn's text is not a row yet.
- Refused while a turn is running, by both transports, with the same sentence.

### How it is built

- `crates/engine/src/branch.rs` is the whole ask: `resolve` (which row, proven), `create_row`
  (inherit + copy), `outcome_note` (what happened, in product words), `memory_note` (what the
  engine's own memory did), `apply_and_open` (busy rule, then the host). Both transports call
  `apply_and_open`, so a phone cannot branch a conversation the desktop refuses to branch.
- **Proof, not trust.** The caller names the row (`message_id`, or `from_end` among rows of a role)
  and repeats the words it painted (`shown`). If the row says something else now — another window,
  another device, the harness — the branch is refused and nothing is created. Same discipline as
  `edit_message`, for the same reason: three clients read one conversation.
- `Store::copy_through` copies the stretch by SQL rather than through `history()`: `history()`
  rebuilds rows for the model and drops what only the UI reads. The copy keeps reasoning, its
  signature and its ms, tool calls, tool meta, file badges, attachments and the search index, and
  moves the new conversation's `updated_at` to now so it sorts at the top.
- **Order differs per core, on purpose.** Rust core: create the row, then adopt the copy
  (`adopt_view!` replays the rows into the prompt and rebinds where turns persist); if adoption
  fails, the row is deleted, so no half-made conversation is left in the sidebar. pi core: adopt the
  source thread, fork pi's thread, then write the row; if the row fails, pi goes back to the source
  thread and the note says an orphan thread file exists and is safe to delete — a conversation the
  model is answering into that nothing lists is worse than a stray file.
- **pi's tree is what made `crates/bridge/src/rewind.rs` get rewritten.** Session files are trees;
  context is built from `leafId` to the root. The old prune kept the first *n* user entries **in file
  order**, which is only the conversation while nothing has ever branched the file. It now walks
  leaf→root (with cycle and missing-parent refusals), treats the header id as a tree boundary, keeps
  the whole exchange rather than just the question, and refuses a compaction whose `firstKeptEntryId`
  is off the kept chain instead of silently rewriting it.
- **A forked thread file may not exist yet** — pi writes it when the first entry is appended. The
  launch and `enter_row` now filter a mapped path through `metadata()`: handing pi a path it cannot
  open fails the launch, and a window that will not start is worse than a branch that starts as a
  fresh thread with its transcript said out loud. The note distinguishes an unspoken branch from a
  conversation imported from the other core.
- `branch_conversation` is drive-only on the gateway, and the desktop command uses
  `rename_all = "snake_case"` so `message_id`/`from_end` are spelled identically on both wires.

### Lessons

- A disabled button that says why is still the right shape after the feature ships — it just moves to
  the case where the control genuinely cannot act yet (a turn with no stored row).
- "Copied the rows" is not "branched". The claim that matters is about the model's memory, so the
  engine has to say what it did to it, and the check grades that sentence per core instead of
  assuming both cores have the same memory.
- A control painted inside a conversation addresses that conversation, so repainting a conversation
  has to move the identity along with the rows. `loadTranscript` painted rows and left the transcript
  naming the previous conversation; Copy, Edit and Branch under them addressed something nobody was
  looking at.
- A refusal claim needs to be measured before the change the world is about to make anyway. The
  mid-turn refusal was checked while the turn was still running: after it settled, the conversation
  had two more rows and the claim read as though the refusal had written them.
- A selftest needs scenarios. `mock-llm.mjs` answers an unmatched prompt with a 400, so three new
  entries went into `tests/scenarios/scenarios.json` rather than the check reusing someone else's
  scenario by accident.
- The ask said "state plainly that the branch starts cold-cache". The note names the **count**
  instead — *"its first answer is assembled from the 4 copied messages, not from what this engine
  had already loaded"* — because the engine cannot see the endpoint's cache and this app already
  refuses to put a cache assertion in a note it cannot observe (`host.rs`'s resume note is graded by
  a test for exactly that: `the resume note asserted … about a cache it cannot see`). A branch that
  copied nothing gets no such sentence; a branch cut mid-conversation may well hit the endpoint's
  prefix cache. The knowable part is how much has to be read, and the rate is already in the panel.

### Receipts

`cargo test --workspace` (guarded, `-j 2 --test-threads=2`): 670 passed, 0 failed — including
`branch::tests` (naming, refusal, copied columns, answer cut, ask handed back), four
`host::tests::branch*` (host moves onto the branch and answers from its own history, an answer branch
carries the answer, a refusal creates nothing, a conversation this host is not on is adopted first),
nine `rewind::tests` (tree cut, leaf that is not the newest entry, fold anchors, unreadable file left
alone), and the gateway parity test (`a_phone_can_branch_a_conversation_and_gets_the_new_row_back`).

Full suite on pi core and on Rust core, window frontmost, mock endpoint on 8190: `SELFTEST ok`, 0
failures, with

    branch ok (2 conversation(s) made from 36; branching your message copied nothing before it and
    handed the ask back, branching its answer copied 4 rows and asked for nothing, the source still
    holds 4 rows, a drifted branch was refused and made nothing, a branch during a turn was refused,
    and pi core said what it did to the model's memory; the sidebar calls it
    "Selftest title, typed in — branch ")

and the same receipt on the rust core with "rust core said what it did to the model's memory".

Falsification, both aimed at the halves a unit test cannot reach: a branch implemented as a rewind
(`truncate_from` the source after the copy) failed with
`a branch is not a rewind: conversation 21 held 4 rows before the branch and 0 after it` and
`answering in the branch grew conversation 21 from 4 rows to 0; the two paths are supposed to grow
apart`; an empty pi `memory_note` failed with
`the pi core made a cwoffee row without saying it forked pi's thread, which is the memory that has to
follow the copy`.

`svelte-check`: 0 errors, 0 warnings. `wire-names`: OK, same desktop/gateway notes as before, with
`branch_conversation` graded on both hosts.

## Feature 2: Turn checkpoints — undoing a turn's files, not its messages

Branch made a second path out of a conversation; this is the other half of "take that back". A cowork
turn does not only say something, it *writes*: notes, spreadsheets, patches in the person's folder.
Deleting the message leaves all of that behind, and the transcript had no way to say so. The feature
is: every file a turn overwrote is copied **before** it is overwritten, indexed against the turn that
took the copy, and offered back from the answer row that made the change — with the messages untouched.

Three rules decided the shape:

1. **The copy is taken by the thing that is about to overwrite the file.** Nothing else can see the
   previous bytes. On the Rust core that is the tool, in-process; on the pi core it is `cwtool`, in a
   separate process. Both take the copy at the same point in the same code (`ToolCtx::pre_state`,
   `checkpoint::capture`), so a turn writes the same way on either core.
2. **A copy nobody can find is worse than no copy.** The bytes live content-addressed under
   `<workspace>/.cws/snapshots/`, and the index rows live in cwoffee's SQLite. `cwtool` takes copies
   only when the bridge tells it there is a store to write rows into (`CWOFFEE_CHECKPOINTS=1`, sent
   alongside `CWOFFEE_DB`), because a snapshot with no row pointing at it is a private file in the
   folder that nobody can list, revert, or explain.
3. **Reverting files is not undoing the conversation.** The rows stay; the note says the transcript is
   untouched and quotes the message count, so the claim is measured rather than promised. A person who
   wants the messages gone has Delete, and mixing the two in one button is how a folder gets rewritten
   by someone who meant to hide a sentence.

What the card says, and why:

* **Path, kind, verb.** `created` is undone by deleting, `modified` by putting bytes back, `deleted`
  by re-creating from the copy. A card that only listed paths would leave "delete my file?" unanswered
  for the file the turn made.
* **One line per file, earliest copy.** A turn that rewrote one note six times is one decision. Dedupe
  is by content on the way in (six writes cost four copies, not six) and by path on the way out.
* **`edited since` is a flag, and overwriting is a button.** If the person changed the file afterwards,
  the default leaves it alone and the card marks it; the only way to overwrite is to press a control
  that reads `Revert anyway`. A checkbox on a card that rarely needs it trains people to tick boxes
  without reading, which is the opposite of what a confirmation is for.
* **Scope is offered, not guessed.** "This turn only" and "This turn and after" are different wishes;
  the second sweeps later turns' changes with it. Both are listed before anything moves.
* **A short list says why it is short.** The per-conversation cap evicts the oldest copies, and the
  card reports the number it dropped (`pruned`, summed from the same record that evicted them) under
  the note rather than presenting a truncated history as the whole one. It is its own line, not a flag
  on a path, because it is a fact about the conversation's history and not a choice about one file.
* **The engine re-reads its own scope.** `shown` is the path list the card painted, repeated back with
  the request; if the rows no longer match, the revert refuses and names both lists, same discipline as
  Edit and Branch — a stale window must not act on a scope nobody was shown.
* **Refused while a turn runs.** A turn writing files right now is exactly the state where restoring a
  copy restores the wrong thing.

Lessons from building it:

* **A confirmation card needs a home that exists in both views.** The first version appended the card to
  the live turn element, so pressing Undo under a row loaded from the record opened nothing — there is
  no live turn there. It now attaches to the action row that opened it (`actions.after(card)`), and the
  selftest caught that by failing on a plan it had already seen: `pressing Undo files opened no card
  naming what would be put back (the plan said ["selftest_checkpoint_note.md"])`, which is a failure
  message that names both sides of the seam.
* **Turn numbers belong to the translator, rows belong to the store.** pi never hands cwoffee a turn id,
  so the bridge exposes the translator's turn (`Output::turn()`) and records marks against it at the
  moment the call ends — not at the settle. A turn stopped partway still changed files, and those are
  the changes a person most wants undone; recording at the settle would have silently dropped them.
* **Answer-row stamping can only happen after the answer exists.** Rows are recorded per call with
  `message_id` null and stamped at the settle, the same as file chips. The engine's own test asserts the
  null-before-stamp state, because "the control sits on the right row" depends on it.
* **Snapshot directories are derived from the workspace, never stored.** `ToolCtx::snapshot_dir()` reads
  the folder the tool is running in. A stored path would survive `SetWorkspace` and hand a revert that
  restores into a folder the conversation no longer works in.
* **Blob keys are shared, so deletion is reference-counted.** Deleting a conversation drops its index
  rows and removes a blob only when no other conversation still points at it; a copy two branches share
  is one file, and deleting one conversation must not break the other's revert.
* **Eviction is batched and said out loud.** The store keeps `CHECKPOINT_CAP` rows per conversation and
  prunes in batches of `CHECKPOINT_BATCH`, recording a `checkpoint_prunes` notice so the transcript can
  say older copies were dropped instead of offering a revert that silently restores less than it claims.
* **Group grammar matters where the sentence carries the refusal.** Sabotaging the busy guard made the
  selftest print the edited-since refusal, which read "accept that 1 it stay as they are". That clause
  is the part that says what did *not* happen, so it now agrees in number with what it names, and both
  readings are unit-tested.

Receipts. Full suite on pi core and Rust core, window frontmost, mock endpoint on 8190: `SELFTEST ok`,
0 failures, with

    checkpoints ok (pi core: one turn wrote selftest_checkpoint_note.md three times into one copy,
    pressing Revert put the file back to the words it held before the turn while the transcript kept
    its 8 rows, the same card then flagged it as edited since with a "Revert anyway" button, the wider
    scope still named one path, a revert asked during a turn was refused and moved nothing, and a turn
    that touched no files refused and said why)

and the same receipt on the rust core. `cargo test --workspace`: 554 engine tests plus gateway,
bridge and store suites, 0 failures — including
`checkpoint::tests::two_edited_files_are_named_as_two_in_the_refusal` and
`agent::tests::a_turn_that_overwrote_a_file_leaves_an_indexed_copy_behind`, which drives a real tool
call through `run_turn` and proves the copy reaches the disk, the row reaches the store, and the revert
puts the bytes back.

Falsification, aimed at the claim a unit test cannot reach: making `apply()` report a restore it never
performed failed with `pressing Revert left selftest_checkpoint_note.md holding "model draft two\nthird
line\n" instead of the words it held before the turn`; deleting the busy guard failed with
`reverting during a turn answered … (running when asked: true)`. Both sabotages were reverted.

`turn-buttons` now requires four turn actions — Copy, Share as Markdown, Branch, Undo files — and
requires the undo control, like Branch, to be inert under a turn the window streamed and to say what it
is waiting for. That count is a claim about the row a reader learns to aim at, so a fifth control that
appears quietly is a failure rather than a feature.

`svelte-check`: 0 errors, 0 warnings. `wire-names`: OK, with `checkpoint_preview` and
`checkpoint_revert` graded on both hosts; the two harness-only readers
(`selftest_file_bytes`, `selftest_write_file`) print the usual "gateway has no such verb" note, which is
correct — they exist so the page-side check can read bytes off disk, and they are confined to the
conversation's folder.

## Feature 3: MCP via lazy exposure — third-party tools that cost nothing until asked

The prompt file said MCP stays unbuilt "until it costs the prompt almost nothing". Two things make a
third-party tool expensive, and they are not the same thing: a server's tool list can be hundreds of
lines of JSON schema sitting in every request forever, and the tool is *somebody else's program*
running on this machine. Lazy exposure answers the first with a budget and the second with an ask.

Three rules decided the shape:

1. **Registered is not declared.** Server tools go into the registry as hidden: `Registry::defs()`
   filters them, so a request carries no `mcp__` name until `explore` looks it up. The model sees one
   sentence in the `<env>` block naming the servers and what they offer. Env and not the system
   prompt: env is rewritten every turn anyway, so adding a server does not permanently edit a cache
   prefix that was priced before it existed.
2. **`explore` is the only door, and its promise is literally true.** `need: "mcp: …"` searches the
   host's *actual* hidden tools (`ToolCtx::lazy_tools`), and the tools it returns are declared on the
   next provider round-trip of the same turn — so "callable from your next call on" means the next
   call, in this turn, measured in the selftest by `tool_inventory` gaining exactly the tool explore
   named. Rebuild-a-registry-inside-explore would have lost startup servers and said "no servers".
3. **Every call asks.** `CallRisk::per_call` is a fact about whose program it is, not a risk level:
   an "allow for this conversation" remembered for `exec` does not cover it, and the gate's session
   shortcut requires `!per_call`. A server's `readOnlyHint` may lower the bar to workspace-sensitive;
   `risk_of()` never returns `Harmless`, in one place.

What the surface says, and why:

* **The catalog says what it left out.** `CATALOG_BYTE_BUDGET` is 1024 bytes on the *tool list body*;
  the explanatory sentence around it is fixed and reported separately (`catalog_body_bytes`,
  `catalog_lead_bytes`, `catalog_bytes`). A truncated catalog ends with "… and N more tool(s) not
  listed here: ask `explore` with `mcp:` to search all of them", because a cut-off list presented as
  complete teaches the model that the door is not there.
* **The approval card leads with the program, not the namespace.** `MCP server fake:
  mcp__fake__write_note …`, since a person deciding in two seconds should not have to decode
  `mcp__<server>__<tool>`. Paths in the arguments that resolve outside the workspace are named on the
  card as an escape — an approval, not a sandbox promise: MCP servers enforce nothing.
* **A dead server is a sentence.** Timeouts come from `[mcp] timeout_secs` (approvals reuse the
  app's `approval_timeout_secs`); a call into a server whose pipes closed answers within the timeout
  naming the server, and the status then says `dead`. Nothing in this feature can hang a turn.
* **Settings › Workspace › Third-party tools is status, not a control.** "Is what I configured
  actually running?", per server, plus the byte counts and the declared/withheld split — and it
  re-reads config and re-applies `CWOFFEE_MCP_SERVERS`, because a page that reads only the file tells
  a systemd user that none are configured. No button launches somebody else's program.
* **A tool card names the server instead of repeating the tense.** `third-party · fake`; the status
  chip already says working/done/failed, and which server ran is the fact a reader cannot re-derive.
* **A refused call says who refused.** A denied MCP call used to close as a bare "failed". The card
  now carries `denied by you (…)` or `blocked by the denylist: …` — the one state where "who said no"
  changes what the reader does next.
* **The pi core says it cannot mount MCP, rather than showing an empty list.** `cwtool` is one
  process per tool call, so a server it spawned would be born and die around each call; there is no
  process in the pi core that can hold a server open. `mcp_status` answers `mounted: false` with the
  reason, `tool_inventory` answers with an **error** instead of `{declared: [], hidden: []}` — two
  empty lists would claim nothing is hidden about something that side cannot observe.

Lessons that came out of building it:

* **Grading the wrong number fails a correct engine.** The first byte gate compared the whole block
  with the budget and went red on a healthy build: `the catalog costs 1051B against a 1024B budget`.
  The budget is about what a server can grow, so it applies to the body; the total is still reported,
  because the prompt pays the total.
* **Every status field a page prints has to exist on every branch.** `undefinedB + undefinedB` is
  what an unmounted payload produced once the page started printing the two parts: the `mounted:
  false` arm had `catalog_bytes` and not the two halves. Arithmetic a reader can check is a promise
  the payload has to keep while reporting nothing.
* **Server names are validated, not sanitised.** `mcp__<server>__<tool>` has to be parseable in one
  direction, so a name containing `__`, or a name that cannot be a tool-name segment, is refused at
  startup (`Config::mcp_problem`) rather than rewritten into something the reader would not recognise.
* **`--die` has no timer.** A server that exits after a delay lets a fast machine win a race and
  report live. Exiting immediately after `tools/list` makes "the pipes are closed" deterministic, and
  matches what a real crashing server does.
* **A card claim needs the card that belongs to the turn being claimed.** This group runs the same
  prompt twice; a document-wide `.blk-tool[data-tool=…]` answered with the earlier turn's card, and a
  denial claim read against the *allowed* turn passed for the wrong reason. Lookups take the newest
  matching card and poll for the line, because a re-render can replace a live card a beat after
  `turnEnd`.
* **`Allow` is not a session grant.** The first per-call sabotage looked caught by an unrelated card
  failure because the selftest had answered `Allow` — which grants nothing. The claim only tests what
  it claims once the card is answered with *Allow for session*.
* **Startup spawns belong inside the runtime.** Deciding whether to spawn happens before
  `tauri::async_runtime::block_on`; `Hub::start(...).await` happens inside it. Calling `await` from
  synchronous `main` is `error[E0728]`, and the decision is deliberately outside so a bad `[mcp]`
  block exits with a message naming the config file instead of after a window exists.

Receipts, from the run that shipped it:

```text
mcp ok (rust core: two servers started and their 8 tools stay withheld while the declared list carries
no MCP name, the catalog's tool list is 765B under its 1024B ceiling (1051B on every prompt with the
sentence around it), the card led with "MCP server fake" and the file the server wrote is on disk with
its words in it, explore's answer made that tool declared in the same turn, the next call asked again
and Deny moved nothing, and a call into the server that had exited came back naming it rather than
hanging)
mcp ok (pi core: 2 servers are configured and nothing is mounted, the page says why in "this core does
not mount MCP tools: its tool h"… rather than showing an empty list, the inventory refuses instead of
answering with two empty lists, and explore told the model there is no server to look up
```

Falsified, each red then restored:

- `Registry::defs()` including hidden tools -> `hidden MCP tools leaked into the declared list before
  anything asked: mcp__dead__t1, mcp__dead__t2, mcp__dead__boom, mcp__fake__t1, ...`
- `per_call: false` on MCP risk -> `the second MCP call ran without asking, after a yes for the same
  tool — a session grant covered a third-party call`
- catalog body over budget / status arithmetic -> the byte claims above.

`cargo test --workspace`: 571 engine tests plus bridge, gateway (42), store and the rest, 0 failures.
`npx svelte-check`: 0 errors, 0 warnings. `node scripts/wire-names.mjs`: OK — `mcp_status` and
`tool_inventory` graded on both hosts. Scenarios: 51.

