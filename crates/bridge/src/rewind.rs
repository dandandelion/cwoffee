//! Taking a pi conversation back to one of its own messages.
//!
//! cwoffee's transcript lives in SQLite and the *model's* memory of the conversation lives in a pi
//! session file, which pi owns. When a message is edited and sent again, the store is cut by
//! `cwoffee_engine::edit`; this is the other half — the pi thread has to stop where the transcript
//! now stops, or the next turn answers a conversation the person can no longer see.
//!
//! pi exposes no RPC for this: `fork` and `clone` make new sessions (that is what Branch will use),
//! and there is no "move the leaf back and delete the rest". So the file is rewritten. It is a
//! documented append-only JSONL tree — a header line, then entries linked by `id`/`parentId` — and
//! the operation is a prefix: keep the header and everything up to the point where the dropped
//! stretch starts, drop the rest, and hand the file back to pi with `switch_session`.
//!
//! Two things make a prefix the right shape here, and both are worth stating because they are the
//! assumptions that break if pi usage changes:
//!
//! * cwoffee's bridge never branches a pi session — Branch is not built — so the file order *is*
//!   the conversation order and the last line is the leaf. If branching lands, this has to walk the
//!   tree from `leafId` instead of reading lines in order.
//! * The cut point is found by counting *user* messages, which is the one thing both sides agree on
//!   without translation: the store counts the messages the person sent, and pi stores one
//!   `role:"user"` entry for each. Assistant entries, tool results, model changes and usage entries
//!   simply ride along with whichever side of the cut they fall on.

use serde_json::Value;

/// What a prune did, so the caller can say it out loud in one sentence.
#[derive(Debug, Clone, PartialEq)]
pub struct Pruned {
    /// The file as it should now read.
    pub text: String,
    /// Entries kept, including the header line.
    pub kept: usize,
    /// Entries dropped.
    pub dropped: usize,
    /// User messages that survived.
    pub user_kept: usize,
}

/// Is this line one of the person's own messages?
fn is_user_message(line: &str) -> bool {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return false,
    };
    v.get("type").and_then(|t| t.as_str()) == Some("message")
        && v.get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("user")
}

/// Keep everything up to, but not including, the (`keep_user` + 1)-th message the person sent.
///
/// Everything before that point stays — including a leading system entry, model and thinking-level
/// changes, and any compaction that happened earlier on this branch (whose `firstKeptEntryId` points
/// backwards, so it cannot dangle). Ask for more user messages than the file holds and nothing is
/// dropped: that is the answer for "the transcript already ends where the thread does", not an error.
pub fn prune_to_user_messages(jsonl: &str, keep_user: usize) -> Result<Pruned, String> {
    // Keep the framing exactly as pi wrote it: one entry per line, and a trailing newline if the
    // file had one. Rewriting with a different frame is how a file that pi is about to append to
    // stops being the file pi expects.
    let ends_with_newline = jsonl.ends_with('\n');
    let lines: Vec<&str> = jsonl.lines().collect();
    if lines.is_empty() || (lines.len() == 1 && lines[0].trim().is_empty()) {
        return Ok(Pruned { text: jsonl.to_string(), kept: lines.len(), dropped: 0, user_kept: 0 });
    }

    let mut cut = None;
    let mut seen_user = 0usize;
    for (i, line) in lines.iter().enumerate() {
        // The header is metadata, not part of the tree, and it has to stay whoever else goes.
        if i == 0 {
            continue;
        }
        if is_user_message(line) {
            if seen_user == keep_user {
                cut = Some(i);
                break;
            }
            seen_user += 1;
        }
    }
    // Fewer user messages in the file than the store holds means the thread is already at least as
    // far along as the record; there is nothing to take back.
    let Some(at) = cut else {
        return Ok(Pruned {
            text: jsonl.to_string(),
            kept: lines.len(),
            dropped: 0,
            user_kept: count_user(&lines),
        });
    };

    let kept = &lines[..at];
    let mut text = kept.join("\n");
    if ends_with_newline {
        text.push('\n');
    }
    Ok(Pruned {
        text,
        kept: kept.len(),
        dropped: lines.len() - kept.len(),
        user_kept: keep_user,
    })
}

fn count_user(lines: &[&str]) -> usize {
    lines.iter().skip(1).filter(|l| is_user_message(l)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread in pi's own shape: header, a system entry, then three exchanges.
    fn thread() -> String {
        [
            r#"{"type":"session","version":3,"id":"uuid-1","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"sys1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"system","content":"","sections":{"cwd":"/tmp"}}}"#,
            r#"{"type":"message","id":"u1","parentId":"sys1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"user","content":"first ask"}}"#,
            r#"{"type":"thinking_level_change","id":"t1","parentId":"u1","timestamp":"2024-12-03T14:00:03.000Z","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"a1","parentId":"t1","timestamp":"2024-12-03T14:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"first answer"}],"model":"m"}}"#,
            r#"{"type":"message","id":"u2","parentId":"a1","timestamp":"2024-12-03T14:00:05.000Z","message":{"role":"user","content":[{"type":"text","text":"second ask"},{"type":"image","data":"aGk=","mimeType":"image/png"}]}}"#,
            r#"{"type":"message","id":"a2","parentId":"u2","timestamp":"2024-12-03T14:00:06.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second answer"}],"model":"m"}}"#,
            r#"{"type":"usage","id":"us1","parentId":"a2","timestamp":"2024-12-03T14:00:07.000Z","kind":"cache_warm","usage":{"totalTokens":10}}"#,
            r#"{"type":"message","id":"u3","parentId":"us1","timestamp":"2024-12-03T14:00:08.000Z","message":{"role":"user","content":"third ask"}}"#,
            r#"{"type":"message","id":"a3","parentId":"u3","timestamp":"2024-12-03T14:00:09.000Z","message":{"role":"assistant","content":[{"type":"text","text":"third answer"}],"model":"m"}}"#,
        ]
        .join("\n")
            + "\n"
    }

    /// Entry ids in a thread, header excluded.
    fn ids(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) != Some("session"))
            .filter_map(|v| v.get("id").and_then(|i| i.as_str()).map(String::from))
            .collect()
    }

    /// Going back to the second ask drops that ask and everything the thread built on it, and
    /// keeps the prompt scaffolding the remaining turns depend on.
    #[test]
    fn a_thread_is_cut_at_the_message_being_sent_again() {
        let p = prune_to_user_messages(&thread(), 1).unwrap();
        assert_eq!(
            ids(&p.text),
            vec!["sys1", "u1", "t1", "a1"],
            "the header is metadata and stays, and everything after the first answer goes with the second ask"
        );
        assert_eq!(p.dropped, 5);
        assert_eq!(p.user_kept, 1);
        assert!(p.text.starts_with(r#"{"type":"session""#), "the header survives");
        assert!(p.text.ends_with('\n'), "and the file is still one entry per line");

        // What is left is still a thread pi can open: every entry's parent is present.
        let kept_ids = ids(&p.text);
        for l in p.text.lines().skip(1) {
            let v: Value = serde_json::from_str(l).unwrap();
            match v.get("parentId") {
                Some(Value::Null) | None => {}
                Some(p) => assert!(kept_ids.contains(&p.as_str().unwrap().to_string()), "dangling parent {p}"),
            }
        }
    }

    /// Asking for the whole thread back leaves the file alone — and a file that is not rewritten is
    /// a thread pi never has to re-read.
    #[test]
    fn asking_for_more_messages_than_the_thread_holds_changes_nothing() {
        let full = thread();
        let p = prune_to_user_messages(&full, 3).unwrap();
        assert_eq!(p.dropped, 0);
        assert_eq!(p.text, full);
        assert_eq!(p.user_kept, 3, "and the count says what is really there");
    }

    /// Back to nothing is a real answer: the prompt scaffolding stays, the conversation goes.
    #[test]
    fn a_thread_can_be_taken_back_to_nothing() {
        let p = prune_to_user_messages(&thread(), 0).unwrap();
        let kept: Vec<&str> = p.text.lines().collect();
        assert_eq!(kept.len(), 2, "header and the system entry");
        assert!(serde_json::from_str::<Value>(kept[0]).unwrap()["type"] == "session");
        assert_eq!(p.dropped, 8);
        assert_eq!(p.user_kept, 0);
    }

    /// The count only answers for entries pi really wrote as the person's messages: a tool result
    /// and a compaction are not asks, and a line this code cannot read is left where it is.
    #[test]
    fn only_pis_own_user_entries_count() {
        let doc = [
            r#"{"type":"session","version":3,"id":"u","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"compaction","id":"c1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","summary":"earlier","firstKeptEntryId":"c1"}"#,
            r#"{"type":"message","id":"tr1","parentId":"c1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"toolResult","toolCallId":"x","content":[{"type":"text","text":"out"}]}}"#,
            "not json at all",
            r#"{"type":"message","id":"u1","parentId":"c1","timestamp":"2024-12-03T14:00:03.000Z","message":{"role":"user","content":"ask"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2024-12-03T14:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        ]
        .join("\n")
            + "\n";
        let p = prune_to_user_messages(&doc, 0).unwrap();
        assert_eq!(p.dropped, 2, "the ask and its answer go; the scaffolding before them stays");
        assert_eq!(ids(&p.text), vec!["c1", "tr1"]);
        assert!(p.text.contains("not json at all"), "a line this cannot read is neither dropped nor counted as an ask");
        assert_eq!(p.user_kept, 0);
    }

    /// An empty file is an empty conversation, not a panic.
    #[test]
    fn an_empty_thread_prunes_to_itself() {
        for src in ["", "\n"] {
            let p = prune_to_user_messages(src, 0).unwrap();
            assert_eq!(p.text, src);
            assert_eq!(p.dropped, 0);
        }
    }
}
