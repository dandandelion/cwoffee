//! Correcting something you already sent.
//!
//! Two asks hide behind one pencil, and they are not the same operation:
//!
//! * **Save these words.** The transcript is corrected and nothing else moves. The conversation
//!   stays the length it was, and the model is *not* told — it was answered on the words that were
//!   there, and every turn after that was built on that answer. Rewriting a sentence the model has
//!   already read would make the rest of the conversation refer to a message that was never sent.
//! * **Save these words and send them again.** Everything after that message goes, and the message
//!   is sent as a new turn: the conversation is rebuilt from that point. That is the ask behind
//!   "I phrased it wrong, start over from there".
//!
//! The store is the transcript, so both cores agree on what the conversation *says*; the host has
//! to be told separately when its own memory of it has to be taken back ([`HostOps::rewind`]).
//!
//! One rule runs through all of this: **nothing is changed on a guess.** The caller names the row
//! it edited (by id, or by position counted from the newest message) *and* repeats the words it saw.
//! If the row now says something else — another window, another device, a send that landed
//! meanwhile — the edit is refused rather than applied to whatever sits at that position now. The
//! alternative is a UI that quietly rewrites a message the person was not looking at.

use serde::{Deserialize, Serialize};

use crate::host::HostOps;
use crate::provider::Message;
use crate::store::StoreOps;

/// What the person asked for.
///
/// `message_id` and `from_end` are alternatives: a message loaded from the store has an id, while
/// a message this window painted itself while sending has no id yet on the client side. `from_end`
/// counts *your* messages from the newest backwards — 1 is the newest — which is stable for a
/// window that is looking at the end of the conversation, which is the only place an edit can be
/// started from.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EditReq {
    pub session: i64,
    #[serde(default)]
    pub message_id: Option<i64>,
    #[serde(default)]
    pub from_end: Option<usize>,
    /// The words this window is showing. Compared against what the row holds.
    #[serde(default)]
    pub shown: String,
    /// The words it should hold.
    #[serde(default)]
    pub next: String,
    /// Take the conversation back to this message and send these words again.
    #[serde(default)]
    pub resend: bool,
}

/// What the store holds now, and what the caller still has to do.
#[derive(Debug, Clone)]
pub struct EditPlan {
    pub session: i64,
    /// How many stored rows went with the edit (0 for a save in place).
    pub dropped: usize,
    /// The message to send, when `resend` was asked for. Its attachments come from the row that
    /// was dropped — the person is re-sending the picture they sent, not a description of it.
    pub resend: Option<Message>,
    /// What to tell the person happened.
    pub note: String,
}

/// Find the row this edit is about, and check the words the caller saw against it.
fn resolve<'a>(rows: &'a [crate::store::StoredMessage], req: &EditReq) -> Result<&'a crate::store::StoredMessage, String> {
    let mine: Vec<&crate::store::StoredMessage> =
        rows.iter().filter(|m| m.role == "user").collect();
    if mine.is_empty() {
        return Err(format!(
            "conversation {} has no messages of yours in it, so there is nothing to edit; nothing was changed",
            req.session
        ));
    }
    let found: &crate::store::StoredMessage = if let Some(id) = req.message_id {
        mine.iter().find(|m| m.id == id).copied().ok_or_else(|| {
            format!(
                "that message is not part of conversation {} any more, so nothing was changed. \
                 Reload the conversation and edit the message you can see.",
                req.session
            )
        })?
    } else if let Some(from_end) = req.from_end {
        if from_end == 0 || from_end > mine.len() {
            return Err(format!(
                "this window asked for the {from_end}th of your messages counting back from the newest, \
                 and conversation {} has {}. Nothing was changed — the messages on screen are not the \
                 ones in the store any more; reload the conversation.",
                req.session,
                mine.len()
            ));
        }
        mine[mine.len() - from_end]
    } else {
        return Err(
            "an edit has to name the message: give it a stored id, or a position counted back from \
             your newest message. Nothing was sent or changed."
                .to_string(),
        )
    };

    if found.content != req.shown {
        return Err(format!(
            "that message says something else now — {} — so nothing was changed. Reload the \
             conversation and edit the version you can see.",
            quoted(&found.content)
        ));
    }
    Ok(found)
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

/// Check it, apply it, and say what has to happen next.
///
/// Everything here happens before any host is told anything, so a refusal leaves the conversation
/// exactly as it was: no row rewritten, nothing dropped, no turn queued.
pub fn apply(store: &dyn StoreOps, req: &EditReq) -> Result<EditPlan, String> {
    if store.session(req.session).map_err(|e| e.to_string())?.is_none() {
        return Err(format!(
            "conversation {} does not exist, so nothing was changed",
            req.session
        ));
    }
    let rows = store.messages(req.session).map_err(|e| e.to_string())?;
    let target = resolve(&rows, req)?;

    let has_pictures = !target.attachments.is_empty();
    if req.next.trim().is_empty() && !has_pictures {
        return Err(format!(
            "there is nothing to save: the new text is empty and that message has no attachments \
             either, so this would erase your words and leave nothing in their place."
        ));
    }

    if !req.resend {
        if !store.edit_message(target.id, &req.next).map_err(|e| e.to_string())? {
            return Err(format!(
                "that message was removed while this edit was in flight, so nothing was changed"
            ));
        }
        return Ok(EditPlan {
            session: req.session,
            dropped: 0,
            resend: None,
            // Says what it did *not* do, because that is the part that surprises people: the
            // transcript is corrected, the model is still working from the sentence it was answered
            // on, and the way to redo the answer is to send it again.
            note: "saved. The transcript now holds those words. The model was not told — it already \
                   answered that message, and every later turn was built on the answer it gave; \
                   “save and send again” is what takes the conversation back to it."
                .to_string(),
        });
    }

    // Everything from that message onwards goes, that message included: a send writes its own row,
    // and that is the row the transcript should show.
    // Read the attachments before the drop, not after: their rows go with the message's
    // (`ON DELETE CASCADE`), so asking afterwards would answer "no pictures" about a message that
    // plainly had one. The person is re-sending what they sent.
    let attachments = store.message_attachments(target.id).map_err(|e| e.to_string())?;
    let dropped = store.truncate_from(req.session, target.id).map_err(|e| e.to_string())?;
    let resend = Message::user_with(req.next.clone(), attachments);

    Ok(EditPlan {
        session: req.session,
        dropped,
        resend: Some(resend),
        note: if dropped == 1 {
            // Nothing followed it: the one row that went is the message being sent again.
            "that message was the last word in the conversation, so it is the only thing that went; \
             it is being sent again."
                .to_string()
        } else {
            let later = dropped - 1;
            format!(
                "the conversation goes back to that message — it and {later} later {} went — and \
                 your words are being sent again.",
                if later == 1 { "message" } else { "messages" }
            )
        },
    })
}

/// What both transports answer with.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditOutcome {
    pub note: String,
    pub dropped: usize,
    pub resending: bool,
    /// Whether the record has the re-sent message in it again by the time this returns.
    ///
    /// A client that reloads the conversation right after being told "it is being sent again" reads
    /// the store, and if the send has not written its row yet it paints a conversation missing the
    /// message it was just promised — an answer with no question under it. Waiting for the row is
    /// what makes that reload honest; `false` says the send is still queued behind something the
    /// host is doing, which the transcript will say more about.
    pub visible: bool,
}

/// The whole ask, end to end: check it, apply it to the record, and — when that is what was asked —
/// take the host's memory back and send the words again.
///
/// One function for both transports because the order is the substance: cut the record, take the
/// memory back, re-send. A host that re-sent before rewinding would answer out of the conversation
/// the person has just cut down, and a gateway that checked the busy rule differently from the
/// desktop would let a phone do what the desktop refuses.
pub async fn apply_and_send(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    req: &EditReq,
) -> Result<EditOutcome, String> {
    // Not while a turn is running. That turn is being answered out of the messages about to be cut,
    // and whatever it writes next lands in a transcript that no longer holds what it was written
    // from. One refusal, worded so the person knows what to do about it.
    let st = host.state();
    if st.busy {
        return Err(format!(
            "a turn is running on conversation {}, so nothing was changed: stop it, or wait for it to \
             finish, and then edit",
            st.session_id
        ));
    }
    let plan = apply(store, req)?;
    let Some(message) = plan.resend else {
        return Ok(EditOutcome { note: plan.note, dropped: plan.dropped, resending: false, visible: true });
    };
    let rows_now = store.message_count(req.session).map_err(|e| e.to_string())?;
    // Both are queued, so the host loop does them in this order.
    host.rewind(req.session);
    host.send_turn_to(message.content, message.attachments, Some(req.session));

    // A send is a fact before it is asked of a model — both cores write the row first — so the row
    // coming back is how the caller can tell the re-send started rather than merely being wished for.
    let mut visible = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_200);
    while std::time::Instant::now() < deadline {
        match store.message_count(req.session) {
            Ok(n) if n > rows_now => {
                visible = true;
                break;
            }
            Err(e) => {
                // The record went unreadable mid-edit. Say that rather than report a success.
                return Err(format!(
                    "the conversation was taken back to that message, but the record could not be read \
                     again ({e}); the send was queued and the transcript will say what came of it"
                ));
            }
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    let note = if visible {
        plan.note
    } else {
        format!(
            "{} — the record has not taken the message back yet, so this host is still busy with \
             something; watch the transcript for what it says about the send.",
            plan.note
        )
    };
    Ok(EditOutcome { note, dropped: plan.dropped, resending: true, visible })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Attachment;
    use crate::store::Store;

    fn seeded() -> (Store, i64, Vec<i64>) {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Notes", "/tmp", "m").unwrap();
        let mut ids = Vec::new();
        for (role, text) in [
            ("user", "first ask"),
            ("assistant", "first answer"),
            ("user", "second ask"),
            ("assistant", "second answer"),
        ] {
            let m = if role == "user" { Message::user(text) } else { Message::assistant(text) };
            ids.push(s.append_message(c, &m).unwrap());
        }
        (s, c, ids)
    }

    /// Save in place: the row changes, the length does not, and nothing is queued to send.
    #[test]
    fn saving_words_in_place_leaves_the_conversation_the_same_length() {
        let (s, c, ids) = seeded();
        let plan = apply(
            &s,
            &EditReq {
                session: c,
                message_id: Some(ids[0]),
                from_end: None,
                shown: "first ask".into(),
                next: "first ask, said better".into(),
                resend: false,
            },
        )
        .unwrap();
        assert_eq!(plan.dropped, 0);
        assert!(plan.resend.is_none(), "a save in place does not queue a turn");
        let rows = s.messages(c).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].content, "first ask, said better");
        assert_eq!(rows[3].content, "second answer", "later turns are not touched");
    }

    /// Save and send again: the message and everything after it go, and the message comes back as
    /// something to send — including the picture that went with it.
    #[test]
    fn sending_again_takes_the_conversation_back_to_that_message() {
        let s = Store::in_memory().unwrap();
        let c = s.create_session("Notes", "/tmp", "m").unwrap();
        let first = s.append_message(
            c,
            &Message::user_with("look at this", vec![Attachment {
                mime: "image/png".into(),
                data_b64: "aGVsbG8=".into(),
                name: Some("shot.png".into()),
            }]),
        )
        .unwrap();
        s.append_message(c, &Message::assistant("I see it")).unwrap();
        s.append_message(c, &Message::user("and now this")).unwrap();

        let plan = apply(
            &s,
            &EditReq {
                session: c,
                message_id: None,
                from_end: Some(2),
                shown: "look at this".into(),
                next: "look at this one instead".into(),
                resend: true,
            },
        )
        .unwrap();
        assert_eq!(plan.dropped, 3, "the message itself and the two after it");
        assert_eq!(s.messages(c).unwrap().len(), 0, "the conversation is back to nothing");
        let back = plan.resend.expect("a resend was asked for");
        assert_eq!(back.content, "look at this one instead");
        assert_eq!(back.attachments.len(), 1, "the picture goes back with the words");
        assert_eq!(back.attachments[0].data_b64, "aGVsbG8=", "and it is the picture, not its label");
        assert!(
            s.message_attachments(first).unwrap().is_empty(),
            "the row really is gone from the store — the payload came back with the resend because \
             it was read before the drop, not because the row survived it"
        );
    }

    /// The guard: a window that has drifted does not get its edit applied to a row it never saw.
    #[test]
    fn an_edit_that_cannot_prove_which_message_it_means_is_refused() {
        let (s, c, ids) = seeded();

        // The window still shows the old words; somebody else already changed them.
        s.edit_message(ids[0], "someone else's words").unwrap();
        let e = apply(
            &s,
            &EditReq {
                session: c,
                message_id: Some(ids[0]),
                shown: "first ask".into(),
                next: "my edit".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("says something else now"), "{e}");
        assert!(e.contains("someone"), "the error has to identify the row it refused: {e}");
        assert_eq!(s.messages(c).unwrap()[0].content, "someone else's words", "refused means untouched");

        // A position that is not there — a window showing more messages than the store has.
        let e = apply(
            &s,
            &EditReq {
                session: c,
                from_end: Some(9),
                shown: "first ask".into(),
                next: "nope".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("has 2"), "the count that is true belongs in the error: {e}");
        assert_eq!(s.messages(c).unwrap().len(), 4);

        // A message from another conversation is not this conversation's business.
        let other = s.create_session("Elsewhere", "/tmp", "m").unwrap();
        let e = apply(
            &s,
            &EditReq {
                session: other,
                message_id: Some(ids[0]),
                shown: "first ask".into(),
                next: "nope".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("no messages of yours"), "{e}");

        // And a message with nothing in it either way is refused rather than blanking the row.
        let e = apply(
            &s,
            &EditReq {
                session: c,
                message_id: Some(ids[2]),
                shown: "second ask".into(),
                next: "   ".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(e.contains("nothing to save"), "{e}");
        assert_eq!(s.messages(c).unwrap()[2].content, "second ask");
    }
}
