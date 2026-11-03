# The files ledger: the whole file story of a conversation

A turn's revert card answers one question — "put this turn's files back". It has never answered the
question a person asks when they are *not* undoing anything: which files has this conversation
touched, what did each turn do to them, what can still be put back, and what is this costing me?

The ledger answers that, and it is a **read over machinery that already exists**. Nothing new
records file state:

| the ledger reads | what it is |
|---|---|
| `checkpoints` rows | one row per file call: turn, path, kind, the copy's hash, the after hash |
| `<workspace>/.cws/snapshots` | the copies themselves, addressed by content hash |
| `checkpoint_prunes` | the notices for copies that went, with the reason they went |
| `tools::changes::line_change` | the same unified diff the tool card paints |

The transcript, the rows and the files on disk are untouched by every read in this feature. The one
verb that changes anything is `forget_copies`, and it changes only copies.

## v17: why a copy went

`checkpoint_prunes` gained `reason`, and there are three reasons a copy is not there. Telling them
apart is the feature; a card that said "no copy" for all three would be describing a limit nobody
hit, or a deletion nobody asked for:

- `cap` — the per-conversation limit (`CHECKPOINT_CAP`, 200 rows) discarded the oldest copies.
  `Ledger.pruned` counts **only** these, from the same overview the revert card reads, so a person
  who never hit the limit is never told they hit it.
- `forget` — the person pressed the ledger's copies control. This lands in `prune_notes`, never in
  `pruned`.
- **out of band** — the snapshot folder was emptied outside the app. Nothing was logged, because
  nothing in the app did it; the row reports `copies_missing` and its blob is simply not on disk.

A row whose copy is gone offers neither a diff nor a revert, and its `says` line names the reason.
A control that would do nothing is worse than no control, which is the rule the revert card already
follows.

## What a row says

`LedgerFile` is drawn from one index row plus one `stat`:

- `kind` — `created` | `modified` | `deleted`, the words the row stores. `created` means there were
  no earlier bytes: reverting it **deletes** the file, and that is what its `says` line reads.
- `copy_present` — the copy's bytes are on disk. A created row is `false`, not "present": saying a
  copy exists when none was ever taken is how a card ends up offering to restore bytes nobody saved.
- `can_revert` — there is an answer row to address the undo at, *and* something to put back. A turn
  whose answer is not written down yet (`message_id` `NULL`, i.e. still running) is `false` and says
  so; that is why the `Files` control waits for the turn to be stored rather than guessing.
- `edited_since` — the file on disk no longer matches `after_hash`, i.e. somebody edited it after
  the turn. The revert would overwrite that edit, and the row says which.
- `path_missing` / `live_exists` / `live_bytes` — which side of the conversation exists now. A
  deleted file keeps its history and reports no live side.

Turns are grouped newest first, and a turn's `files` count is the length of `file_rows`, so a line
that reads "3 files" and lists two is caught by the count rather than by somebody noticing.

## Two byte numbers, kept apart

Copies are content-addressed, so a blob can be pointed at by two turns of one conversation, or by
two conversations that overwrote the same file. `snapshot_stats` therefore prints both numbers and
says which is which:

- `rows` / `row_bytes` — what the index says. This double-counts shared blobs, by design, because
  it is the count of *copies the turns took*.
- `blobs` / `blob_bytes` — distinct files in the store and their bytes on disk. This is the number
  `du` agrees with.
- `shared` — blobs another conversation also points at. Forgetting this conversation leaves them.
- `orphan_blobs` / `orphan_bytes` — files in the store that no index row in any conversation points
  at. That gap is the leak this reports rather than hides.

`forget_copies` frees bytes the same way it counts them: it dedupes the conversation's hashes before
deleting anything, so a receipt for two rows sharing one blob says one file's bytes freed. A receipt
about a deletion that did not happen is exactly the kind of sentence this feature exists to prevent.

## A diff needs two sides

`file_diff` compares the stored copy against the bytes on disk through the same `unified()` the tool
card uses, and refuses when there is only one side:

- a row that **created** the file has no earlier bytes. Printing its live content as a diff would
  read as every line having been added by somebody. What the turn wrote is on the tool card.
- a row whose copy is gone (`cap`, `forget`, or out of band) has no stored side. The live file alone
  is not a comparison.

Both refusals say "Nothing was diffed" and write nothing. A stored copy against a file that is now
*gone* is a real comparison — before against absence — so that one is drawn.

The row is named by **id**, and the request must carry the path the card painted as `shown`. A window
that has been sitting open cannot ask for "the copy of `a.md`" and get whatever `a.md` belongs to for
the newest turn by now; a mismatch is refused and says what the row actually holds. Rows whose path
resolves outside the workspace are refused too: a diff of a file no tool of this app could have
written is not part of the conversation's story.

## Forgetting copies

`forget_copies` deletes files, so it is built like the destructive verbs next to it:

- **drive-only and audited** (`AUDITED`), like `checkpoint_revert` and `switch_leaf`. A watch token
  gets "this token can watch, not drive".
- **two presses**, the second labelled with the count the card printed.
- **the count must match the record.** `shown` is the copy count the card drew; if the record says
  something else, nothing is deleted and the refusal prints both numbers. Zero is refused as the
  mismatch it is rather than answered as a no-op.
- **refused mid-turn.** A turn is taking copies of files right now; deleting underneath it would
  leave rows pointing at bytes that were just removed.
- **reachability is not this conversation's question.** Before deleting a blob it checks whether any
  *other* conversation points at it, and leaves those. The receipt says how many were kept and why.
- **the transcript and the files stay.** Rows, messages and the workspace files are untouched; the
  only thing that goes is the copy index and the blobs nobody else can reach.

Afterwards the ledger reads as an empty record with one notice: the next `checkpoint_preview` refuses,
the `Files` card says "Nothing to forget", and the notice says who removed the copies.

## Caps, and what happens when they bite

`LEDGER_TURNS` (60 turns) and `LEDGER_PATHS` (200 paths) bound one card. The counts stay the
record's — `files_total` and `paths_total` are the whole record, `turns_hidden` and `paths_hidden`
say what the card left off, and the note names it. The cap hides the *oldest* turns, because a person
opening this is asking what the conversation did recently.

## Both cores

The ledger reads the store, and both cores write the same store:

- **Rust core** — the file tools capture through `checkpoint::capture` on the way out of a tool call,
  and the host stamps the rows to the answer row when the turn is persisted.
- **pi core** — the same capture runs inside `cwtool`, the tool wrapper, with `CWOFFEE_CHECKPOINTS=1`;
  the bridge stamps rows to the answer row when pi's turn settles. The ledger reads the result and
  knows nothing about which core wrote it, so `files_ledger`, `file_diff` and `snapshot_stats` are
  word-for-word identical on both cores, and the selftest group runs on both.

`forget_copies` asks the live host whether a turn is running. On the Rust core that is `SessionHost`'s
state; on the pi core it is `PiHost`'s, which is why the verb is answered by the bridge's host rather
than by the store alone.

## Tests

- `ledger::tests` (engine) — grouping and flags, a created row's line and its refusal, an unstamped
  turn, a blob deleted out of band, a cap eviction's notice and its absent rows, forget-vs-cap, the
  two byte numbers and the deduped receipt, a shared blob surviving a forget of the other
  conversation, orphan bytes, the stale-path and foreign-row refusals writing nothing, an escaped
  path, a stale `shown` on the delete, the caps, and a path's turn list.
- `tests::the_file_ledger_reads_are_watch_and_forgetting_copies_is_drive_and_audited` (gateway) — the
  role split on the wire, the identity guard, the refused delete changing nothing, and the freed bytes
  of two rows sharing one blob.
- `selftest.sh ledger` on both cores — three writing turns, the ledger read, the card on screen, a
  diff quoting both sides' real bytes read from disk, a stale-path refusal that read nothing, the
  mid-turn refusal, and a forget that empties the store while leaving the files and the messages
  alone. The cap arm is cargo-side: reaching it means 200 file calls in one turn, which is a load
  test; the notice line is graded through the forget path, which renders the same rows.
