# The terminal's doors, and the gate that drives a pty

`crates/tui` is the same engine as the desktop, in 7 MB and without a browser. Until this batch it
was also the only shell in the repo with **no automated check of any kind**: 1194 lines of skin,
18 keybindings, two host commands (`SetMode`, `SetReasoning`), and nothing that could notice when
the engine grew a door the terminal never opened.

Two facts made that a bug rather than a gap:

- `host.rs` sets `checkpoint_store: true` for every turn. So a TUI turn was taking copies of files
  the terminal user could not list, diff, or put back — bytes with no door, paid for silently.
- `[mcp]` was read nowhere in the terminal. `explore` there answered *"No MCP server is running"*
  about servers the person had configured, which is the honesty rule inverted: a control claiming
  an absence that isn't there.

## The doors are calls, not implementations

`crates/tui/src/commands.rs` is thin on purpose. Each command calls the function the desktop and
the phone already call, so a third shell cannot drift a second revert:

| command | what runs it |
|---|---|
| `/branch [N]` | `branch::apply_and_open` |
| `/leaf N` | `tree::move_leaf` |
| `/tree` | `tree::read` |
| `/files` | `ledger::read` + `ledger::stats` |
| `/undo` | `checkpoint::preview`, then `checkpoint::revert` on the yes |
| `/forget` | `ledger::forget` on the second press |
| `/mcp` | the hub this start spawned, read from its state |

The proof each door asks for is the proof the engine already demands: `/branch` repeats back the
text **the terminal painted**, not a fresh read of the row, so the drift guard means something.
`/leaf` does the same against the nodes `/tree` listed — which is why `Outcome` carries the nodes
it printed instead of letting the shell re-read the record and address a row by position.

An empty ledger refuses with the desktop card's own sentence (`Nothing to forget`), because an
empty list that reads as a broken list is the complaint that made the wording a claim.

Refusals are the engine's sentences, quoted: mid-turn branch/leaf/revert/forget refuse, and a
steer cannot answer a card or move a gate. There is **no watch role in a terminal** — one keyboard,
one host — so `/help` says that instead of pretending the phone's rules apply here.

## Two things that were broken by being looked at

**Engine notes corrupted the screen.** The engine talks on stderr: the sampling line, a saved
setting, a search config, a titling failure. A terminal that owns the screen cannot take those —
they land at whatever cell the cursor happens to be in. Observed in a pty, not theorised: the
header read `Cwoffee  ccwoffee-mockt ·20sessionn0 (·odthinkingwxhighow·`, because a boot note was
painted through it. `park_stderr` now points fd 2 at a log file for the length of a painting run
and hands it back on exit, where the lines are printed; the file's path is said in the transcript
at boot, because a note nobody can find is a note that was never written. Boot facts the *terminal*
owns (which servers came up, which one died, what the config file was) are transcript lines now.

**A card swallowed every keystroke.** `y`/`a`/`n` answering an approval is fine until the person
types a message during the turn: the terminal handed every key to the card, so typing went
nowhere and the mid-turn queue was unreachable while a card was up. The desktop never did that —
typing goes to the composer, Enter queues a message *beside* the card, and only the button
resolves it. The terminal now answers its card keys **only while the composer is empty**, and says
so on the card; the first letter of a message types instead of answering. The same rule covers the
`/undo` and `/forget` confirmations, where an accidental `y` would overwrite files.

## `scripts/tui-gate.mjs` — group `tui`

The gate drives `target/release/cwoffee` on a pty (its own tmux server, on a socket named for the
run, so it cannot touch anyone else's) against `scripts/mock-llm.mjs`. It never touches
`http://127.0.0.1:8090`. It is run through `cargo-guard` from `selftest.sh`, and `selftest.sh`
builds `-p cwoffee-tui` in the same line as the app so a stale terminal binary cannot be graded as
a fresh one.

What it grades is the **disk and the store**, and the screen only where the claim is about the
screen:

- the turn really wrote the file (`25B` read off disk);
- `/files` names that path — read from the listing's own block, because the tool card above it
  quotes the same path in its arguments and matching *that* line would grade the model's call
  rather than the door;
- `/undo`, confirmed with `y`, leaves the file holding **the words it held before** — read from the
  file, not from the line the terminal printed;
- `/branch` copies into a new conversation and leaves the original's rows byte-identical, and puts
  the row's words back in the composer without sending them (the composer's own line, not the
  transcript's);
- a press beside a live approval card queues and answers nothing, `Ctrl-U` empties the composer so
  `y` means `y`, the card's answer runs the tool, and the model's next request carries the pressed
  words (`/mock/state` `steer_hits`), with exactly one new user row in the store;
- the landing receipt says where it landed, read by scrolling the terminal's own transcript;
- `/branch` mid-turn refuses in the engine's sentence **and creates nothing** (the session count is
  unchanged) — a refusal that acts is worse than no refusal;
- Stop hands the waiting words back into the composer and says so;
- `explore mcp:` with no servers answers `No MCP server is running`, read from the **tool row in
  the store** — what the model was given, not what the viewport was showing;
- with this run's own `scripts/fake-mcp-server.mjs` mounted, explore names `mcp__fake__write_note`
  and `MCP server fake`, and `/mcp` reports `fake: live`, the dead server as `dead: …`, and the
  catalog block's bytes against `CATALOG_BYTE_BUDGET`.

Two claims the gate cannot enforce, so they are not claimed:

- **The boot lines.** They go to stderr before the alternate screen, and a pty cannot show them
  afterwards. The hub's state is graded from `/mcp`, which is what a person can actually re-read;
  the gate prints a note saying which half it measured.
- **A file diff.** The terminal has no `/diff`. It lists paths, previews what a revert would do,
  and reverts; comparing a stored copy against the live file is still a desktop and phone thing,
  and README's "Not built" says so rather than letting the door list imply it exists.
