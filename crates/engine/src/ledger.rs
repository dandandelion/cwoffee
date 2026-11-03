//! The whole file story of one conversation: which turn touched which path, what it
//! did to it, and what can still be put back.
//!
//! Checkpoints were built for undo. A turn's revert card reads the rows for that one
//! answer, so the only file history a person can see is the one they are about to
//! reverse — and "what did this conversation do to my folder?" has no answer at all.
//! The ledger answers it, and nothing here is new machinery: it reads the same index
//! rows, the same content-addressed store, and the same `unified()` the tool card
//! paints. Reading is what was missing.
//!
//! Three things the shape is built around:
//!
//! * **A copy that is gone is said, not hidden.** The per-conversation cap deletes rows,
//!   and the snapshot folder can be cleared outside the app. A ledger row whose bytes
//!   are gone offers neither a diff nor a revert, and says which of the two reasons it
//!   is empty — a control that would do nothing is worse than none, which is the rule
//!   the revert card already follows.
//! * **Dedupe is visible.** Copies are content-addressed, so two turns that wrote the
//!   same bytes share one blob, and two conversations that overwrote the same file share
//!   it too. A byte count that added every row's `bytes` would be a number nobody can
//!   reconcile with `du`, so `snapshot_stats` gives both: what the index says, and what
//!   the store actually holds, counted once per blob.
//! * **Orphan bytes are their own line.** Dropping a conversation's copies has to leave
//!   blobs another conversation still points at. The gap between "files in the folder"
//!   and "files an index row points at" is orphan bytes, and the stats name it rather
//!   than letting it read as used space.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::checkpoint::{self, Kind};
use crate::host::HostOps;
use crate::store::{CheckpointRow, StoreOps};
use crate::tools::changes::line_change;
use crate::tools::path::confine;

/// Turns drawn on one ledger read. A conversation that wrote a file on eighty turns gets
/// the newest sixty and a line saying what was left off — the same discipline as the tree
/// card's node cap, because a silent truncation reads like a complete history.
pub const LEDGER_TURNS: usize = 60;
/// Paths in the roll-up. Generous enough that a conversation's whole folder fits.
pub const LEDGER_PATHS: usize = 200;
/// A live file bigger than this is reported by size, not diffed. Reading a whole tree of
/// 200 MB files into one IPC reply is not what a card needs.
pub const LIVE_DIFF_LIMIT: u64 = 4 * 1024 * 1024;

/// One file, as one turn left it.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerFile {
    /// The index row. A diff is addressed at this, so a stale window cannot ask for a
    /// copy by path and get whatever the newest row for that path happens to be.
    pub checkpoint: i64,
    pub path: String,
    /// `created` | `modified` | `deleted` — the same words the row stores and the card
    /// prints, because they are the file's story rather than a diff's arithmetic.
    pub kind: String,
    /// Bytes of the copy taken before the call. Zero for a file the turn created: there
    /// was nothing to copy, and reverting that row deletes the file rather than restoring
    /// one.
    pub bytes: i64,
    /// The copy is on disk. False for a created file (by design) and for a row whose blob
    /// was evicted or deleted out of band.
    pub copy_present: bool,
    /// A revert of this row would do something: the turn has an answer row to address it
    /// at, and there is a copy — or a file — to put back.
    pub can_revert: bool,
    /// The file on disk is no longer what this turn left behind.
    pub edited_since: bool,
    /// The path itself is gone.
    pub path_missing: bool,
    /// The file is in the folder now, and how big it is. The card needs this to say which side of a
    /// comparison exists: a created file with a live copy has one side, a deleted file with no copy
    /// has neither, and those are different lines to read.
    pub live_exists: bool,
    pub live_bytes: u64,
    /// One line the row draws verbatim: the direction a revert would take, or the reason
    /// there is none.
    pub says: String,
}

/// One turn's line, the revert card's line widened to the conversation.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerTurn {
    pub turn: i64,
    /// The answer row this turn's copies belong to. `None` while the turn has no row yet,
    /// which is why a running turn's control waits rather than guessing.
    pub message_id: Option<i64>,
    pub files: usize,
    /// The files this turn touched, in path order. The card draws these; `files` is the same count,
    /// so a line that reads "3 files" and lists two is a bug the count catches.
    pub file_rows: Vec<LedgerFile>,
    pub paths: Vec<String>,
    /// How many of this turn's rows can actually be reverted.
    pub revertable: usize,
    /// True when this turn is the newest one that can be undone — what the turn card's
    /// `Files` control leads with.
    pub latest: bool,
    pub note: String,
}

/// One path across the whole conversation.
#[derive(Debug, Clone, Serialize)]
pub struct LedgerPath {
    pub path: String,
    /// Turns that touched it, oldest first.
    pub turns: Vec<i64>,
    /// The kinds in order, so `created, modified, modified` is readable as the file's life.
    pub kinds: String,
    pub copies: usize,
    pub live_exists: bool,
    pub live_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ledger {
    pub session: i64,
    pub workspace: String,
    /// Newest turn first: the card is about what happened recently.
    pub turns: Vec<LedgerTurn>,
    pub paths: Vec<LedgerPath>,
    /// Turns this conversation wrote files in, before the cap.
    pub turns_total: usize,
    pub turns_hidden: usize,
    /// Files drawn.
    pub files_total: usize,
    /// Distinct paths.
    pub paths_total: usize,
    /// Paths left off the roll-up because it is capped. They are still counted in
    /// `paths_total`; the card just does not draw them.
    pub paths_hidden: usize,
    pub copies: usize,
    /// Copies the cap discarded, summed over every eviction notice this conversation has.
    pub pruned: usize,
    /// The notices themselves, oldest first — the transcript says *why* a turn offers
    /// nothing rather than offering nothing silently.
    pub prune_notes: Vec<String>,
    /// Rows whose blob is gone even though the cap never evicted them: the folder was
    /// cleared outside the app.
    pub copies_missing: usize,
    /// Rows with no answer row yet.
    pub unstamped: usize,
    pub edited_since: usize,
    pub note: String,
}

/// What the snapshot store of one conversation costs, and what is left in it.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotStats {
    pub session: i64,
    pub workspace: String,
    /// `<workspace>/.cws/snapshots`, where the copies live.
    pub dir: String,
    pub dir_exists: bool,
    /// Index rows for this conversation.
    pub rows: usize,
    /// Sum of their `bytes`. This double-counts shared blobs, and says so.
    pub row_bytes: u64,
    /// Distinct blobs this conversation points at.
    pub blobs: usize,
    /// Bytes of those blobs on disk — deduped, so this is the number `du` agrees with.
    pub blob_bytes: u64,
    /// Blobs another conversation also points at. Forgetting this conversation leaves them.
    pub shared: usize,
    /// Files in the store that no index row in any conversation points at.
    pub orphan_blobs: usize,
    pub orphan_bytes: u64,
    /// Other conversations sharing this folder's store.
    pub neighbours: usize,
    pub note: String,
}

/// What a stored copy and the live file say about each other.
#[derive(Debug, Clone, Serialize)]
pub struct FileDiff {
    pub session: i64,
    pub checkpoint: i64,
    pub turn: i64,
    pub message_id: Option<i64>,
    pub path: String,
    pub kind: String,
    pub stored_bytes: i64,
    /// The copy is on disk. False means there is no "before" side to show.
    pub stored_present: bool,
    pub live_exists: bool,
    pub live_bytes: u64,
    /// Counts against the stored copy, not against what the model thought was there.
    pub added: usize,
    pub removed: usize,
    /// The diff, or `None` when there was nothing to compare or one side was unreadable.
    pub diff: Option<String>,
    /// One side is too big to diff. The counts still stand; only the text is dropped.
    pub too_big: bool,
    /// One side is not text. Sizes and hashes still get reported.
    pub binary: bool,
    pub edited_since: bool,
    pub can_revert: bool,
    pub note: String,
}

/// What `forget_copies` gave up.
#[derive(Debug, Clone, Serialize)]
pub struct ForgetOutcome {
    pub session: i64,
    pub dropped: usize,
    pub freed_bytes: u64,
    /// Blobs left in place because another conversation points at them.
    pub kept: usize,
    pub note: String,
}

/// Where one row's copy and one row's file currently are.
struct Live {
    /// The path the tools would write to, resolved through `confine()`.
    resolved: Option<PathBuf>,
    exists: bool,
    bytes: u64,
    hash: Option<String>,
}

fn live_of(workspace: &Path, rel: &str) -> Live {
    // The same gate a write goes through. The row came from a tool that had already
    // confined itself, but the store is a record and records can be edited by anything
    // that opens the database; a ledger that resolved `../../.ssh/id_ed25519` happily
    // would be reading a file no tool could ever have touched.
    let resolved = confine(workspace, rel).ok();
    let exists = resolved.as_ref().map(|p| p.is_file()).unwrap_or(false);
    let bytes = resolved
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let hash = resolved
        .as_ref()
        .and_then(|p| checkpoint::read_bytes(p))
        .map(|b| checkpoint::hash_of(&b));
    Live {
        resolved,
        exists,
        bytes,
        hash,
    }
}

fn copy_present(dir: &Path, row: &CheckpointRow) -> bool {
    // A created file has no copy by design — reverting it deletes the file — so its row reads
    // `false` rather than "present". Saying a copy exists when there never was one is how a card
    // ends up offering to restore bytes nobody saved.
    !row.hash.is_empty() && checkpoint::blob_path(dir, &row.hash).is_file()
}

fn says(row: &CheckpointRow, copy_present: bool, message_id: Option<i64>) -> String {
    if message_id.is_none() {
        return "that turn has no answer row yet, so nothing can be addressed at it".to_string();
    }
    if row.kind != Kind::Created && !copy_present {
        return "no copy was kept, so there is nothing to put back".to_string();
    }
    row.kind.direction().to_string()
}

fn can_revert(row: &CheckpointRow, copy_present: bool) -> bool {
    row.message_id.is_some() && (row.kind == Kind::Created || copy_present)
}

/// The ledger for one conversation. Read-only: nothing here touches the folder.
pub fn read(store: &dyn StoreOps, session: i64) -> Result<Ledger, String> {
    let workspace = store
        .session_workspace(session)
        .map_err(|e| format!("conversation {session} is not in the record: {e}"))?;
    if workspace.trim().is_empty() {
        return Err(format!(
            "conversation {session} has no folder, so it has never written a file this app could copy"
        ));
    }
    let ws = Path::new(&workspace);
    let dir = checkpoint::snapshot_dir(ws);
    let rows = store
        .checkpoints_for_session(session)
        .map_err(|e| format!("the file record of conversation {session} could not be read: {e}"))?;
    let prunes = store
        .checkpoint_prunes(session)
        .map_err(|e| format!("the prune notices of conversation {session} could not be read: {e}"))?;
    // The limit's own count, from the same overview the revert card reads. The notices below are
    // every reason copies went — including a person asking for them — and a card that added those
    // together would tell somebody they had hit a limit they never hit.
    let pruned = store
        .checkpoint_overview(session)
        .map_err(|e| format!("the file overview of conversation {session} could not be read: {e}"))?
        .pruned;

    // Grouped by turn, and inside a turn by path order, because the card reads as a list
    // of files rather than a list of database rows.
    let mut by_turn: BTreeMap<i64, Vec<&CheckpointRow>> = BTreeMap::new();
    for r in rows.iter() {
        by_turn.entry(r.turn).or_default().push(r);
    }

    let mut turns: Vec<LedgerTurn> = Vec::new();
    let mut paths: BTreeMap<String, LedgerPath> = BTreeMap::new();
    let mut files_total = 0usize;
    let mut copies = 0usize;
    let mut copies_missing = 0usize;
    let mut unstamped = 0usize;
    let mut edited_total = 0usize;

    for (turn, mut group) in by_turn {
        group.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        let latest_message = group.iter().filter_map(|r| r.message_id).max();
        let mut files: Vec<LedgerFile> = Vec::new();
        let mut revertable = 0usize;
        for row in &group {
            let live = live_of(ws, &row.path);
            let present = copy_present(&dir, row);
            let edited = checkpoint::edited_since(row.kind, &row.after_hash, live.hash.as_deref());
            if row.kind != Kind::Created && !present {
                copies_missing += 1;
            }
            if row.message_id.is_none() {
                unstamped += 1;
            }
            if edited {
                edited_total += 1;
            }
            if can_revert(row, present) {
                revertable += 1;
            }
            if present && row.kind != Kind::Created {
                copies += 1;
            }
            files_total += 1;
            files.push(LedgerFile {
                checkpoint: row.id,
                path: row.path.clone(),
                kind: row.kind.as_str().to_string(),
                bytes: row.bytes,
                copy_present: present,
                can_revert: can_revert(row, present),
                edited_since: edited,
                path_missing: !live.exists,
                live_exists: live.exists,
                live_bytes: live.bytes,
                says: says(row, present, row.message_id),
            });
            let entry = paths.entry(row.path.clone()).or_insert_with(|| LedgerPath {
                path: row.path.clone(),
                turns: Vec::new(),
                kinds: String::new(),
                copies: 0,
                live_exists: live.exists,
                live_bytes: live.bytes,
            });
            if entry.turns.last().copied() != Some(turn) {
                entry.turns.push(turn);
            }
            if !entry.kinds.is_empty() {
                entry.kinds.push_str(", ");
            }
            entry.kinds.push_str(row.kind.as_str());
            if present && row.kind != Kind::Created {
                entry.copies += 1;
            }
            // The live side is read every row of every turn: a file the person edited
            // after the last turn has to read edited on the roll-up too, and the newest
            // row is the one that knows that.
            entry.live_exists = live.exists;
            entry.live_bytes = live.bytes;
        }
        let note = if revertable == 0 {
            if group.iter().all(|r| r.message_id.is_none()) {
                format!(
                    "{} file{} from this turn, none of it stored yet — the copies are named once the answer is written down",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "{} file{} from this turn, and no copy of {} left to put back",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    if files.len() == 1 { "it is" } else { "they are" }
                )
            }
        } else {
            format!(
                "{} file{}; {} of {} can be put back{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                revertable,
                files.len(),
                if latest_message.is_none() {
                    " (that turn's answer row is not written yet)".to_string()
                } else {
                    String::new()
                }
            )
        };
        let ps: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        turns.push(LedgerTurn {
            turn,
            message_id: latest_message,
            files: files.len(),
            file_rows: files,
            paths: ps,
            revertable,
            latest: false,
            note,
        });
    }

    // Newest turn first: a person opening this is asking what the conversation did
    // recently, and a cap that hides turns has to hide the oldest ones.
    turns.sort_by(|a, b| b.turn.cmp(&a.turn));
    if let Some(t) = turns.first_mut() {
        t.latest = true;
    }
    let turns_total = turns.len();
    let turns_hidden = turns_total.saturating_sub(LEDGER_TURNS);
    if turns_hidden > 0 {
        turns.truncate(LEDGER_TURNS);
    }

    let mut path_list: Vec<LedgerPath> = paths.values().cloned().collect();
    let paths_total = path_list.len();
    let paths_hidden = paths_total.saturating_sub(LEDGER_PATHS);
    if paths_hidden > 0 {
        path_list.truncate(LEDGER_PATHS);
    }

    let mut note = if turns_total == 0 {
        "no turn of this conversation wrote a file this app kept a copy of.".to_string()
    } else {
        format!(
            "{} turn{} wrote files; {} file{} across {} path{}; {} cop{} kept.",
            turns_total,
            if turns_total == 1 { "" } else { "s" },
            files_total,
            if files_total == 1 { "" } else { "s" },
            paths_total,
            if paths_total == 1 { "" } else { "s" },
            copies,
            if copies == 1 { "y" } else { "ies" }
        )
    };
    if turns_hidden > 0 {
        note.push_str(&format!(
            " {} older turn{} not listed on this card.",
            turns_hidden,
            if turns_hidden == 1 { "" } else { "s" }
        ));
    }
    if paths_hidden > 0 {
        note.push_str(&format!(
            " {} path{} not rolled up here.",
            paths_hidden,
            if paths_hidden == 1 { "" } else { "s" }
        ));
    }
    if pruned > 0 {
        note.push_str(&format!(
            " {} cop{} discarded to stay under the limit — those turns offer no diff and no revert.",
            pruned,
            if pruned == 1 { "y was" } else { "ies were" }
        ));
    }
    if copies_missing > 0 {
        note.push_str(&format!(
            " {} cop{} listed here has no bytes on disk — the snapshot folder was emptied outside this app.",
            copies_missing,
            if copies_missing == 1 { "y" } else { "ies" }
        ));
    }
    if edited_total > 0 {
        note.push_str(&format!(
            " {} file{} now hold{} something other than what the turn left behind.",
            edited_total,
            if edited_total == 1 { "" } else { "s" },
            if edited_total == 1 { "s" } else { "" }
        ));
    }

    Ok(Ledger {
        session,
        workspace,
        turns,
        paths: path_list,
        turns_total,
        turns_hidden,
        files_total,
        paths_total,
        paths_hidden,
        copies,
        pruned,
        prune_notes: prunes.iter().map(|p| p.note.clone()).collect(),
        copies_missing,
        unstamped,
        edited_since: edited_total,
        note,
    })
}

/// The diff between one stored copy and the bytes on disk now.
///
/// `shown` is the path the card painted, and the row is named by id: the pair is what
/// stops a window that has been sitting open from diffing whatever that path holds for
/// the newest turn by now. A mismatch changes nothing and says what the row says instead.
pub fn diff_of(store: &dyn StoreOps, session: i64, checkpoint_id: i64, shown: &str) -> Result<FileDiff, String> {
    let workspace = store
        .session_workspace(session)
        .map_err(|e| format!("conversation {session} is not in the record: {e}"))?;
    if workspace.trim().is_empty() {
        return Err(format!("conversation {session} has no folder to read files in"));
    }
    let ws = Path::new(&workspace);
    let dir = checkpoint::snapshot_dir(ws);
    let rows = store
        .checkpoints_for_session(session)
        .map_err(|e| format!("the file record of conversation {session} could not be read: {e}"))?;
    let row = rows
        .iter()
        .find(|r| r.id == checkpoint_id)
        .ok_or_else(|| {
            format!(
                "that copy is not in this conversation's file record any more — it was pruned, forgotten, or \
                 belongs to another conversation. Nothing was diffed."
            )
        })?;
    if row.path != shown {
        return Err(format!(
            "the card asked for {}, and record {} says this turn touched {}. Nothing was diffed — ask again \
             to see the current list.",
            shown,
            checkpoint_id,
            row.path
        ));
    }

    let live = live_of(ws, &row.path);
    if live.resolved.is_none() {
        // The row names a path outside the conversation's folder. Nothing about it can be
        // shown: a diff would read a file no tool of this app could have written, and the
        // ledger's whole claim is that it describes what the turns did.
        return Err(format!(
            "record {} names {}, which is outside {} — nothing was diffed",
            checkpoint_id, row.path, workspace
        ));
    }
    let stored = if row.kind == Kind::Created {
        None
    } else if row.hash.is_empty() {
        None
    } else {
        checkpoint::read_bytes(&checkpoint::blob_path(&dir, &row.hash))
    };
    if stored.is_none() {
        // A diff needs two sides. This turn created the file, so there were no earlier bytes; or
        // the copy it took is gone. Printing the live file alone would read as "everything in this
        // file was added by somebody", which is a story about a copy that does not exist. What the
        // file holds now is the tool card's diff, and the row's own line says which case it is.
        if row.kind == Kind::Created {
            return Err(format!(
                "record {checkpoint_id} wrote {shown}: this turn created it, so there is no earlier copy to diff \
                 against. Nothing was diffed. Nothing was written."
            ));
        }
        return Err(format!(
            "record {checkpoint_id} is the copy of {shown} this turn took, and those bytes are no longer in the \
             store — the copy limit discarded them, or they were deleted outside this app. Nothing was diffed: \
             the file on disk by itself is not a comparison. Nothing was written."
        ));
    }
    let stored_present = stored.is_some();
    let live_bytes = live.bytes;

    let mut too_big = false;
    let mut binary = false;
    let stored_text = stored.as_ref().and_then(|b| {
        if b.iter().any(|c| *c == 0) {
            binary = true;
            return None;
        }
        match std::str::from_utf8(b) {
            Ok(s) => Some(s.to_string()),
            Err(_) => {
                binary = true;
                None
            }
        }
    });
    let live_text = if live.exists {
        if live_bytes > LIVE_DIFF_LIMIT {
            too_big = true;
            None
        } else {
            live.resolved.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| {
                if b.iter().any(|c| *c == 0) {
                    binary = true;
                    None
                } else {
                    match std::str::from_utf8(&b) {
                        Ok(s) => Some(s.to_string()),
                        Err(_) => {
                            binary = true;
                            None
                        }
                    }
                }
            })
        }
    } else {
        None
    };

    // `line_change` is what the tool card prints, so the ledger's numbers and the card's
    // come from one function. The `old` side here is the copy the app took, never what
    // the model believed was on disk.
    let change = match (&stored_text, &live_text) {
        (Some(prev), Some(now)) => Some(line_change(ws, live.resolved.as_deref().unwrap_or(ws), Some(prev), now)),
        (Some(prev), None) => Some(line_change(ws, live.resolved.as_deref().unwrap_or(ws), Some(prev), "")),
        (None, Some(now)) => Some(line_change(ws, live.resolved.as_deref().unwrap_or(ws), None, now)),
        (None, None) => None,
    };

    let edited = checkpoint::edited_since(row.kind, &row.after_hash, live.hash.as_deref());
    let present = copy_present(&dir, row);
    let mut note = match (stored_present, live.exists) {
        (true, true) => format!(
            "{} vs the bytes on disk now: {} added, {} removed.",
            row.path,
            change.as_ref().map(|c| c.added).unwrap_or(0),
            change.as_ref().map(|c| c.removed).unwrap_or(0)
        ),
        (true, false) => format!(
            "{} is gone from the folder; the copy this app took is still there, and a revert would write it back.",
            row.path
        ),
        (false, true) => {
            if row.kind == Kind::Created {
                format!(
                    "{} was written by this turn, so there is no earlier copy to compare it with; the live file \
                     holds {} bytes and a revert would delete it.",
                    row.path, live_bytes
                )
            } else {
                format!(
                    "no copy of {} was kept, so there is nothing on this app's side of the comparison; the live \
                     file holds {} bytes.",
                    row.path, live_bytes
                )
            }
        }
        (false, false) => format!(
            "{} is gone from the folder and no copy of it was kept — there is neither side to show.",
            row.path
        ),
    };
    if binary {
        note.push_str(" One side is not text, so no diff was printed; the sizes above are the comparison.");
    }
    if too_big {
        note.push_str(&format!(
            " The live file is over {} and was not read; the counts are not stated.",
            checkpoint::human_bytes(LIVE_DIFF_LIMIT)
        ));
    }
    if edited {
        note.push_str(" The file no longer matches what that turn left behind.");
    }

    Ok(FileDiff {
        session,
        checkpoint: row.id,
        turn: row.turn,
        message_id: row.message_id,
        path: row.path.clone(),
        kind: row.kind.as_str().to_string(),
        stored_bytes: row.bytes,
        stored_present,
        live_exists: live.exists,
        live_bytes,
        added: change.as_ref().map(|c| c.added).unwrap_or(0),
        removed: change.as_ref().map(|c| c.removed).unwrap_or(0),
        diff: change.as_ref().and_then(|c| c.diff.clone()),
        too_big,
        binary,
        edited_since: edited,
        can_revert: can_revert(row, present),
        note,
    })
}

/// What one conversation's copies cost, next to what the store actually holds.
pub fn stats(store: &dyn StoreOps, session: i64) -> Result<SnapshotStats, String> {
    let workspace = store
        .session_workspace(session)
        .map_err(|e| format!("conversation {session} is not in the record: {e}"))?;
    if workspace.trim().is_empty() {
        return Err(format!("conversation {session} has no folder, so it keeps no copies"));
    }
    let ws = Path::new(&workspace);
    let dir = checkpoint::snapshot_dir(ws);
    let rows = store
        .checkpoints_for_session(session)
        .map_err(|e| format!("the file record of conversation {session} could not be read: {e}"))?;

    let mut mine: BTreeSet<String> = BTreeSet::new();
    let mut row_bytes = 0u64;
    for r in &rows {
        row_bytes += r.bytes.max(0) as u64;
        if !r.hash.is_empty() {
            mine.insert(r.hash.clone());
        }
    }

    // What the whole record points at, not just this conversation: sharing is the whole
    // point of content-addressing, and "would forgetting this conversation free it?" is
    // a question about every conversation.
    let referenced: BTreeSet<String> = store
        .referenced_hashes()
        .map_err(|e| format!("the file record could not be read: {e}"))?
        .into_iter()
        .collect();

    let mut blob_bytes = 0u64;
    for h in &mine {
        if let Ok(md) = std::fs::metadata(checkpoint::blob_path(&dir, h)) {
            blob_bytes += md.len();
        }
    }
    let shared = mine
        .iter()
        .filter(|h| {
            store
                .rows_pointing_at_outside(h, session)
                .map(|n| n > 0)
                .unwrap_or(false)
        })
        .count();

    let mut orphan_blobs = 0usize;
    let mut orphan_bytes = 0u64;
    if dir.is_dir() {
        for (hash, bytes) in blobs_on_disk(&dir) {
            if !referenced.contains(&hash) {
                orphan_blobs += 1;
                orphan_bytes += bytes;
            }
        }
    }

    let neighbours = store
        .sessions_sharing_workspace(&workspace)
        .map(|v| v.iter().filter(|s| **s != session).count())
        .unwrap_or(0);

    let mut note = format!(
        "{} cop{} indexed; {} distinct cop{} on disk, {}.",
        rows.len(),
        if rows.len() == 1 { "y" } else { "ies" },
        mine.len(),
        if mine.len() == 1 { "y" } else { "ies" },
        checkpoint::human_bytes(blob_bytes)
    );
    if row_bytes != blob_bytes {
        note.push_str(&format!(
            " The index adds up to {} — shared bytes are counted once by the store and once per row by the index.",
            checkpoint::human_bytes(row_bytes)
        ));
    }
    if shared > 0 {
        note.push_str(&format!(
            " {} of them are also pointed at by other conversation{} in this folder, so forgetting this one leaves them.",
            shared,
            if neighbours == 1 { "" } else { "s" }
        ));
    }
    if orphan_blobs > 0 {
        note.push_str(&format!(
            " {} file{} in the snapshot store — {} — no index row points at: those are orphan bytes, not this \
             conversation's copies.",
            orphan_blobs,
            if orphan_blobs == 1 { "" } else { "s" },
            checkpoint::human_bytes(orphan_bytes)
        ));
    }
    if !dir.is_dir() {
        note.push_str(" This folder has no snapshot store yet.");
    }

    Ok(SnapshotStats {
        session,
        workspace,
        dir: dir.display().to_string(),
        dir_exists: dir.is_dir(),
        rows: rows.len(),
        row_bytes,
        blobs: mine.len(),
        blob_bytes,
        shared,
        orphan_blobs,
        orphan_bytes,
        neighbours,
        note,
    })
}

/// Every blob file under the two-level shard layout, with its size. Anything that is not
/// shaped like a blob is skipped rather than guessed at.
pub fn blobs_on_disk(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

fn walk(root: &Path, at: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(rd) = std::fs::read_dir(at) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.len() != 64 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        // Only count it as a blob if it sits where `blob_path` would put it. A file that
        // happens to be named like a hash somewhere else in the folder is somebody's data.
        if checkpoint::blob_path(root, name) != path {
            continue;
        }
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        out.push((name.to_string(), bytes));
    }
}

/// Drop this conversation's copies and say what that freed.
///
/// `shown` is the copy count the card printed. A mismatch changes nothing: "forget this
/// conversation's snapshots" is a request about a specific list, and a window that has
/// been sitting open is holding a list of something the conversation may no longer be.
/// Refused while a turn runs — a capture taken mid-forget would write a row pointing at
/// bytes that were just deleted.
pub fn forget(
    store: &dyn StoreOps,
    host: &dyn HostOps,
    session: i64,
    shown: usize,
) -> Result<ForgetOutcome, String> {
    if host.state().busy {
        return Err(
            "a turn is running, so no copies were forgotten: the turn is taking copies of files right now, \
             and deleting underneath it would leave rows pointing at bytes that are gone."
                .to_string(),
        );
    }
    let workspace = store
        .session_workspace(session)
        .map_err(|e| format!("conversation {session} is not in the record: {e}"))?;
    if workspace.trim().is_empty() {
        return Err(format!("conversation {session} has no folder, so there is nothing to forget"));
    }
    let rows = store
        .checkpoints_for_session(session)
        .map_err(|e| format!("the file record of conversation {session} could not be read: {e}"))?;
    if rows.len() != shown {
        return Err(format!(
            "the card said this conversation holds {} cop{}; the record says {}. Nothing was forgotten — \
             ask again to see the current list.",
            shown,
            if shown == 1 { "y" } else { "ies" },
            rows.len()
        ));
    }
    if rows.is_empty() {
        return Err("this conversation holds no copies, so there is nothing to forget".to_string());
    }

    // Reachability first, and it is not this conversation's question alone: two
    // conversations that overwrote the same bytes share one blob, and freeing it because
    // one of them asked would leave the other unable to revert. The store does that check
    // across every conversation and reports how many blobs it had to leave behind.
    let (dropped, freed, left) = store
        .forget_checkpoints(session, &workspace)
        .map_err(|e| format!("the copies could not be forgotten: {e}"))?;

    let mut note = format!(
        "{} cop{} forgotten from this conversation, {} freed.",
        dropped,
        if dropped == 1 { "y was" } else { "ies were" },
        checkpoint::human_bytes(freed)
    );
    if left > 0 {
        note.push_str(&format!(
            " {} blob{} left in the store because another conversation points at them.",
            left,
            if left == 1 { " was" } else { "s were" }
        ));
    }
    note.push_str(" The turns and their messages are untouched; the file history is what went.");
    Ok(ForgetOutcome {
        session,
        dropped,
        freed_bytes: freed,
        kept: left,
        note,
    })
}

#[cfg(test)]
mod tests {
    //! The ledger is a read over machinery that already exists, so these tests grade the read:
    //! grouping, the flags, the two byte numbers, and the refusals. Two claims are graded in the
    //! selftest instead because they need a running host — that forgetting copies refuses mid-turn,
    //! and that the ledger card's counts match what is painted under them.

    use super::*;
    use crate::checkpoint::{capture, PreState};
    use crate::host::{HostConfig, SessionHost};
    use crate::provider::{ChatRequest, DeltaSink, Provider, ProviderError};
    use crate::store::Store;
    use std::sync::Arc;

    fn ws(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cwoffee-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A provider that answers with one word. The ledger never streams through it; it is here so
    /// `forget` can ask a real host whether a turn is running.
    struct Quiet;
    #[async_trait::async_trait]
    impl Provider for Quiet {
        fn id(&self) -> &str {
            "quiet"
        }
        fn label(&self) -> &str {
            "quiet"
        }
        async fn stream(&self, _req: &ChatRequest, _sink: &DeltaSink) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn idle_host(workspace: &Path, store: Arc<Store>) -> Arc<SessionHost> {
        Arc::new(SessionHost::spawn(
            Arc::new(Quiet),
            crate::tools::Registry::new(),
            Arc::clone(&store),
            HostConfig::new("m", workspace.to_path_buf()),
        ))
    }

    /// One file call of one turn, captured and recorded the way the host records it: the copy
    /// written first, then the index row that names it.
    fn touch(store: &Store, session: i64, turn: i64, w: &Path, path: &str, before: Option<&[u8]>, after: &[u8]) {
        let f = w.join(path);
        let pre = match before {
            None => {
                let _ = std::fs::remove_file(&f);
                PreState::Absent
            }
            Some(b) => {
                std::fs::write(&f, b).unwrap();
                PreState::Present(b.to_vec())
            }
        };
        // The capture happens the way a tool call does it: the file is read as it stands, the copy
        // is taken, and only then does the write land.
        let mark = capture(Some(&checkpoint::snapshot_dir(w)), w, &f, &pre, Some(after)).unwrap();
        std::fs::write(&f, after).unwrap();
        store.record_checkpoints(session, turn, &[mark]).unwrap();
    }

    fn first_hash(store: &Store, session: i64) -> String {
        store
            .checkpoints_for_session(session)
            .unwrap()
            .first()
            .unwrap()
            .hash
            .clone()
    }

    #[test]
    fn one_turn_of_file_work_is_one_line_with_a_flag_for_each_file() {
        let w = ws("one-turn");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", None, b"one\n");
        touch(&store, session, 2, &w, "a.md", Some(b"one\n"), b"two\n");
        touch(&store, session, 2, &w, "b.md", None, b"fresh\n");

        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.turns.len(), 2, "two turns of file work");
        assert_eq!(l.turns[0].turn, 2, "newest turn first: the card is about recently");
        assert!(l.turns[0].latest, "and it says which turn is the newest undoable one");
        assert_eq!(l.turns[1].files, 1, "turn 1 touched one path");
        assert_eq!(l.turns[0].files, 2, "turn 2 touched two paths");
        assert_eq!(l.files_total, 3, "three rows in the file record");
        assert_eq!(l.turns[1].file_rows[0].kind, "created", "a file with nothing behind it is created");
        assert_eq!(l.turns[0].file_rows[0].kind, "modified", "the second turn overwrote it");
        assert_eq!(l.turns[0].file_rows[1].kind, "created", "and wrote a new file");
        assert_eq!(l.turns[0].file_rows.len(), l.turns[0].files, "the count matches the rows drawn");
        assert!(l.note.contains("2 turns wrote files"), "{l:?}");
        assert!(l.note.contains("3 files"), "{l:?}");
        assert!(l.note.contains("2 paths"), "{l:?}");
        assert_eq!(l.turns_hidden, 0);
        assert_eq!(l.paths_hidden, 0);
        assert_eq!(l.copies, 1, "one overwrite kept one copy");
    }

    #[test]
    fn a_deleted_file_is_still_listed_with_its_history_and_no_live_side() {
        let w = ws("deleted");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "gone.md", Some(b"was here\n"), b"is gone\n");
        store.stamp_checkpoints_turn(session, 1, 11).unwrap();
        std::fs::remove_file(w.join("gone.md")).unwrap();

        let l = read(store.as_ref(), session).unwrap();
        let row = &l.turns[0].file_rows[0];
        assert_eq!(row.kind, "modified");
        assert!(!row.live_exists, "the file is not in the folder");
        assert!(row.path_missing, "and the row says the path is gone");
        assert!(row.copy_present, "the copy this app took is still there");
        assert!(row.can_revert, "so the turn can still put it back");
        assert_eq!(row.says, "restore the contents from before the turn", "{row:?}");
        assert_eq!(l.files_total, 1, "the record is not edited when a file moves");
    }

    #[test]
    fn a_created_file_says_undoing_it_deletes_it_and_never_offers_a_copy() {
        let w = ws("created");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "new.md", None, b"written\n");
        store.stamp_checkpoints_turn(session, 1, 11).unwrap();

        let l = read(store.as_ref(), session).unwrap();
        let row = &l.turns[0].file_rows[0];
        assert_eq!(row.kind, "created");
        assert!(!row.copy_present, "there was nothing earlier to copy");
        assert!(row.can_revert, "undoing it deletes the file, which needs no copy");
        assert_eq!(row.says, "delete the file the turn wrote", "{row:?}");
        assert_eq!(l.copies, 0, "no copy was kept for this turn");

        // A created file has no comparison to print, so the card is refused rather than handed the
        // live file as if its every line had been added by somebody. What the turn wrote is on the
        // tool card; this row's job is to say what a revert would do.
        let err = diff_of(store.as_ref(), session, row.checkpoint, "new.md").unwrap_err();
        assert!(err.contains("this turn created it"), "{err}");
        assert!(err.contains("Nothing was diffed"), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
        assert_eq!(std::fs::read(w.join("new.md")).unwrap(), b"written\n", "the refusal wrote nothing");
    }

    #[test]
    fn a_turn_the_host_has_not_written_down_yet_says_so_and_offers_no_revert() {
        let w = ws("unstamped");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        // No stamp: the copies exist, the answer row does not yet.
        let l = read(store.as_ref(), session).unwrap();
        let row = &l.turns[0].file_rows[0];
        assert!(!row.can_revert, "there is no message to undo back to");
        assert!(row.says.contains("no answer row yet"), "{row:?}");
        assert!(row.copy_present, "the copy is still a copy");
        assert_eq!(l.unstamped, 1);
        assert_eq!(l.turns[0].revertable, 0);
        assert!(
            l.turns[0].note.contains("none of it stored yet"),
            "{:?}",
            l.turns[0].note
        );
    }

    #[test]
    fn a_copy_the_store_no_longer_holds_is_named_as_missing_and_offers_nothing() {
        let w = ws("missing-blob");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        store.stamp_checkpoints_turn(session, 1, 11).unwrap();
        let hash = first_hash(&store, session);
        std::fs::remove_file(checkpoint::blob_path(&checkpoint::snapshot_dir(&w), &hash)).unwrap();

        let l = read(store.as_ref(), session).unwrap();
        let row = &l.turns[0].file_rows[0];
        assert!(!row.copy_present, "the copy is gone");
        assert!(!row.can_revert, "so the card cannot offer to put it back");
        assert_eq!(row.says, "no copy was kept, so there is nothing to put back", "{row:?}");
        assert_eq!(l.copies_missing, 1, "and the count says so");
        assert!(l.note.contains("no bytes on disk"), "{l:?}");
        assert!(l.note.contains("emptied outside this app"), "{l:?}");

        // A one-sided comparison is not a comparison: the card refuses and says why, rather than
        // printing the live file as if everything in it had been added.
        let err = diff_of(store.as_ref(), session, row.checkpoint, "a.md").unwrap_err();
        assert!(err.contains("no longer in the store"), "{err}");
        assert!(err.contains("Nothing was diffed"), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
    }

    #[test]
    fn an_evicted_row_leaves_a_line_that_offers_nothing() {
        let w = ws("cap");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        // Past the per-conversation cap: every turn overwrites one file, so every turn keeps a copy.
        for turn in 1..=(checkpoint::CHECKPOINT_CAP + 5) {
            touch(&store, session, turn as i64, &w, "a.md", Some(b"one\n"), b"two\n");
            store.stamp_checkpoints_turn(session, turn as i64, turn as i64).unwrap();
        }
        let kept = store.checkpoints_for_session(session).unwrap().len();
        assert!(kept <= checkpoint::CHECKPOINT_CAP, "the store keeps at most the cap: {kept} rows");
        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.files_total, kept, "the ledger's count is the record's, not a wish");
        assert!(l.pruned > 0, "the eviction is reported: {}", l.pruned);
        assert!(l.note.contains("under the limit"), "{l:?}");
        assert!(l.note.contains("no diff and no revert"), "{l:?}");
        assert_eq!(l.prune_notes.len(), 1, "one notice per eviction: {:?}", l.prune_notes);
        assert!(l.prune_notes[0].contains("oldest ones went first"), "{:?}", l.prune_notes);
        // The discarded turns are absent rather than present and inert, and the notice is what tells
        // a person why the oldest turn they remember is not on the list.
        assert_eq!(
            l.turns.first().unwrap().turn,
            checkpoint::CHECKPOINT_CAP as i64 + 5,
            "newest turn drawn"
        );
        assert!(
            l.turns.iter().all(|t| t.revertable == t.files),
            "every row still drawn can be put back"
        );
        assert_eq!(l.turns_total, kept);
        assert_eq!(l.turns_hidden, kept - LEDGER_TURNS, "{l:?}");
        assert!(l.note.contains("not listed on this card"), "{l:?}");
        // And a diff addressed at a row the cap took refuses instead of guessing at a newer one.
        let gone_id = 1i64;
        let err = diff_of(store.as_ref(), session, gone_id, "a.md").unwrap_err();
        assert!(err.contains("Nothing was diffed"), "{err}");
        assert!(err.contains("pruned"), "{err}");
    }

    #[tokio::test]
    async fn a_forgotten_notice_is_not_reported_as_a_cap() {
        let w = ws("forget-vs-cap");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        store.stamp_checkpoints_turn(session, 1, 11).unwrap();
        let h = idle_host(&w, Arc::clone(&store));
        let out = forget(store.as_ref(), h.as_ref(), session, 1).unwrap();
        assert_eq!(out.dropped, 1);
        assert!(out.note.contains("forgotten"), "{out:?}");

        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.pruned, 0, "a person's request is not a limit reaching it");
        assert_eq!(l.files_total, 0);
        assert!(l.note.contains("no turn of this conversation wrote a file"), "{l:?}");
        // The notice survives the thing it explains, and says who asked for it.
        assert_eq!(l.prune_notes.len(), 1, "{:?}", l.prune_notes);
        assert!(l.prune_notes[0].contains("at the person's request"), "{:?}", l.prune_notes);
        assert!(!l.prune_notes[0].contains("oldest ones went first"), "{:?}", l.prune_notes);
        assert_eq!(l.turns.len(), 0, "no turn of file work is left to draw");
    }

    #[tokio::test]
    async fn the_two_byte_numbers_stay_apart() {
        let w = ws("bytes");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        // Two paths whose copies are the same bytes: the index counts two rows, the store holds one
        // file. Printing one of the two as "disk usage" is the bug this test exists for.
        std::fs::write(w.join("a.md"), b"shared bytes\n").unwrap();
        std::fs::write(w.join("b.md"), b"shared bytes\n").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"shared bytes\n"), b"a new\n");
        touch(&store, session, 2, &w, "b.md", Some(b"shared bytes\n"), b"b new\n");

        let s = stats(store.as_ref(), session).unwrap();
        assert_eq!(s.rows, 2);
        assert_eq!(s.blobs, 1, "content-addressed: the same bytes are one file");
        assert_eq!(s.blob_bytes, b"shared bytes\n".len() as u64);
        assert_eq!(s.row_bytes, b"shared bytes\n".len() as u64 * 2);
        assert!(s.row_bytes > s.blob_bytes, "and they are not the same number");
        assert_eq!(s.shared, 0, "one conversation holds them all");
        assert_eq!(s.orphan_blobs, 0);
        assert_eq!(s.neighbours, 0);
        assert!(s.dir_exists, "the store is where the ledger says it is: {}", s.dir);
        assert!(s.note.contains("index rows") || s.note.contains("index"), "{s:?}");
        assert!(s.note.contains("distinct") || s.note.contains("store holds"), "{s:?}");

        // The same dedupe has to survive the verb that deletes: a receipt saying it freed twice the
        // bytes is a receipt about a deletion that did not happen.
        let h = idle_host(&w, Arc::clone(&store));
        let out = forget(store.as_ref(), h.as_ref(), session, 2).unwrap();
        assert_eq!(out.dropped, 2, "two rows went");
        assert_eq!(
            out.freed_bytes,
            b"shared bytes\n".len() as u64,
            "and one file's bytes left the disk: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_blob_another_conversation_points_at_is_not_freed() {
        let w = ws("shared");
        let store = Arc::new(Store::in_memory().unwrap());
        let a = store.create_session("a", w.to_str().unwrap(), "m").unwrap();
        let b = store.create_session("b", w.to_str().unwrap(), "m").unwrap();
        std::fs::write(w.join("a.md"), b"shared\n").unwrap();
        std::fs::write(w.join("b.md"), b"shared\n").unwrap();
        touch(&store, a, 1, &w, "a.md", Some(b"shared\n"), b"a new\n");
        touch(&store, b, 1, &w, "b.md", Some(b"shared\n"), b"b new\n");
        store.stamp_checkpoints_turn(a, 1, 11).unwrap();
        store.stamp_checkpoints_turn(b, 1, 22).unwrap();

        let sa = stats(store.as_ref(), a).unwrap();
        assert_eq!(sa.shared, 1, "conversation b points at the same copy");
        assert_eq!(sa.neighbours, 1, "and the stats name the neighbour");
        let hash = first_hash(&store, a);
        assert_eq!(store.rows_pointing_at_outside(&hash, a).unwrap(), 1);

        let h = idle_host(&w, Arc::clone(&store));
        let out = forget(store.as_ref(), h.as_ref(), a, 1).unwrap();
        assert_eq!(out.kept, 1, "the receipt says a copy was kept for another conversation");
        assert!(out.note.contains("another conversation"), "{out:?}");
        assert!(
            checkpoint::blob_path(&checkpoint::snapshot_dir(&w), &hash).exists(),
            "conversation b's copy is still on disk"
        );
        // Conversation b keeps its revert.
        let lb = read(store.as_ref(), b).unwrap();
        assert!(lb.turns[0].file_rows[0].can_revert, "{:?}", lb.turns[0].file_rows[0]);
        assert_eq!(stats(store.as_ref(), b).unwrap().blobs, 1);
        assert_eq!(stats(store.as_ref(), b).unwrap().shared, 0, "b holds it alone now");
        assert_eq!(stats(store.as_ref(), a).unwrap().rows, 0);
        assert_eq!(std::fs::read(w.join("a.md")).unwrap(), b"a new\n", "forgetting took copies, not files");
    }

    #[test]
    fn bytes_no_index_row_points_at_are_counted_as_orphans() {
        let w = ws("orphan");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"kept\n"), b"new\n");
        let dir = checkpoint::snapshot_dir(&w);
        // A real blob address, so the reader cannot dismiss it as somebody's file that happens to
        // be named like a hash.
        let stray = checkpoint::hash_of(b"nobody points at me\n");
        std::fs::create_dir_all(checkpoint::blob_path(&dir, &stray).parent().unwrap()).unwrap();
        std::fs::write(checkpoint::blob_path(&dir, &stray), b"nobody points at me\n").unwrap();

        let s = stats(store.as_ref(), session).unwrap();
        assert_eq!(s.blobs, 1);
        assert_eq!(s.orphan_blobs, 1, "a file in the store no row names");
        assert_eq!(s.orphan_bytes, b"nobody points at me\n".len() as u64);
        assert!(s.note.contains("no index row"), "{s:?}");
    }

    #[test]
    fn a_stale_path_or_a_foreign_row_on_a_diff_is_refused_and_reads_nothing() {
        let w = ws("stale");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        touch(&store, session, 2, &w, "b.md", Some(b"other\n"), b"b new\n");
        let l = read(store.as_ref(), session).unwrap();
        let row_a = l
            .turns
            .iter()
            .flat_map(|t| t.file_rows.iter())
            .find(|f| f.path == "a.md")
            .unwrap();

        // A window painting `a.md` now holds `b.md`: refusing is the only honest answer.
        let err = diff_of(store.as_ref(), session, row_a.checkpoint, "b.md").unwrap_err();
        assert!(err.contains("Nothing was diffed"), "{err}");
        assert!(err.contains("says this turn touched a.md"), "{err}");
        // A row from a conversation this window is not reading.
        let other = store.create_session("o", w.to_str().unwrap(), "m").unwrap();
        let err2 = diff_of(store.as_ref(), other, row_a.checkpoint, "a.md").unwrap_err();
        assert!(err2.contains("another conversation"), "{err2}");
        // Nothing was written by any of the refusals.
        assert_eq!(std::fs::read(w.join("a.md")).unwrap(), b"two\n");
        assert_eq!(std::fs::read(w.join("b.md")).unwrap(), b"b new\n");
    }

    #[test]
    fn a_path_that_escapes_the_workspace_is_refused() {
        let w = ws("escape");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        // A row the record would never write by itself, planted to check the reader's guard.
        {
            let conn = store.raw();
            conn.execute(
                "INSERT INTO checkpoints (session_id, turn, message_id, path, kind, hash, after_hash, bytes, created_at) \
                 VALUES (?1, 1, NULL, '../outside.txt', 'modified', '', '', 0, 0)",
                rusqlite::params![session],
            )
            .unwrap();
        }
        let err = diff_of(store.as_ref(), session, 1, "../outside.txt").unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert!(err.contains("Nothing was diffed") || err.contains("nothing was diffed"), "{err}");
        assert!(
            !w.parent().unwrap().join("outside.txt").exists(),
            "nothing was read or written outside the folder"
        );
        // The ledger still lists it — it is part of the conversation's record — and says it cannot
        // resolve the path rather than pretending the file is absent.
        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.files_total, 1);
        assert!(l.turns[0].file_rows[0].path_missing, "{:?}", l.turns[0].file_rows[0]);
    }

    #[tokio::test]
    async fn forget_refuses_a_count_that_is_not_the_record() {
        let w = ws("stale-forget");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        let h = idle_host(&w, Arc::clone(&store));
        let err = forget(store.as_ref(), h.as_ref(), session, 7).unwrap_err();
        assert!(err.contains("Nothing was forgotten"), "{err}");
        assert!(err.contains("the record says 1"), "{err}");
        assert_eq!(store.checkpoints_for_session(session).unwrap().len(), 1, "the record is intact");
        assert_eq!(stats(store.as_ref(), session).unwrap().blobs, 1, "and so is the store");
        // Zero from a card that counted nothing is refused as the mismatch it is, not treated as a
        // no-op that reads like a deletion.
        let err0 = forget(store.as_ref(), h.as_ref(), session, 0).unwrap_err();
        assert!(err0.contains("the record says 1"), "{err0}");
        assert!(err0.contains("Nothing was forgotten"), "{err0}");
        // And an empty record asked honestly: there is nothing, and the answer says so.
        let empty = store.create_session("e", w.to_str().unwrap(), "m").unwrap();
        let err1 = forget(store.as_ref(), h.as_ref(), empty, 0).unwrap_err();
        assert!(err1.contains("no copies"), "{err1}");
    }

    #[tokio::test]
    async fn forget_leaves_the_transcript_and_the_files_alone() {
        let w = ws("untouched");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        store
            .append_message(session, &crate::provider::Message::user("write it"))
            .unwrap();
        store
            .append_message(session, &crate::provider::Message::assistant("done"))
            .unwrap();
        touch(&store, session, 1, &w, "a.md", Some(b"one\n"), b"two\n");
        store.stamp_checkpoints_turn(session, 1, 2).unwrap();

        let h = idle_host(&w, Arc::clone(&store));
        let out = forget(store.as_ref(), h.as_ref(), session, 1).unwrap();
        assert_eq!(out.freed_bytes, b"one\n".len() as u64, "{out:?}");
        assert_eq!(store.message_count(session).unwrap(), 2, "the transcript is untouched");
        assert_eq!(std::fs::read(w.join("a.md")).unwrap(), b"two\n", "the file is untouched");
        assert_eq!(store.checkpoints_for_session(session).unwrap().len(), 0);
        assert_eq!(
            store.checkpoint_overview(session).unwrap().pruned,
            0,
            "and the overview still counts only cap evictions"
        );
    }

    #[test]
    fn a_long_conversation_says_how_much_the_ledger_left_out() {
        let w = ws("long");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        for turn in 1..=(LEDGER_TURNS + 7) {
            touch(&store, session, turn as i64, &w, "a.md", Some(b"one\n"), b"two\n");
        }
        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.turns.len(), LEDGER_TURNS, "the card is capped");
        assert_eq!(l.turns_hidden, 7, "{l:?}");
        assert_eq!(l.files_total, LEDGER_TURNS + 7, "the count is the record's, not the card's");
        assert!(l.note.contains("7 older turns not listed"), "{l:?}");
        assert_eq!(l.paths_hidden, 0, "one path, so nothing is hidden from the roll-up");
        // The cap hides the oldest turns, which is what a person reading "what did it do lately"
        // wants kept; the notice is the difference between a cap and a conversation that never
        // wrote files.
        assert_eq!(l.turns.last().unwrap().turn, 8, "the oldest turn drawn is the 8th");
    }

    #[test]
    fn a_path_touched_by_many_turns_lists_the_turns_it_was_written_on() {
        let w = ws("paths");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", w.to_str().unwrap(), "m").unwrap();
        touch(&store, session, 1, &w, "a.md", None, b"one\n");
        touch(&store, session, 3, &w, "a.md", Some(b"one\n"), b"two\n");
        touch(&store, session, 9, &w, "b.md", Some(b"x\n"), b"y\n");
        let l = read(store.as_ref(), session).unwrap();
        assert_eq!(l.paths.len(), 2);
        let a = l.paths.iter().find(|p| p.path == "a.md").unwrap();
        assert_eq!(a.turns, vec![1, 3], "one line per path, listing the turns that wrote it");
        assert_eq!(a.kinds, "created, modified");
        assert_eq!(a.copies, 1);
        assert!(a.live_exists);
        assert_eq!(a.live_bytes, b"two\n".len() as u64);
        assert!(l.note.contains("2 paths"), "{l:?}");
    }

    #[tokio::test]
    async fn a_conversation_with_no_folder_is_said_to_have_written_nothing() {
        let w = ws("no-folder");
        let store = Arc::new(Store::in_memory().unwrap());
        let session = store.create_session("t", "", "m").unwrap();
        let err = read(store.as_ref(), session).unwrap_err();
        assert!(err.contains("no folder"), "{err}");
        let err2 = stats(store.as_ref(), session).unwrap_err();
        assert!(err2.contains("no folder"), "{err2}");
        let h = idle_host(&w, Arc::clone(&store));
        let err3 = forget(store.as_ref(), h.as_ref(), session, 0).unwrap_err();
        assert!(err3.contains("no folder"), "{err3}");
        assert_eq!(store.message_count(session).unwrap(), 0, "and nothing was written anywhere");
    }
}
