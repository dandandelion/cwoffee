//! The terminal's doors, opened through the functions the other shells already call.
//!
//! `crates/tui/src/main.rs` is a skin: rendering and keybindings, no engine logic. This module is
//! the part that *does* something, and the rule it is written to is that the terminal must not grow
//! a second implementation of anything the desktop or the phone already has:
//!
//! | the command | what runs it |
//! |---|---|
//! | `/branch` | [`cwoffee_engine::branch::apply_and_open`] — the same call the desktop command and the gateway's `branch_conversation` make |
//! | `/leaf` | [`cwoffee_engine::tree::move_leaf`] |
//! | `/tree` | [`cwoffee_engine::tree::read`] |
//! | `/files` | [`cwoffee_engine::ledger::read`] and [`cwoffee_engine::ledger::stats`] |
//! | `/undo` | [`cwoffee_engine::checkpoint::preview`] then [`cwoffee_engine::checkpoint::revert`] |
//! | `/forget` | [`cwoffee_engine::ledger::forget`] |
//!
//! So every refusal the terminal prints is the engine's sentence, and a mid-turn ask is refused here
//! exactly as it is on a phone: two implementations of a revert would drift, and the drift would be a
//! file put back in one window and not the other.
//!
//! Two things the terminal has that the other shells do not, and they are stated rather than
//! pretended:
//!
//!   * **there is no watch role in a terminal.** A TUI is one person at one keyboard driving one
//!     host. Nothing here refuses a verb because the caller is only watching — so `/help` says that
//!     out loud instead of letting a reader assume the phone's rules apply.
//!   * **there is no pointer.** Confirmations are keyed, and the escalation the desktop card puts on
//!     a button — *Revert anyway* when a file has been edited since the turn — is a line that names
//!     the key.

use cwoffee_engine::branch::{apply_and_open, BranchReq};
use cwoffee_engine::checkpoint::{preview, revert, RevertPreview, RevertReq};
use cwoffee_engine::host::HostOps;
use cwoffee_engine::ledger::{forget, read as read_ledger, stats, ForgetOutcome, Ledger, SnapshotStats};
use cwoffee_engine::store::StoreOps;
use cwoffee_engine::tree::{move_leaf, read as read_tree, LeafReq, Tree};

/// What a command produced: lines to print in the transcript, and — for the two verbs that
/// overwrite something — a confirmation that owns the keyboard until it is answered.
pub struct Outcome {
    pub lines: Vec<String>,
    pub pending: Option<Pending>,
    /// Whether the command changed which conversation the host is answering, so the shell can
    /// re-read its session id instead of guessing from what it printed.
    pub moved: bool,
    /// Words a command put back in the composer, and nothing more: `/branch` hands the row's text
    /// back rather than sending it, because what to do with that message is the whole question the
    /// branch is asking. The shell assigns it; it never sends it itself.
    pub prefill: Option<String>,
    /// The nodes a `/tree` listing printed, kept so `/leaf N` answers with the words that were
    /// actually shown. Re-reading the record for the same listing would make the drift guard a
    /// formality — the proof has to come from what the terminal painted.
    pub nodes: Vec<cwoffee_engine::tree::TreeNode>,
}

impl Outcome {
    pub fn lines(lines: Vec<String>) -> Self {
        Self { lines, pending: None, moved: false, nodes: Vec::new(), prefill: None }
    }

    pub fn refused(why: String) -> Self {
        Self {
            lines: vec![format!("refused: {why}")],
            pending: None,
            moved: false,
            nodes: Vec::new(),
            prefill: None,
        }
    }

}

/// A decision the terminal has asked for and is waiting on. `y` does it, `n` or Esc walks away.
pub enum Pending {
    Undo { preview: RevertPreview },
    Forget { shown: usize },
}

impl Pending {
    /// The prompt the terminal draws above the key list. An escalation says so in its own words:
    /// "Revert" and "Revert anyway" are different decisions and one label would hide the difference.
    pub fn prompt(&self) -> String {
        match self {
            Pending::Undo { preview } => {
                if preview.edited_since > 0 {
                    format!(
                        "Revert anyway — {} file(s) were edited since that turn?  [y] overwrite them · [n] leave them",
                        preview.edited_since
                    )
                } else {
                    "Revert that turn's files?  [y] put them back · [n] leave them".to_string()
                }
            }
            Pending::Forget { shown } => format!(
                "Forget {shown} cop{} from the store? The conversation's own files are left alone.  [y] delete · [n] keep",
                if *shown == 1 { "y" } else { "ies" }
            ),
        }
    }
}

/// `/branch [N]` — a second conversation cut out of this one, and the terminal moves onto it.
///
/// `shown` is the text the terminal painted for that row, not what it just read out of the record:
/// the drift guard only means something if the client answers from what it showed the person.
pub async fn branch(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    session: i64,
    from_end: usize,
    shown: Option<String>,
) -> Outcome {
    let Some(text) = shown else {
        return Outcome::refused(
            "this terminal has no copy of that message to repeat back, so nothing was branched — a \
             branch addressed by position alone is a branch addressed by whatever the row id happened \
             to be".to_string(),
        );
    };
    let req = BranchReq {
        session,
        message_id: None,
        from_end: Some(from_end),
        role: "user".to_string(),
        shown: text,
    };
    match apply_and_open(store, host, &req).await {
        Ok(out) => Outcome {
            moved: true,
            pending: None,
            nodes: Vec::new(),
            prefill: out.prefill.clone(),
            lines: vec![
                format!(
                    "conversation {session} branched: {} row(s) copied, and this terminal is on {} \"
                     ({}) now",
                    out.copied, out.session, out.title
                ),
                format!("  {}", out.note),
                match &out.prefill {
                    Some(t) => format!(
                        "  the composer has the words back: press Enter to ask them of the branch, \
                         or edit them first ({})",
                        clip(t, 70)
                    ),
                    None => "  the branch starts from an assistant row, so the composer stays empty"
                        .to_string(),
                },
            ],
        },
        Err(why) => Outcome::refused(why),
    }
}

/// `/tree` — what this conversation is reading, and what it holds that it is not reading.
pub async fn tree(store: &dyn StoreOps, host: &dyn HostOps, session: i64) -> Outcome {
    match read_tree(store, host, session).await {
        Ok(t) => {
            let mut o = Outcome::lines(tree_lines(&t));
            o.nodes = t.nodes.clone();
            o
        }
        Err(why) => Outcome::refused(why),
    }
}

/// Which node `N` names in the listing the terminal printed. 1-based, because the terminal numbers
/// the lines it shows — and a number that names nothing is refused rather than rounded, so `/leaf 3`
/// on a two-node tree cannot move the leaf to whichever row happened to sit at index 1.
pub fn pick_node<'a>(
    nodes: &'a [cwoffee_engine::tree::TreeNode],
    nth: usize,
) -> Result<&'a cwoffee_engine::tree::TreeNode, String> {
    if nth == 0 {
        // 0 is not a line the terminal drew. Saturating the subtraction would quietly turn
        // `/leaf 0` into the first node, which is a leaf moved to a row nobody pointed at.
        return Err(format!(
            "there is no node 0 in the tree this terminal printed ({} listed); the lines start at 1",
            nodes.len()
        ));
    }
    nodes.get(nth - 1).ok_or_else(|| {
        format!(
            "there is no node {nth} in the tree this terminal printed ({} listed)",
            nodes.len()
        )
    })
}

/// `/leaf N` — stand the leaf at the N-th node of the list this terminal just printed.
///
/// The node is addressed by the node's own words (`shown`), which is the same proof the desktop's
/// tree panel sends. Mid-turn it refuses, and the refusal is `crate::tree`'s sentence.
pub async fn leaf(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    session: i64,
    nodes: &[cwoffee_engine::tree::TreeNode],
    nth: usize,
) -> Outcome {
    let node = match pick_node(nodes, nth) {
        Ok(n) => n,
        Err(why) => return Outcome::refused(why),
    };
    let Some(row) = node.row else {
        return Outcome::refused(
            "that node is pi's own entry, not a cwoffee row, so the leaf cannot stand there — the \
             record and pi's thread agree to disagree and both say which one they are on"
                .to_string(),
        );
    };
    let req = LeafReq {
        session,
        message_id: Some(row),
        from_end: None,
        role: node.role.clone(),
        shown: node.content.clone(),
    };
    match move_leaf(store, host, &req).await {
        Ok(out) => {
            let mut o = Outcome::lines(vec![out.note.clone()]);
            o.moved = true;
            if let Some(ask) = &out.prefill {
                o.lines.push(format!(
                    "  the words back in the input: {} - sending them joins that branch",
                    clip(ask, 70)
                ));
            }
            o
        }
        Err(why) => Outcome::refused(why),
    }
}

/// `/files` — the conversation's whole file story, plus what its copies cost.
///
/// An empty ledger refuses with the same sentence the desktop card prints on its control
/// ("Nothing to forget"), because an empty list that reads as a bug in the list is the complaint
/// that made the wording a claim instead of a blank.
pub fn files(store: &dyn StoreOps, session: i64) -> Outcome {
    let led: Ledger = match read_ledger(store, session) {
        Ok(l) => l,
        Err(why) => return Outcome::refused(why),
    };
    let st = match stats(store, session) {
        Ok(s) => s,
        Err(why) => return Outcome::refused(why),
    };
    if led.turns.is_empty() && led.turns_total == 0 {
        return Outcome::lines(vec![
            // The desktop's card says `Nothing to forget` on its control when the ledger is empty,
            // and the terminal refuses in those words too: an empty list that reads as a broken
            // list is the complaint that made the wording a claim rather than a blank.
            "Nothing to forget: this conversation has written no files this app copied".to_string(),
            led.note,
        ]);
    }
    let mut lines = vec![format!(
        "files of conversation {session} — {} turn(s), {} path(s), {} cop{} kept",
        led.turns_total,
        led.paths_total,
        led.copies,
        if led.copies == 1 { "y" } else { "ies" }
    )];
    for t in led.turns.iter().rev() {
        let target = match t.message_id {
            Some(id) => format!("row {id}"),
            None => "no answer row yet".to_string(),
        };
        lines.push(format!(
            "  turn {} ({}, {} file(s), {} revertable{})",
            t.turn,
            target,
            t.files,
            t.revertable,
            if t.latest { ", newest" } else { "" }
        ));
        for f in &t.file_rows {
            lines.push(format!(
                "    {} · {} · copy {}{}{}",
                f.path,
                f.kind,
                if f.copy_present { format!("{}B", f.bytes) } else { "gone".to_string() },
                if f.edited_since { ", edited since" } else { "" },
                if f.path_missing { ", path gone" } else { "" }
            ));
        }
        if !t.note.is_empty() {
            lines.push(format!("    {}", t.note));
        }
    }
    if led.turns_hidden > 0 {
        lines.push(format!("  {} older turn(s) not listed", led.turns_hidden));
    }
    for n in &led.prune_notes {
        lines.push(format!("  {n}"));
    }
    if led.copies_missing > 0 {
        lines.push(format!(
            "  {} cop{} deleted outside this app; those rows offer no diff and no revert",
            led.copies_missing,
            if led.copies_missing == 1 { "y is" } else { "ies are" }
        ));
    }
    lines.push(stats_line(&st));
    if !led.note.is_empty() {
        lines.push(format!("  {}", led.note));
    }
    Outcome::lines(lines)
}

/// `/undo [N]` — the newest turn that can be put back, listed before anything moves.
pub fn undo_preview(store: &dyn StoreOps, session: i64) -> Outcome {
    let led = match read_ledger(store, session) {
        Ok(l) => l,
        Err(why) => return Outcome::refused(why),
    };
    let Some(turn) = led.turns.iter().rev().find(|t| t.revertable > 0 && t.message_id.is_some()) else {
        return Outcome::refused(
            "that conversation touched no files this app kept a copy of, so there is nothing to revert"
                .to_string(),
        );
    };
    let mid = turn.message_id.unwrap_or(0);
    let shown: Vec<String> = turn.file_rows.iter().map(|f| f.path.clone()).collect();
    let req = RevertReq {
        session,
        message_id: mid,
        through_latest: false,
        shown,
        overwrite_edited: false,
    };
    match preview(store, &req) {
        Ok(p) => {
            let mut lines = vec![format!(
                "turn {} of conversation {session} — {} path(s){}",
                p.turn,
                p.items.len(),
                if p.edited_since > 0 { format!(", {} edited since", p.edited_since) } else { String::new() }
            )];
            for i in &p.items {
                lines.push(format!(
                    "  {} · {}{}{}",
                    i.path,
                    i.action,
                    if i.edited_since { ", edited since" } else { "" },
                    if i.copy_missing { ", copy gone" } else { "" }
                ));
            }
            if p.missing_copy > 0 {
                lines.push(format!("  {} path(s) have no copy left to restore", p.missing_copy));
            }
            if p.pruned > 0 {
                lines.push(format!(
                    "  the copy limit discarded {} cop{} of this conversation — this is what is left, \
                     not everything it changed",
                    p.pruned,
                    if p.pruned == 1 { "y" } else { "ies" }
                ));
            }
            if !p.note.is_empty() {
                lines.push(format!("  {}", p.note));
            }
            lines.push("  the messages stay: this puts files back and deletes nothing".to_string());
            Outcome {
                lines,
                pending: Some(Pending::Undo { preview: p }),
                moved: false,
                nodes: Vec::new(),
                prefill: None,
            }
        }
        Err(why) => Outcome::refused(why),
    }
}

/// The answer to the undo prompt. `yes` is the escalation when files moved afterwards.
pub fn undo_confirm(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    p: &RevertPreview,
    yes: bool,
) -> Outcome {
    if !yes {
        return Outcome::lines(vec![
            "nothing was reverted: the files hold what they hold now".to_string(),
        ]);
    }
    let shown: Vec<String> = p.items.iter().map(|i| i.path.clone()).collect();
    let req = RevertReq {
        session: p.session,
        message_id: p.message_id,
        through_latest: false,
        shown,
        overwrite_edited: p.edited_since > 0,
    };
    match revert(store, host, &req) {
        Ok(out) => {
            let mut lines = vec![format!(
                "reverted row {} of conversation {}: {} file(s) put back, {} removed, {} left alone — \
                 {} message row(s) still in the record, untouched",
                out.message_id,
                out.session,
                out.report.restored,
                out.report.removed,
                out.report.skipped_edited,
                out.messages
            )];
            for t in &out.report.touched {
                lines.push(format!("  {t}"));
            }
            for f in &out.report.failed {
                lines.push(format!("  could not touch: {f}"));
            }
            if !out.note.is_empty() {
                lines.push(format!("  {}", out.note));
            }
            Outcome::lines(lines)
        }
        Err(why) => Outcome::refused(why),
    }
}

/// `/forget` — the first press lists what is there and arms the second.
pub fn forget_arm(store: &dyn StoreOps, session: i64) -> Outcome {
    let st = match stats(store, session) {
        Ok(s) => s,
        Err(why) => return Outcome::refused(why),
    };
    if st.rows == 0 {
        return Outcome::lines(vec![
            "Nothing to forget: this conversation keeps no copies".to_string(),
            stats_line(&st),
        ]);
    }
    Outcome {
        nodes: Vec::new(),
        prefill: None,
        lines: vec![
            format!(
                "press again to delete {} cop{} - {}",
                st.rows,
                if st.rows == 1 { "y" } else { "ies" },
                clip(&st.note, 120)
            ),
            stats_line(&st),
            "  this deletes the stored copies, not the conversation's files or its messages".to_string(),
        ],
        pending: Some(Pending::Forget { shown: st.rows }),
        moved: false,
    }
}

/// The second press. `shown` is the count the terminal just listed, which the engine checks against
/// the record: a stale listing must not delete what a later turn copied.
pub fn forget_confirm(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    session: i64,
    shown: usize,
) -> Outcome {
    match forget(store, host, session, shown) {
        Ok(out) => Outcome::lines(forget_lines(&out)),
        Err(why) => Outcome::refused(why),
    }
}

/// What the terminal prints when a confirmation is refused. Both sentences say that nothing
/// happened, because a denial that leaves a reader guessing whether the files moved is a denial
/// that gets answered with `y` next time.
pub fn cancel_pending(p: &Pending) -> Outcome {
    match p {
        Pending::Undo { .. } => Outcome::lines(vec![
            "nothing was reverted: the files hold what they hold now".to_string(),
        ]),
        Pending::Forget { .. } => Outcome::lines(vec![
            "nothing was forgotten: the copies stay in the store".to_string(),
        ]),
    }
}

fn tree_lines(t: &Tree) -> Vec<String> {
    let mut lines = vec![format!(
        "tree of conversation {} — {} node(s), {} off the branch the next answer reads{}",
        t.session,
        t.nodes.len(),
        t.off_branch,
        match t.leaf {
            Some(id) => format!(", leaf at row {id}"),
            None => ", leaf at its newest message".to_string(),
        }
    )];
    for (i, n) in t.nodes.iter().enumerate() {
        let mut marks = String::new();
        if n.current {
            marks.push_str(" ◀ the next answer reads through here");
        }
        if n.pi_current {
            marks.push_str(" ◀ pi's thread is here");
        }
        if n.drift {
            marks.push_str(" ◀ one record has this node and the other does not");
        }
        lines.push(format!(
            "  {}) {}{}{}",
            i + 1,
            "  ".repeat(n.depth),
            n.role,
            clip(&n.quote, 60)
        ));
        if !marks.is_empty() {
            lines.push(format!("      {marks}"));
        }
        if !n.on_branch {
            lines.push(format!(
                "      off the branch being read: still in the record, not in the next answer's context"
            ));
        }
    }
    if t.nodes_hidden > 0 {
        lines.push(format!("  {} older node(s) not listed", t.nodes_hidden));
    }
    for d in &t.drift {
        lines.push(format!("  {d}"));
    }
    if !t.pi.consulted {
        lines.push(format!("  {}", t.pi.why));
    }
    lines.push(format!("  {}", t.note));
    lines.push("  /leaf N stands the leaf at node N — nothing is copied and nothing is deleted".to_string());
    lines
}

fn forget_lines(out: &ForgetOutcome) -> Vec<String> {
    let mut lines = vec![format!(
        "forgot {} cop{} of conversation {}, freeing {} bytes{}",
        out.dropped,
        if out.dropped == 1 { "y" } else { "ies" },
        out.session,
        out.freed_bytes,
        if out.kept > 0 { format!(", {} kept because another conversation shares them", out.kept) } else { String::new() }
    )];
    if !out.note.is_empty() {
        lines.push(format!("  {}", out.note));
    }
    lines
}

fn stats_line(st: &SnapshotStats) -> String {
    format!(
        "  store: {} index row(s) = {}B counted, {} blob(s) = {}B on disk{}, {}",
        st.rows,
        st.row_bytes,
        st.blobs,
        st.blob_bytes,
        if st.blobs < st.rows { ", so rows sharing a blob are counted once here" } else { "" },
        if st.orphan_blobs > 0 {
            format!("{} orphan blob(s) no conversation points at", st.orphan_blobs)
        } else {
            "no orphan blobs".to_string()
        }
    )
}

/// A wall-clock reading as `HH:MM:SS`, for the landing receipt. The stamp is the press time the
/// engine carried (`typed_at`), not a clock this terminal reads at paint time.
pub fn clock(secs: i64) -> String {
    let total = secs.max(0) % 86400;
    format!("{:02}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
}

fn clip(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        format!("{}…", c[..n].iter().collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwoffee_engine::checkpoint::{Kind, PlanItem};

    fn preview(edited: usize) -> RevertPreview {
        RevertPreview {
            session: 7,
            message_id: 3,
            turn: 2,
            scope: "that turn only".to_string(),
            workspace: "/tmp/cwoffee-tui-test".to_string(),
            items: vec![PlanItem {
                path: "notes.md".to_string(),
                kind: Kind::Modified,
                action: "put back the copy this turn took".to_string(),
                bytes: 12,
                edited_since: edited > 0,
                copy_missing: false,
                path_missing: false,
            }],
            edited_since: edited,
            missing_copy: 0,
            pruned: 0,
            note: String::new(),
        }
    }

    /// "Revert" and "Revert anyway" are different decisions. One prompt wording would hide the
    /// difference, and the escalation exists precisely because a file edited afterwards is the case
    /// where a person means something else.
    #[test]
    fn an_escalation_is_said_as_an_escalation() {
        let plain = Pending::Undo { preview: preview(0) }.prompt();
        let escalated = Pending::Undo { preview: preview(2) }.prompt();
        assert!(plain.starts_with("Revert that turn's files?"), "{plain}");
        assert!(!plain.contains("anyway"), "{plain}");
        assert!(escalated.contains("Revert anyway"), "{escalated}");
        assert!(escalated.contains("2 file(s) were edited since"), "{escalated}");
    }

    /// Two presses, and the second names the count it is deleting. `ledger::forget` refuses a stale
    /// listing, so the number the terminal printed is the number it must send back.
    #[test]
    fn a_forget_confirmation_names_the_count_it_listed() {
        assert!(Pending::Forget { shown: 1 }.prompt().contains("Forget 1 copy"));
        assert!(Pending::Forget { shown: 4 }.prompt().contains("Forget 4 copies"));
        assert!(Pending::Forget { shown: 4 }.prompt().contains("conversation's own files are left alone"));
    }

    /// A refusal is a sentence about what did not happen. A reader who cannot tell whether files
    /// moved answers the next prompt with `y`.
    #[test]
    fn both_denials_say_that_nothing_happened() {
        assert!(cancel_pending(&Pending::Undo { preview: preview(0) }).lines[0].contains("nothing was reverted"));
        assert!(cancel_pending(&Pending::Forget { shown: 3 }).lines[0].contains("nothing was forgotten"));
    }

    /// The desktop's control reads `Nothing to forget` on an empty ledger; the terminal refuses in
    /// those words so two shells do not describe an empty list differently.
    #[test]
    fn an_empty_ledger_refuses_in_the_window_s_words() {
        let store = cwoffee_engine::store::Store::open(":memory:").expect("an in-memory record");
        let session = store.create_session("empty", "/tmp", "cwoffee-mock").expect("a conversation");
        let out = files(&store, session);
        assert!(out.lines[0].starts_with("Nothing to forget"), "{}", out.lines[0]);
        assert!(out.pending.is_none());
    }

    /// `/leaf` addresses a node by the words the terminal listed, so asking for a node that was
    /// never printed has to refuse rather than move the leaf to whatever index was meant.
    #[test]
    fn a_node_the_terminal_never_listed_is_refused() {
        let err = pick_node(&[], 3).expect_err("an empty listing names no node 3");
        assert!(err.contains("no node 3"), "{err}");
        assert!(err.contains("0 listed"), "{err}");
        assert!(err.contains("this terminal printed"), "{err}");
        let one = cwoffee_engine::tree::TreeNode {
            row: Some(9),
            pi_entry: None,
            role: "user".to_string(),
            quote: "hi".to_string(),
            content: "hi there".to_string(),
            on_branch: true,
            current: true,
            pi_current: false,
            depth: 0,
            drift: false,
        };
        assert_eq!(pick_node(&[one.clone()], 1).unwrap().row, Some(9));
        assert!(pick_node(&[one.clone()], 2).is_err());
        assert!(pick_node(&[one.clone()], 0).is_err(), "0 is not a line the terminal drew");
    }

    /// The landing stamp is the press time the engine carried. Reading a clock at paint time would
    /// say the model got the message when the frame happened to be drawn.
    #[test]
    fn the_stamp_is_a_wall_clock_reading_of_the_press() {
        assert_eq!(clock(3661), "01:01:01");
        assert_eq!(clock(86400 + 60), "00:01:00");
        assert_eq!(clock(-5), "00:00:00");
    }
}
