//! Taking a different path from a message you already sent.
//!
//! A branch is not a copy of the transcript for looks. It is a second conversation that
//! *continues* from somewhere inside the first, and the only thing that makes it real is that
//! the model is told exactly what the new conversation says and nothing more. A copied
//! transcript the model has not read is a conversation whose next answer ignores its own
//! history, which is the failure [`crate::edit`] exists to avoid in the other direction.
//!
//! Two asks share this menu item, and both are honest about what they take:
//!
//! * **Branch from one of your messages.** The new conversation holds everything *before* that
//!   message **on the path the source is reading**, and the message itself goes back into the
//!   composer instead of into the record: the point is to ask it again and get somewhere different.
//!   The original keeps every later message.
//! * **Branch at an answer.** The new conversation holds that answer and everything before it, and
//!   stops there — which, asked at the last answer, is a copy of the whole conversation. Take that
//!   one somewhere else (a different folder, a different question) without touching the original.
//!
//! "The path it is reading" matters as soon as a leaf has been moved ([`crate::tree`]): a conversation
//! can hold rows on two branches, and only one of them is what its transcript shows and what its model
//! answers from. Copying by row id across the whole file would hand someone a conversation of rows they
//! never saw, branched from a path they were not standing on.
//!
//! What a branch is **not**: a rewind. The original conversation is never cut. If it were, this
//! would be [`crate::edit`]'s save-and-send-again with an extra step.
//!
//! Where the two copies live is the reason this module exists. The store is the transcript, so
//! both cores copy rows the same way ([`StoreOps::copy_through`]). What differs is the model's own
//! memory: the Rust core rebuilds its prompt from the copied rows, and the pi core forks pi's
//! session **tree**, which is where pi keeps branches at all — a prefix of a file is not a branch
//! when the file has more than one path through it, see [`crate::rewind`]'s notes on `leafId`.

use serde::{Deserialize, Serialize};

use crate::host::HostOps;
use crate::store::{StoreOps, StoredMessage};

/// What the person asked for.
///
/// The identity discipline is the same as [`crate::edit::EditReq`]'s, for the same reason: with a
/// desktop window, a phone tab and a harness all able to reach one conversation, a request that
/// only says "the third message" can land on a row this window never showed. So the caller names
/// the row — by stored id, or by counting back from the newest message of a given role — *and*
/// repeats the words it painted. A row that says something else now is refused, not branched from.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BranchReq {
    pub session: i64,
    #[serde(default)]
    pub message_id: Option<i64>,
    /// Count back from the newest message whose role is `role`. 1 is the newest.
    #[serde(default)]
    pub from_end: Option<usize>,
    /// Which kind of row `from_end` counts. Read off the row when `message_id` is given.
    #[serde(default)]
    pub role: String,
    /// The words this window is showing on that row. Compared against what the row holds.
    #[serde(default)]
    pub shown: String,
}

/// Which row the branch starts from, and what that means for the copy.
#[derive(Debug, Clone)]
pub struct BranchPoint {
    /// The row this was asked at.
    pub row_id: i64,
    pub role: String,
    /// Copy every row with `id <= keep_through`. For one of the person's own messages that is the
    /// row *before* it, so the new conversation does not hold the message being asked again; for an
    /// answer it is the answer itself, because the branch is meant to carry it and continue.
    pub keep_through: i64,
    /// The words to hand back for the composer: set when the branch starts from a message of the
    /// person's own, which is the case where the ask is "send this again, get somewhere else".
    pub ask: Option<String>,
    /// How many stored rows sit after the copied stretch — the part that stays only in the original.
    pub trailing: usize,
    /// True when the copy is everything the original holds: asked at the last answer, this branch
    /// is a copy of the whole conversation rather than a fork in the middle of it.
    pub whole: bool,
}

/// The new conversation, and what it took to make it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchOutcome {
    /// The branch's own row id: what the client opens next.
    pub session: i64,
    pub title: String,
    /// Rows carried into the new conversation.
    pub copied: usize,
    /// Words to prefill the composer with, when the branch starts from a message of the person's
    /// own. Prefilled rather than sent, because what to do with that message is the whole question
    /// the branch is asking — and it keeps the composer's attachments out of this path.
    pub prefill: Option<String>,
    pub note: String,
}

/// Find the row this branch starts from, and check the words the caller saw against it.
pub fn resolve(rows: &[StoredMessage], req: &BranchReq) -> Result<BranchPoint, String> {
    if rows.is_empty() {
        return Err(format!(
            "conversation {} has nothing in it, so there is nothing to branch from; nothing was created",
            req.session
        ));
    }
    let role = match req.role.as_str() {
        "assistant" => "assistant",
        _ => "user",
    };
    let found: &StoredMessage = if let Some(id) = req.message_id {
        rows.iter().find(|m| m.id == id).ok_or_else(|| {
            format!(
                "that message is not part of conversation {} any more, so nothing was branched. \
                 Reload the conversation and branch from a message you can see.",
                req.session
            )
        })?
    } else if let Some(from_end) = req.from_end {
        let same_role: Vec<&StoredMessage> = rows.iter().filter(|m| m.role == role).collect();
        if from_end == 0 || from_end > same_role.len() {
            return Err(format!(
                "this window asked for the {from_end}th {} message counting back from the newest, and \
                 conversation {} has {}. Nothing was branched — the messages on screen are not the ones \
                 in the store any more; reload the conversation.",
                role,
                req.session,
                same_role.len()
            ));
        }
        same_role[same_role.len() - from_end]
    } else {
        return Err(format!(
            "a branch has to name the message it starts from: give it a stored id, or a position \
             counted back from your newest {} message. Nothing was created.",
            if role == "assistant" { "answer" } else { "message" }
        ));
    };

    // The same proof as an edit: the words this window painted, or nothing is created. An empty
    // `shown` is only allowed when the row really is empty, so a branch cannot be started from a
    // message this window never rendered.
    if req.shown != found.content {
        return Err(format!(
            "that message says something else now — {} — so nothing was branched. Reload the \
             conversation and branch from the version you can see.",
            quoted(&found.content)
        ));
    }

    let idx = rows.iter().position(|m| m.id == found.id).unwrap_or(rows.len() - 1);
    let keep_through = if found.role == "user" { found.id - 1 } else { found.id };
    let trailing = rows.len() - idx - 1;
    Ok(BranchPoint {
        row_id: found.id,
        role: found.role.clone(),
        keep_through,
        ask: (found.role == "user").then(|| found.content.clone()),
        trailing,
        // Copied everything the original holds: the answer asked at was its last row.
        whole: found.role == "assistant" && trailing == 0,
    })
}

/// A short, single-line version of somebody's message, for an error that has to identify it.
fn quoted(s: &str) -> String {
    const MAX: usize = 80;
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= MAX {
        return format!("“{flat}”");
    }
    let head: String = flat.chars().take(MAX).collect();
    format!("“{head}…”")
}

/// Name the branch so the sidebar says what it is and where it came from.
///
/// Not "(copy)": a branch is not a duplicate to be cleaned up, it is a conversation that diverged,
/// and two rows reading "notes (copy)" / "notes (copy) (copy)" say nothing about which came from
/// which. The source's title is named in full so a person can see the parent in the list.
pub fn branch_title(original: &str) -> String {
    let base = original.trim();
    if base.is_empty() {
        return "branch of an untitled conversation".to_string();
    }
    // A branch of a branch names its own parent, not the great-grandparent: the row that was
    // branched is the one the person was looking at.
    let stem = base
        .split_once(" — branch of ")
        .map(|(head, _)| head.trim())
        .unwrap_or(base);
    let stem = clip(stem);
    format!("{stem} — branch of {}", clip(base))
}

/// Shorten a title for the sidebar row without losing the end that identifies it.
///
/// A branch row carries two titles in one line, so a long source title would be doubled: the row
/// would be all branch-name and no conversation. Both halves get the same cap, because a row that
/// says "xxxxxxxx… — branch of xxxxxxxx…" tells you nothing about either conversation.
fn clip(s: &str) -> String {
    const MAX: usize = 40;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…")
}

/// What to tell the person, in the transcript they are looking at.
///
/// Every sentence here says what did *not* happen as much as what did: the original conversation is
/// still whole, and that is the difference between this and a rewind. Someone who meant "copy this
/// and let me take it somewhere else" needs to hear the original is intact; someone who meant "take
/// it back" needs to hear that this is not that, and to use the pencil instead.
pub fn outcome_note(from: i64, p: &BranchPoint, new_session: i64, copied: usize, title: &str) -> String {
    let rows = if copied == 1 { "message" } else { "messages" };
    // What the first answer has to read before it can answer. Said as a cost, not as a cache
    // state: the engine cannot see the endpoint's cache — `host.rs` refuses to put a cache
    // assertion in a note for the same reason — and the count is the part that is knowable. A
    // branch that copied nothing pays for nothing, so the sentence is only said when there is a
    // copy to read.
    let cold = if copied == 0 {
        String::new()
    } else {
        format!(
            " Its first answer is assembled from the {copied} copied {rows}, not from what this \
             engine had already loaded, so that turn reads the copy before it replies."
        )
    };
    if p.whole {
        return format!(
            "branched the whole conversation: {copied} {rows} copied into conversation {new_session} \
             ({title}). Both conversations hold them, and they part company at the next message you \
             send. Conversation {from} is untouched — this is a second path, not a rewind.{cold}"
        );
    }
    if p.role == "user" {
        let kept =
            if copied == 0 { "nothing before it".to_string() } else { format!("{copied} earlier {rows}") };
        // "the original still has this message" is the sentence the person needs, and it reads
        // differently depending on whether there *was* anything after it. Naming the count is the
        // difference between "nothing was taken away" being a promise and being a number.
        let after = if p.trailing == 0 {
            format!(
                "It was the last word in conversation {from}, so that conversation reads exactly as \
                 it did — a second path, not a rewind."
            )
        } else {
            format!(
                "Conversation {from} keeps that message and the {} after it: this is a second path, \
                 not a rewind.",
                match p.trailing {
                    1 => "1 message".to_string(),
                    n => format!("{n} messages"),
                }
            )
        };
        format!(
            "branched from that message into conversation {new_session} ({title}), carrying {kept}. \
             {after} Your words are back in the composer — say them again and this branch answers, \
             conversation {from} does not.{cold}"
        )
    } else {
        let later = match p.trailing {
            0 => "nothing".to_string(),
            1 => "1 message".to_string(),
            n => format!("{n} messages"),
        };
        format!(
            "branched at that answer into conversation {new_session} ({title}), carrying it and \
             {copied} earlier {rows}; conversation {from} keeps {later} and is untouched — this is a \
             second path, not a rewind. It continues from the answer you branched at.{cold}"
        )
    }
}

/// What the engine did to *its own* memory of the conversation, as one sentence.
///
/// A copied transcript the model has not read is not a branch, and the person cannot tell the two
/// apart from the sidebar. So the note says which memory was copied, in the terms of the engine
/// that owns it: the Rust core rebuilds its prompt out of the stored rows, pi keeps a thread file
/// of its own and forked it. `None` for a core this function does not know returns nothing rather
/// than a sentence that might be untrue.
pub fn memory_note(core: &str, thread: Option<&str>) -> String {
    match core {
        "rust" => "This engine keeps its memory in the conversation's own messages, so it carries \
                  those into the branch: it answers the new conversation from the copied history now."
            .to_string(),
        "pi" => match thread {
            Some(f) => format!(
                "pi forked this conversation's thread into {f}; the thread it was on is where it was."
            ),
            None => "pi forked this conversation's thread; pi writes that file when the first message \
                    lands on it."
                .to_string(),
        },
        _ => String::new(),
    }
}

/// Create the branch's row and copy the stretch of the transcript it is made from.
///
/// Deliberately before the model is told anything: a refusal from [`resolve`] then leaves no row
/// in the sidebar, no thread in pi's directory and no half-copied transcript. The row is created
/// with the source's folder, model, mode and web grant, because a branch of a cowork conversation
/// that suddenly has no folder, or of a chat that suddenly cannot search, is a different kind of
/// conversation than the one the person clicked Branch on.
pub fn create_row(
    store: &dyn StoreOps,
    from: i64,
    point: &BranchPoint,
) -> Result<(i64, String, usize), String> {
    let source = store
        .session(from)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("conversation {from} does not exist, so nothing was branched"))?;
    let title = branch_title(&source.title);
    let new_id = store
        .create_session(&title, &source.workspace, &source.model)
        .map_err(|e| e.to_string())?;
    store
        .set_session_persona(new_id, &source.persona)
        .map_err(|e| e.to_string())?;
    store
        .set_session_web(new_id, source.web_on)
        .map_err(|e| e.to_string())?;
    // Who this conversation was cut out of, and which row it was cut at. The session tree is drawn
    // from these columns, so a branch that records nothing is a conversation nobody branched from as
    // far as every window that asks — including the one that just pressed Branch.
    //
    // Recorded before any row is copied, and the new row is taken back if the write fails: a branch
    // in the list with no parent is the one thing the tree cannot reconstruct later, and leaving it
    // there would be a claim about the conversation's history that nothing can correct.
    if let Err(e) = store.set_lineage(new_id, from, &point.role, Some(point.row_id)) {
        let _ = store.delete_session(new_id);
        return Err(format!(
            "the branch's parent could not be recorded ({e}), so nothing was branched"
        ));
    }
    let copied = if point.keep_through >= 1 {
        store
            .copy_through(from, new_id, point.keep_through)
            .map_err(|e| e.to_string())?
    } else {
        0
    };
    Ok((new_id, title, copied))
}

/// The whole ask, end to end: check it, and let the host make the branch in whichever way its core
/// does that — the Rust core copies rows and adopts them, the pi core forks pi's tree and records
/// the fork.
///
/// One entry point for both transports because the busy rule and the proof of which message was
/// meant are the substance, and a phone must not be able to branch a conversation the desktop
/// refuses to branch mid-turn.
pub async fn apply_and_open(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    req: &BranchReq,
) -> Result<BranchOutcome, String> {
    // Not while a turn is running. On the pi core a fork re-binds the live thread while pi is
    // answering out of it, and on the Rust core the turn would finish into whichever conversation
    // the branch just made current. One refusal, worded so the person knows what to do about it.
    let st = host.state();
    if st.busy {
        return Err(format!(
            "a turn is running on conversation {}, so nothing was branched: stop it, or wait for it \
             to finish, and then branch",
            st.session_id
        ));
    }
    // Checked here as well as inside the host, so a transport's error says which conversation was
    // refused before anything was queued.
    if store.session(req.session).map_err(|e| e.to_string())?.is_none() {
        return Err(format!(
            "conversation {} does not exist, so nothing was branched",
            req.session
        ));
    }
    host.branch(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Attachment, Message, Role};
    use crate::store::Store;

    fn seeded() -> (Store, i64, Vec<i64>) {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Roast notes", "/tmp", "m").unwrap();
        s.set_session_persona(c, "chat").unwrap();
        s.set_session_web(c, true).unwrap();
        let mut ids = Vec::new();
        for (role, text) in [
            ("user", "first ask"),
            ("assistant", "first answer"),
            ("user", "second ask"),
            ("assistant", "second answer"),
        ] {
            let m = match role {
                "user" => Message::user(text),
                _ => Message::assistant(text),
            };
            ids.push(s.append_message(c, &m).unwrap());
        }
        (s, c, ids)
    }

    /// A branch from one of the person's messages keeps what came before and hands the ask back.
    #[test]
    fn branching_from_a_message_keeps_what_came_before_and_gives_the_ask_back() {
        let (s, c, ids) = seeded();
        let p = resolve(
            &s.messages(c).unwrap(),
            &BranchReq {
                session: c,
                message_id: Some(ids[2]),
                shown: "second ask".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.keep_through, ids[2] - 1, "the message itself is not copied");
        assert_eq!(p.ask.as_deref(), Some("second ask"));
        assert_eq!(p.trailing, 1, "one message stays only in the original");
        assert!(!p.whole);

        let (new_id, title, copied) = create_row(&s, c, &p).unwrap();
        assert_eq!(copied, 2);
        let rows = s.messages(new_id).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].content, "first ask");
        assert_eq!(rows[1].content, "first answer");
        assert_eq!(s.messages(c).unwrap().len(), 4, "the original keeps everything");
        assert_eq!(title, "Roast notes — branch of Roast notes");
        // The branch is the same kind of conversation as the one it came from.
        let row = s.session(new_id).unwrap().unwrap();
        assert_eq!(row.persona, "chat", "a chat's branch is still a chat");
        assert!(row.web_on, "and it keeps the web grant that chat had");
        assert_eq!(row.workspace, "/tmp", "and the folder it lives in");
    }

    /// The branch records the conversation it came out of and the row it was cut at.
    ///
    /// This is not bookkeeping for a debug view: the session tree is drawn from these columns, so a
    /// branch that records nothing is, to every window that asks, a conversation nobody ever cut —
    /// including the window that pressed the control a moment ago.
    #[test]
    fn a_branch_records_where_it_came_from_and_where_it_was_cut() {
        let (s, c, ids) = seeded();
        let mine = s.messages(c).unwrap();
        let p = resolve(
            &mine,
            &BranchReq { session: c, message_id: Some(ids[2]), shown: "second ask".into(), ..Default::default() },
        )
        .unwrap();
        let (new_id, _, _) = create_row(&s, c, &p).unwrap();
        let row = s.session(new_id).unwrap().unwrap();
        assert_eq!(row.parent, Some(c), "the tree has to be able to name its parent");
        assert_eq!(row.branch_role, "user");
        assert_eq!(row.branch_row, Some(ids[2]), "and the row it was cut at");
        assert_eq!(
            s.children_of(c).unwrap().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![new_id],
            "the parent's own row list must draw the child under it"
        );

        // Cut from an answer: the answer is inside the branch, so the recorded cut names the answer.
        let q = resolve(
            &mine,
            &BranchReq { session: c, message_id: Some(ids[1]), shown: "first answer".into(), ..Default::default() },
        )
        .unwrap();
        let (kid2, _, _) = create_row(&s, c, &q).unwrap();
        let row2 = s.session(kid2).unwrap().unwrap();
        assert_eq!(row2.branch_role, "assistant");
        assert_eq!(row2.branch_row, Some(ids[1]));
        assert_eq!(s.children_of(c).unwrap().len(), 2, "two paths out of one conversation");
    }

    /// A branch carries the path the source is reading, not every row its file holds.
    ///
    /// This is the case where "everything before that row" and "the conversation" come apart: the leaf
    /// was moved in front of everything, two rows were sent on that branch, and the four rows the leaf
    /// walked past are still in the record. A copy taken by row id alone would hand the person a
    /// conversation of rows they never saw in that conversation, answering out of a path they were not
    /// on — while the transcript they branched from showed two rows.
    #[test]
    fn a_branch_carries_the_branch_being_read_and_none_of_what_the_leaf_walked_past() {
        let (s, c, _) = seeded();
        s.set_leaf(c, Some(0)).unwrap();
        let ask = s.append_message(c, &Message::user("third ask")).unwrap();
        let ans = s.append_message(c, &Message::assistant("third answer")).unwrap();
        assert_eq!(s.messages(c).unwrap().len(), 2, "the conversation reads its branch");
        assert_eq!(s.off_branch_rows(c).unwrap(), 4, "and reports the rest as off the path");

        let p = resolve(
            &s.messages(c).unwrap(),
            &BranchReq { session: c, message_id: Some(ans), shown: "third answer".into(), ..Default::default() },
        )
        .unwrap();
        let (kid, _, copied) = create_row(&s, c, &p).unwrap();
        assert_eq!(copied, 2, "the branch's two rows, not the four the leaf left behind");
        assert_eq!(
            s.messages(kid).unwrap().iter().map(|m| m.content.clone()).collect::<Vec<_>>(),
            vec!["third ask", "third answer"],
            "and in reading order, so the copy answers from the branch it was cut from"
        );
        assert_eq!(s.message_count(kid).unwrap(), 2, "nothing off-path was inserted");
        assert_eq!(s.message_count(c).unwrap(), 6, "the source keeps every row it held");
        let _ = ask;
    }

    /// Branching at the last answer is a copy of the whole conversation; branching at an earlier
    /// answer carries that answer and drops what followed.
    #[test]
    fn branching_at_an_answer_carries_it_and_stops_there() {
        let (s, c, ids) = seeded();
        let rows = s.messages(c).unwrap();
        let mid = resolve(
            &rows,
            &BranchReq {
                session: c,
                message_id: Some(ids[1]),
                role: "assistant".into(),
                shown: "first answer".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(mid.keep_through, ids[1], "the answer asked at is carried");
        assert_eq!(mid.trailing, 2);
        assert!(!mid.whole);
        assert!(mid.ask.is_none(), "an answer is not something to ask again");
        let (new_id, _, copied) = create_row(&s, c, &mid).unwrap();
        assert_eq!(copied, 2);
        assert_eq!(s.messages(new_id).unwrap().len(), 2);

        let last = resolve(
            &rows,
            &BranchReq {
                session: c,
                message_id: Some(ids[3]),
                role: "assistant".into(),
                shown: "second answer".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(last.whole, "asked at the last answer, the branch is the whole conversation");
        let (_, _, copied) = create_row(&s, c, &last).unwrap();
        assert_eq!(copied, 4);
        assert_eq!(s.messages(c).unwrap().len(), 4);
    }

    /// A branch carries the columns the model and the UI each read: thinking and its signature,
    /// tool calls, the file badges, and the picture. A copy that lost them would be a transcript
    /// that looks branched and answers like a fresh conversation.
    #[test]
    fn a_branch_copies_the_columns_the_model_and_the_ui_each_read() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("With a picture", "/tmp", "m").unwrap();
        let ask = s
            .append_message(
                c,
                &Message::user_with("look at this", vec![Attachment {
                    mime: "image/png".into(),
                    data_b64: "aGVsbG8=".into(),
                    name: Some("shot.png".into()),
                }]),
            )
            .unwrap();
        let mut ans = Message::assistant("done");
        ans.role = Role::Assistant;
        ans.reasoning = Some("thought a while".into());
        ans.reasoning_signature = Some("sig-1".into());
        ans.reasoning_ms = Some(4_321);
        ans.changes = vec![crate::tools::changes::FileChange {
            path: "notes.md".into(),
            added: 3,
            removed: 1,
            diff: Some("@@ -1 +1 @@".into()),
            created: false,
        }];
        let a = s.append_message(c, &ans).unwrap();
        s.set_message_files(a, &[crate::tools::OutputFile {
            path: "notes.md".into(),
            name: "notes.md".into(),
            mime: "text/markdown".into(),
            bytes: 120,
            kind: crate::tools::FileKind::Artifact,
            created: true,
        }])
        .unwrap();

        let p = resolve(
            &s.messages(c).unwrap(),
            &BranchReq { session: c, message_id: Some(a), role: "assistant".into(), shown: "done".into(), ..Default::default() },
        )
        .unwrap();
        let (new_id, _, copied) = create_row(&s, c, &p).unwrap();
        assert_eq!(copied, 2);
        let rows = s.messages(new_id).unwrap();
        assert_ne!(rows[0].id, ask, "the branch has its own row ids, not the source's");
        assert_eq!(rows[0].content, "look at this", "in the order the original had them");
        assert_eq!(rows[1].content, "done");
        assert_eq!(rows[1].reasoning, "thought a while", "the thinking survives");
        assert_eq!(rows[1].reasoning_signature, "sig-1", "a replayed thinking block needs its signature");
        assert_eq!(rows[1].reasoning_ms, 4_321, "the transcript still says how long it thought");
        assert_eq!(rows[1].changes.len(), 1, "the +3 -1 badge survives");
        assert_eq!(rows[1].files.len(), 1, "the file chip survives");
        let pics = s.message_attachments(rows[0].id).unwrap();
        assert_eq!(pics.len(), 1, "so does the picture");
        assert_eq!(pics[0].data_b64, "aGVsbG8=");
        assert_eq!(pics[0].name.as_deref(), Some("shot.png"));
        // Search sees the branch under its own ids, not only the original.
        assert!(!s.search("look at this", 5).unwrap().is_empty());
        assert!(s
            .search("look at this", 5)
            .unwrap()
            .iter()
            .any(|h| h.session_id == new_id));
    }

    /// The guard: a window that has drifted does not get a branch made from a row it never saw.
    #[test]
    fn a_branch_that_cannot_prove_which_message_it_means_is_refused() {
        let (s, c, ids) = seeded();

        // Somebody else changed the row this window was showing, so the rows handed to
        // `resolve` are the ones read at branch time — which is what the guard is compared
        // against. A stale window's copy of the text is the caller's `shown`, not these rows.
        s.edit_message(ids[2], "someone else's ask").unwrap();
        let e = resolve(
            &s.messages(c).unwrap(),
            &BranchReq { session: c, message_id: Some(ids[2]), shown: "second ask".into(), ..Default::default() },
        )
        .unwrap_err();
        assert!(e.contains("says something else now"), "{e}");
        assert!(e.contains("someone"), "the error names the row it refused: {e}");
        let rows = s.messages(c).unwrap();

        // A position that is not there — a window showing more messages than the store has.
        let e = resolve(
            &rows,
            &BranchReq { session: c, from_end: Some(9), shown: "first ask".into(), ..Default::default() },
        )
        .unwrap_err();
        assert!(e.contains("has 2"), "the count that is true belongs in the error: {e}");

        // Nothing named at all.
        let e = resolve(&rows, &BranchReq { session: c, shown: "first ask".into(), ..Default::default() }).unwrap_err();
        assert!(e.contains("name the message"), "{e}");

        // A row this window never rendered: no words to check it against.
        let e = resolve(
            &rows,
            &BranchReq { session: c, message_id: Some(ids[0]), shown: "".into(), ..Default::default() },
        )
        .unwrap_err();
        assert!(e.contains("says something else now"), "{e}");

        // And a refused resolve creates nothing.
        let before = s.list_sessions(50).unwrap().len();
        assert!(create_row(&s, 999, &BranchPoint { row_id: 1, role: "user".into(), keep_through: 0, ask: None, trailing: 0, whole: false }).is_err());
        assert_eq!(s.list_sessions(50).unwrap().len(), before, "a refusal leaves the list alone");
    }

    /// A branch of a branch names the row it was made from, not the conversation two steps back.
    #[test]
    fn naming_a_branch_names_its_own_parent_and_stays_readable() {
        assert_eq!(branch_title("Roast notes"), "Roast notes — branch of Roast notes");
        assert_eq!(
            branch_title("Roast notes — branch of Roast notes"),
            "Roast notes — branch of Roast notes — branch of Roast notes"
        );
        assert_eq!(branch_title(""), "branch of an untitled conversation");
        let long = "x".repeat(120);
        let t = branch_title(&long);
        assert!(t.chars().count() < 100, "a 120-character title does not get doubled into the sidebar: {t}");
        assert!(t.contains("branch of"), "{t}");
    }
}
