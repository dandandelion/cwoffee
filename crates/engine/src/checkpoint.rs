//! Turn checkpoints: a copy of what a file call overwrote, so the turn can be undone.
//!
//! Two halves, and the split is the whole design.
//!
//! The **bytes** go in a content-addressed store under the workspace's own scratch
//! folder (`.cws/snapshots/`), keyed by SHA-256. Content-addressing means a turn that
//! rewrote the same file six times stores four copies rather than six, and that two
//! conversations editing the same file share one copy of the state they had in common.
//!
//! The **index** goes in the app's SQLite store, beside the messages it describes:
//! session, turn, path, hash, and whether the file existed beforehand. A checkpoint the
//! app cannot find again after a restart is not a checkpoint, and the transcript is the
//! thing that survives restarts.
//!
//! Reverting touches the files on disk and nothing else. The transcript stays whole: an
//! answer the model wrote is a record of what happened, and deleting it would leave the
//! conversation describing a turn with no evidence of it. Revert is refused while a turn
//! is running — racing the model for a file it is writing is how a revert restores the
//! wrong thing.
//!
//! The scope is deliberately narrow, and the card says so: calls that go through
//! `write_file` / `edit_file`, including the `.docx` and `.pdf` writers. `exec` can write
//! anything a shell can reach, and snapshotting *that* would mean shadowing the machine.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::CheckpointRow;

/// Where snapshots live inside a workspace. `.cws/` is already the folder the app keeps
/// its own files in, so this needs no new ignore rule and no new place to wonder about.
pub const SNAP_SUBDIR: &str = ".cws/snapshots";

/// Checkpoint rows kept per conversation. The cap counts rows rather than bytes because
/// what grows without bound is a long conversation touching many files; file size is
/// bounded separately below. When it bites, the eviction is recorded so the transcript
/// can say "older snapshots pruned" instead of quietly offering fewer reverts.
pub const CHECKPOINT_CAP: usize = 200;

/// How far below the cap an eviction goes. Evicting one row at a time would log one
/// notice per row — forty "older snapshots pruned" lines for one long conversation — and
/// would hit the disk forty times. A batch makes the eviction an event a person can read.
pub const CHECKPOINT_BATCH: usize = 20;

/// Largest file this will copy. A 2 GB file is not something a turn overwrote in a way a
/// person wants undone, and copying it would be the slowest part of the call.
pub const SNAP_BYTES_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Did not exist before the call. Reverting deletes it.
    Created,
    /// Existed, and the call changed it. Reverting writes the earlier copy back.
    Modified,
    /// Existed and the call removed it. Reverting puts it back.
    Deleted,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Modified => "modified",
            Kind::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "created" => Some(Kind::Created),
            "modified" => Some(Kind::Modified),
            "deleted" => Some(Kind::Deleted),
            _ => None,
        }
    }

    /// What the card prints as the direction of the revert.
    pub fn direction(&self) -> &'static str {
        match self {
            Kind::Created => "delete the file the turn wrote",
            Kind::Modified => "restore the contents from before the turn",
            Kind::Deleted => "put the file back",
        }
    }
}

/// What one file call captured, carried as UI metadata on the tool result — the same
/// footing as `files` and `changes`, so it costs no prompt bytes and cannot disturb the
/// endpoint's prefix cache. The host turns marks into index rows, because only the host
/// knows which turn is running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mark {
    /// Workspace-relative, the same rule as `OutputFile::path`: an absolute path here
    /// would let a tool result point a revert at anything on the machine.
    pub path: String,
    pub kind: Kind,
    /// Blob key. Empty when nothing was stored — a created file had nothing to copy.
    pub hash: String,
    pub bytes: u64,
    /// Hash of the bytes the file held *after* the call. Revert compares the file on
    /// disk with this and says "edited since" when it no longer matches.
    pub after_hash: String,
    /// Set when no copy could be taken (too big, unreadable, disk full, no store). The
    /// card shows it: a turn with no revert offered has to say why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn snapshot_dir(workspace: &Path) -> PathBuf {
    workspace.join(SNAP_SUBDIR)
}

/// Enough precision to name a size in a card without a person doing arithmetic.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn hash_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Two levels of shard, so a long-lived workspace does not end with ten thousand files
/// sitting in one directory.
pub fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    if hash.len() < 5 {
        return dir.join(hash);
    }
    dir.join(&hash[..2]).join(&hash[2..4]).join(hash)
}

/// Read a file for checkpointing. Binary files are copied as bytes — a `.docx` has no
/// text worth decoding — so the copy is the file rather than a reading of it.
pub fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > SNAP_BYTES_LIMIT {
        return None;
    }
    std::fs::read(path).ok()
}

/// What was on the path before the call, as the caller saw it.
///
/// Three cases, not two. "There was no file" and "there was a file I am not going to
/// read" must not collapse into the same option: if a 200 MB file looked absent, a revert
/// would treat it as a file the turn created and **delete it** — the one failure mode a
/// repair tool is not allowed to have.
pub enum PreState {
    /// Nothing was there.
    Absent,
    /// A file, and its bytes.
    Present(Vec<u8>),
    /// A file too big to copy. Recorded as modified with no copy, so the card says the
    /// change cannot be undone rather than offering to delete the file.
    TooBig(u64),
    /// A path that is not a file at all (a directory, a symlink to nothing).
    NotAFile,
}

impl PreState {
    /// Read the path the way a checkpoint needs it read.
    pub fn of(path: &Path) -> PreState {
        match std::fs::metadata(path) {
            Err(_) => PreState::Absent,
            Ok(md) if !md.is_file() => PreState::NotAFile,
            Ok(md) if md.len() > SNAP_BYTES_LIMIT => PreState::TooBig(md.len()),
            Ok(_) => match std::fs::read(path) {
                Ok(b) => PreState::Present(b),
                Err(_) => PreState::NotAFile,
            },
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            PreState::Present(b) => Some(b),
            _ => None,
        }
    }
}

/// Copy the pre-call bytes into the snapshot store and describe the capture.
///
/// `dir` is `None` when the host has no snapshot store (a probe, an example, a tool host
/// never told where the workspace is). Then a file call behaves exactly as it did before
/// this feature, and the card claims no revert.
///
/// Dedupe is by hash: an existing blob is left alone rather than rewritten, which is what
/// makes six rewrites of one file cost four copies instead of six.
pub fn capture(
    dir: Option<&Path>,
    workspace: &Path,
    path: &Path,
    pre: &PreState,
    after: Option<&[u8]>,
) -> Option<Mark> {
    // No store, no mark. A host that keeps no copies is not a host that failed to keep
    // one: the card should show nothing and the transcript should offer no revert control,
    // rather than explaining a limitation the person never met.
    let dir = dir?;
    // A path the tool reached from outside the workspace cannot be reverted by this code
    // (`resolve` refuses it below), so recording it would promise an undo that can never
    // happen. `display_rel` hands back the absolute path in exactly that case.
    let rel = crate::tools::path::display_rel(workspace, path);
    if Path::new(&rel).is_absolute() {
        return None;
    }
    let kind = match pre {
        PreState::Absent => match after {
            Some(_) => Kind::Created,
            None => return None,
        },
        PreState::Present(_) | PreState::TooBig(_) => match after {
            Some(_) => Kind::Modified,
            None => Kind::Deleted,
        },
        PreState::NotAFile => return None,
    };
    let after_hash = after.map(hash_of).unwrap_or_default();

    if let PreState::TooBig(size) = pre {
        // Named before `rel` moves into the mark: the card has to say which file it is
        // refusing to copy. With no store there is nothing to refuse, and no revert being
        // offered, so nothing is said.
        let note = format!(
            "{rel} is {}, past what this copies, so this change cannot be undone",
            human_bytes(*size)
        );
        return Some(Mark {
            path: rel,
            kind,
            hash: String::new(),
            bytes: *size,
            after_hash,
            note: Some(note),
        });
    }

    let Some(before) = pre.bytes() else {
        // Nothing was overwritten, and there is nothing to copy. The mark is still
        // recorded, because "delete this file to undo the turn" is a fact about the turn
        // that has to survive the restart.
        return Some(Mark {
            path: rel,
            kind,
            hash: String::new(),
            bytes: 0,
            after_hash,
            note: None,
        });
    };
    // The borrow of `pre` ends here; the copy below is what gets stored.
    let before: &[u8] = before;

    let hash = hash_of(before);
    let target = blob_path(dir, &hash);
    if !target.exists() {
        if let Err(e) = write_blob(&target, before) {
            // Named in the note before `rel` moves into the mark: the card has to say
            // *which file* lost its copy, not merely that a copy failed somewhere.
            let note = format!(
                "the copy of {rel} could not be stored ({e}), so this change cannot be undone"
            );
            return Some(Mark {
                path: rel,
                kind,
                hash: String::new(),
                bytes: before.len() as u64,
                after_hash,
                note: Some(note),
            });
        }
    }
    Some(Mark {
        path: rel,
        kind,
        hash,
        bytes: before.len() as u64,
        after_hash,
        note: None,
    })
}

/// Write the blob through a temp file in the same shard. A half-written blob is a
/// checkpoint that restores a truncated file, which is worse than no checkpoint at all:
/// the card would say the turn is revertible, and the revert would quietly lose bytes.
fn write_blob(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Whether the file on disk has moved since the turn left it.
///
/// One function, used by the preview and by the apply, because the card must not promise
/// "restore" for a file the apply is going to skip.
pub fn edited_since(kind: Kind, after_hash: &str, now: Option<&str>) -> bool {
    match kind {
        // The turn wrote it. Someone else touching it means the bytes changed; the file
        // being gone means there is nothing to delete, which is not an edit.
        Kind::Created => now.map(|n| n != after_hash).unwrap_or(false),
        // The turn left `after_hash`. Anything else — including the file having been
        // deleted by hand — means the person has done something since.
        Kind::Modified | Kind::Deleted => now != Some(after_hash),
    }
}

fn current_hash(path: &Path) -> Option<String> {
    read_bytes(path).map(|b| hash_of(&b))
}

/// Resolve a stored path inside the workspace, refusing anything that escapes.
///
/// The row was written by a tool that had already confined itself, but the store is a
/// record and records can be edited by anything that opens the database. A revert that
/// happily resolved `../../.ssh/id_ed25519` would be a bug in a *repair* tool, which is
/// the worst possible place for one.
pub fn resolve(workspace: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return Err(format!("{rel:?} is not a workspace-relative path"));
    }
    let mut norm = PathBuf::new();
    norm.push(workspace);
    for part in Path::new(rel).components() {
        match part {
            Component::ParentDir => {
                if !norm.pop() {
                    return Err(format!("{rel:?} points outside the workspace"));
                }
            }
            Component::Normal(c) => norm.push(c),
            // CurDir, RootDir, Prefix: a relative path that was written by the tool layer
            // never contains them, and anything that does is not ours to follow.
            _ => return Err(format!("{rel:?} points outside the workspace")),
        }
    }
    if norm == workspace {
        return Err(format!("{rel:?} is the workspace itself"));
    }
    // Re-check after the walk: `..` out of the workspace and back in still lands inside
    // it, and that landing is not the path the tool asked for.
    let ws = workspace.components().collect::<Vec<_>>();
    let got = norm.components().collect::<Vec<_>>();
    if got.len() <= ws.len() || got[..ws.len()] != ws[..] {
        return Err(format!("{rel:?} points outside the workspace"));
    }
    Ok(norm)
}

/// One path a revert would touch, as the approval card shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    pub path: String,
    pub kind: Kind,
    /// What the revert will do, in words the card prints verbatim.
    pub action: String,
    pub bytes: u64,
    /// The file on disk no longer matches what the turn left behind: the person has
    /// edited it since, and a revert would overwrite those edits.
    pub edited_since: bool,
    /// The copy is gone — evicted by the cap, or the snapshot folder was cleared by
    /// hand. The card says so instead of promising a restore.
    pub copy_missing: bool,
    /// The path itself is gone (deleted outside the app).
    pub path_missing: bool,
}

/// What each stored path would become. The card is built from this, not from the UI's
/// guess about it.
pub fn plan(workspace: &Path, dir: Option<&Path>, rows: &[CheckpointRow]) -> Vec<PlanItem> {
    targets(rows)
        .into_iter()
        .map(|t| {
            let path = resolve(workspace, &t.path).ok();
            let now = path.as_ref().and_then(|p| current_hash(p));
            let copy_missing = t.kind != Kind::Created
                && (t.hash.is_empty()
                    || dir.map(|d| !blob_path(d, &t.hash).exists()).unwrap_or(true));
            PlanItem {
                path: t.path,
                kind: t.kind,
                action: t.kind.direction().to_string(),
                bytes: t.bytes,
                edited_since: edited_since(t.kind, &t.after_hash, now.as_deref()),
                copy_missing,
                path_missing: path.map(|p| !p.exists()).unwrap_or(false),
            }
        })
        .collect()
}

/// One path a scope of checkpoints resolves to, and what to do with it.
///
/// The pair of fields is the point: the copy to put back comes from the **earliest** row
/// of that path (undoing the scope means returning to where it started), while the state
/// to expect on disk comes from the **latest** row (that is what the scope left behind, and
/// anything past it is the person's own edit). A turn that rewrote one file three times has
/// three rows and one target; treating them as three targets would restore the middle of the
/// turn and call the file "edited since" on its way out.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: String,
    pub kind: Kind,
    /// The copy to restore. Empty for a file the scope created.
    pub hash: String,
    /// What the scope left on disk.
    pub after_hash: String,
    pub bytes: u64,
    /// Rows folded into this target, for the note that says how many were undone.
    pub rows: usize,
}

pub fn targets(rows: &[CheckpointRow]) -> Vec<Target> {
    let mut map: HashMap<&str, Target> = HashMap::new();
    for r in rows.iter() {
        match map.get_mut(r.path.as_str()) {
            None => {
                map.insert(
                    r.path.as_str(),
                    Target {
                        path: r.path.clone(),
                        kind: r.kind,
                        hash: r.hash.clone(),
                        after_hash: r.after_hash.clone(),
                        bytes: r.bytes as u64,
                        rows: 1,
                    },
                );
            }
            // Rows arrive oldest-first. Keep the first copy, take the newest expectation.
            Some(t) => {
                t.after_hash = r.after_hash.clone();
                if t.kind == Kind::Created {
                    // Still the file this scope wrote; its later bytes are what it became.
                    t.hash = String::new();
                }
                t.rows += 1;
            }
        }
    }
    let mut out: Vec<Target> = map.values().cloned().collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// What a revert did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevertReport {
    pub restored: usize,
    pub removed: usize,
    /// Left alone because the person edited the file afterwards.
    pub skipped_edited: usize,
    /// Could not be touched, one named line each.
    pub failed: Vec<String>,
    /// Paths actually changed.
    pub touched: Vec<String>,
}

/// Put the files back. `overwrite_edited` is the answer to the card that listed the
/// paths; nothing here decides on its own.
pub fn apply(
    workspace: &Path,
    dir: &Path,
    rows: &[CheckpointRow],
    overwrite_edited: bool,
) -> RevertReport {
    let mut rep = RevertReport::default();
    for r in targets(rows) {
        let path = match resolve(workspace, &r.path) {
            Ok(p) => p,
            Err(e) => {
                rep.failed.push(format!("{}: {e}", r.path));
                continue;
            }
        };
        let now = current_hash(&path);
        if edited_since(r.kind, &r.after_hash, now.as_deref()) && !overwrite_edited {
            rep.skipped_edited += 1;
            continue;
        }
        match r.kind {
            Kind::Created => {
                if !path.exists() {
                    continue; // already gone: nothing to remove, nothing to report
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        rep.removed += 1;
                        rep.touched.push(r.path.clone());
                    }
                    Err(e) => rep.failed.push(format!("{}: cannot remove it: {e}", r.path)),
                }
            }
            Kind::Modified | Kind::Deleted => {
                let blob = blob_path(dir, &r.hash);
                if r.hash.is_empty() || !blob.exists() {
                    rep.failed.push(format!(
                        "{}: the copy is gone, so it cannot be put back",
                        r.path
                    ));
                    continue;
                }
                let bytes = match std::fs::read(&blob) {
                    Ok(b) => b,
                    Err(e) => {
                        rep.failed.push(format!("{}: the copy could not be read: {e}", r.path));
                        continue;
                    }
                };
                if let Some(parent) = path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        rep.failed.push(format!(
                            "{}: cannot create {}: {e}",
                            r.path,
                            parent.display()
                        ));
                        continue;
                    }
                }
                // Same discipline as `edit_file`: write aside, then rename, so a failure
                // mid-write leaves the file the person was looking at intact.
                let tmp = path.with_extension("cwoffee-revert");
                if let Err(e) = std::fs::write(&tmp, &bytes) {
                    let _ = std::fs::remove_file(&tmp);
                    rep.failed.push(format!("{}: cannot write it: {e}", r.path));
                    continue;
                }
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    let _ = std::fs::remove_file(&tmp);
                    rep.failed.push(format!("{}: cannot replace it: {e}", r.path));
                    continue;
                }
                rep.restored += 1;
                rep.touched.push(r.path.clone());
            }
        }
    }
    rep
}

/// A person-facing summary of what a revert did, for the toast and the host note.
pub fn report_note(rep: &RevertReport) -> String {
    fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
        if n == 1 {
            one
        } else {
            many
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if rep.restored > 0 {
        parts.push(format!(
            "{} {} restored to how {} {} before",
            rep.restored,
            plural(rep.restored, "file was", "files were"),
            plural(rep.restored, "it", "they"),
            plural(rep.restored, "was", "were")
        ));
    }
    if rep.removed > 0 {
        parts.push(format!(
            "{} {} that turn wrote {} deleted",
            rep.removed,
            plural(rep.removed, "file", "files"),
            plural(rep.removed, "was", "were")
        ));
    }
    if rep.skipped_edited > 0 {
        parts.push(format!(
            "{} {} you had edited since {} left alone",
            rep.skipped_edited,
            plural(rep.skipped_edited, "file", "files"),
            plural(rep.skipped_edited, "was", "were")
        ));
    }
    if !rep.failed.is_empty() {
        parts.push(format!(
            "{} could not be touched: {}",
            rep.failed.len(),
            rep.failed.join("; ")
        ));
    }
    if parts.is_empty() {
        return "nothing on disk needed changing: those files are already as they were before that turn".to_string();
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CheckpointPrune, Store};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cwoffee-ckpt-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn row(id: i64, path: &str, kind: Kind, hash: &str, after: &str) -> CheckpointRow {
        CheckpointRow {
            id,
            session: 1,
            turn: 1,
            message_id: None,
            path: path.to_string(),
            kind,
            hash: hash.to_string(),
            after_hash: after.to_string(),
            bytes: 1,
            created_at: 0,
        }
    }

    #[test]
    fn the_copy_is_addressed_by_content_so_rewriting_one_file_stores_it_once() {
        let ws = tmp("dedupe");
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "first\n").unwrap();

        let m1 = capture(Some(&dir), &ws, &f, &PreState::Present(b"first\n".to_vec()), Some(b"second\n")).unwrap();
        let m2 = capture(Some(&dir), &ws, &f, &PreState::Present(b"first\n".to_vec()), Some(b"third\n")).unwrap();
        assert_eq!(m1.hash, m2.hash, "the same bytes are one copy, not two");
        assert_eq!(
            std::fs::read_dir(blob_path(&dir, &m1.hash).parent().unwrap())
                .unwrap()
                .count(),
            1,
            "and one file on disk, not two"
        );
        let m3 = capture(Some(&dir), &ws, &f, &PreState::Present(b"second\n".to_vec()), Some(b"fourth\n")).unwrap();
        assert_ne!(m1.hash, m3.hash, "different bytes are a different copy");
        assert_eq!(m3.after_hash, hash_of(b"fourth\n"));
        assert_eq!(m1.bytes, 6);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_file_the_turn_created_is_reverted_by_deleting_it_and_needs_no_copy() {
        let ws = tmp("created");
        let dir = snapshot_dir(&ws);
        let f = ws.join("report.md");
        let m = capture(Some(&dir), &ws, &f, &PreState::Absent, Some(b"new file\n")).unwrap();
        assert_eq!(m.kind, Kind::Created);
        assert!(m.hash.is_empty(), "nothing was overwritten to copy");
        std::fs::write(&f, "new file\n").unwrap();

        let rows = [row(1, "report.md", Kind::Created, "", &m.after_hash)];
        let p = plan(&ws, Some(&dir), &rows);
        assert!(!p[0].edited_since, "still exactly what the turn wrote");
        assert!(!p[0].copy_missing, "a created file needs no copy");
        assert!(p[0].action.contains("delete"));

        let rep = apply(&ws, &dir, &rows, false);
        assert_eq!(rep.removed, 1);
        assert!(!f.exists());
        assert_eq!(rep.restored, 0);
        // Reverting twice is a no-op, not an error: the file is already gone.
        let again = apply(&ws, &dir, &rows, false);
        assert_eq!(again.removed, 0);
        assert!(again.failed.is_empty());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_file_the_person_edited_since_is_left_alone_unless_told_otherwise() {
        let ws = tmp("edited");
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "one\n").unwrap();
        let m = capture(Some(&dir), &ws, &f, &PreState::Present(b"one\n".to_vec()), Some(b"two\n")).unwrap();
        std::fs::write(&f, "two\n").unwrap();
        // The person edits it by hand, after the turn.
        std::fs::write(&f, "two, plus my own line\n").unwrap();
        let rows = [row(1, "notes.md", Kind::Modified, &m.hash, &m.after_hash)];

        let p = plan(&ws, Some(&dir), &rows);
        assert!(p[0].edited_since, "the card has to say so before the yes");

        let skipped = apply(&ws, &dir, &rows, false);
        assert_eq!(skipped.skipped_edited, 1);
        assert_eq!(skipped.restored, 0);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "two, plus my own line\n",
            "their own work is still there"
        );

        let forced = apply(&ws, &dir, &rows, true);
        assert_eq!(forced.restored, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_missing_copy_is_reported_instead_of_promising_a_restore() {
        let ws = tmp("missing");
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "one\n").unwrap();
        let m = capture(Some(&dir), &ws, &f, &PreState::Present(b"one\n".to_vec()), Some(b"two\n")).unwrap();
        std::fs::write(&f, "two\n").unwrap();
        // The snapshot folder was cleared by hand.
        std::fs::remove_dir_all(&dir).ok();

        let rows = [row(1, "notes.md", Kind::Modified, &m.hash, &m.after_hash)];
        let p = plan(&ws, Some(&dir), &rows);
        assert!(p[0].copy_missing, "the card knows before it asks");
        let rep = apply(&ws, &dir, &rows, false);
        assert_eq!(rep.restored, 0);
        assert_eq!(rep.failed.len(), 1);
        assert!(rep.failed[0].contains("the copy is gone"), "{}", rep.failed[0]);
        assert!(rep.skipped_edited == 0, "a missing copy is not 'edited since'");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_stored_path_that_leaves_the_workspace_is_refused_not_reverted() {
        let ws = tmp("escape");
        let outside = std::env::temp_dir().join(format!(
            "cwoffee-ckpt-outside-{}",
            std::process::id()
        ));
        std::fs::write(&outside, "someone else's file\n").unwrap();

        assert!(resolve(&ws, &outside.to_string_lossy().to_string()).is_err(), "absolute");
        assert!(resolve(&ws, "../someone-else").is_err(), "traversal");
        assert!(resolve(&ws, "notes/../../someone-else").is_err(), "traversal through a subdir");
        assert!(resolve(&ws, "").is_err(), "empty");
        assert!(resolve(&ws, ".").is_err(), "the workspace itself");
        assert_eq!(resolve(&ws, "notes/x.md").unwrap(), ws.join("notes/x.md"));
        // And a revert that is handed such a row names the refusal, without writing.
        let rows = [row(
            1,
            &outside.to_string_lossy().to_string(),
            Kind::Modified,
            "abcd",
            "",
        )];
        let rep = apply(&ws, &snapshot_dir(&ws), &rows, true);
        assert_eq!(rep.restored, 0);
        assert_eq!(rep.failed.len(), 1);
        assert!(rep.failed[0].contains("outside the workspace") || rep.failed[0].contains("workspace-relative"), "{}", rep.failed[0]);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "someone else's file\n");
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_turn_that_rewrote_one_file_twice_is_undone_to_where_the_turn_began() {
        let ws = tmp("twice");
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "one\n").unwrap();
        let a = capture(Some(&dir), &ws, &f, &PreState::Present(b"one\n".to_vec()), Some(b"two\n")).unwrap();
        std::fs::write(&f, "two\n").unwrap();
        let b = capture(Some(&dir), &ws, &f, &PreState::Present(b"two\n".to_vec()), Some(b"three\n")).unwrap();
        std::fs::write(&f, "three\n").unwrap();

        let rows = [
            row(1, "notes.md", Kind::Modified, &a.hash, &a.after_hash),
            row(2, "notes.md", Kind::Modified, &b.hash, &b.after_hash),
        ];
        let rep = apply(&ws, &dir, &rows, false);
        assert_eq!(rep.restored, 1, "one file, not two writes");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "one\n",
            "the state when the turn began, not the middle of the turn"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn the_cap_evicts_the_oldest_and_records_that_it_did() {
        let ws = tmp("cap");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        std::fs::create_dir_all(&dir).unwrap();

        for i in 0..(CHECKPOINT_CAP + 40) {
            let hash = format!("{i:064x}");
            let blob = blob_path(&dir, &hash);
            std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
            std::fs::write(&blob, format!("copy {i}\n")).unwrap();
            s.record_checkpoints(
                sess,
                1,
                &[Mark {
                    path: format!("f{i}.txt"),
                    kind: Kind::Modified,
                    hash,
                    bytes: 8,
                    after_hash: String::new(),
                    note: None,
                }],
            )
            .unwrap();
        }
        let rows = s.checkpoints_for_session(sess).unwrap();
        assert_eq!(rows.len(), CHECKPOINT_CAP, "the conversation stops growing");
        assert_eq!(rows[0].path, "f40.txt", "the oldest went first");

        let prunes: Vec<CheckpointPrune> = s.checkpoint_prunes(sess).unwrap();
        assert_eq!(prunes.len(), 2, "one notice per batch, not one per row");
        assert_eq!(prunes.iter().map(|p| p.dropped).sum::<usize>(), 40);
        assert!(prunes[0].note.contains("pruned"), "{}", prunes[0].note);
        assert!(
            !blob_path(&dir, &format!("{:064x}", 0)).exists(),
            "the evicted copy is off the disk"
        );
        assert!(blob_path(&dir, &format!("{:064x}", CHECKPOINT_CAP + 39)).exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn deleting_a_conversation_takes_its_copies_but_not_a_copy_another_one_shares() {
        let ws = tmp("cascade");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let a = s.create_session("a", ws.to_str().unwrap(), "m").unwrap();
        let b = s.create_session("b", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);

        // Two conversations over the same bytes share one copy, so deleting one must not
        // take a blob the other still points at.
        let shared = hash_of(b"shared\n");
        let blob = blob_path(&dir, &shared);
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"shared\n").unwrap();
        let mark = Mark {
            path: "x.txt".into(),
            kind: Kind::Modified,
            hash: shared.clone(),
            bytes: 7,
            after_hash: String::new(),
            note: None,
        };
        s.record_checkpoints(a, 1, &[mark.clone()]).unwrap();
        s.record_checkpoints(b, 1, &[mark]).unwrap();

        s.delete_session(a).unwrap();
        assert!(blob.exists(), "the other conversation still points at it");
        assert_eq!(s.checkpoints_for_session(b).unwrap().len(), 1);

        s.delete_session(b).unwrap();
        assert!(!blob.exists(), "nothing points at it now");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_host_with_no_snapshot_store_takes_no_marks_and_claims_no_revert() {
        let ws = tmp("nostore");
        let f = ws.join("notes.md");
        std::fs::write(&f, "one\n").unwrap();
        assert!(
            capture(None, &ws, &f, &PreState::Present(b"one\n".to_vec()), Some(b"two\n")).is_none(),
            "no store is not a failure to explain, it is nothing to record"
        );
        assert!(capture(None, &ws, &f, &PreState::Absent, Some(b"x\n")).is_none());
        assert!(
            capture(None, &ws, &f, &PreState::TooBig(9_000_000), Some(b"x\n")).is_none(),
            "not even the refusal note, because nobody is being offered a revert"
        );
        assert!(!snapshot_dir(&ws).exists(), "no folder appears on the disk either");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn the_report_says_what_happened_in_the_words_the_user_reads() {
        let rep = RevertReport {
            restored: 2,
            removed: 1,
            skipped_edited: 1,
            failed: Vec::new(),
            touched: vec!["a".into(), "b".into(), "c".into()],
        };
        let s = report_note(&rep);
        assert!(s.contains("2 files were restored to how they were before"), "{s}");
        assert!(s.contains("1 file that turn wrote was deleted"), "{s}");
        assert!(s.contains("1 file you had edited since was left alone"), "{s}");
        let none = report_note(&RevertReport::default());
        assert!(none.contains("already as they were"), "{none}");
        let bad = RevertReport {
            failed: vec!["x.txt: cannot write it: read-only".into()],
            ..Default::default()
        };
        assert!(report_note(&bad).contains("read-only"), "{}", report_note(&bad));
    }

    /// A store + host pair for the revert tests: the revert needs a host to ask whether a
    /// turn is running, and nothing else about a host matters here.
    /// A host to ask whether a turn is running: the revert's refusal needs one, and
    /// nothing else about a host is being tested here.
    fn idle_host() -> crate::host::SessionHost {
        crate::host::tests::host_for_test(vec!["ok".into()])
    }

    #[test]
    fn the_card_lists_what_would_change_and_says_which_turns_it_covers() {
        let ws = tmp("preview");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "yours\n").unwrap();
        let m = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"yours\n".to_vec()),
            Some(b"model wrote\n"),
        )
        .unwrap();
        std::fs::write(&f, "model wrote\n").unwrap();
        s.record_checkpoints(sess, 1, &[m]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 11).unwrap();

        let p = preview(
            &s,
            &RevertReq {
                session: sess,
                message_id: 11,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: false,
            },
        )
        .unwrap();
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].action, "restore the contents from before the turn");
        assert_eq!(p.turn, 1);
        assert_eq!(p.workspace, ws.to_str().unwrap());
        assert_eq!(p.edited_since, 0, "the file is still what the turn left");
        assert_eq!(p.missing_copy, 0);
        assert!(p.note.contains("1 file would be reverted"), "{}", p.note);
        assert!(p.note.contains("transcript is untouched"), "{}", p.note);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_turn_that_touched_no_files_is_refused_rather_than_offered_a_revert() {
        let ws = tmp("empty-scope");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let msg = preview(
            &s,
            &RevertReq {
                session: sess,
                message_id: 7,
                through_latest: false,
                shown: Vec::new(),
                overwrite_edited: false,
            },
        )
        .err()
        .unwrap();
        assert!(msg.contains("touched no files"), "{msg}");
        assert!(msg.contains("nothing to revert"), "{msg}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn a_card_listing_different_files_than_the_record_has_now_is_refused() {
        let ws = tmp("drift");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let a = ws.join("a.txt");
        let b = ws.join("b.txt");
        std::fs::write(&a, "one\n").unwrap();
        std::fs::write(&b, "two\n").unwrap();
        let ma = capture(
            Some(&dir),
            &ws,
            &a,
            &PreState::Present(b"one\n".to_vec()),
            Some(b"x\n"),
        )
        .unwrap();
        s.record_checkpoints(sess, 1, &[ma]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 21).unwrap();
        // Another window runs a turn in the same scope while this card was open.
        let mb = capture(
            Some(&dir),
            &ws,
            &b,
            &PreState::Present(b"two\n".to_vec()),
            Some(b"y\n"),
        )
        .unwrap();
        s.record_checkpoints(sess, 1, &[mb]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 21).unwrap();

        let h = idle_host();
        let err = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 21,
                through_latest: false,
                shown: vec!["a.txt".into()],
                overwrite_edited: false,
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("not the ones this card listed"), "{err}");
        assert!(err.contains("\"b.txt\""), "{err}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one\n", "nothing was written");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "two\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn a_revert_while_a_turn_is_running_is_refused_and_nothing_is_written() {
        let ws = tmp("busy");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "yours\n").unwrap();
        let m = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"yours\n".to_vec()),
            Some(b"mine\n"),
        )
        .unwrap();
        std::fs::write(&f, "mine\n").unwrap();
        s.record_checkpoints(sess, 1, &[m]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 5).unwrap();

        let h = idle_host();
        h.set_busy_for_test(true);
        let err = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 5,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: true,
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("a turn is running"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "mine\n",
            "the model's file is still there, untouched"
        );
        h.set_busy_for_test(false);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn a_revert_leaves_the_transcript_and_the_index_alone_and_says_so() {
        let ws = tmp("record");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "yours\n").unwrap();
        let m = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"yours\n".to_vec()),
            Some(b"mine\n"),
        )
        .unwrap();
        std::fs::write(&f, "mine\n").unwrap();
        s.record_checkpoints(sess, 1, &[m]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 3).unwrap();
        s.append_message(sess, &crate::provider::Message::user("do the thing"))
            .unwrap();
        s.append_message(sess, &crate::provider::Message::assistant("done"))
            .unwrap();

        let h = idle_host();
        let out = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 3,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: false,
            },
        )
        .unwrap();
        assert_eq!(out.report.restored, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "yours\n");
        assert_eq!(out.messages, 2, "the transcript is what it was");
        assert!(out.note.contains("transcript is untouched"), "{}", out.note);
        assert!(out.note.contains("2 messages"), "{}", out.note);
        // Reverted, not consumed: the row stays, so a second revert is a no-op rather than
        // a mystery and the turn keeps offering the same control.
        assert_eq!(s.checkpoints_for_message(sess, 3).unwrap().len(), 1);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn reverting_to_before_a_turn_takes_every_later_turn_with_it() {
        let ws = tmp("scope");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "start\n").unwrap();
        let t1 = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"start\n".to_vec()),
            Some(b"turn one\n"),
        )
        .unwrap();
        std::fs::write(&f, "turn one\n").unwrap();
        let t2 = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"turn one\n".to_vec()),
            Some(b"turn two\n"),
        )
        .unwrap();
        std::fs::write(&f, "turn two\n").unwrap();
        s.record_checkpoints(sess, 1, &[t1]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 10).unwrap();
        s.record_checkpoints(sess, 2, &[t2]).unwrap();
        s.stamp_checkpoints_turn(sess, 2, 12).unwrap();

        let h = idle_host();
        // Only turn 2: back to "turn one".
        let one = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 12,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: true,
            },
        )
        .unwrap();
        assert_eq!(one.report.restored, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "turn one\n");
        assert!(one.scope.contains("only"), "{}", one.scope);

        // From turn 1 through the latest: back to "start".
        let all = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 10,
                through_latest: true,
                shown: vec!["notes.md".into()],
                overwrite_edited: true,
            },
        )
        .unwrap();
        assert_eq!(all.report.restored, 1);
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "start\n",
            "the earliest copy in the scope is the undo"
        );
        assert!(all.scope.contains("every turn after"), "{}", all.scope);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn edited_files_need_the_choice_offered_before_anything_is_written() {
        let ws = tmp("choice");
        let s = Store::open(&ws.join("app.db")).unwrap();
        let sess = s.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        let f = ws.join("notes.md");
        std::fs::write(&f, "yours\n").unwrap();
        let m = capture(
            Some(&dir),
            &ws,
            &f,
            &PreState::Present(b"yours\n".to_vec()),
            Some(b"mine\n"),
        )
        .unwrap();
        std::fs::write(&f, "mine\n").unwrap();
        s.record_checkpoints(sess, 1, &[m]).unwrap();
        s.stamp_checkpoints_turn(sess, 1, 9).unwrap();
        // The person edits it afterwards.
        std::fs::write(&f, "mine, and my own line\n").unwrap();

        let h = idle_host();
        let err = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 9,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: false,
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("edited since"), "{err}");
        assert!(err.contains("notes.md"), "{err}");
        // The sentence has to agree with the number it names. The first version of this
        // refusal printed "accept that 1 it stay as they are" for one file, which reads like
        // noise at exactly the clause that says what did *not* happen — and a selftest only
        // caught it because a sabotage made the sentence the failure text.
        assert!(err.contains("1 file was edited since"), "{err}");
        assert!(err.contains("notes.md stays as it is"), "{err}");
        assert!(!err.contains("stay as they are"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "mine, and my own line\n",
            "a partial revert nobody agreed to is not a revert"
        );

        let out = revert(
            &s,
            &h,
            &RevertReq {
                session: sess,
                message_id: 9,
                through_latest: false,
                shown: vec!["notes.md".into()],
                overwrite_edited: true,
            },
        )
        .unwrap();
        assert_eq!(out.report.restored, 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "yours\n");
        std::fs::remove_dir_all(&ws).ok();
    }


    /// The refusal names every file it is leaving alone, and agrees in number with them.
    /// One file reads "stays as it is"; two read "stay as they are". Getting this wrong is
    /// not cosmetic: the clause is the one that says what did *not* happen, and a sentence
    /// that reads like noise is a sentence a person skips past before pressing again.
    /// A conversation that hit the cap must say so on the card, not hand back a shorter list and
    /// let it read as the whole history of the folder. The number comes from the record — the same
    /// record that dropped the copies — so the sentence is a fact about this conversation rather than
    /// a hedge about the feature.
    #[tokio::test]
    async fn a_pruned_conversation_says_what_was_discarded() {
        let ws = tmp("pruned_note");
        let store = Store::open(&ws.join("app.db")).unwrap();
        let sess = store.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let dir = snapshot_dir(&ws);
        // One turn, many distinct files, so the cap has something to evict.
        let mut marks = Vec::new();
        for i in 0..(CHECKPOINT_CAP + 8) {
            let name = format!("f{i}.md");
            let f = ws.join(&name);
            std::fs::write(&f, format!("before {i}\n")).unwrap();
            let pre = PreState::of(&f);
            std::fs::write(&f, format!("after {i}\n")).unwrap();
            marks.push(capture(Some(&dir), &ws, &f, &pre, Some(format!("after {i}\n").as_bytes())).unwrap());
        }
        store.record_checkpoints(sess, 1, &marks).unwrap();
        store.stamp_checkpoints_turn(sess, 1, 3).unwrap();

        let ov = store.checkpoint_overview(sess).unwrap();
        assert!(ov.pruned > 0, "the cap should have evicted: {}", ov.total);

        let h = idle_host();
        let p = preview(
            &store,
            &RevertReq {
                session: sess,
                message_id: 3,
                through_latest: false,
                shown: Vec::new(),
                overwrite_edited: false,
            },
        )
        .expect("the surviving copies are still a plan");
        assert_eq!(p.pruned, ov.pruned);
        assert!(p.note.contains("discarded to stay under the limit"), "{}", p.note);
        assert!(p.note.contains("The transcript is untouched"), "{}", p.note);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn two_edited_files_are_named_as_two_in_the_refusal() {
        let ws = tmp("edited_two");
        let dir = snapshot_dir(&ws);
        let store = Store::in_memory().unwrap();
        let sess = store.create_session("t", ws.to_str().unwrap(), "m").unwrap();
        let mut marks = Vec::new();
        for name in ["a.md", "b.md"] {
            let f = ws.join(name);
            std::fs::write(&f, "yours
").unwrap();
            let pre = PreState::of(&f);
            std::fs::write(&f, "theirs
").unwrap();
            marks.push(capture(Some(&dir), &ws, &f, &pre, Some(b"theirs")).unwrap());
        }
        store.record_checkpoints(sess, 1, &marks).unwrap();
        store.stamp_checkpoints_turn(sess, 1, 7).unwrap();
        // The person edits both afterwards.
        std::fs::write(ws.join("a.md"), "a, mine
").unwrap();
        std::fs::write(ws.join("b.md"), "b, mine
").unwrap();

        let h = idle_host();
        let err = revert(
            &store,
            &h,
            &RevertReq {
                session: sess,
                message_id: 7,
                through_latest: false,
                shown: vec!["a.md".into(), "b.md".into()],
                overwrite_edited: false,
            },
        )
        .err()
        .unwrap();
        assert!(err.contains("2 files were edited since"), "{err}");
        assert!(err.contains("a.md, b.md"), "{err}");
        assert!(err.contains("stay as they are"), "{err}");
        assert!(!err.contains("stays as it is"), "{err}");
        assert_eq!(std::fs::read_to_string(ws.join("a.md")).unwrap(), "a, mine\n");
        assert_eq!(std::fs::read_to_string(ws.join("b.md")).unwrap(), "b, mine\n");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_file_too_big_to_copy_is_not_checkpointed() {
        let ws = tmp("big");
        let dir = snapshot_dir(&ws);
        let f = ws.join("huge.bin");
        std::fs::write(&f, vec![b'a'; (SNAP_BYTES_LIMIT + 1) as usize]).unwrap();
        assert!(read_bytes(&f).is_none(), "a huge file is not copied around");
        // And it is *not* treated as absent: a revert that thought a 200 MB file was one
        // the turn wrote would delete it.
        let pre = PreState::of(&f);
        assert!(matches!(pre, PreState::TooBig(_)));
        let m = capture(Some(&dir), &ws, &f, &pre, Some(b"x")).unwrap();
        assert_eq!(m.kind, Kind::Modified, "it was there, and it changed");
        assert!(m.hash.is_empty(), "no copy was taken");
        assert!(m.note.as_deref().unwrap().contains("past what this copies"), "{}", m.note.unwrap());
        let rows = [row(1, "huge.bin", Kind::Modified, "", &hash_of(b"x"))];
        let p = plan(&ws, Some(&dir), &rows);
        assert!(p[0].copy_missing, "the card knows there is nothing to restore");
        assert_eq!(apply(&ws, &dir, &rows, true).restored, 0);
        assert!(f.exists(), "the file is still there");
        std::fs::remove_dir_all(&ws).ok();
    }
}

// ---------------------------------------------------------------------------
// Asking first, then doing it
// ---------------------------------------------------------------------------

/// A revert request, from whichever window or device asked.
///
/// `shown` is the identity discipline this app already uses for editing a message and for
/// branching one: the caller names the row it means *and* repeats what it put in front of
/// the person. Here that is the list of paths the card displayed. A second window can run
/// a turn between the preview and the answer, which would add paths to the same scope — and
/// a revert that quietly includes a file the person never saw would be overwriting their
/// work under a card that promised something else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertReq {
    pub session: i64,
    /// The answer row whose turn is being undone.
    pub message_id: i64,
    /// False: only that turn. True: that turn and every turn after it — "back to before
    /// turn N", which is what the row menu offers.
    pub through_latest: bool,
    /// The paths the card listed, as it listed them.
    pub shown: Vec<String>,
    /// The answer to "these files have been edited since: overwrite them anyway?"
    pub overwrite_edited: bool,
}

/// What the card shows before anything is touched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertPreview {
    pub session: i64,
    pub message_id: i64,
    /// The turn the scope starts at, so the card can name it as the row it names.
    pub turn: i64,
    pub scope: String,
    pub workspace: String,
    pub items: Vec<PlanItem>,
    /// How many paths are already different from what the turn left behind.
    pub edited_since: usize,
    /// How many have no copy left to restore.
    pub missing_copy: usize,
    /// Copies this conversation lost to the per-conversation cap. Non-zero means the list below is
    /// what is left, not everything the conversation ever changed, and the note says so.
    pub pruned: usize,
    pub note: String,
}

/// What a revert did, and what it left alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertOutcome {
    pub session: i64,
    pub message_id: i64,
    pub scope: String,
    pub report: RevertReport,
    pub note: String,
    /// Message rows still in the transcript. Revert never edits them, and saying the
    /// number is how the UI can prove that rather than assert it.
    pub messages: usize,
}

pub fn scope_name(through_latest: bool) -> &'static str {
    if through_latest {
        "that turn and every turn after it"
    } else {
        "that turn only"
    }
}

fn rows_for(store: &dyn crate::store::StoreOps, req: &RevertReq) -> Result<Vec<CheckpointRow>, String> {
    let rows = if req.through_latest {
        store
            .checkpoints_from_message(req.session, req.message_id)
            .map_err(|e| e.to_string())?
    } else {
        store
            .checkpoints_for_message(req.session, req.message_id)
            .map_err(|e| e.to_string())?
    };
    Ok(rows)
}

/// The card's content, computed from the store and the disk. Nothing is modified.
pub fn preview(store: &dyn crate::store::StoreOps, req: &RevertReq) -> Result<RevertPreview, String> {
    let workspace = store
        .session_workspace(req.session)
        .map_err(|e| format!("conversation {} is not in the record: {e}", req.session))?;
    if workspace.trim().is_empty() {
        return Err(format!(
            "conversation {} has no folder, so there are no files to put back",
            req.session
        ));
    }
    let rows = rows_for(store, req)?;
    if rows.is_empty() {
        return Err(format!(
            "that turn touched no files this app kept a copy of, so there is nothing to revert"
        ));
    }
    let dir = snapshot_dir(std::path::Path::new(&workspace));
    let items = plan(std::path::Path::new(&workspace), Some(&dir), &rows);
    let edited_since = items.iter().filter(|i| i.edited_since).count();
    let missing_copy = items.iter().filter(|i| i.copy_missing).count();
    let turn = rows[0].turn;
    // The cap evicts old copies, and a card that lists what survived without saying anything was
    // dropped reads like a complete history of the folder. The count comes from the record, so the
    // card can say "older copies were discarded" as a fact rather than a hedge.
    let pruned = store
        .checkpoint_overview(req.session)
        .map(|o| o.pruned)
        .unwrap_or(0);
    let mut note = format!(
        "{} file{} would be reverted: {}. The transcript is untouched — no message is changed or removed.",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        scope_name(req.through_latest)
    );
    if pruned > 0 {
        note.push_str(&format!(
            " {} older cop{} discarded to stay under the limit: this is what is left of that conversation's file history.",
            pruned,
            if pruned == 1 { "y was" } else { "ies were" }
        ));
    }
    Ok(RevertPreview {
        session: req.session,
        message_id: req.message_id,
        turn,
        scope: scope_name(req.through_latest).to_string(),
        workspace,
        items,
        edited_since,
        missing_copy,
        pruned,
        note,
    })
}

/// Do it. Refused while a turn is running, and refused if the scope has changed since the
/// card was drawn.
pub fn revert(
    store: &dyn crate::store::StoreOps,
    host: &dyn crate::host::HostOps,
    req: &RevertReq,
) -> Result<RevertOutcome, String> {
    if host.state().busy {
        return Err(format!(
            "a turn is running, so nothing was reverted: a turn in flight is writing files \
                     right now, and putting copies back underneath it would restore the wrong thing. \
                     Stop it, or wait for it to finish."
        ));
    }
    let preview = preview(store, req)?;
    let now: Vec<String> = preview.items.iter().map(|i| i.path.clone()).collect();
    if now != req.shown {
        // Same shape as the edit and branch refusals: name what is there now, change
        // nothing. The person decides whether the new list is still what they meant.
        return Err(format!(
            "the files in that scope are not the ones this card listed. The card said {}; \
             the record now says {}. Nothing was reverted — ask again to see \
             the new list.",
            listed(&req.shown),
            listed(&now)
        ));
    }
    if preview.edited_since > 0 && !req.overwrite_edited {
        // Not a refusal: the apply skips those paths. But a card that listed them has to
        // have offered the choice, and the caller who never showed one should not be
        // quietly getting a partial revert.
        let names: Vec<String> = preview
            .items
            .iter()
            .filter(|i| i.edited_since)
            .map(|i| i.path.clone())
            .collect();
        // One sentence, agreeing in number with what it names. The first version printed
        // "accept that 1 it stay as they are", which is the sort of clause a reader skips
        // because it reads like noise — and this clause is the part that tells them what did
        // *not* happen.
        return Err(format!(
            "{} {} edited since that turn ({}) and would be left alone. Ask again with the \
                     choice offered, or revert without overwriting and accept that {} {} as {}",
            names.len(),
            if names.len() == 1 { "file was" } else { "files were" },
            names.join(", "),
            names.join(", "),
            if names.len() == 1 { "stays" } else { "stay" },
            if names.len() == 1 { "it is" } else { "they are" }
        ));
    }

    let workspace = std::path::PathBuf::from(&preview.workspace);
    let dir = snapshot_dir(&workspace);
    let rows = rows_for(store, req)?;
    let report = apply(&workspace, &dir, &rows, req.overwrite_edited);
    let messages = store.message_count(req.session).map_err(|e| e.to_string())?;
    let note = format!(
        "Reverted {} file{} ({}). The transcript is untouched: conversation {} still holds {} message{}.",
        report.touched.len(),
        if report.touched.len() == 1 { "" } else { "s" },
        report_note(&report),
        req.session,
        messages,
        if messages == 1 { "" } else { "s" }
    );
    Ok(RevertOutcome {
        session: req.session,
        message_id: req.message_id,
        scope: preview.scope,
        report,
        note,
        messages,
    })
}

fn listed(paths: &[String]) -> String {
    if paths.is_empty() {
        return "no files".to_string();
    }
    paths
        .iter()
        .map(|p| format!("{p:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}
