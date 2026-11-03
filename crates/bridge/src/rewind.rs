//! Taking a pi session file back to where cwoffee's transcript now ends.
//!
//! The Rust core rebuilds its prompt from SQLite rows, so "forget what came after this
//! message" is a re-read. pi does not: it owns a JSONL file and builds its context from that
//! file, so the only way to take a turn back on the pi core is to rewrite the file and make pi
//! re-read it (`switch_session`).
//!
//! A pi session file is a **tree**, not a transcript. Entries carry `id`/`parentId`, and more
//! than one entry can share a parent — which is how Branch works: pi's `fork` copies a prefix of
//! the source file into a new file, and interactive `/tree` puts a second path under the same
//! parent inside one file. What the model was told is therefore decided by two fields, not by the
//! order of the lines:
//!
//! * **`leafId`** — the entry the conversation is *at*. Context is built by walking from the leaf
//!   to the root, so a file's line order is not its conversation order. Ask pi for `leafId`
//!   (`get_entries`) and take the file's last entry when the child is down.
//! * **`firstKeptEntryId`** — on a compaction entry, where the model's context starts: everything
//!   before it is folded away. pi takes a fold from the path it is folding, so its anchor is always
//!   earlier on the same branch. A fold whose anchor is *not* on the kept chain is a file that
//!   contradicts itself, and there is no honest rewrite of one: it is refused, not repaired.
//! * **`type: "session"`** — the header line. Metadata only, never part of the tree, and pi's
//!   reader wants it first, so it is copied verbatim rather than parsed into the walk.
//!
//! The cut is made at a message boundary, but it keeps the whole exchange: the answer to that
//! message and its tool results stay. cwoffee's transcript ends with the reply, not with the
//! question, and a thread cut at the question would leave the model unable to remember an answer
//! the person can still read — the record and the model disagreeing about what happened, which is
//! the failure this module exists to prevent.
//!
//! An earlier version of this module assumed the file was linear: it kept the first N user
//! messages *in file order*. That holds only while nothing has ever branched a file. Once it does,
//! "first N user messages in line order" keeps entries from a path the person is not on, and the
//! rewritten file teaches pi a conversation that never happened on this branch.
//! `a_branch_is_cut_on_the_branch_being_kept_not_on_the_files_line_order` pins that assumption off,
//! and `a_leaf_that_is_not_the_newest_entry_cuts_that_branch` pins the other half: the walk starts
//! where the person is, not where the file ends.

use serde_json::Value;

/// A short, single-line version of a message, for an error that has to identify which one it
/// refused. Shared with the branch check, where two records of one conversation are compared and
/// the reader needs to see the words each side holds.
pub fn quote(s: &str) -> String {
    const MAX: usize = 60;
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return format!("“{flat}”");
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("“{head}…”")
}

/// What a rewrite did, in the numbers the caller has to report.
#[derive(Debug)]
pub struct Pruned {
    /// The new file contents, header line first.
    pub text: String,
    /// User messages on the kept chain.
    pub kept_users: usize,
    /// Entries kept, header excluded.
    pub kept_entries: usize,
    /// Entries from the file that are not in the rewritten one. On a branched file that includes
    /// the entries of paths nobody is on, which is why the count is reported rather than assumed
    /// to be "the turns after the cut".
    pub dropped_entries: usize,
    /// True when entries sat under more than one parent, or the kept chain is not the whole file:
    /// i.e. this file had more than one path through it.
    pub branched: bool,
    /// Set when the rewrite did not start from the file's last entry, i.e. the person was on a
    /// branch rather than at the newest word. Says where it started, for the transcript note.
    pub leaf: String,
}

/// Rewrite a session file so its active branch ends at the `keep_user`-th user message.
///
/// `leaf_id` is pi's own `leafId` for this file when the caller can ask for it. `None` means the
/// caller could not (the child is down), and the honest fallback is the file's last entry — which
/// is what pi's own leaf is for anything this app wrote, since every append makes its entry the
/// leaf. It is worth saying out loud rather than presenting as fact, which is what [`Pruned::leaf`]
/// is for.
pub fn prune_to_user_messages(jsonl: &str, keep_user: usize) -> Result<Pruned, String> {
    prune_to_user_messages_with_leaf(jsonl, keep_user, None)
}

/// The same rewrite, starting from a named leaf. See [`prune_to_user_messages`].
pub fn prune_to_user_messages_with_leaf(
    jsonl: &str,
    keep_user: usize,
    leaf_id: Option<&str>,
) -> Result<Pruned, String> {
    // (line, parsed entry) — the line text is kept verbatim so nothing about pi's own encoding of
    // an entry changes on the way through us.
    let mut lines: Vec<(String, Value)> = Vec::new();
    for (n, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .map_err(|e| format!("session line {} is not valid JSON: {e}", n + 1))?;
        lines.push((line.to_string(), v));
    }

    // The header is not an entry: it has no parentId and pi's reader needs it as line 1. It is
    // matched by `type`, and by being the first line, so a file whose header moved is refused
    // rather than silently rewritten without one.
    let header = match lines.first() {
        Some((line, v)) if v.get("type").and_then(Value::as_str) == Some("session") => Some(line.clone()),
        Some(_) => {
            return Err(
                "this pi session file does not start with a session header, so it was left alone: \
                 rewriting a thread pi owns without its header would make it unopenable"
                    .to_string(),
            )
        }
        None => None,
    };

    // Tree, keyed by id. Entries without a parent are roots; the header is excluded above.
    // Some pi files root their first entry at the header's own id rather than at `null`. The
    // header is not part of the tree, so that parent is a boundary, not a missing entry.
    let header_id = header
        .as_ref()
        .and_then(|h| serde_json::from_str::<Value>(h).ok())
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string));

    let mut by_id: Vec<(String, usize)> = Vec::new(); // (id, index into `entries`)
    let mut entries: Vec<&Value> = Vec::new();
    let mut children: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (_line, v) in lines.iter().skip(header.as_ref().map(|_| 1).unwrap_or(0)) {
        let id = v.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if id.is_empty() {
            return Err(
                "a pi session entry has no id, so its place in the tree cannot be worked out; the \
                 file was left alone rather than rewritten by line order"
                    .to_string(),
            );
        }
        let idx = entries.len();
        entries.push(v);
        by_id.push((id.clone(), idx));
        match v.get("parentId").and_then(Value::as_str) {
            Some(p) if Some(p.to_string()) == header_id => roots.push(idx),
            Some(p) => children.entry(p.to_string()).or_default().push(idx),
            None => roots.push(idx),
        }
    }
    let index_of = |id: &str| by_id.iter().find(|(k, _)| k == id).map(|(_, i)| *i);

    if entries.is_empty() {
        // A thread with no entries is not a conversation, and cutting it to nothing is what the
        // caller asked for: the record is already empty, and pi's file is a header waiting for a
        // first message. Rewriting it as a header alone is the honest answer, not an error.
        let text = match header.as_ref() {
            Some(h) => format!("{h}\n"),
            None => String::new(),
        };
        return Ok(Pruned {
            text,
            kept_users: 0,
            kept_entries: 0,
            dropped_entries: 0,
            branched: false,
            leaf: "an empty thread".to_string(),
        });
    }

    // Where the walk starts. A leaf that is not in the file is a contradiction between pi and the
    // file it is holding, and guessing would rewrite a thread out of nothing.
    let leaf = match leaf_id {
        Some(id) => match index_of(id) {
            Some(i) => i,
            None => {
                return Err(format!(
                    "pi says this conversation is at entry {id}, and that entry is not in the file \
                     it just read; nothing was rewritten, because there is no branch to cut"
                ))
            }
        },
        None if !entries.is_empty() => entries.len() - 1,
        None => 0,
    };

    // Leaf -> root, then reversed: this is the conversation the model was told, in the order it
    // was told it. A parent that is missing means the file is a fragment, and a prefix of a
    // fragment is not a conversation — say so instead of handing pi half a thread.
    let mut chain: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = Some(leaf);
    while let Some(i) = cur {
        if !seen.insert(i) {
            return Err(
                "the entries in this pi session file form a cycle, which is not a conversation; \
                 nothing was rewritten"
                    .to_string(),
            );
        }
        chain.push(i);
        match entries[i].get("parentId").and_then(Value::as_str) {
            Some(p) if Some(p.to_string()) == header_id => cur = None,
            Some(p) => match index_of(p) {
                Some(j) => cur = Some(j),
                None => {
                    return Err(format!(
                        "entry {} of this pi session file names a parent ({p}) that is not in the \
                         file, so the conversation it belongs to cannot be reconstructed; nothing \
                         was rewritten",
                        entries[i].get("id").and_then(Value::as_str).unwrap_or("?")
                    ));
                }
            },
            None => cur = None,
        }
    }
    chain.reverse();

    // Cut after the `keep_user`-th user message on this branch — and keep what follows it up to the
    // next message of the person's own. The record the caller cut from ends with the answer to that
    // message, not with the question, and a thread cut at the question would leave the model
    // unable to remember a reply the transcript still shows. "It went back to that message" means
    // the conversation reads as it did at that point, answer included.
    //
    // More user messages than the branch has is refused, not truncated: on a branched file the count
    // the caller has comes from cwoffee's transcript, which holds every path, while this branch
    // holds one. Rewriting the file to a shorter prefix to make the numbers agree would be
    // inventing a conversation.
    let mut users = 0usize;
    let mut cut = None;
    let is_user = |i: usize| {
        entries[i]
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("user")
    };
    for (i, &idx) in chain.iter().enumerate() {
        if is_user(idx) {
            users += 1;
            if users == keep_user {
                cut = Some(i);
                break;
            }
        }
    }
    let kept_chain = if keep_user == 0 {
        Vec::new()
    } else {
        match cut {
            Some(i) => {
                let mut end = i + 1;
                while end < chain.len() && !is_user(chain[end]) {
                    end += 1;
                }
                chain[..end].to_vec()
            }
            None => {
                return Err(format!(
                    "this pi branch holds {users} of your messages and the transcript asked it to end \
                     at {keep_user}; on a branched conversation the branch you are on is not the whole \
                     file, so nothing was rewritten"
                ))
            }
        }
    };

    // A compaction entry says "the model's context starts at `firstKeptEntryId`". Anything kept
    // must therefore name an entry that is kept too, or the rewritten file would tell pi to start
    // reading at a message that is not in it.
    //
    // pi's own writes never do this — a fold is always taken from the path being folded, so its
    // anchor sits earlier on the same branch, and a cut that ends before the fold drops the fold
    // with the rest of the tail. A fold whose anchor is missing from the kept chain means the file
    // disagrees with itself, and there is no honest rewrite of that: dropping the fold would orphan
    // the entries appended under it, and repointing its anchor would keep a summary of messages
    // nobody is on. Refuse, and leave the thread as it is.
    let kept_ids: std::collections::HashSet<&str> = kept_chain
        .iter()
        .map(|&i| entries[i].get("id").and_then(Value::as_str).unwrap_or_default())
        .collect();
    for &i in kept_chain.iter() {
        if entries[i].get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        if let Some(anchor) = entries[i].get("firstKeptEntryId").and_then(Value::as_str) {
            if !kept_ids.contains(anchor) {
                let id = entries[i].get("id").and_then(Value::as_str).unwrap_or("?");
                return Err(format!(
                    "a folded summary on this branch ({id}) points at a message ({anchor}) that is \
                     not on it, so this thread's file contradicts itself and was left alone. Reopen \
                     conversation to let pi re-read it, and try again; nothing was rewritten."
                ));
            }
        }
    }
    let keep_list: Vec<usize> = kept_chain.clone();

    // More than one path through the file: some entry has two children. A cut is not a branch —
    // dropping the tail of one path is what a rewind does to any file.
    let branched = children.values().any(|v| v.len() > 1);
    let dropped_entries = entries.len() - keep_list.len();

    let mut out = String::new();
    if let Some(ref h) = header {
        out.push_str(&h);
        out.push('\n');
    }
    for &i in keep_list.iter() {
        out.push_str(&lines[i + header.as_ref().map(|_| 1).unwrap_or(0)].0);
        out.push('\n');
    }

    Ok(Pruned {
        text: out,
        kept_users: users.min(keep_user),
        kept_entries: keep_list.len(),
        dropped_entries,
        branched,
        leaf: if entries.is_empty() {
            "an empty thread".to_string()
        } else if leaf_id.is_none() && leaf == entries.len() - 1 {
            "the newest entry in the file".to_string()
        } else {
            format!(
                "entry {}",
                entries[leaf].get("id").and_then(Value::as_str).unwrap_or("?")
            )
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        v.to_string()
    }

    fn header(id: &str) -> Value {
        serde_json::json!({ "type": "session", "id": id, "timestamp": "2026-01-01T00:00:00Z" })
    }

    /// One entry. `"h"` stands for the file's root: pi's own first entry has `parentId: null`,
    /// and a real file may instead name the header's id, which the rewriter treats the same way.
    fn msg(id: &str, parent: &str, role: &str, text: &str) -> Value {
        let parent = if parent == "h" { Value::Null } else { Value::String(parent.to_string()) };
        serde_json::json!({
            "type": "message", "id": id, "parentId": parent,
            "message": { "role": role, "content": [{ "type": "text", "text": text }] }
        })
    }

    fn file(lines: &[Value]) -> String {
        lines.iter().map(|v| line(v.clone())).collect::<Vec<_>>().join("\n") + "\n"
    }

    /// A thread with a fold in the middle of it, the shape a long conversation leaves behind.
    fn compaction_file() -> String {
        file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            msg("a1", "u1", "assistant", "first answer"),
            msg("u2", "a1", "user", "second ask"),
            msg("a2", "u2", "assistant", "second answer"),
            serde_json::json!({
                "type": "compaction", "id": "cmp", "parentId": "a2",
                "firstKeptEntryId": "u2", "summary": "folded the first exchange away"
            }),
            msg("u3", "cmp", "user", "third ask"),
            msg("a3", "u3", "assistant", "third answer"),
        ])
    }

    /// Nothing the rewritten file claims about an entry it does not contain: a fold anchor or a
    /// parent that is missing is a file pi cannot build a context from.
    fn assert_no_dangling_anchor(text: &str) {
        let ids: Vec<String> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        for l in text.lines() {
            let v: Value = serde_json::from_str(l).unwrap();
            if let Some(a) = v.get("firstKeptEntryId").and_then(Value::as_str) {
                assert!(ids.iter().any(|i| i == a), "rewritten file claims a fold anchor ({a}) that is not in it");
            }
            if let Some(p) = v.get("parentId").and_then(Value::as_str) {
                assert!(ids.iter().any(|i| i == p), "rewritten file keeps an entry whose parent ({p}) is not in it");
            }
        }
    }

    /// Count of user messages in the rewritten file, read back as pi would read it.
    fn users_of(text: &str) -> usize {
        text.lines()
            .filter(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                v.get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
            })
            .count()
    }

    /// A linear file still behaves the way a linear file should: the header survives, and so does
    /// the prefix through the Nth user message. This is the old behaviour, kept on purpose — the
    /// change is what happens when the file is *not* linear.
    #[test]
    fn a_linear_thread_is_cut_at_the_right_message_and_keeps_its_header() {
        let f = file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            msg("a1", "u1", "assistant", "first answer"),
            msg("u2", "a1", "user", "second ask"),
            msg("a2", "u2", "assistant", "second answer"),
            msg("u3", "a2", "user", "third ask"),
        ]);
        let p = prune_to_user_messages(&f, 2).unwrap();
        assert_eq!(users_of(&p.text), 2);
        assert_eq!(p.kept_entries, 4, "u1, its answer, u2, and that answer too");
        // The exchange asked at is kept whole: the record the caller cut from still ends with the
        // answer to that message, so the thread has to end with it too.
        assert!(p.text.contains("second ask"));
        assert!(p.text.contains("second answer"), "the reply to the message asked at is kept: the record the cut came from still holds it");
        assert_eq!(p.dropped_entries, 1, "only the third ask went");
        assert!(!p.branched, "one path through it");
        let first: Value = serde_json::from_str(p.text.lines().next().unwrap()).unwrap();
        assert_eq!(first.get("type").and_then(Value::as_str), Some("session"));
        assert_eq!(p.leaf, "the newest entry in the file", "no leaf was given, so the last line is the leaf");

        // Everything, and nothing: both ends of the range.
        assert_eq!(users_of(&prune_to_user_messages(&f, 3).unwrap().text), 3);
        assert!(prune_to_user_messages(&f, 3).unwrap().text.contains("third ask"));
        let none = prune_to_user_messages(&f, 0).unwrap();
        assert_eq!(none.kept_entries, 0);
        assert_eq!(users_of(&none.text), 0);
        assert_eq!(none.text.lines().count(), 1, "the header is not a conversation");
    }

    /// The assumption this module used to make, pinned off.
    ///
    /// Here a second answer hangs off the first message, written at the end of the file — exactly
    /// what a branch looks like, because a fork appends its new entries rather than splicing them.
    /// The active branch (leaf = the alternate answer) contains **one** of the person's messages;
    /// the file contains two user messages in line order. Reading line order would keep `u2`,
    /// `a1`'s successor and everything after it, teaching pi a conversation this branch never had.
    #[test]
    fn a_branch_is_cut_on_the_branch_being_kept_not_on_the_files_line_order() {
        let f = file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            msg("a1", "u1", "assistant", "the answer on the other path"),
            msg("u2", "a1", "user", "second ask, only on that other path"),
            msg("a2", "u2", "assistant", "and its answer"),
            // The branch: same parent as `a1`, appended last, which is what makes it the leaf.
            msg("alt", "u1", "assistant", "the answer this branch took"),
        ]);
        // Asked from the file's end, the leaf is the branch entry — not `a2`.
        let p = prune_to_user_messages(&f, 1).unwrap();
        assert_eq!(users_of(&p.text), 1);
        assert!(p.text.contains("the answer this branch took"), "the kept branch is the branch the person is on");
        assert!(!p.text.contains("the answer on the other path"), "the other path is not copied in");
        assert!(!p.text.contains("second ask, only on that other path"), "and neither are its messages");
        assert!(p.branched, "this file had two paths through it");
        assert_eq!(p.dropped_entries, 3, "a1, u2, a2 — entries on a path nobody is on");

        // Asking for the second message of a branch that only has one is refused, not truncated.
        let e = prune_to_user_messages(&f, 2).unwrap_err();
        assert!(e.contains("holds 1"), "the count that is true about this branch belongs in the error: {e}");
        assert!(e.contains("nothing was rewritten"), "{e}");
    }

    /// A named leaf is respected even when it is not the file's last entry — which is what happens
    /// when somebody has moved back to an earlier node.
    #[test]
    fn a_leaf_that_is_not_the_newest_entry_cuts_that_branch() {
        let f = file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            msg("a1", "u1", "assistant", "first answer"),
            msg("u2", "a1", "user", "second ask"),
            msg("a2", "u2", "assistant", "second answer"),
            msg("alt", "u1", "assistant", "the other answer"),
        ]);
        let p = prune_to_user_messages_with_leaf(&f, 2, Some("a2")).unwrap();
        assert_eq!(users_of(&p.text), 2);
        assert!(!p.text.contains("the other answer"), "the branch that was not chosen is not kept");
        assert!(p.branched);
        assert_eq!(p.leaf, "entry a2", "and the note says where the walk started");

        // Same file, walked from the other branch: only the first exchange survives.
        let p2 = prune_to_user_messages_with_leaf(&f, 1, Some("alt")).unwrap();
        assert_eq!(users_of(&p2.text), 1);
        assert!(!p2.text.contains("second ask"));
    }

    /// A compaction inside the kept stretch survives, and nothing in the rewritten file points at
    /// an entry the rewrite left out.
    #[test]
    fn a_compaction_inside_the_kept_stretch_keeps_its_anchor() {
        let f = compaction_file();
        let p = prune_to_user_messages(&f, 3).unwrap();
        assert_eq!(users_of(&p.text), 3);
        assert!(p.text.contains("\"firstKeptEntryId\":\"u2\""));
        assert_no_dangling_anchor(&p.text);
    }

    /// A fold past the cut goes with the rest of the tail, and the rewritten file still holds no
    /// claim about a message that is not in it.
    #[test]
    fn a_fold_past_the_cut_goes_with_the_tail_it_is_in() {
        let f = compaction_file();
        let p = prune_to_user_messages(&f, 1).unwrap();
        assert_eq!(users_of(&p.text), 1);
        assert!(!p.text.contains("\"id\":\"cmp\""), "the fold is past the answer that was kept");
        assert_eq!(p.dropped_entries, 5, "u2, a2, the fold, u3 and its answer went as one tail");
        assert_no_dangling_anchor(&p.text);
    }

    /// The case `firstKeptEntryId` has to be handled for: a fold on **this** branch whose
    /// anchor was folded from a message on another path. pi would read a file whose context starts
    /// at an entry that is not in it, so the fold cannot stay.
    #[test]
    fn a_fold_whose_anchor_is_on_another_path_cannot_stay_on_this_one() {
        let f = file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            msg("a1", "u1", "assistant", "first answer"),
            serde_json::json!({
                "type": "compaction", "id": "cmp", "parentId": "a1",
                "firstKeptEntryId": "u2", "summary": "folded from a path this branch is not on"
            }),
            msg("u3", "cmp", "user", "third ask"),
            // The other path, where `u2` — the fold's anchor — actually lives.
            msg("u2", "a1", "user", "second ask, on the other path"),
            msg("a2", "u2", "assistant", "and its answer"),
        ]);
        // Refused, not rewritten: dropping the fold would orphan `u3` under it, and keeping it
        // would keep a summary of a path nobody is on.
        let e = prune_to_user_messages_with_leaf(&f, 2, Some("u3")).unwrap_err();
        assert!(e.contains("cmp"), "the fold is named: {e}");
        assert!(e.contains("u2"), "and so is the message it points at: {e}");
        assert!(e.contains("nothing was rewritten"), "{e}");
    }

    /// A compaction entry is not one of the person's messages, so it does not consume the budget.
    #[test]
    fn a_compaction_is_not_counted_as_one_of_your_messages() {
        let f = file(&[
            header("s1"),
            msg("u1", "h", "user", "first ask"),
            serde_json::json!({
                "type": "compaction", "id": "cmp", "parentId": "u1",
                "firstKeptEntryId": "u1", "summary": "a fold between two messages"
            }),
            msg("u2", "cmp", "user", "second ask"),
        ]);
        let p = prune_to_user_messages(&f, 2).unwrap();
        assert_eq!(users_of(&p.text), 2);
        assert_eq!(p.kept_entries, 3, "u1, the fold, u2");
    }

    /// Files this module cannot read as a tree are refused, never rewritten by guesswork. Each of
    /// these would have been "handled" by the old line-order reader — by writing a broken thread.
    #[test]
    fn a_file_that_is_not_a_readable_tree_is_left_alone() {
        // No header: pi's reader would want it, and dropping it makes the file unopenable.
        let f = file(&[msg("u1", "h", "user", "ask")]);
        assert!(prune_to_user_messages(&f, 1).unwrap_err().contains("session header"));

        // An entry with no id cannot be placed in the tree.
        let f = format!("{}\n{}\n", line(header("s1")), line(serde_json::json!({ "type": "message", "parentId": "h", "message": { "role": "user", "content": [] } })));
        assert!(prune_to_user_messages(&f, 1).unwrap_err().contains("no id"));

        // A parent that is not in the file: a fragment, not a conversation.
        let f = file(&[header("s1"), msg("u1", "nowhere", "user", "ask")]);
        let e = prune_to_user_messages(&f, 1).unwrap_err();
        assert!(e.contains("not in the file"), "{e}");

        // A cycle would spin the walk forever.
        let f = file(&[header("s1"), msg("u1", "a1", "user", "ask"), msg("a1", "u1", "assistant", "answer")]);
        assert!(prune_to_user_messages(&f, 1).unwrap_err().contains("cycle"));

        // A leaf pi names that is not in the file is a disagreement between two sources of truth.
        let f = file(&[header("s1"), msg("u1", "h", "user", "ask")]);
        let e = prune_to_user_messages_with_leaf(&f, 1, Some("ghost")).unwrap_err();
        assert!(e.contains("ghost"), "the missing entry is named: {e}");

        // Malformed JSON says which line, because that is the line to go look at.
        let bad = format!("{}\nnot json\n", line(header("s1")));
        assert!(prune_to_user_messages(&bad, 1).unwrap_err().contains("line 2"));
    }

    /// Nothing at all in the file is an empty conversation, not an error: the caller's transcript
    /// is already cut to nothing, and pi's thread should match it.
    #[test]
    fn an_empty_file_and_a_cut_to_nothing_are_both_answers() {
        let p = prune_to_user_messages("", 0).unwrap();
        assert_eq!(p.text, "");
        assert_eq!(p.kept_users, 0);

        let f = file(&[header("s1"), msg("u1", "h", "user", "ask"), msg("a1", "u1", "assistant", "answer")]);
        let p = prune_to_user_messages(&f, 0).unwrap();
        assert_eq!(p.text.lines().count(), 1, "header only");
        assert_eq!(p.dropped_entries, 2);
        assert!(!p.branched, "a cut is not a branch");
    }
}
