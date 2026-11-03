//! Mid-turn steering: what you type while a turn is running, and where it goes.
//!
//! Before this existed a mid-turn press changed nothing: the composer said "Enter does not send
//! while cwoffee is answering", kept the words in the box, and the person pressed again later. Pi
//! already had the capability (`steer`, `follow_up`, `clear_queue`, and the `queue_update` event
//! that describes the queue), and cwoffee was throwing it away in `translate.rs`. Both cores now
//! take the press and put it in one place, so the two engines answer the same gesture the same way.
//!
//! Two promises, and they are not the same promise:
//!
//!   * **a steer** lands *inside this turn* — after the current step's tool results are back and
//!     before the next request, so the model reads it in the turn that is already running.
//!   * **a follow-up** lands *after this turn*, as the next thing the model is asked.
//!
//! The words matter: a card that calls a follow-up "steered into this turn" is a lie about when the
//! model will read it. So every sentence here is one function, used by both cores, and the checks
//! grade those sentences rather than a paraphrase of them.
//!
//! Three rules the queue is built around:
//!
//!   * **Append-only, stamped when it was typed.** A queued message becomes its own user row whose
//!     `created_at` is the moment the person pressed Enter, not the moment it landed. A transcript
//!     sorted by arrival says the person asked something after they had actually asked it.
//!   * **Nothing is claimed early.** The landing receipt is emitted when the message goes into the
//!     prompt the *next* request is built from. Before that the pending line says it has not been
//!     sent, and no card may say the model holds it.
//!   * **A turn that stops gives the words back.** Stop — or a turn that ends in error — drains the
//!     queue into the composer. pi documents exactly that for its Esc key (`clear_queue` before
//!     `abort`, then restore the returned text in the editor). Silent loss is the failure this
//!     module is designed against.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::provider::Attachment;

/// Which promise a mid-turn press makes. The words are pi's own RPC verbs, so the setting on the
/// desktop, the sentence in the composer and the command on the wire are one vocabulary rather than
/// three kept in step by hand.
/// The wire spelling is pi's own command name (`follow_up`, not `followUp`) on purpose: the config
/// line, the settings word, the RPC verb sent to pi and the event frame all say the same thing, so
/// there is nothing to keep in step by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidTurn {
    /// Into this turn, at its next step.
    Steer,
    /// After this turn.
    FollowUp,
}

impl Default for MidTurn {
    /// Steering is the default because it is the thing a person means when they type during an
    /// answer: change what it is doing. A follow-up is the deliberate other promise, and the
    /// composer says which one Enter is making.
    fn default() -> Self {
        MidTurn::Steer
    }
}

impl MidTurn {
    /// The config word. `follow_up` is pi's command name; `follow-up` and `followup` are accepted
    /// because a person writing a config file types the hyphenated English, and a setting that
    /// silently becomes the default is the failure `config.rs` already refuses to have.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "steer" => Some(Self::Steer),
            "follow_up" | "followup" => Some(Self::FollowUp),
            _ => None,
        }
    }

    /// The pending line's own label: where the words will go, in four words. Said by the engine so
    /// a card on a phone and a card on the desktop cannot paraphrase the promise differently.
    pub fn label(self) -> &'static str {
        match self {
            Self::Steer => "into this turn",
            Self::FollowUp => "after this turn",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
        }
    }

    /// The composer's announcement: which kind the next press is, in one sentence shape for both
    /// kinds so a reader notices the difference rather than the layout.
    pub fn announce(self) -> &'static str {
        match self {
            Self::Steer =>
                "Steering: Enter puts your message into this turn at its next step",
            Self::FollowUp =>
                "Follow-up: Enter asks your message after this turn finishes",
        }
    }

    /// Where the message will be read, said once the queue accepted it. A pending line that
    /// promises the model "has" it is the claim this avoids: nothing has it until a request carries
    /// it.
    pub fn pending_note(self) -> &'static str {
        match self {
            Self::Steer => "waiting for this turn's next step — not sent to the model yet",
            Self::FollowUp => "waiting for this turn to end — not sent to the model yet",
        }
    }

    /// The `queue_update` field a queued message shows up in, and the RPC verb that queues it.
    pub fn pi_field(self) -> &'static str {
        match self {
            Self::Steer => "steering",
            Self::FollowUp => "followUp",
        }
    }

    pub fn pi_command(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
        }
    }
}

/// One queued message, in the queue's own terms: it carries the picture bytes, so it is what the
/// host and the agent hold, and never goes on the wire.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: u64,
    pub text: String,
    pub kind: MidTurn,
    /// Seconds, on the same clock as `messages.created_at`, because this value ends up as a row's
    /// stamp. The stamp is the moment Enter was pressed.
    pub typed_at: i64,
    /// The conversation this was typed into. A steer that wandered into another conversation's turn
    /// is the failure the `Send` address rule exists to stop, and a queue is no different.
    pub session: i64,
    pub atts: Vec<Attachment>,
}

impl Item {
    /// What a client may be told: the words, the promise, and how many pictures came with it.
    /// The bytes stay here — a base64 image inside a status snapshot is a megabyte of IPC per
    /// repaint, and a pending line only needs to say a picture is attached.
    pub fn view(&self) -> View {
        View {
            id: self.id,
            text: self.text.clone(),
            kind: self.kind,
            typed_at: self.typed_at,
            session: self.session,
            attachments: self.atts.len(),
            label: self.kind.label().to_string(),
            note: self.kind.pending_note().to_string(),
        }
    }
}

/// The queue as a client sees it. Ids are **cwoffee's**, not pi's: pi's queue is a list of plain
/// strings and cannot name an item, so withdrawal and the landing receipt are addressed by an id
/// this side assigned and keeps, on both cores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub id: u64,
    pub text: String,
    pub kind: MidTurn,
    pub typed_at: i64,
    pub session: i64,
    pub attachments: usize,
    /// "into this turn" / "after this turn", from [`MidTurn::label`].
    pub label: String,
    /// The engine's own pending sentence, from [`MidTurn::pending_note`]. A card that wrote its own
    /// wording here would be a second claim about where the message goes, and the two would drift
    /// the first time one of them was edited.
    pub note: String,
}

/// The queue, shared between the shell that receives presses and the loop that drains them.
///
/// Shared by handle rather than by mailbox command on purpose: the Rust core runs its turn *inside*
/// its own loop, so a `HostCmd` sent mid-turn is not read until the turn is over — which is exactly
/// the behaviour this feature replaces. A lock the agent polls at the step boundary is the only
/// place a press can arrive while the model is still working.
#[derive(Debug, Clone)]
pub struct Queue {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    items: Vec<Item>,
}

impl Queue {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(Inner::default())) }
    }

    /// Accept a press. `typed_at` is stamped here, at the boundary the words arrive at, so a row
    /// written minutes later still says when the person actually pressed Enter.
    pub fn push(
        &self,
        session: i64,
        kind: MidTurn,
        text: impl Into<String>,
        atts: Vec<Attachment>,
        typed_at: i64,
    ) -> Item {
        let mut g = self.inner.lock().unwrap();
        g.next_id += 1;
        let item = Item { id: g.next_id, text: text.into(), kind, typed_at, session, atts };
        g.items.push(item.clone());
        item
    }

    /// Take one item back, by the id this queue handed out. `None` means it is not there — it
    /// landed, or somebody else withdrew it, and either way the words are not coming back.
    pub fn withdraw(&self, id: u64) -> Option<Item> {
        let mut g = self.inner.lock().unwrap();
        let at = g.items.iter().position(|i| i.id == id)?;
        Some(g.items.remove(at))
    }

    /// Drop everything, in order, and hand it back. This is pi's `clear_queue` shape: it returns
    /// the text so a client can put it where it came from.
    pub fn drain(&self) -> Vec<Item> {
        let mut g = self.inner.lock().unwrap();
        std::mem::take(&mut g.items)
    }

    /// Drop everything of one kind, in the order it was typed, and leave the rest.
    pub fn take_kind(&self, kind: MidTurn, session: i64) -> Vec<Item> {
        let mut g = self.inner.lock().unwrap();
        let mut out = Vec::new();
        g.items.retain(|i| {
            if i.kind == kind && i.session == session {
                out.push(i.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// What is waiting, as a whole snapshot rather than a delta: a client that missed one event
    /// must not be left holding a line for a message that is no longer queued.
    pub fn snapshot(&self) -> Vec<View> {
        self.inner.lock().unwrap().items.iter().map(Item::view).collect()
    }

    /// The same, in item form — used by the pi bridge, which has to re-queue the words after a
    /// `clear_queue` and so needs the attachments back.
    pub fn items(&self) -> Vec<Item> {
        self.inner.lock().unwrap().items.clone()
    }

    pub fn has(&self, id: u64) -> bool {
        self.inner.lock().unwrap().items.iter().any(|i| i.id == id)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().items.len()
    }

    /// Whether anything is waiting for *this* conversation. The composer's mid-turn line counts
    /// them, and counting another conversation's queue is a number about nothing on screen.
    pub fn len_for(&self, session: i64) -> usize {
        self.inner.lock().unwrap().items.iter().filter(|i| i.session == session).count()
    }

    /// Drop everything waiting for one conversation, in typing order, and hand it back. Used when
    /// a run ends with messages still queued: their promise was about *that* turn, so they go back
    /// to the composer rather than migrating into whichever turn comes next.
    pub fn take_session(&self, session: i64) -> Vec<Item> {
        let mut g = self.inner.lock().unwrap();
        let mut out = Vec::new();
        g.items.retain(|i| {
            if i.session == session {
                out.push(i.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Reconcile against pi's `queue_update`, which carries complete queues of **strings** and no
    /// ids. `keep` is what pi says is still queued, steering first then follow-up; whatever this
    /// side holds that pi no longer names has been delivered (or dropped by another client), and
    /// stops being shown.
    ///
    /// When the same words were typed twice, the *earliest* occurrence is the one pi delivered — it
    /// removes queued text from the front — so the survivors are the latest occurrences. Getting
    /// this backwards would mark a message as landed while an identical one is still waiting.
    pub fn reconcile(&self, keep: &[String]) -> Vec<Item> {
        let mut g = self.inner.lock().unwrap();
        let mut budget: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for t in keep {
            *budget.entry(t.clone()).or_default() += 1;
        }
        // Count survivors from the back, then rebuild in typing order.
        let items = std::mem::take(&mut g.items);
        let mut survive = vec![false; items.len()];
        for (idx, item) in items.iter().enumerate().rev() {
            let n = budget.get(&item.text).copied().unwrap_or(0);
            if n > 0 {
                survive[idx] = true;
                budget.insert(item.text.clone(), n - 1);
            }
        }
        let mut dropped = Vec::new();
        for (idx, item) in items.into_iter().enumerate() {
            if survive[idx] {
                g.items.push(item);
            } else {
                dropped.push(item);
            }
        }
        dropped
    }

    /// Put items back wholesale — used after pi's `clear_queue` re-queues the messages a withdrawal
    /// did not name, so cwoffee's ids survive a round trip through a protocol that has none.
    pub fn replace(&self, items: Vec<Item>) {
        let mut g = self.inner.lock().unwrap();
        g.items = items;
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// The receipt a landed message owes the person: *where* it landed, in the turn's own terms.
///
/// `step` is the round-trip that carried it, counted from the start of the turn. "Into this turn at
/// its next step" is checkable — the next request is the one holding the row — whereas "the model
/// has it now" is a claim about somebody else's attention.
pub fn landing_note(kind: MidTurn, step: usize) -> String {
    match kind {
        MidTurn::Steer => format!(
            "your message went into this turn at step {step}, after the tool results and before the \
             next request — that request is the one carrying it"
        ),
        MidTurn::FollowUp => format!(
            "your message was asked after this turn, as its own turn (step {step}) — the answer below \
             is to it, and nothing before it changed"
        ),
    }
}

/// Once it has landed there is no taking it back: the bytes are on the wire, and a control offering
/// to unsend them would offer something no record can undo.
pub fn landed_note(id: u64) -> String {
    format!(
        "message {id} is already in the conversation. A message that landed cannot be unsent — the \
         row stays; Stop ends the answer, not the question."
    )
}

/// The composer's line while a turn is running: what pressing Enter will do, and that the words
/// can still be taken back while they are waiting.
pub fn composer_hint(kind: MidTurn) -> String {
    match kind {
        MidTurn::Steer =>
            "Steering this turn — Enter puts it in at the next step. Take it back before it lands.",
        MidTurn::FollowUp =>
            "Queuing a follow-up — Enter asks it after this turn. Take it back before it lands.",
    }
    .to_string()
}

/// A press that found nothing running. The honest answer names the alternative rather than queueing
/// a message nothing will ever drain: pi's queues are only polled by a run, and a steer typed
/// against an idle session would sit there until one starts.
pub fn idle_refusal() -> String {
    "nothing is running in this conversation, so there is no turn to steer — send it as a message \
     and it starts a turn.".to_string()
}

/// A press aimed at a conversation the host is not answering. Same shape as the `Send` address
/// refusal: a mismatch is refused rather than rerouted, because two clients editing each other's
/// conversation is how a transcript ends up holding a sentence nobody typed into it.
pub fn wrong_session_refusal(want: i64, here: i64) -> String {
    format!(
        "that message named conversation {want}, which this host is not answering — it is on \
         {here}. Nothing was queued: a mid-turn message cannot move the host to another conversation."
    )
}

/// What Stop (or an errored turn) says about the words that had not landed.
pub fn returned_note(n: usize) -> String {
    match n {
        0 => "nothing was waiting when the turn stopped.".to_string(),
        1 => "1 message had not reached the model. Its words are back in the composer; nothing was \
              sent."
            .to_string(),
        n => format!(
            "{n} messages had not reached the model. Their words are back in the composer; nothing \
             was sent."
        ),
    }
}

/// A withdrawal the queue could not honour, because the message had already landed.
pub fn withdraw_gone_refusal() -> String {
    "that message is no longer in the queue — it went into the turn or was taken back already. \
     Nothing was removed."
        .to_string()
}

/// The sentence for a mid-turn message that cannot answer a card. Steering changes what the model
/// is told next; it does not answer for a person, and it does not move a gate.
pub fn card_refusal() -> String {
    "a mid-turn message cannot answer an approval card or a question, cannot change the approval \
     mode, and cannot change the tools this turn started with — the card is the only place that \
     answers, and it stays up until a person presses it."
        .to_string()
}

/// What the settings screen says the stored choice means. The composer quotes
/// [`MidTurn::announce`]; this names where the choice lives, because a phone tab and the desktop
/// press the same Enter and must not be promised different things.
pub fn setting_note(kind: MidTurn) -> String {
    format!(
        "{}. It is stored on the host, so every window that sends into this conversation is given \
         the same promise; a message already waiting keeps the kind it was typed as.",
        match kind {
            MidTurn::Steer => "A message typed while a turn is running goes into that turn at its next step",
            MidTurn::FollowUp => "A message typed while a turn is running is asked after that turn",
        }
    )
}

/// The mode cwoffee asks pi for with `set_steering_mode` / `set_follow_up_mode`.
///
/// cwoffee drains **all** pending messages of a kind at one boundary, so pi is told `all` too: left
/// at pi's `one-at-a-time` default, three messages typed during one tool batch would land one per
/// turn on pi and all three at the next step on the Rust core — one gesture, two answers. The
/// choice of press kind is cwoffee's setting; this is the delivery half of pi's vocabulary, and it
/// is settled rather than defaulted so the two cores cannot drift.
pub const PI_QUEUE_MODE: &str = "all";

/// Attachment validation at the queue boundary, with **this host's** limits — the same gate `Send`
/// runs, so a mid-turn picture cannot be a bigger picture than a sent one. A queue with numbers of
/// its own would reject a picture the turn accepts, which reads to the user as flakiness.
pub fn checked_attachments(
    atts: &[Attachment],
    limits: crate::host::AttachmentLimits,
) -> Result<(), String> {
    crate::host::check_attachments(atts, limits)
}

/// Seconds, on the clock `messages.created_at` uses. One place so the stamp a row gets and the
/// stamp a pending line shows cannot be two different numbers.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_word_accepts_what_a_person_types_and_refuses_the_rest() {
        assert_eq!(MidTurn::parse("steer"), Some(MidTurn::Steer));
        assert_eq!(MidTurn::parse("follow_up"), Some(MidTurn::FollowUp));
        assert_eq!(MidTurn::parse("Follow-Up"), Some(MidTurn::FollowUp));
        assert_eq!(MidTurn::parse(" followup "), Some(MidTurn::FollowUp));
        assert_eq!(MidTurn::parse("whatever"), None, "a word that is not a promise is not a mode");
        assert_eq!(MidTurn::default(), MidTurn::Steer);
        assert_eq!(MidTurn::Steer.word(), "steer");
        assert_eq!(MidTurn::FollowUp.word(), "follow_up");
        assert_eq!(MidTurn::Steer.pi_command(), "steer");
        assert_eq!(MidTurn::FollowUp.pi_command(), "follow_up");
        assert_eq!(MidTurn::FollowUp.pi_field(), "followUp");
    }

    /// The two promises must not read alike. A follow-up worded like a steer tells the person the
    /// model is about to read their message inside a turn that will finish first.
    #[test]
    fn a_follow_up_is_never_worded_like_a_steer() {
        let steer = MidTurn::Steer.announce();
        let follow = MidTurn::FollowUp.announce();
        assert!(steer.contains("this turn") && !steer.contains("after this turn"));
        assert!(follow.contains("after this turn") && !follow.contains("into this turn"));
        assert!(MidTurn::Steer.pending_note().contains("next step"));
        assert!(MidTurn::FollowUp.pending_note().contains("to end"));
        for note in [
            MidTurn::Steer.pending_note(),
            MidTurn::FollowUp.pending_note(),
            composer_hint(MidTurn::Steer).as_str(),
            composer_hint(MidTurn::FollowUp).as_str(),
        ] {
            assert!(
                !note.contains("the model has it"),
                "a pending line may not claim the model holds it: {note}"
            );
        }
        assert!(landing_note(MidTurn::Steer, 3).contains("before the next request"));
        assert!(landing_note(MidTurn::FollowUp, 3).contains("asked after this turn"));
        assert!(landing_note(MidTurn::FollowUp, 3).contains("step 3"));
    }

    /// The stamp is the press, not the arrival, and withdrawal is by cwoffee's id on both cores —
    /// pi's queue is a list of strings, so an id this side invented is the only handle there is.
    #[test]
    fn a_queued_message_keeps_the_time_it_was_typed_and_can_be_taken_back() {
        let q = Queue::new();
        let a = q.push(7, MidTurn::Steer, "stop and do this instead", vec![], 1_000);
        let b = q.push(7, MidTurn::FollowUp, "summarise when you are done", vec![], 1_500);
        assert_eq!(a.typed_at, 1_000, "the stamp is taken at the press");
        assert_eq!(b.typed_at, 1_500);
        assert_eq!(q.len(), 2);
        assert_eq!(q.withdraw(b.id).expect("the follow-up is queued").typed_at, 1_500);
        assert_eq!(q.len(), 1);
        assert!(q.withdraw(b.id).is_none(), "a second withdrawal takes nothing back");
        assert!(q.has(a.id));
        assert_eq!(q.snapshot()[0].id, a.id, "the id a client was given is the id the queue answers to");
    }

    /// The step boundary takes steers and leaves follow-ups; the run's end takes the other half.
    /// Keyed by conversation, so a phone's steer cannot land in the desktop's turn.
    #[test]
    fn the_step_boundary_takes_only_this_conversations_steers() {
        let q = Queue::new();
        q.push(7, MidTurn::Steer, "one", vec![], 1);
        q.push(7, MidTurn::FollowUp, "two", vec![], 2);
        q.push(9, MidTurn::Steer, "someone else's conversation", vec![], 3);
        let steers = q.take_kind(MidTurn::Steer, 7);
        assert_eq!(steers.iter().map(|i| i.text.clone()).collect::<Vec<_>>(), vec!["one"]);
        assert_eq!(q.snapshot().len(), 2, "the follow-up and the other conversation stay put");
        assert_eq!(q.len_for(7), 1);
        assert_eq!(q.len_for(9), 1);
        let follow = q.take_kind(MidTurn::FollowUp, 7);
        assert_eq!(follow.len(), 1);
        assert_eq!(
            q.items().iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            vec!["someone else's conversation"]
        );
    }

    /// Stop hands everything back, in order, with the text — pi documents exactly this shape for
    /// its Esc key, and the Rust core has to answer the same gesture.
    #[test]
    fn stopping_hands_every_unlanded_message_back_in_order() {
        let q = Queue::new();
        q.push(7, MidTurn::Steer, "first", vec![], 1);
        q.push(7, MidTurn::FollowUp, "second", vec![], 2);
        let back = q.drain();
        assert_eq!(back.iter().map(|i| i.text.clone()).collect::<Vec<_>>(), vec!["first", "second"]);
        assert_eq!(q.len(), 0);
        assert_eq!(
            returned_note(2),
            "2 messages had not reached the model. Their words are back in the composer; nothing was sent."
        );
        assert_eq!(
            returned_note(1),
            "1 message had not reached the model. Its words are back in the composer; nothing was sent."
        );
        assert!(returned_note(0).contains("nothing was waiting"));
    }

    /// pi's `queue_update` carries complete queues with no ids. Reconciling by text must drop the
    /// delivered item and keep the rest, including when the same words were typed twice.
    #[test]
    fn reconciling_against_pis_string_queue_keeps_ids_and_counts_duplicates_honestly() {
        let q = Queue::new();
        let a = q.push(7, MidTurn::Steer, "again", vec![], 1);
        let b = q.push(7, MidTurn::Steer, "again", vec![], 2);
        let c = q.push(7, MidTurn::FollowUp, "later", vec![], 3);
        // pi says one "again" is still queued and the follow-up is gone: the *earlier* "again" is
        // the one pi delivered (it removes from the front), so the survivor is the later one.
        let dropped = q.reconcile(&["again".into()]);
        assert_eq!(dropped.iter().map(|v| v.id).collect::<Vec<_>>(), vec![a.id, c.id]);
        assert_eq!(q.snapshot().iter().map(|v| v.id).collect::<Vec<_>>(), vec![b.id]);
        // pi says nothing is queued: everything goes, and the next snapshot is empty rather than
        // holding a line for a message the model already read.
        assert_eq!(q.reconcile(&[]).len(), 1);
        assert!(q.snapshot().is_empty());
    }

    /// The refusals name the alternative. "Nothing is running" alone leaves the reader with words in
    /// the box and no idea what to do with them.
    #[test]
    fn the_refusals_say_what_to_do_instead() {
        assert!(idle_refusal().contains("send it as a message"));
        assert!(wrong_session_refusal(4, 9).contains("conversation 4"));
        assert!(wrong_session_refusal(4, 9).contains("on 9"));
        assert!(wrong_session_refusal(4, 9).contains("cannot move the host"));
        assert!(withdraw_gone_refusal().contains("Nothing was removed"));
        assert!(landed_note(3).contains("cannot be unsent"));
        assert!(card_refusal().contains("approval card"));
        assert!(card_refusal().contains("approval mode"));
        assert!(card_refusal().contains("tools this turn started with"));
    }

    /// The setting note names the host as the home of the choice, because a phone tab and the
    /// desktop press the same Enter and the honest answer is one stored value.
    #[test]
    fn the_setting_note_names_the_host_as_the_home_of_the_choice() {
        let s = setting_note(MidTurn::FollowUp);
        assert!(s.contains("asked after that turn"));
        assert!(s.contains("stored on the host"));
        assert!(setting_note(MidTurn::Steer).contains("next step"));
    }

    /// An attachment is checked where it is queued, with the engine's own limits — the same gate
    /// `Send` runs, so a mid-turn picture cannot be a bigger picture than a sent one.
    #[test]
    fn a_queued_picture_is_refused_where_a_sent_one_would_be() {
        let big = Attachment {
            mime: "image/png".into(),
            data_b64: "A".repeat(9 * 1024 * 1024),
            name: Some("huge.png".into()),
        };
        let limits = crate::host::AttachmentLimits::defaults();
        assert!(checked_attachments(&[big], limits).is_err(), "a picture over the engine's limit must not queue");
        assert!(checked_attachments(&[], limits).is_ok());
    }

    /// The view carries no bytes: a snapshot of a queue with a picture in it must not put the
    /// picture on every status line.
    #[test]
    fn a_pending_view_describes_a_picture_without_carrying_it() {
        let q = Queue::new();
        let att = Attachment {
            mime: "image/png".into(),
            data_b64: "A".repeat(4096),
            name: Some("p.png".into()),
        };
        let item = q.push(7, MidTurn::Steer, "look at this", vec![att], 11);
        let v = item.view();
        assert_eq!(v.attachments, 1);
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"attachments\":1"));
        assert!(!json.contains("AAAA"), "the bytes must not ride the snapshot: {} bytes of json", json.len());
        // The card's words come from the engine, not from whichever client paints it.
        assert_eq!(v.label, "into this turn");
        assert!(v.note.contains("not sent to the model yet"), "{v:?}");
    }

    /// The delivery mode pi is told is the one cwoffee's own queue keeps, or the two cores drain a
    /// queue differently and one gesture gets two answers.
    #[test]
    fn the_queue_drains_every_message_of_a_kind_because_pi_is_told_the_same() {
        assert_eq!(PI_QUEUE_MODE, "all");
        let q = Queue::new();
        for (n, text) in ["one", "two", "three"].iter().enumerate() {
            q.push(7, MidTurn::Steer, *text, vec![], 100 + n as i64);
        }
        assert_eq!(q.take_kind(MidTurn::Steer, 7).len(), 3, "one boundary drains the whole queue");
    }

    /// `now_secs` shares the store's clock, because a stamp that ticks in a different unit lands as
    /// a row dated 1970 or in the far future.
    #[test]
    fn the_stamp_is_on_the_stores_clock() {
        let n = now_secs();
        assert!(n > 1_700_000_000, "seconds, not milliseconds: {n}");
    }
}
