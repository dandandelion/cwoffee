# The session tree: lineage and the leaf

Three verbs move a conversation, and only one of them is a leaf:

| verb | what it does to the record | what it does to the model |
|---|---|---|
| `edit_message` with `resend` (rewind) | deletes every row after the one being corrected | re-seeds from what survives |
| `branch_conversation` | **copies** a stretch of rows into a new conversation | pi forks its thread; the Rust core replays the copy |
| `switch_leaf` | moves one column on the session row | pi forks to the prefix; the Rust core re-reads the branch |

A leaf is the node a conversation's next answer reads *through*. Nothing is copied and nothing is
deleted: the rows past that node stay in the file, on a branch that is not being read. That is the
whole difference from a rewind, and it is why the record and the screen can disagree — the transcript
still shows what was said, and the model is reading a subset of it.

## v16: four columns, and what each one means

`sessions` gained `parent`, `branch_role`, `branch_row`, `leaf_row`; `messages` gained `parent_row`.
The migration is the usual `ALTER TABLE` inside the versioned upgrade, and a v15 database opens with
lineage and reads exactly as it did before — asserted by `tree::tests::a_v15_database_gains_lineage_and_reads_the_same`.

`leaf_row` has three states, and all three are answers rather than gaps:

- `None` — nobody moved the leaf; the conversation reads its **newest** row. This is what every
  conversation looks like until someone presses the control, and what a row written before v16 looks
  like forever.
- `Some(row)` — the path that runs from that row up to the root is what gets read.
- `Some(0)` — the leaf sits in front of the first message. The model is handed nothing but its prompt,
  and the card marks no node as current, because saying "row 1 is being read" there would be a lie.

`parent` is written by `branch::create_row`, **before** any row is copied, together with the role and
row id the branch was cut at. If that write fails the new session row is deleted and the branch is
refused: a conversation in the list with no parent is the one thing the tree cannot reconstruct later,
and leaving it there would leave a claim about the conversation's history that nothing can correct.

## Reading a conversation is reading its branch

`Store::messages()`/`history()` walk from the leaf to the root rather than reading every row the file
holds. That is the assumption this feature removed, and it is the one to know about when something
looks like a bug:

- `session_messages` on a conversation with two paths returns **fewer rows** than the conversation
  holds. The sidebar's `message_count` is the whole count, so "nothing was deleted" is checkable from
  the row next to it, and the composer line says what is being read.
- `truncate_from` resolves the leaf by walking **the leaf's own chain**, not by taking `MAX(id)`. On a
  conversation with two branches those differ: a leaf inside a rewind has to move to the first
  surviving row on its path, and a leaf earlier than the cut has to be left alone.
- `append_message` attaches to the current leaf tip and advances it, which is how a message sent after
  a leaf switch joins the branch the leaf was left on.
- `copy_through` copies the source's **branch**, not every row with `id <= cut`. Those are the same set
  until a leaf moves; after that they are not, and a branch cut at a node would otherwise carry rows the
  person never saw in that conversation — a transcript that looks branched and answers from a path nobody
  was standing on. Rows the leaf walked past stay in the record and are reported as off-path.

## pi's thread is a second record, and it is read, not inferred

`get_entries` answers the thread the live process holds: a flat list of entries in file order, each with
`parentId`, plus a `leafId`. The file the entries were appended to is a list, and the conversation order
is not the list order as soon as anything is forked: a branch appended last hangs off an entry written
first, so reading the list marks the first path written, which is not the path the next answer reads.
`pi_entries_from_json` therefore marks `on_branch` by walking **up from `leafId`**.

`get_tree` — the other answer pi gives — is not usable here, and that is worth knowing before
something reaches for it: it returns a defensive copy of the tree as nested `children` arrays, one JSON
level per entry, so a sixty-message conversation exceeds the bridge reader's recursion limit and the
whole record comes back unparseable (gotcha 168). It is also on the page's every-open path, so the
cheap read is the one the composer can afford.

`crates/bridge/tests/fixtures/pi-tree-branched.json` is a captured `get_entries` answer built so the
list order and the conversation order disagree, and the tests read it through the tree path rather than
`rewind.rs`'s `leafId`-only path. What they hold:

- an entry of pi's thread that no row answers to is a **line of the tree**, not a silent omission;
- pi's own writing (`model_change`, `thinking_level_change`, the system prompt it assembled) is a node
  and **not** drift — the record holds messages, answers and tool results, and never the prompt the
  engine built;
- an entry on a branch nobody is reading is said as off-path, which is a different fact from
  disagreeing about the answer being read;
- when pi's leaf and the record's leaf stand on different **messages**, the card says so, and each side
  keeps its own mark (`current` is the record's, `pi_current` is pi's); when pi's thread ends on its own
  bookkeeping while the record reads nothing, that is two records agreeing, not two apart.

## Moving pi's leaf is a fork, and it leaves the old thread behind

pi's RPC has no `set_leaf`. `navigateTree` exists in an extension context, and the RPC parent cannot
call it; asked mid-turn it refuses outright. The primitive that *is* reachable over the RPC is the one
`branch_conversation` already runs on: `fork` copies a prefix of the thread into a new file and
re-binds the process to it, and `clone` is the same thing asked at the leaf.

So on the pi core a leaf move means:

1. adopt the conversation's row (pi can only fork the thread it is on),
2. compare the record's user messages against `get_fork_messages` — counts *and* words, because a
   fork taken at the wrong entry is a conversation answering from history nobody can see,
3. fork before the target user message, or before the message after the target answer; `clone` when
   the target is an answer at the leaf,
4. write the record's leaf only after pi accepted the fork, and switch back if the write or the state
   read fails.

The old thread is not deleted. It stays on disk holding the off-path messages, which is what the
receipt says out loud: the person can reopen it, and nothing has been thrown away. Both cores answer
the same verb with a sentence about **their own** memory, because "pi moved its leaf", "this engine
re-read the branch", and "this host is holding another conversation" are three different facts and the
one that matters is whether the model moved with the record.

## Both transports, one function

`session_tree` is a watch verb; `switch_leaf` is a drive verb in `AUDITED`. The gateway arm and the
desktop command call the same `cwoffee_engine::tree::read` / `move_leaf`, so a phone sees the same
tree, the same mid-turn refusal and the same "that row says something else now" refusal as the window
beside it. Watch-role can draw the leaf; it cannot move it.

The identity guard is the same discipline `branch` uses: a move names a row **and** repeats the words
that window painted for it. A stale tab, a phone in a pocket and the harness all read one conversation,
and a leaf moved by a row id alone is a leaf moved onto whatever that id holds by now.
