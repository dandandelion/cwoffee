# Benchmarks

Every number here is measured on the dev box, not estimated. Reproduce with the
commands at the bottom; if a number regresses, update it here or explain why in
the commit that changed it.

## Memory budget

Target ~250–350 MB PSS, hard CI ceiling 400 MB, overall envelope ~500 MB.

**PSS, not RSS.** RSS double-counts shared pages: Firefox measured 97.9 MB RSS
against 24.3 MB PSS on this box. Gating on RSS would fail Cwoffee at ~465 MB for a
process set that really occupies 282 MB.

### Desktop app (`cwoffee-app`), release build, embedded assets

Measured twice on the same build, because the total moves with which WebKit
helpers happen to be alive:

| Process | sample A | sample B |
|---|---|---|
| `cwoffee-app` (engine + Tauri + GTK) | 87.6 MB | 106.7 MB |
| `WebKitWebProcess` | 114.7 MB | 129.5 MB |
| `WebKitNetworkProcess` | 27.5 MB | 38.1 MB |
| `glycin-svg` + 2 × `bwrap` | — | 8.0 MB |
| **total** | **229.8 MB** | **282.2 MB** |

Quote **~230–285 MB PSS** rather than a single figure. The spread is mostly
WebKit's, and the `glycin-svg` / `bwrap` entries in the second sample are not
Cwoffee's: WebKitGTK sandboxes its own image loaders with bubblewrap. They are real
memory and belong in the number, but they are the webview's cost, not the
engine's.

RSS for the same state is 450–465 MB — 65% higher, and the reason RSS is not the
gate.

Debug build + vite dev server measures ~290 MB PSS. That is the number to compare
against when developing, not the shipping number.

### TUI (`cwoffee`), release build

**7.0 MB PSS** peak over 10 s at idle, measured with a PSS watchdog rather than
`RLIMIT_AS` (which caps virtual address space, not resident memory, and produces
misleading allocation failures in tokio/reqwest binaries).

This is the argument for the engine/UI split: the webview costs ~220–275 MB and
the agent costs ~7 MB. Anything that does not need a browser should not carry one.

### Binary size

| Binary | Size |
|---|---|
| `target/release/cwoffee` | 7.4 MB |
| `target/release/cwoffee-app` | 12.1 MB (includes the embedded frontend) |
| `bundle/rpm/cwoffee-0.0.0-1.x86_64.rpm` | 4.79 MiB |
| `bundle/appimage/cwoffee_0.0.0_amd64.AppImage` | 112.1 MiB |

The AppImage is **23× the RPM** and that is not a bug: it carries its own GTK /
WebKitGTK / JavaScriptCore stack so it runs on a machine that has none of them,
while the RPM declares them as dependencies and links the system copies. Runtime
memory is essentially unchanged — a single AppImage instance measures 275.8 MB PSS
against the RPM's 230–285 MB range — because it is the same webview either way,
just a private copy of it. Ship the RPM to Fedora machines; the AppImage is for
"this other box might not have webkit2gtk".

The packaged artifact was measured separately after installing nothing — the RPM
binary is the same one `tauri build` produced, and it was checked to contain the
hashed asset filenames before running. It renders (`render OK`) with no dev server
and measures **284.1 MB PSS**, matching the direct release measurement.

## Streaming smoothness

Measured in the webview with the dev harness, real model at ~50 tok/s:

```
turn end chars=1443 frames=209 dropped=0 worst=18ms
```

266–1418 frames per turn observed across runs, 0–3 dropped, worst frame 17–43 ms
at DPR 2.0. The 3-dropped case is a tool card being inserted mid-turn, which
changes layout; that is expected and bounded.

### The reveal is append-only, and that is what the harness measures

The animation gives each arriving word its own inline span (**opacity 0 → 1, no
filter**) with duration derived from the observed word rate, `600 / words_per_second`
clamped to **110–170ms**, written on the span itself, and a per-group stagger capped
at **90ms**. Nothing rewrites a span once created, and nothing re-rasters one: the
blur this used to animate lifted each word onto its own layer and dropped it back at
the end of its animation, which at 30–45 tok/s is a pop every ~25ms.

What the in-app harness asserts, in the app's own webview:

```
append-only reveal ok (28 word spans, none rewritten)
commit is layout-neutral ok (15 spans kept, drift 0.0px)
label alignment ok (-1.7px generating, -1.7px settled)
markdown mid-stream ok (h1+bold+list committed, no tail left half-drawn)
stream follow ok (worst step 0px, off-bottom 0px)
```

Every span created during a streamed reasoning block is still connected to the
document after later paints, the spans' text concatenates back to exactly what the
model sent, none of them is block-level, and words from an earlier arrival sit at
`opacity: 1` with `filter: none` while new words land — no style write is recorded
on the streaming paragraph at any point in the turn. The vertical offset between the
`CWOFFEE` label and the first line of the answer is **-1.7px while generating and
-1.7px once settled**, so the name no longer snaps into place at turn end.

Filmstrip diffing of the answer block through the gateway (fixed clip, real model
at ~50 tok/s) shows settled frames inert: mean per-frame pixel difference 0.00,
maximum 0.12, with the only visible change coinciding with new characters.

### Streaming markdown and follow, measured on the live gateway UI

A scripted browser run typed a prompt that forces headings, bold and a list, then
screenshotted the transcript every ~120ms while the answer streamed (153 frames):

| what | measured |
|---|---|
| heading became an `h2` at frame | 88, while the tail still read `This` |
| `**bold**` became `<strong>` at frame | 94, while the tail still read `- Bullet` |
| list became `<li>` at frame | 100 |
| committed blocks ever showing literal `**` | **0** |
| frames whose unfinished block was still raw text | 15 of 65 |
| spans mid-animation at any instant (max) | **5** |
| frames with a span carrying non-zero blur | 14 |
| distance from the bottom through the turn (max / median) | **0px / 0px** |

Same run before the follow fix, tracking one line of text frame by frame: it held a
position for 100 frames, then moved 112px, then 92px, then 46px. The desktop self-test now
asserts the replacement contract directly — worst single-paint step 24px measured, fails
above 60px, fails if the newest line ends more than 40px below the fold.

Frame diffs of the user's own recording (`bug-submit-4.mp4`, 120fps source, sampled at
20fps) are how the lurch was found: seven frames carried a mean difference of ~12/255 with
170k pixels changed and **no** translation vector that explained them, i.e. the whole
viewport was being rewritten rather than scrolled. A plain per-frame diff cannot tell those
apart; the shift search can.

Memory after all of it: PSS **259.5 MB** against the 400 MB ceiling.

### Committing a block in place: the numbers behind "it still blinks"

A plain paragraph that finishes streaming is closed without being re-rendered, so
the claim under test is that nothing already on screen changes. Measured in the
app's own webview (WebKitGTK, the engine the complaint is about):

| what | measured |
|---|---|
| word spans kept by a committed plain paragraph | **15 of 15**, still connected |
| movement of the paragraph above a commit | **0.0px** (assert fails above 0.6px) |
| `characterData` rewrites outside the block being written | **0** |
| style writes on the streaming paragraph per turn | **0** |
| settled words at opacity < 0.98 or under a filter | **0** |
| spans mid-reveal at any instant (max / mean) | **11 / 3.5** |
| distance from the bottom of the view (max / median / p95) | **0px / 0px / 0px** |
| largest single-frame scroll step | **34px** (one line + margin) |
| structural re-commits per turn (list, table replacing raw text) | **2** |

Those two structural re-commits are the only remaining whole-block reflows, and
they are deliberate: a `<ul>` is not the same box as five lines beginning with `-`,
so it is re-laid-out once instead of being animated into place.

A pixel-level pass over 798 screencast frames of the same turn found 5 frame pairs
whose settled crop changed after vertical shift alignment; on inspection each was
the streaming frontier entering the crop band, not settled text being redrawn.
Comparing crops without a shift search reports ~19k changed pixels for a one-line
scroll, which is why the shift search is part of the method and not an optimisation.

Memory after this batch: PSS **264.6 MB** against the 400 MB ceiling.

## Prefix caching (why the prompt builder is append-only)

Two turns against `qwen3.8-flash-next-iq3_s`, second turn reuses the first's
prefix:

```
turn1: prompt=324 cached=0   raw_prefill=119/s decode=20.5/s spec=40/75
turn2: prompt=347 cached=319 (92% hit) raw_prefill=38/s ttft=767ms eff_prefill=452/s
```

Raw prefill on this model is 20–120 tok/s. A cache miss is the difference between
a 0.8 s and a 9 s first token on a long transcript, which is why:

- the system prompt contains no volatile data (no clock, no cwd, no git status),
- volatile context is appended to the newest user message,
- history is append-only and byte-stable,
- tool definitions are registered in a fixed order,
- compaction is explicit, rare, counted, and surfaced in the UI.

The compaction budget derives from what the endpoint advertises rather than a
constant: for the loaded model at a stated 262,144 token window the engine uses
186,695 tokens (`window * 0.72 - 2048`, floor 4096) and re-derives it on every model
switch. The previous constant of 8000 compacted a 43k transcript on that model and
reported "context full", which was a claim about the model that had never been asked.

## GPU

Cwoffee must render on the Intel iGPU (`renderD128`). The NVIDIA GPU is at ~15993 /
16384 MiB from local inference and its EGL vendor sorts first by default.

`scripts/gpu-check.sh cwoffee-app` must show Cwoffee processes holding `renderD128`
only. Verified for both debug and release builds.

## How to reproduce

```bash
# gates, against a running app
./scripts/gpu-check.sh cwoffee-app
./scripts/memory.sh cwoffee-app 400      # walks the whole descendant tree
node scripts/gateway-smoke.mjs           # browser-side promises, mutates nothing

# streaming + engine path, driven from a script
VITE_AUTOTURN=1 VITE_AUTOTURN_PROMPT='Say hello in five words.' ./scripts/dev.sh

# live provider smoke test
cargo run -q -p cwoffee-engine --example live
cargo run -q -p cwoffee-engine --example live_agent -- http://127.0.0.1:8090/v1

# sandbox isolation (re-run after any bubblewrap / kernel / distro upgrade)
./scripts/sandbox-probe.sh

# markdown sanitizer
(cd ui && node --experimental-strip-types scripts/md_test.ts)
```

## Safety note on probing

This box runs inference and has limited headroom. Any probe that spawns a process
must be watchdog-armed against **PSS**, not `RLIMIT_AS`. `RLIMIT_AS` caps virtual
address space; tokio/reqwest binaries legitimately map large VmSize at small PSS,
and the result is a misleading "memory allocation failed" that looks like an app
bug. A TUI regression once reached 8.7 GB PSS and pushed the machine into swap.

## Document output

Measured on this box, LibreOffice 26.2.6.3:

| conversion | result |
|---|---|
| markdown → HTML → PDF | works, `writer_web_pdf_Export`, ~34 KB for a one-page note |
| docx → PDF | **fails**: "source file could not be loaded" |
| HTML → docx | **fails**: "no export filter found" |

The Fedora LibreOffice here has no MS Word 2007 XML filter, so it cannot read or
write .docx at all. That is why the PDF path is markdown → HTML → PDF and not
markdown → docx → PDF: the renderer cannot consume the intermediate we would
have produced. The .docx is still written as a deliverable for Word users.

This also means LibreOffice cannot validate our hand-rolled .docx. `scripts/check-docx.py`
does it instead with `zipfile` + `ElementTree` (stdlib only). It will not prove
Word accepts the file; it does catch the failure that matters, which is a part
that is not valid XML.

End-to-end with the real model (`cargo run -p cwoffee-engine --example live_docs_agent`):

```
tool ▸ write_xlsx   args {"path":"budget.xlsx","sheets":[{"name":"Budget",
                     "rows":[["item","cost"],["rent",1200],["food",300],["power",90]]}]}
tool ▸ write_file   args {"path":"summary.pdf","content":"# Monthly Budget Summary..."}
tool ▸ exec         args {"cmd":"ls -l ..."}        # model verified its own output

summary.pdf (21593 bytes)   budget.xlsx (5581 bytes)
pdftotext -> "Monthly Budget Summary / The total ... is 1590."
xlsx cells -> 1200, 300, 90 stored as numbers, not text
prompt=4349 cached=2728 (63% hit) decode=27.3/s
```

Two dependency decisions worth keeping:

- `docx-rs` was tried and pulled **88 transitive crates** (zstd, xz, zopfli, a
  JPEG decoder). Writing the five OOXML parts directly with `zip` (already a
  dependency of rust_xlsxwriter, so no new version) took the engine from 445 to
  317 crates and puts the escaping under our control — model output quotes
  fetched web pages, and a writer that passes text through raw is an injection
  path into a file the user opens in Word.
- PDF is not generated from scratch. That is a font-metrics project, and the
  honest alternative is a "PDF" that is really a bad text dump.

## Thinking effort: what actually controls it

Reported symptom: the llama.cpp web UI thinks for ~40k tokens on `high`/`xhigh`
while Cwoffee thinks far less on any setting. Both halves of that are true, and
the cause is not the setting.

The knob, for this server (a llama.cpp-lineage server, `/props` build
`Strata 0.1.27`), is a **chat template variable**, not a request parameter:

```jinja
{%- set resolved_reasoning_effort = reasoning_effort|default('xhigh') %}
{%- if resolved_reasoning_effort == 'high' %} ... fold to 'xhigh' ...
{%- if resolved_reasoning_effort == 'xhigh' %}
    {%- set reasoning_instructions = 'Reasoning effort is set to xhigh. Please think
     carefully through the task, validate key assumptions, consider plausible
     alternatives, and prioritize correctness ...' %}
{%- elif resolved_reasoning_effort == 'low' %}
    {%- set reasoning_instructions = 'Reasoning effort is set to low. Keep your
     thinking brief and focused ...' %}
```

Two findings, both measured on the same question (`/v1/chat/completions`,
`max_tokens=1500`, model `qwen3.8-flash-next-iq3_s`):

| prompt shape | reasoning chars | completion tokens | finish |
| --- | --- | --- | --- |
| user message only, no tools | 5260 (hit the 1500 cap) | 1500 | `length` |
| + Cwoffee system prompt, + tools | 603 | 266 | `stop` |
| + system prompt without "Be concise" | 709 | 380 | `stop` |
| + system prompt, no tools | 680 | 308 | `stop` |
| + system prompt + "reason at length …" | 1426 | 513 | `stop` |

So:

- **Any system message collapses thinking on this model.** Bare request thinks
  until the token cap; add a system prompt — any system prompt, tools or not —
  and it settles in ~250 tokens. The web UI thinks more because it sends no
  system prompt. This is a property of the checkpoint, not a Cwoffee bug, and
  Cwoffee is not going to drop its system prompt to win a thinking-length chart.
- **The effort menu was half-wired.** `Flavor::LlamaCpp` sent only
  `chat_template_kwargs.enable_thinking`, a boolean. Low/medium/high/xhigh were
  one bit. It now sends `reasoning_effort` too, mapped onto the values the
  template accepts (`low | medium | xhigh`, with `high` folded the same way the
  template folds it — sending `high` would hit the template's `raise_exception`
  and fail the request outright).
- **The levels now do something on this backend.** At `high`/`xhigh` the system
  prompt gains one line, "Deliberation: reason at length. Use as much thinking
  as the problem needs; do not stop early." That line is measured above: 603 →
  1426 reasoning characters. The template's own xhigh sentence produced *no*
  measurable change, which is why the wording here is ours.
- The effort is part of the prompt bytes, so switching levels drops the prefix
  cache. That cost was already being paid: the template injects an
  effort-dependent sentence into the system message, so the rendered prompt
  changes on any level switch whether we write anything or not.
- `AgentConfig::default()` capped output at `max_tokens: Some(2048)`, thinking
  included. The desktop path passed `None` (server default, `n_predict=-1`) so
  the cap was invisible there, but any path using the default silently truncated
  thinking. It is `None` now.

Honesty rules that came out of this: the endpoint's capabilities are read from
`/v1/models` and `/props` where it states them, `/props` capabilities are
attached only to the model its `model_alias` names, unknown is rendered as "not
reported" rather than "no", and tool support is marked as inferred (it is
detected from the served chat template because no endpoint states it).

## Blink: what an entry animation actually costs, and whether the app replays one

Investigating "almost every animated element blinks at some point". Isolated in plain HTML in
a bare WebKitGTK 2.54.0 window, a solid square whose entrance is measured on real compositor
output at 120fps:

| page behaviour | frames fully unpainted | ≈ ms invisible |
| --- | --- | --- |
| fresh node, **no** animation | 0 | 0 |
| fresh node + `rise`, fill `both` | 14 | 117 |
| fresh node + `rise`, fill `none` | 14 | 117 |
| fresh node + `rise`, fill `backwards` | 14 | 117 |
| same node, class removed and re-added | 13-14 | 108-117 |
| fresh node + opacity-only fade (200ms) | 17-23 | 142-192 |
| 16 variants: opacity / transform / `blur` / combinations / `will-change` / opacity on a text node | clean, 1450 frames | — |

In the production UI, same engine, during a real turn:

| measurement | value |
| --- | --- |
| `cwoffee-tin` starts / ends / cancelled in one turn | 593 / 411 / 2 |
| nodes animated **twice** (the blink signature) | **0** |
| node age at animation start | 100-135 ms (insertion → first frame) |
| reveal opacity per paint | `0 → .04 → .17 → .39 → .59 → .74 → .85 → .92 → .97 → 1` |
| page paint pacing during a turn | median 17ms, p90 18ms, worst 48ms (59.7fps) |
| animation starts when opening another session and back | **0** |
| engine snapshots (`get_snapshot`) sampled over 40s of boot + turn + 2 session switches | 460, frames where settled text stopped being drawn: **0** |
| self-test: entrances watched / replays / cut short | 56 / **0** / 0 |

So the app's append-only, animate-once transcript holds. The danger is not the animation, it
is a re-render that re-creates a node carrying one: `.turn > *` alone would hide an entire
turn for 150-260ms. That is why the self-test now fails on any entrance that is a repeat, or
that lands on a node older than the check, or on a `.turn` replaced mid-turn.

Measurement rates, recorded so nobody reads a film wrong: the GNOME screencast produced 2.9fps
on a near-idle page, 26fps during a turn and 120fps on a page of looping animations — it is
variable-rate and dedupes identical frames. The in-page rAF counter says the page painted at
59.7fps throughout.

Memory after this change: PSS **244.8-267.0 MB** across three samples (main process plus the
WebKit web/network children) against the 400 MB ceiling; RSS 462.7-482.8 MB.

### The leftover blink was scroll, and these are the numbers on it

The user's own recording (`~/Videos/Screencasts/blinking.mp4`, 201 frames, ~26fps, window crop)
aligned frame to frame by searching for the best vertical offset:

| measured on the user's recording (before) | value |
|---|---|
| frames whose best alignment is offset 0 | most of them — the page sat still |
| then displacements at 4.94s / 5.72s / 6.17s / 7.50s | **66 / 46 / 44 / 40 device px** = 33 / 23 / 22 / 20 CSS px |
| residual after alignment on those frames | 12-24 (text arriving); 0.4-1.6 where the move was a pure scroll |
| motion in the frames either side | none — stop, teleport, stop |
| implied velocity of one such jump | 600-870 CSS px/s in a single frame |

After moving the follow onto a frame clock with a velocity cap and a ramped catch-up, recorded on
the same kind of take (62s, 1191 frames, ~19fps) with the same alignment method:

| measured after | value |
|---|---|
| page velocity per recorded frame — median / p95 / max | **277 / 575 / 873 CSS px/s** |
| frames aligned by a rigid move with residual < 1 | continuous, and the residual says *moved*, not *rewritten* |
| normalised alpha in the user-bubble band across moving frames | .057 → .057 → .060 (a scroll moves pixels; a blink changes alpha) |
| normalised alpha in the frontier band while a line fills in | .040 → .045 → .050 → .055 → .060 → .063, no dips except on frames the shift search explains |

And in-page, where the frame clock is actually visible (self-test, real engine, 505 sampled frames):

| check | before | after |
|---|---|---|
| worst single-frame `scrollTop` step | 23-33px (paint granularity) | **8px** steady, 9-16px on a ramped catch-up |
| worst change of speed in one frame | everything, instantly | **8px/frame** |
| newest line, furthest below the fold | unbounded (204px seen) | **56px** |
| where the follow ends up | — | **0px** off the bottom |
| the line a scrolled-up reader stopped on, when a block committed below it | — | **0.0px** |

The rules that hold it, in `transcript.ts`: 6px/frame steady, a seventh of any remaining gap,
6px/frame² of acceleration, place-rather-than-walk past one screen and a half of gap, and exactly
one step per frame no matter how many callers asked for a follow.

### The reveal is a fade with a little focus, bounded at both ends

`cwoffee-tin` is now `opacity 0→1` with `blur(1.2px)→0`, the blur gone by 55% of the animation and
the end value `blur(0)` (identity) so the layer goes away while the word is still darkening.
Duration 130-200ms from the measured word rate, stagger inside an arrival capped at 100ms.

| bound | value | why it is a rule and not a taste call |
|---|---|---|
| spans mid-fade at the busiest moment (self-test) | **6** | fewer than 4 fails as "the reveal is invisible" — an opacity-only fade at 110-170ms is what triggered that |
| same, upper bound | ≤24 | more is a grey ghost tail sitting two lines above the reading position |
| spans still under 90% opacity 400ms after the last arrival | **0** | the ghost tail coming back |
| settled spans with a filter still applied | **0** | a filter outliving the fade means the layer is being torn down under settled text |

Memory at the point of writing: PSS **267 MB**, RSS 482.8 MB, against the 400 MB gate.

## The blink that was never in the page

The discriminator, supplied by the user: the gateway-served bundle in a real browser does not
blink; the native window does. Same bundle (`grep -c SELFTEST dist/assets/*.js` = 0 in both), same
process, same session. Everything below the DOM was suspect, and everything above it had been
measured clean: 0 entrance replays, 0 settled spans rewritten, commit drift 0.0px, follow steps
≤7px/frame.

### Engine A/B, same build, same prompt

| `WEBKIT_DISABLE_DMABUF_RENDERER` | what the user sees |
|---|---|
| unset — WebKitGTK 2.54.0's zero-copy path | blink on every streaming turn |
| `1` — copy path | gone, completely |

`CWOFFEE_WEBKIT_MODE` selects the path in code (`gpu.rs`), the default is `no-dmabuf`, and the
boot log records it. Both flags reach the web process: verified by reading
`/proc/<WebKitWebProcess>/environ` of the running app, not by trusting the setter.

### What the copy path costs — the page's own frame clock during a real streaming turn

| webkit path | frames sampled | median | p95 | worst | off-screen spans |
|---|---|---|---|---|---|
| zero-copy DMABUF | 1074 / 17.9s | 17.0ms | 18.0ms | 24.0ms | 0 |
| copy path (shipped default) | 954 / 18.4s | 17.0ms | **32.0ms** | 54.0ms | 0 |

The 60Hz steady clock is identical; the tail roughly doubles. Self-test bound is p95 ≤45ms with
median ≤21ms, which is a statement about the shipped rendering path rather than a number tuned to
pass. One caveat, learned the hard way: an occluded or locked screen stops the frame loop, and the
first sample blamed that on the app as an 7932ms stall.

### A queue routed by the wrong block kind

The failure that produced "the table is in the store but never became a table", reproduced in this
engine with the drain removed and restored:

| | thinking block body | answer block body | `<table>` count |
|---|---|---|---|
| drain on switch (shipped) | "The user wants just a table in markdown format." | `name value alpha 1 beta 2` | **1** |
| drain commented out | *(empty)* | "The user wants just a table…" + table text, one paragraph | **0** |

Coverage without a model: self-test `[2] switch drain ok`, and for the block splitter, md_test:
`a table ends at its trailing blank line`, `a table still mid-row has no end`, `a paragraph does
not end at a trailing blank line`, `splitCommitted commits a trailing table too`.

### Harness hygiene this round added

- `waitTurnDone` budget 150s → 240s: a cold prefill of ~2k tokens measured **12.8 tok/s** here, a
  153s first token, with the turn completing correctly just outside the old window. The failure
  message now says which side to look at.
- The real-turn table check reports store-vs-DOM on failure (`blocks=[…]`, last answer body, stored
  text) instead of a bare "not rendered", because those are two different bugs.
- A check that leaves a session behind changes which sessions a *later* check opens; the cadence
  turn is swept immediately rather than by the end-of-run sweep.

---

## Batch: gutter, question sets, live markdown, approvals, sharing, search providers

### The assistant label must not move when the live dot appears

The complaint was that the label lost its gutter while an answer streamed. Measured in the shipped
build, mid-stream and settled, including an answer that opens with a heading:

| sample | gutter (text edge − label edge), generating | gutter, settled |
|---|---|---|
| plain answer | **28.0px** | **28.0px** |
| answer starting with `## heading` | **28.0px** | **28.0px** |

The fix was structural, not cosmetic: an *inline* live dot made the label's line box wider than the
58px gutter it is laid out in, and `text-align: right` has no effect on an overflowing line, so the
label ended up flush against the text. The dot is now absolutely positioned off the label
(`.blk.live > .blk-head::before { position: absolute; left: -11px }`), which is why generating and
settled are the same number. Reasoning blocks keep the dot inline because their header is a static
full-width line, not a gutter.

Two related measurements from the same run, both on synthetic blocks so they do not depend on what
the model decides to emit:

```
thinking aligns under its label ok          (label, rule and body edge share an x; body padded in)
label gap holds through the turn (8px, drift 0px)
```

The second one is the "label crowds the text while it streams" complaint, measured at three points
during a streamed answer and again at turn end: the gap is 8px and does not move. Before the fix the
live dot's inline box made that gap 0px.

Vertical offset of a heading-first answer is ~12px by design (the label hangs from the block top,
the heading has a top margin), so the check asserts the gutter and live↔settled *stability* rather
than an absolute heading offset.

### Markdown that only arrives at `turnEnd` is markdown that arrives late

| construct | appears during streaming | how |
|---|---|---|
| `## heading`, `---` | at its own newline | block terminators no longer require a blank line |
| table | before the first non-table row | table blocks end at the row that stops looking like a table |
| `**bold**`, `` `code` ``, `~~strike~~`, `*em*` | as soon as the construct closes | `InlineStream` in the live word-span path |

Inline markup is decided per word-span append, and spans already painted are never rewritten: the
check measures that the previously painted word did not move (**0.0px**) and that marker characters
are never shown. False positives are covered in `md_test.ts`: `snake_case`, `2**3`, spaced
asterisks, and markers inside a code span.

### An approval card for a long command is a wall unless it is scrollable

A 5.9k-character `exec` payload with a 4-line here-document:

| | before | after |
|---|---|---|
| body height | grew past the window; Deny unreachable | **366px shown of 5901px**, scrollable |
| Deny button | below the fold | **109px above the fold** |
| prompt text | whole command quoted | first 4 lines / 320 chars + `[+399 more lines, 5901 characters total]` |

The engine also refuses to run an oversized inline write through `exec` (limit **1200 chars**) and
points the model at `write_file`. The guard is deliberately narrow: only redirections in the command
*head* count as file writes, so scripts piped through a here-document, `>/dev/null` and `2>&1` still
run (each is a test).

### Sharing must not become someone else's output

`share_markdown` returns `{name, markdown}` and writes nothing. Desktop opens a save dialog; browser
and gateway download. The self-test asserts the absence of a chip and that each turn owns its files:

```
turn files ok (each turn owns its own files, and a share leaves no chip)
```

The bug behind the "deleted file attached to a later turn" report was the live transcript's
`turnFiles` surviving across turns, so a later `addFiles` re-parented stale chips.

### Web search: three providers, and the managed one measured

`scripts/search-smoke.mjs --backend auto --probe "rust tokio worker threads how many"` against the
running host:

| step | result |
|---|---|
| switch duckduckgo → auto | accepted, `describe()` says "SearXNG managed by Cwoffee on port 8899" |
| first probe (container cold start included) | **3243ms**, 5 real hits |
| container footprint | **118.4MB** of a 268.4MB cgroup limit, CPU 8.4% idle |
| idle reaper | `stopped cwoffee-searxng after 129s without a search; it starts again with the next one` |
| disable | backend returned to `duckduckgo`; nothing was uninstalled, no container was removed |

Enabling the managed provider writes `settings.yml` and `limiter.toml` under
`~/.config/cwoffee/searxng/` (readable, not a cache dir) and pins port **8899** so an instance from
elsewhere keeps 8888. If the container cannot start, the tool result begins with a note naming the
fallback: the model cannot report "I searched the web" about a provider nobody chose.

Engine tests for the ask protocol: **21 passed, 0 failures** in 0.31s under `scripts/cargo-guard.sh`
(peak **1.03GB** with a 1.4GB ceiling). The full engine suite was deliberately **not** run on this
box; see Trap 82 for why.

### Multi-question `ask_user`, measured in the shipped UI

```
question set ok (one card, navigation kept answers, one submit, settled read-only)
```

What that line asserts: one `.ask-submit` for N questions, `Question 1 of 2` in the nav, Back
restoring the first question **with its selection intact**, `Send 1 of 2 answers` while a question
is unanswered plus a hint that it goes as no answer, exactly one `respond_question` per question id,
then no option row, input, Back/Next or Send left in the settled card, a head line of
`answered 2 of 2`, and a Q&A summary that is hidden until the head is expanded. Declining sends an
empty answer for each and reads `declined`.

Engine side: a set is one budget (`timeout × (1 + 0.5·(n−1))`), partial answers are kept and
labelled, and every question that went unanswered gets its own `questionExpired`.

### Frame clock: the baseline is now part of the measurement

| sample (shipped build, this box) | idle baseline | streaming median | streaming p95 | worst | page long tasks |
|---|---|---|---|---|---|
| endpoint quiet | 17.0ms | **19.0ms** | 56.0ms | 74ms | **0 tasks, 0ms** |
| model prefilling for someone else on 9 cores | 17.0ms | **41–44ms** | ~114ms | ~144ms | **0 tasks, 0ms** |

The check used to fail the app for that window. It now attributes: the page's own long tasks are
counted, and when they are zero the numbers are logged and the verdict is skipped with a named note,
because a 50 GB model holding the CPU decides the web process's frame rate. Note what the two rows
have in common: the *idle* clock is 17.0ms in both, so the difference is not the page's code and not
the machine's capability - it is whether the model is mid-prefill. Quiet-box numbers (median 19ms,
worst 74ms) are what the absolute thresholds were calibrated on.

### Premise failure is now a first-class result

Two cascades cost more time than the bugs they reported, so both are now measured where they happen:

| cascade | root cause | what the harness says now |
|---|---|---|
| 7 failures about CSS geometry | view was a draft; `.empty-mode .scroller { display: none }` → every rect 0 | `skipped … ; .transcript measures 0x0 (empty screen shown=true, turns=0, viewport 1180x777)` and a run-level fail at ≥4 skips |
| the same failures after the view was opened | the *recovery itself* repainted the transcript, detaching nodes the check had already captured (`chain=div.blk.blk-reasoning < div.turn`, no parent) | premise checked **before** the check builds its elements; when it still drifts: `the view moved under this check mid-measurement: …` |
| 3 failures about turns not finishing | endpoint prefill **17 tok/s** (ttft 127s) | `skipped …; the endpoint prefills at 17.0 tok/s …` and a run-level fail at ≥3 skips |

The recovery is attempted before the skip: the harness opens a session with `message_count > 0`
(not merely the first row - an empty session keeps the empty screen up) and waits for the transcript
to measure ≥50px. And the premise is checked *before* a check paints what it intends to measure:
three runs of "expanded thinking block has no width: 0" came from the recovery itself wiping the
transcript after the nodes had been captured, which a `selftest diag` line proved by printing the
ancestor chain and whether the node was under `.transcript` at all.

---

## Batch: the harness stopped paying for prefill

### Why this one is measured at all

The in-app harness drove the user's llama.cpp endpoint. That endpoint has **one slot** and carries a
~200k-token conversation, so every self-test run evicted a warm prefix cache and made the *next real
conversation* pay to re-ingest it. Under contention on this box, measured prefill dropped to
**17 tok/s** (`selftest note: the endpoint prefills at 17.0 tok/s`), which turned a ~2k-token harness
prompt into ~2 minutes per turn and produced cascades of turn-dependent failures that were really
one cache miss.

### What replaced it, and what it costs

`scripts/mock-llm.mjs` answers in llama.cpp's wire shape from `tests/scenarios/scenarios.json`;
`scripts/selftest.sh` starts it, points the app at it with a scratch config/data/workspace and its own
gateway port, and reports the mock's own activity table afterwards.

| suite | model | result |
|---|---|---|
| `cargo test -p cwoffee-engine --test mock_endpoint` | scripted HTTP | **7 passed, 0 failed in 0.57s** (mock at `--speed 0.2`) |
| `cargo test -p cwoffee-engine --lib tools::ask::` | in-process | 21 passed, 0.30s |
| gateway / app / markdown / svelte-check | none | 30 / 9 passed, `markdown: all checks passed`, 0 errors 0 warnings |
| full in-app harness (`./scripts/selftest.sh`) | scripted HTTP | **SELFTEST ok: 24 requests, 24 turns served, 16 distinct scenarios, no unmatched prompts** |

Turn latency under the mock, from the harness's own turn lines: a streamed 1.4k-character answer with
a heading and four paragraphs takes **~2.2s wall** at the script's pacing (7 chars/11ms), and the
provider's first delta arrives in **single-digit milliseconds** — the shape the streaming checks need,
with no prefill at any point.

What that does to the frame clock, measured in the same run against the idle baseline:

| sample | median | p95 | worst | verdict |
|---|---|---|---|---|
| idle, nothing streaming | 17.0ms | 17.0ms | — | baseline |
| while a scripted turn streams | **17.0ms** | **24.0ms** | 28.0ms | `frame cadence ok` against the absolute 60 Hz thresholds |
| while a *real* turn streams (previous batch, same box) | 41–44ms | ~114ms | ~144ms | judged against the baseline, because the model held nine cores |

So the streaming renderer was never the problem: with the same page and the same machine, replacing
the model with a script returned the frame clock to 60 Hz. That is also why the cadence check can now
run at its absolute thresholds instead of leaning on attribution.

Structure arriving mid-stream, from the scripted table turn (`heading 500ms and table 200ms before
turnEnd, 3 data rows, 3 list items`) — against a live model this check could only ever be a guess.

### The mock's own footprint

| measure | value |
|---|---|
| process | one node process, stdlib only |
| RSS after three streamed turns (incl. the 1.4k-character four-paragraph answer) | **3.9 MB** |
| listening | 127.0.0.1 only; refuses any other bind (a mock has no auth) |
| scenario file | 18 scenarios, read once at start-up |

That last row is why `selftest.sh` refuses to start when something already answers on its port: a
mock left running from an earlier run serves the scenario file it read an hour ago, and a run against
it silently ignores every scenario added since. One such run reported `no scenario for this prompt`
for a scenario that existed in the repo.

### The bug the scripted endpoint found, in the app rather than in the tests

`ask_user` multi-question sets never reached the UI. The engine's `Question` struct carried
`set: {id, index, total}`, `EngineEvent::QuestionRequested` did not have the field, and
`host::question_event()` — the single function the desktop and the gateway both use to build that
event — silently dropped it. So a two-question call rendered as two unrelated popups, while every
engine test passed and the synthetic UI test passed too, because it called
`transcript.showQuestionSet()` directly instead of going through the wire.

Pinned at the seam where it broke, in `host.rs`:

```
host::question_wire_tests::a_question_event_carries_the_set_it_was_asked_in ... ok
host::question_wire_tests::a_single_question_event_omits_the_set_key_entirely ... ok
```

The second one matters as much as the first: a single question's JSON must not grow a `set` key,
because that shape is what every existing client renders.

And the mock caught a second, dumber mistake of mine: `ask_user` options are
`{label, detail}`, and my first scenarios sent `{label, note}` — serde ignored the unknown key, the
tool refused the call with `every option needs a non-empty "label"`, and the model carried on as if
nothing had happened. Canned tool arguments are a wire-contract test, not decoration.

---

## Batch: a bullet is a bullet the moment the next one starts

### Item 1: the search panel's three-line status block

`.pop-item` is a grid, and `.pop-sub` is pinned to grid row 2 for every item. The
managed-SearXNG block has **two** subs, so "podman · 8899 · idle in 120s · standing up"
was painted on top of "port 8899 · idle 120s · config …" — one garbled line, found by the
user's eyes and not by the harness, which had no geometry check for that panel.

| measure | value |
|---|---|
| panel | 732×272 inside a 1180×777 webview, 5 rows, none overlapping |
| panel vs footer | ends 1px above the footer's top (tolerance 4px; 1px is rounding) |
| status block | three rows at distinct y; asserted on a **synthetic** `.pop-item.status` so the check runs on any backend, including DuckDuckGo where the real block is absent |

The check is now `search panel geometry ok (5 rows, 732x272 inside 1180x777, status block stacks, live status not applicable on this backend)`.

### Item 2: `finish_reason` was parsed and thrown away

`Collected.finish` carried `#[allow(dead_code)]`, and the final-answer branch reported
`TurnEnd { finish: Stop }` whatever the provider had said. So a turn that an output cap
bit looked, in the log and in the UI, exactly like a turn the model finished.

| fault | what the user saw | what happens now |
|---|---|---|
| answer cut mid-sentence | prose stops mid-word, no note | note `the answer hit the output limit after N tokens - continuing where it stopped (1 of 2)`, then a second round-trip that resumes the sentence; up to 2 continuations, then an explicit note naming `max_output_tokens` and `-np` |
| thinking consumed the whole budget | tool card + `Done · 9 tool call(s)` and **no answer** | error naming it: `no answer: the model used all N completion tokens on thinking and stopped at the output limit. Lower the thinking level, or raise …` |
| turn ends at a cap after continuations | looked clean | `TurnEnd { finish: length }`, not `stop` |

New user-facing knob, because none existed: `max_output_tokens` in `config.toml`, wired
through `Config` → `HostConfig.max_tokens` in all three shells. Unset means the server's
`-np` decides, which is what Cwoffee has always done — but now a cap that bites is visible
and actionable instead of silent.

Proved by `mock_endpoint` (2 round trips, both halves in history, the note present,
`finish = length` for the thinking case) and by the new scenarios. The mock itself had the
same fault as the engine — it turned every `finish` that was not `tool_calls` into `stop` —
so the first version of these tests could not fail. Fixing the mock is what made the tests
mean anything.

### Item 3: markdown structure that waits for a block that has not ended

A list block, per CommonMark, ends at a blank line. The live painter waited for that, so a
four-bullet answer showed four lines of `-` text and then became four bullets in one
replacement. The unit was wrong: **an item is finished as soon as the line starting the
next item has arrived**, which the bytes already on the wire state.

| measure | value |
|---|---|
| item count during the stream | `0→1→2→3→4` — 4 of 4 were real `<li>` before `turnEnd` |
| list elements built | **1** `<ul>`; more would mean the list was chopped into stacked lists |
| marker characters painted | none (`-` is syntax, not sentence) |
| first bullet's position while the rest arrived | **0.0px** relative to its list, text unchanged |
| link inside a bullet | became an `<a>` (the one construct parsed once at the item boundary) |
| nested / checkbox / thematic-break lists | left to the parser: `- [ ] task` and `  - nested` are not built live |

`firstItemEnd` is the new rule, unit-tested in `md_test.ts` (wrapped lines belong to the
item above, lazy continuation too, a heading closes the item, a nested marker stops the
scan rather than being flattened, a half-arrived marker returns "wait").

### Gates

`SELFTEST ok` (0 failures) against the scripted endpoint; `markdown: all checks passed`;
`svelte-check` 0 errors 0 warnings; engine `mock_endpoint` **9 passed**, `config::` 13 passed;
production app PSS **245.2 MB** (≤ 400), `render OK`, gateway smoke OK, search-smoke OK.

---

## The toolbar stopped guessing, and markup arrives as structure while it is typed

Three reports, one shape: the app stated a fact it had not read yet, and markdown
became structure only when the model stopped typing.

### What the footer says before it is asked

The search pill read `unknown` until the search popover was opened; the gateway pill
could read `off` while a phone was connected. Both values existed at boot and were
fetched on demand.

| measure | value |
|---|---|
| footer at boot, before any popover | search `duckduckgo` (backend=duckduckgo), gateway `:8199 · 0 devices` running=true |
| gateway boot | started with `block_on` before the window, so the first `gateway_status` cannot land mid-bind |
| a gateway that could not bind | note is `off - Address already in use (os error 98)`, not `off` — the case where a phone reaches a gateway this instance does not own |

### A bold span split across two paints

The user's screenshot showed `**What it is**:` with the words bold and the closing
`**` printed as text. The inline stream had opened `<strong>`; the next paint passed
the **block** host into a guard that compared it with the **insertion point**, which
was now the `<strong>`, so the rest of the item was painted as literal source.

| measure | value |
|---|---|
| strong spans in the answer | 2, and 0 literal `*` on screen while streaming |
| samples with bold already on screen | 151 mid-stream samples |
| words lost from a split construct | none (`Sourcing sets the ceiling` present in full) |
| negative control | reverting one condition reproduces the report verbatim: `Sourcing** matters more than the roast...` |

### Tables while the rows arrive, not after

A table used to become a `<table>` in one replacement at the end of its block. Once the
header row and the delimiter row have landed, the shape is known, so the skeleton is
built then and each completed line becomes a `<tr>` — using the parser's own cell rules,
so a live row and a parsed row are the same element.

| measure | before | after |
|---|---|---|
| body rows present mid-stream | 0 | `0→2→3` (2 of 3 minimum asserted) |
| table on screen | at `turnEnd` | 452 ms before `turnEnd` |
| rows dropped when the stream ends mid-table | — | 0 (a row without its newline is decided at stream end) |
| a pipe-led line with no delimiter row | prose | prose, and still on screen |

Fences, nested lists, checkbox lists and thematic breaks stay with the parser: their
shape is not knowable from the bytes that have arrived.

### Gates

`SELFTEST ok` (0 failures) against the scripted endpoint; `markdown: all checks
passed`; `svelte-check` 0 errors 0 warnings; `cwoffee-app` gateway_ctl **4 passed**
(including the failed-bind note); production app PSS **246.3 MB** (≤ 400),
`render OK`, `webkit render path: no-dmabuf`, gateway smoke OK, search-smoke OK.

## Batch: the model can open a spreadsheet, and the read is bounded

Stage 1 of the spreadsheet plan is one tool, `sheet_read`, read-only, over `.xlsx`,
`.csv` and `.tsv`. Before it the engine could *make* a workbook (`write_xlsx`) and
could not look at one: `read_file` refused `.xlsx` as binary, so "what is in my
budget sheet" was answered with a refusal to read a file the user was pointing at.

| gate | number |
|---|---|
| engine unit tests, `tools::sheets` | **19 passed**, 0 failed |
| suites this could have broken | `tools:: approval:: prompt::` **195 passed**; `mock_endpoint` **9 passed** |
| self-test (scripted endpoint) | `SELFTEST ok`, 0 failures |
| the new check's line | `sheet_read ok (grid rows 1-2, cap held at 6 cells, continuation starts at row 3, the grid the card shows is what the prompt got)` |
| production app | binary 16.4 MB, PSS **243.1 MB** (≤ 400), `render OK`, `webkit render path: no-dmabuf`, gateway smoke OK, search-smoke OK, holds `renderD128` only |

### The new check can fail, which is the only reason it is worth having

The scripted scenario makes the model write a CSV and then read it back. The step
that plays the answer carries `require: ["Region", "North", "ask again with
range"]`, checked against the **tool results in the request** — the canned answer
proves the script, the tool result in the prompt proves the tool. Armed with an
impossible required string:

```
[mock-llm] REQUIRE FAILED: sheet-read-csv step 2: zzz-not-in-any-tool-output
SELFTEST FAIL: the grid arrived but the answer did not: 8 answer block(s),
               the scripted loop stopped before its last step
```

The UI assertion reads the *tool card*, which holds what the engine really
returned, and cross-checks the paging hint against the rows actually painted:
`continuation starts at row 3` is derived from the last row on screen, not from
the range the model happened to ask for.

### The bound is a bill, not a style rule

| measure | value |
|---|---|
| fixture | 1,200-row CSV, **39,873 bytes** |
| returned under the default 2000-cell cap | **18,526 bytes, 500 of 1200 rows** |
| next range offered | `A502:D` — one past the last row actually shown |
| caps | default 2000 cells, hard 8000, scan stop 400k, file 25 MB, one zip part 64 MB |
| wire flag | `truncated` is the union of the cell cap and the byte clamp |

The cap is on cells rather than bytes on purpose: on a 262k-context, single-slot
endpoint, a full dump is not a large message, it is a cost every later turn pays
again because it is in history.

### Why nothing was written

`sheet_read` writes nothing, and Stage 2 will not be a re-save. OOXML is a zip of
twenty-odd parts; a writer that models sheets and styles and nothing else silently
deletes the charts, pivots, external links and query tables it never read — which
is why `structure` mode counts them and names them before any edit is attempted.
The plan is a surgical package edit that leaves every untouched part byte-identical,
with a preservation test as its gate. Until that test exists, the answer to "edit
my spreadsheet" is a read and an explanation, not a rewrite.
