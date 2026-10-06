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
