#!/usr/bin/env python3
"""Remove harness probe turns that earlier self-test runs wrote into real
conversations.

A probe turn is one user message plus everything the engine appended after it
until the next user message. The phrase list is deliberately exact: harness
prompts are machine-worded ("What colour is this image? Answer in one word."),
and a real user message that happens to ask about an image must survive.

The FTS index is a standalone fts5 table, not a content-table mirror, so its
rows are deleted by message_id as well - leaving them behind means search keeps
offering conversations that no longer contain the text.
"""
import re
import sqlite3
import sys

DB = sys.argv[1] if len(sys.argv) > 1 else None
if not DB:
    raise SystemExit("usage: probe_cleanup.py <db path> [--apply]")

APPLY = "--apply" in sys.argv

PHRASES = [
    "What colour is this image?",
    "Use the exec tool to run: echo cwoffee-ok",
    "Selftest wipe check",
    "Selftest live marker",
    "Use write_file to create selftest-note.md",
    "Name six colours, each on its own line",
    "Count out loud from one to fifteen",
    "Think out loud about why a coffee cup makes a good icon",
    "Reply with the single word done",
    "Use write_file to save about 50 words about espresso",
    "Say the single word: 42",
    "Repeat the word latte three times",
    # Turns a browser harness typed into whatever session was on screen. Not a
    # model prompt at all: a scripted Playwright probe typing into the composer.
    "Answer in two sentences about why espresso is short",
    "Say the single word pong",
    # A verification run that filmed the streaming reveal through the gateway UI.
    "with the markdown shown: a level-2 heading",
    "Name six colours, each on its own line, and nothing else",
    # Runs that filmed the streaming transcript: a fixed shape prompt typed into
    # the composer by a Playwright harness, then measured frame by frame.
    "Write about coffee roasting. Exactly this shape, no preamble",
    "Write about coffee roasting in exactly four paragraphs",
    "Answer in the chat only, do not create or open any files",
    "Answer in the chat only, use no tools and create no files",
    'Say the word "pigeon" and nothing else',
]

conn = sqlite3.connect(DB)
conn.execute("PRAGMA foreign_keys = ON")

pat = re.compile("|".join(re.escape(p) for p in PHRASES), re.I)

# Manual `DELETE FROM sessions ...` on this database leaves messages behind, and
# orphaned messages keep joining into search results that point at a session the
# sidebar no longer has. Same for fts rows whose message or session is gone.
orphan_msgs = [
    r[0]
    for r in conn.execute(
        "SELECT id FROM messages WHERE session_id NOT IN (SELECT id FROM sessions)"
    )
]
orphan_fts = [
    r[0]
    for r in conn.execute(
        "SELECT rowid FROM messages_fts WHERE message_id NOT IN (SELECT id FROM messages)"
    )
]
print(f"orphan messages: {len(orphan_msgs)}, orphan fts rows: {len(orphan_fts)}")

sessions = [r[0] for r in conn.execute("SELECT id FROM sessions ORDER BY id")]
victims = []   # (session_id, [message ids])
for sid in sessions:
    rows = conn.execute(
        "SELECT id, role, content FROM messages WHERE session_id = ? ORDER BY id", (sid,)
    ).fetchall()
    drop = False
    span = []
    for mid, role, content in rows:
        if role == "user":
            if drop:
                victims.append((sid, span))
            drop = bool(pat.search(content or ""))
            span = [mid] if drop else []
        elif drop:
            span.append(mid)
    if drop:
        victims.append((sid, span))

total = sum(len(ids) for _, ids in victims)
print(f"probe turns: {len(victims)}, messages: {total}")
for sid, ids in victims:
    head = conn.execute("SELECT content FROM messages WHERE id = ?", (ids[0],)).fetchone()
    print(f"  session {sid}: {len(ids)} msg(s)  {head[0][:56]!r}")

if not APPLY:
    print("(dry run: pass --apply to delete)")
    raise SystemExit(0)

for sid, ids in victims:
    q = ",".join("?" * len(ids))
    conn.execute(f"DELETE FROM messages WHERE id IN ({q})", ids)
    conn.execute(f"DELETE FROM messages_fts WHERE message_id IN ({q})", ids)

for chunk in (orphan_msgs[i : i + 500] for i in range(0, len(orphan_msgs), 500)):
    q = ",".join("?" * len(chunk))
    conn.execute(f"DELETE FROM messages WHERE id IN ({q})", chunk)
    conn.execute(f"DELETE FROM messages_fts WHERE message_id IN ({q})", chunk)
for chunk in (orphan_fts[i : i + 500] for i in range(0, len(orphan_fts), 500)):
    q = ",".join("?" * len(chunk))
    conn.execute(f"DELETE FROM messages_fts WHERE rowid IN ({q})", chunk)
conn.commit()

# Counts that used to be derived: keep the sidebar honest.
for sid in {s for s, _ in victims}:
    n = conn.execute("SELECT COUNT(*) FROM messages WHERE session_id = ?", (sid,)).fetchone()[0]
    print(f"  session {sid} now has {n} message(s)")
conn.execute("VACUUM")
conn.commit()
print("applied")
