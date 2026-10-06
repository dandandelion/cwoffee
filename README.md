# Cwoffee

*Cwoffee — a coworker that happens to run on your machine. Same warm palette in
light and dark, now with a cup instead of a cow.*

**A local-first, BYOM co-work agent for Linux. One process tree, ~230–285 MB, your model, your machine.**

Chat, tools, and research — Excel, PDF, docs, image input, sandboxed command
execution — against any OpenAI-compatible, Anthropic-compatible, or custom endpoint.

```
Qoder:  ~1.3 GB across 14 processes, cloud
Cwoffee:  230–285 MB PSS, one process tree, local
```

The memory figure is measured, not estimated, on the production build with the
frontend embedded. It is quoted as a range because the total moves with which WebKit
helpers are alive: 87.6–106.7 MB for the app (engine + Tauri + GTK), 114.7–129.5 MB
WebKit web process, 27.5–38.1 MB WebKit network process, plus 8 MB of WebKit's own
sandboxed image loaders in the heavier sample. The same code without a browser — the
TUI — is **7.0 MB PSS**. See [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

> Status: M0–M2 built and running. The engine, both clients, the sandbox, the store,
> document output, image attachments and the token-guarded remote-access gateway are
> live and verified against a real local model. Packaging produces an installable
> RPM and an AppImage. Not yet done: Knowledge Center, browser module, MCP.

## Run it

```bash
# desktop app against a local llama.cpp server
CWOFFEE_URL=http://127.0.0.1:8090/v1 ./scripts/dev.sh

# terminal client (6.8 MB, same engine)
./target/debug/cwoffee --url http://127.0.0.1:8090/v1 --model qwen3.8-flash-next-iq3_s

# config file instead of env vars: ~/.config/cwoffee/config.toml
cwoffee --config /path/to/config.toml
```

Precedence is **CLI flags > environment > config file > defaults**.

Sessions are created lazily: "new session" is a blank view, not a blank row. The
row appears when you send the first message, already titled by it.

Keyboard: `Ctrl+K` composer, `Ctrl+Enter` send, `Ctrl+N` new session, `Ctrl+B`
sidebar, `Alt+1-9` switch session. The `?` in the status bar lists them. Light and
dark themes; the choice persists, and the default follows the OS.

Images go in by picker, paste, or drag — same guardrails whichever way (mime
allow-list, count and byte caps, enforced by the engine rather than the UI).

Gates:

```bash
./scripts/memory.sh cwoffee-app 400      # PSS, includes WebKit children
./scripts/gpu-check.sh cwoffee-app       # must hold renderD128 only
./scripts/sandbox-probe.sh             # re-run after any bwrap/kernel upgrade
```

## Docs

| File | What |
|---|---|
| [`PLAN.md`](PLAN.md) | thesis, architecture, v1 scope, milestones, memory budget, perf rules, risks |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | every measured number and the command that reproduces it |
| [`SETUP.md`](SETUP.md) | what's installed, what still needs `sudo`, endpoint details |
| [`config/cwoffee.toml.example`](config/cwoffee.toml.example) | providers, models, context budget, sandbox policy |

## Layout

```
crates/engine      the product — agent loop, providers, tools, storage (no UI knowledge)
crates/tauri-app   thin desktop host (links the engine in-process)
crates/tui         ratatui client — the permanent dev/debug harness
ui/                Svelte 5 + Tailwind v4
scripts/           memory.sh (the PSS gate), gpu-check.sh, sandbox-probe.sh, check-docx.py
```

The engine has no idea a UI exists. Both clients drive one `SessionHost` over
channels, which is also the seam for a future daemon/remote mode.

## The three rules that make it fast

1. **Append-only, byte-stable prompts** — so the server's prefix cache always hits.
   Measured: 92% cache hit on turn two, 452 tok/s effective prefill against 38 tok/s raw.
2. **Imperative transcript, never a reactive list** — deltas become text nodes
   batched once per animation frame. 0 dropped frames over 209 frames at 50 tok/s.
3. **Plain text while streaming, markdown on stream end** — never re-parse per token.

## What the status bar actually measures

The server reports token counts only when a turn finishes, so a bar bound to those
numbers sits at em-dashes through the entire turn the user is watching. The engine
therefore emits `TurnStats` a few times a second while a turn streams: TTFT the
moment the first delta lands, then a character-derived token rate, marked `≈` in
the bar and replaced by the server's own numbers when `Usage` arrives.

Measured against llama.cpp on this machine: live estimate 30.3/s then 28.0/s,
server-reported 28.2/s. The estimate is close enough to act on and is labelled as
an estimate rather than presented as a measurement.

The footer does not spell every number out: one **metrics** button carries the two
that explain most latency (cache hit, token rate, context used against the
endpoint's stated ceiling) and opens a popover with the rest — prefill effective
and raw, decode now and session average, first-token time, turn length,
speculative acceptance, tokens in and out, compactions, frame drops, IPC and GPU
state. Context gets a bar because "1,432" on its own answers no question.

Next to it sits the **gateway** pill: off / on with the port and a live device
count, and a panel that turns remote access on and off, chooses loopback or LAN,
and lists copyable URLs. `docs/GATEWAY.md` explains what that costs in security
terms, because it is the one control in the app that widens what a stranger could
reach.

### Thinking levels, measured

The picker also shows what the endpoint says about each model rather than a bare
id: context window, input modalities, tool calling, slots, and the model file and
server build when the server reveals them. Everything is optional and the UI says
"not reported" when the endpoint is silent — `unknown` is never rendered as `no`.
Tool support is the one inferred value (there is no capability flag anywhere; it is
detected from the served chat template), so the row carries a `*` and a footnote
saying so.

Thinking effort used to be one bit on llama.cpp (`enable_thinking`). It now sends
the template's own `reasoning_effort`, and at `high`/`xhigh` the system prompt adds
one line that this model actually responds to: reasoning went from 603 to 1426
characters on an identical question. It does **not** reach the 40k-token marathons
the server's own web UI produces, and the reason is in `docs/BENCHMARKS.md`: on this
checkpoint, *any* system prompt collapses thinking from "runs to the token cap" to
about 250 tokens. The web UI thinks more because it sends no system prompt at all.

### How much context is allowed

Compaction is what keeps prefill cheap on a local model, so the budget that triggers it
matters: too low and the app throws away history it did not need to, and tells you the
context is full when the model has room for six times the transcript.

Unpinned, the budget is derived from the window the **endpoint advertises for the loaded
model** - `window * 0.72 - 2048`, floored at 4096 - and it is re-derived whenever you switch
models. On this machine that is 186,695 tokens against a stated 262,144, and a 43k transcript
sits comfortably inside it. Pin `context.target_tokens` in the config only to keep
conversations short on purpose; a pin is a decision, and the engine honours it and says
"budget pinned in config" in the log rather than silently preferring its own arithmetic.

The old default was a hardcoded 8000 regardless of model, which compacted a 43k conversation
on a 262k model and reported "context full". That is why the number now comes from the server.

### When the model runs out of tool budget

A turn may keep calling tools; the loop is bounded so a confused model cannot spin forever.
When the bound is reached, the engine does not stop mid-sentence and log a warning - it makes
one final request with **no tools offered at all** and asks for an account of the work: what
it did, what is unfinished, what it would do next. That summary streams and is stored like any
other answer.

The instruction that asks for it is transient: it goes into that one request and is never
pushed into the transcript, so the user does not get a fake message from themselves and the
append-only prefix stays intact. If the summary comes back empty the turn still ends with a
visible note rather than silence.

### Motion

Everything moves on one curve (`cubic-bezier(.2,.7,.2,1)`) with three durations, and
each transition answers a question: a new block eases in because it just arrived, a
tool card expands rather than snaps because you asked it to, the metrics bar slides
because a value changed, the send button's ring appears because you are about to
press it. `prefers-reduced-motion` turns all of it off. The one looping animation in
the app is the steam on the empty screen's cup.

Streaming text **arrives** rather than appearing. Each word that lands since the last
repaint becomes its own inline `<span>`, blurs from 1.6px and fades from zero, and is
**never touched again**. Nothing about that word is re-laid-out, re-parsed or re-animated
for the rest of the turn: the block is append-only, exactly like the prompt that produced
it. Duration is not a constant either — it is derived from the observed word rate
(`600/words_per_second`, clamped to 110-300ms), so a fast model reveals quickly and a slow
one lingers, and the stagger across a group of words is capped at 168ms so a burst of
tokens cannot queue up an animation backlog that plays for a second after the text stops.

The unit is the **word**, not the chunk. Earlier designs animated whatever arrived since
the last paint, then folded finished chunks back into a re-written paragraph. Folding is a
rewrite, and a rewrite re-rasters: every already-settled word blinked again roughly half a
second after it landed, which reads as the animation "playing catch up" with the text. The
append-only version has no fold step to blink in.

While a turn streams the block is plain text in `white-space: pre-wrap`, and Markdown is
applied **exactly once**, at stream end. That keeps newlines visible as they arrive (without
it an answer lands as one grey wall and then breaks up, which is the one layout jump in a
turn) without parsing Markdown per token.

What the harness proves, in the app's own webview rather than in a screenshot: 28 word spans
for four streamed reasoning deltas, every one of them still connected to the document after
later paints, their `textContent` concatenating back to exactly what the model sent, and no
span block-level. That is the whole contract — an animation that is invisible in the computed
styles but rewrites settled text is still a bug, and a check on node identity catches it.

Two details that paragraph depends on:

- A word span is **inline**. Built as a block element instead, every arrival starts a new
  line: the block grows straight down the page, gets taller on each paint, and the
  transcript — which pins to the tail — nudges the whole view with it. That is a flicker
  report waiting to happen, and it is why thinking once looked like it "grew vertically
  instead of filling the width".
- The animation fills **backwards**, not `both`. With `both`, a finished span keeps its last
  keyframe, `filter: blur(0)`, forever, and a filter claims its own raster layer. The text
  then exists twice — the layer the animation holds, and the layer the paragraph is drawn
  into — and swapping between them pops at the end of every transition.

## Files, and asking instead of guessing

The agent's file tools are `read_file`, `write_file`, `edit_file`, `write_xlsx`,
`download_file`. Two decisions there are worth knowing about:

**`edit_file` is search-and-replace, not a diff.** A 4-bit local model copying a
line it can see does far better than one generating hunks with correct line
numbers. An exact single match is applied. No match returns the line that *looks*
most like what was asked for, which turns the model's usual
"almost right, wrong indentation" failure into a one-iteration correction.
Multiple matches are refused unless the call says `replace_all`. Text is written
to a sibling temp file and renamed over the original, so a failure partway cannot
truncate your file. Anything that can be edited - text, markdown, code - should
go through `edit_file`, which changes a few lines instead of re-emitting the whole
document and rewriting your whole chat window for it.

**`ask_user` stops the agent guessing.** One question, one to six options, one
marked recommended, and a typed answer always available. The TUI answers with
number keys. The turn parks while it waits; cancel does not make the question
disappear but releases the parked tool with a "cancelled" result the model can act
on. Over the gateway, a watch-only client sees the question and cannot answer it.

Three rules the question card follows, because a card that answers on contact is a
card that answers by accident:

- **Clicking an option selects it; Send answer sends it.** A misclick on a question
  about deleting something has a real cost, so the click is provisional and the
  submit is explicit.
- **The model decides whether several answers are allowed** (`multi`). When it does,
  the rows become checkboxes, second clicks untick, and the button counts what is
  going: "Send 3 answers".
- **The answer restates the question.** A quantized local model two tool calls
  later has no idea that "PDF" was an answer to anything, so the tool result carries
  the question and the options it offered alongside what you chose or typed.

A question that closes with no answer - the timeout expired, or you cancelled the
turn while it was parked - closes in every window as "no answer in time". Left
alone it would sit there offering Send, and the answer would aim at a tool call that
has already returned.

## What you can do with a finished turn

Under a finished answer sit three controls, quiet on purpose: **Copy** puts that answer on the
clipboard as Markdown - the text the model wrote, not the rendering of it - and **Share as
Markdown** writes the whole conversation into `exports/` in that conversation's own workspace,
then shows the resulting file as the same clickable chip every other generated file gets. The
chip is the receipt: it proves where the write landed and opens it.

The document is for a person. It names the session and the working folder, keeps the prose,
summarises each tool call in one English line ("wrote reports/q3.xlsx", "ran a command"), lists
the files produced, and leaves the reasoning out while saying how long it took. No tool-call
ids, no `<env>` plumbing, no chain of thought. Writing goes through the engine rather than the
UI, so the containment rules, the collision-avoiding suffixes (`-2`, `-3`, never an overwrite -
you may have edited the last one by hand) and the output metadata are identical whether you
clicked in the desktop app or on a phone.

**Branch** is there and disabled, with "not built yet" in its tooltip. A control that looks
available and does nothing teaches the user to distrust the rest of the interface.

Right-clicking anywhere replaces the browser's menu - Back, Forward, Reload, which in a
frameless window are three ways to damage the app - with one built from what is under the
pointer: open a file or copy its path on a chip, copy a code block or preview it, copy a
message, quote it into the composer, copy the selection. It is clamped to the viewport, closes
on Escape or a click away, and stays open when the transcript scrolls itself to follow the
stream. Inside the composer the **native** menu is deliberately left alone: the webview's own
Paste entry is wired to the clipboard in a way a page cannot reach, so suppressing it would
remove a capability rather than improve a menu.

## What an approval actually gates

Every tool call is classified per call, not per tool, because the same tool is not
equally dangerous twice:

| class | example | `ask` mode |
|---|---|---|
| `harmless` | reading a file inside the workspace, a new file, a web search, asking you a question | runs |
| `workspace` | overwriting an existing file, editing a file in place | asks |
| `sensitive` | `exec`, any path that leaves the workspace | asks |

The interesting column is the one that says *where*. A call that reaches outside the
workspace is reported with the resolved absolute path - "allow reading
/home/you/.ssh/id_ed25519?" - and the permission you grant covers **that call**, not
the rest of the session. "Allow for session" grants a tool; it has never granted an
escape, and that is deliberate: one yes to `read_file` should not be open access to
your home directory.

Approval prompts name the file rather than dumping the call: `edit in place:
notes.md (find and replace inside the file)`. The raw argument blob is the one thing
you cannot weigh a decision on.

**Stop beats an unanswered prompt.** A prompt the model raised and nobody answers -
a tab that went to sleep, a picker you never opened - would otherwise hold the turn
open for the whole approval timeout, so the Stop button looks dead for five minutes.
The engine lets a Stop press win over a pending approval, the tool gets a
`tool_result` saying the turn was stopped, and the model is not given another turn
after you pressed stop. A park is released rather than left to rot: that is also why
unanswered approvals fail closed instead of hanging.

## Workspaces

The sidebar groups sessions by the folder they live in, and the folder the tools
write into follows the session you open. That is not decoration: without it
`write_file` lands wherever the *last* session pointed, and a session that says it
lives in `~/Projects` quietly writes into `~/cwoffee-workspace`.

- A stored workspace that no longer exists (or is not writable) is **refused**: the
  engine says which session, which path and why, and the tools stay where they were.
  Silently falling back would write your files into the wrong tree.
- The engine is the authority on what a workspace may be: a path has to exist, be a
  directory, and be writable, and the writability is proved by writing a probe file
  rather than by trusting the mode bits.
- `+` on a workspace row opens a draft that already knows its folder. Nothing is
  written and no session row appears until you send the first message.
- The `+` in the Workspaces header opens the native folder picker immediately. A panel
  that makes you type a path before it will open the picker that picks it for you is a
  step that should not exist. (In the browser there is no picker to open — the folder
  is on the host, not on the phone — so a typed host path stays as the fallback there.)
- **A session keeps the folder it started in.** Moving one mid-conversation makes the
  grouping a lie: the sidebar row names the old folder while `write_file` lands in the
  new one. The engine refuses it, and the composer's folder control is disabled with
  the reason — a thing you cannot do should not invite a click. Working somewhere else
  means a new session in that folder, which the sidebar `+` does in one click; a draft
  that has not been sent yet can still choose its folder.
- `/tmp` is not a folder you work in, it is where sessions live before anyone chooses
  one. Sessions stored there group under **Default** instead of getting a `tmp` row
  beside a `No workspace` row, and registering the temp root as a workspace is refused
  and said out loud in a toast rather than only written to a log.
- Renaming a group changes the label, never the path.

Every file a turn produces appears once as an attachment however many times it was
written, and file tools show the path they act on in the card header, so you can
see what a call touched without expanding it.

A file tool also reports the size of its own edit, in the header line, in English:
`Edited a file · notes.md · +1 −1 · OK`. Expanding the card shows a real unified
diff — hunk header, context lines, red removals, green additions. The counts come
from the lines that actually changed rather than from the model's description of
what it did, and they are stored with the message, so a session reopened next week
still says what that edit cost. For an edit the diff replaces the raw argument blob:
showing the same change twice, once as 400 characters of JSON, is noise.

### What a finished turn looks like

A collapsed transcript should read as a list of things that happened, not as a log of
API calls. So:

- Tool cards are headed in English, and tense carries state: `Running a command` while
  it runs, `Ran a command · OK` when it ends, `Could not edit that file` when the
  arguments never parsed. Raw ids (`write_file`, `exec`) appear nowhere in a transcript.
- A finished tool card is its header line and nothing else — no clamped block of JSON
  underneath it. Click for the arguments, the output and the diff. While a call is live
  the body peeks, because a command with nothing under it looks stuck.
- Thinking collapses the same way: while it runs you see three lines of it; once it
  settles the block is `Thought for 12s ›` and nothing more.
- Your own message is a right-aligned bubble, at most three quarters of the measure,
  text left-aligned inside it. The `CWOFFEE` label hangs in the left gutter instead of
  taking 74px off every block, so the answer, the thinking blocks and the tool cards
  all start on one edge — the edge your bubble ends on.

## Which conversation a turn belongs to

One engine, several screens. A desktop window, a phone tab and the app's own test harness
can be attached at once, so a turn carries the conversation it was written in: the composer
sends the session id you are looking at, and the host follows it. A turn addressed to `0`,
meaning a draft, opens a new conversation rather than appending to someone else's. An id
that matches no conversation sends nothing and says so, rather than falling back to whichever
session the engine happened to have open.

**Looking is not working.** Clicking a session in the sidebar moves the *view* only: it loads
that transcript and leaves the engine where it was. It used to run a full session switch,
which re-adopted that session's folder and re-checked its prompt cache, so reading someone
else's conversation printed a workspace-change line and a cold-cache warning as though you
had done something. The host moves when a turn is addressed to a conversation, not when a
mouse crosses a row - and the folder chip above the composer follows the conversation on
screen, not the one the engine is inside.

Switching sessions during a turn is allowed and instant: the view moves at once while the
host finishes the turn it is inside. In the sidebar, a turning arc means that session is
answering; a pulse means a turn finished there while you were looking elsewhere.

## The empty screen

The cup, the greeting, the tips and the composer are centred as one group - centring only
the composer left the cup glued to the top of the pane. The steam is drawn: three curves
whose feet sit on the coffee surface in the artwork's own coordinate space, each stroked
twice (a wide blurred body and a brighter thread inside) with a puff travelling up the
curve and a gradient that fades it out toward the top. Anchoring it to the artwork's
space rather than to pixel offsets is what keeps it coming out of the cup at any size.

At the top of the sidebar sits a Cowork / Chat tab. It is not wired to anything: Chat mode
does not exist yet, and a control that pretends otherwise is worse than a missing one.
`+ New session` survives as a small `+`, plus Ctrl+N and the `+` on each workspace row.


## Icons

Every icon is inline SVG at one stroke weight, coloured by `currentColor`: composer
controls, sidebar, file chips, code-block actions. No emoji and no icon font. An emoji
renders in full colour from a system font, so a two-tone app sprouts a blue picture frame
and a yellow folder, and renders differently on every platform - which is exactly what the
file chips were doing.


## Two things that are security, not polish

- **`exec` runs inside `bwrap`** — read-only root, workspace-only writes, no network,
  PID namespace, deadline enforced inside the namespace. The denylist is a guardrail;
  the sandbox is the boundary, and `sandbox-probe.sh` proves it rather than trusting
  the flags.
- **Document writers escape before they emit any markup.** Model output quotes fetched
  web pages, so `<script>` and `javascript:` arrive in the text. A writer that passes
  them through is an injection path into a file the user opens in Word.
- **Links leave the webview, and attachments are refused at the engine boundary.**
  Rendered markdown links go to the OS through tauri's opener plugin (scope-checked),
  never by navigating the app window. Image attachments are validated by the engine —
  mime allow-list, count and byte caps, base64 shape — not by the composer, because
  the composer is not going to be the only client forever.

## Name

**Cwoffee** — *cowork* with the coffee in it. A co-work agent you drink with:
it sits at your desk, it uses your model, and the palette is a flat white in
light mode and an espresso in dark.

The mark is drawn by hand and lives at `assets/brand/cwoffee-source.png`. Nothing
else is hand-drawn: `scripts/make-icon.py` crops that one file to its own bounds and
resamples it into everything the app ships — the window icon embedded in the binary,
the hicolor set the dock looks up, the bundle icons for RPM/AppImage, `icon.ico`, the
mark in the titlebar and on the empty screen, and the favicon a gateway tab uses. If
an icon looks wrong, the fix is the source file or the crop rule in the script, never
a redraw somewhere else.

It used to be **Cowry** (a cowrie shell, one of the oldest currencies — value
exchanged between people, and the *cow* of *cowork*). The rename is cosmetic in
intent but real in the filesystem, so it migrates rather than orphans anything:

| Was | Now |
|---|---|
| `~/.config/cowry/` | `~/.config/cwoffee/` |
| `~/.local/share/cowry/sessions.db` | `~/.local/share/cwoffee/sessions.db` |
| `~/cowry-workspace` | `~/cwoffee-workspace` (yours was moved, not recreated) |
| `COWRY_URL`, `COWRY_MODEL`, … | `CWOFFEE_URL`, … — the old names still work |
| `cowry`, `cowry-app` binaries | `cwoffee`, `cwoffee-app` |
| localStorage `cowry.theme.v1` | `cwoffee.theme.v1` — old keys are read and re-saved |

The data migration runs at startup and is conservative: if both directories
exist it moves the legacy database only when the new one is empty, and keeps the
copy it displaced aside as `*.pre-cwoffee-<timestamp>` instead of deleting it.
Anything it will not decide for itself, it says out loud in the transcript
rather than in a log file you would never open.
