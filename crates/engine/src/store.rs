//! SQLite persistence: sessions, messages, full-text search, telemetry.
//!
//! Two choices worth defending:
//!
//!   * **bundled SQLite.** A desktop app that depends on the distro's libsqlite3
//!     can find FTS5 missing at *runtime*, on the user's machine, as a crash
//!     rather than a build error. Compiling it in costs build time and buys
//!     certainty.
//!   * **WAL mode.** The UI reads the session list while the agent is appending
//!     messages. Without WAL that is a busy-database error under load.
//!
//! The FTS table duplicates message text rather than using an external-content
//! table. It costs some disk; it buys trivially correct triggers and working
//! `snippet()` without the rowid gymnastics external content requires. For chat
//! transcripts that is the right trade.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::provider::{Message, Role};
use crate::telemetry::TurnTelemetry;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    /// The schema we opened is not one we know how to read.
    Migration { from: i64, to: i64 },
    /// FTS5 is not available. With `bundled` this should be unreachable; if it
    /// ever fires, it means someone turned the feature off.
    FtsUnavailable(String),
    /// A value the record refuses to hold. The store is the one place every client agrees, so a
    /// shape check written here is enforced on the desktop, the browser and the phone at once —
    /// and a value that never enters the row cannot come back out as markup, a style attribute or
    /// a claim about a folder.
    Invalid(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Migration { from, to } => {
                write!(f, "database schema v{from} is newer than this build supports (v{to})")
            }
            StoreError::FtsUnavailable(m) => write!(f, "full-text search unavailable: {m}"),
            StoreError::Invalid(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// Current schema version. Bump with a matching arm in `migrate`.
// v12: `ui_prefs` — the display settings a client sets for itself. It lives in this
// database and not in a browser's localStorage because localStorage is per-origin: a gateway
// on another port is another store, and the desktop, the browser and the phone are three
// origins between them. A preference only one of the three remembers is not a setting.
// v15: `checkpoints` and `checkpoint_prunes` — what a file call overwrote, so a turn can be
// undone. The bytes live in a content-addressed folder under the workspace (see
// `crate::checkpoint`); these rows are the index that makes them findable after a restart.
// A copy the app cannot locate again is not a checkpoint, and the database is the thing that
// survives restarts. `checkpoint_prunes` records eviction under the per-conversation cap, so
// the transcript can say "older snapshots were pruned" instead of offering fewer reverts and
// not explaining why.
// v16: lineage. Branching made a second conversation out of part of one, and nothing recorded
// which row came from which or where the cut was made — the sidebar showed a branch as an
// unrelated row, and `get_tree` on the pi core was read for nothing but a leaf id. Four columns
// on the session (`parent`, `branch_role`, `branch_row`, `leaf_row`) and one on the message
// (`parent_row`), so a conversation is a **tree of rows** in both cores and one tree is readable
// whichever core made the branch. `parent_row` defaults to NULL, which is read as "the row before
// it" — the rule every pre-v16 database already lived by — so an old store reads unchanged.
// v17: why a copy is gone. `checkpoint_prunes` recorded only the per-conversation cap, and the
// ledger adds a second way copies leave — a person forgetting them on request. Summing every
// notice and printing "discarded to stay under the limit" would be a sentence about a limit that
// nobody reached, so each notice now says which of the two it is, and the revert card's count
// stays the cap's own.
const SCHEMA_VERSION: i64 = 17;

/// A workspace the user has registered (or that holds sessions).
/// How many conversations one folder can be asked for at a time. See
/// `Store::sessions_in` for why the cap exists and what a caller owes the user when it is
/// reached.
pub const SESSIONS_IN_CAP: i64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub path: String,
    /// Optional display name. Empty means "show the folder name", which is what
    /// people want nine times out of ten and why it is not a required field.
    pub label: String,
    /// A colour for this folder's mark in the sidebar, as `#rrggbb`, or empty for "no colour".
    ///
    /// It is stored rather than kept in a window's localStorage for the same reason the label is:
    /// the sidebar is a record of the person's own arrangement, and a phone or a second window that
    /// cannot tell the inventory folder from the notes folder is reading a list the person already
    /// fixed up once. The palette is the client's business; the record only insists the value is a
    /// colour, so a stale or hand-edited config cannot put arbitrary text into a style attribute.
    pub color: String,
    pub sessions: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub title: String,
    pub workspace: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
    /// Which mode made this conversation: "cowork" (tools, a folder, approval cards)
    /// or "chat" (nothing but the model). It is recorded once, at creation, because it
    /// describes how the conversation came to exist — a chat opened while the app happens
    /// to be in cowork mode is still a chat, and it belongs in the chat list.
    pub persona: String,
    /// Whether this conversation may reach the web — the chat-mode `web_search` / `web_fetch` pair,
    /// mounted for the model and described in its prompt.
    ///
    /// Stored per conversation because that is what the person is choosing: "let *this* chat look
    /// things up". A conversation whose chip remembers its own state is also the only way the host
    /// can reopen a chat and mount the tools it had last time, instead of answering from a process
    /// that quietly lost its network reach.
    #[serde(default)]
    pub web_on: bool,
    /// The conversation this one was branched from. `None` means it is a **root** — a
    /// conversation nobody cut out of another — which is a fact, not a missing value: a row
    /// with no lineage renders as the start of its own tree.
    #[serde(default)]
    pub parent: Option<i64>,
    /// Which kind of row the child was cut at (`user` / `assistant`, empty when there is no
    /// parent). It decides what the branch point means: cut at one of the person's messages,
    /// the branch holds what came *before* it; cut at an answer, the branch holds that answer.
    #[serde(default)]
    pub branch_role: String,
    /// The parent's row the cut was made at. `None` with a parent means the branch was made
    /// before this column existed, and the tree says so rather than guessing a row.
    #[serde(default)]
    pub branch_row: Option<i64>,
    /// The node the next answer reads through — the leaf of this conversation's own row tree.
    /// `None` means "the newest row", which is what every conversation this app has ever written
    /// had. It moves on a leaf switch and advances on every send; it copies nothing and deletes
    /// nothing, which is the whole difference between that and a rewind.
    #[serde(default)]
    pub leaf_row: Option<i64>,
}

/// The session row, in one place. Three readers (`session`, `list_sessions`, `sessions_in`) once
/// spelled the same eleven columns separately, which meant a twelfth column — lineage — had to be
/// added three times and could be added twice.
const SESSION_SELECT: &str = "SELECT s.id, s.title, s.workspace, s.model, s.created_at, s.updated_at, \
                        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id), \
                        s.persona, s.web_on, s.parent, s.branch_role, s.branch_row, s.leaf_row \
                 FROM sessions s WHERE s.id = ?1";

/// The same columns with the clause the caller supplies, so the window and the folder query
/// cannot disagree about what a `Session` is made of.
///
/// A function rather than a `&str` template with a `{}` hole: `format!` needs a literal format
/// string, and a constant holding one is exactly the drift this select exists to prevent — a query
/// written inline somewhere else, missing the lineage columns, reading the same rows as a
/// conversation with no parent.
fn sessions_query(clause: &str) -> String {
    format!(
        "SELECT s.id, s.title, s.workspace, s.model, s.created_at, s.updated_at, \
         (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id), \
         s.persona, s.web_on, s.parent, s.branch_role, s.branch_row, s.leaf_row \
         FROM sessions s {clause}"
    )
}

fn session_from_row(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        title: r.get(1)?,
        workspace: r.get(2)?,
        model: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        message_count: r.get(6)?,
        persona: r.get(7)?,
        web_on: r.get::<_, i64>(8)? != 0,
        parent: r.get::<_, Option<i64>>(9)?,
        branch_role: r.get::<_, String>(10)?,
        branch_row: r.get::<_, Option<i64>>(11)?,
        leaf_row: r.get::<_, Option<i64>>(12)?,
    })
}

/// One checkpoint index row: a path a file call touched, and where its copy lives.
///
/// `path` is workspace-relative, the same rule as a file badge — an absolute path here
/// would let anything that edits the database point a revert at any file on the machine
/// (`crate::checkpoint::resolve` refuses it on the way back out).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckpointRow {
    pub id: i64,
    pub session: i64,
    pub turn: i64,
    /// The answer row this call belongs to, once the turn has been persisted. Null while
    /// the turn is still running, which is why a turn's revert control waits for storage
    /// rather than guessing at the row it will become.
    pub message_id: Option<i64>,
    pub path: String,
    pub kind: crate::checkpoint::Kind,
    /// Content-addressed blob key; empty when there was nothing to copy (a file the turn
    /// created is reverted by deleting it).
    pub hash: String,
    /// What the file held right after the call. Revert compares the file on disk against
    /// this to say "edited since".
    pub after_hash: String,
    pub bytes: i64,
    pub created_at: i64,
}

/// A time the per-conversation cap evicted copies. Kept as a row because the transcript
/// has to be able to say, after a restart, why an old turn offers no revert.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckpointPrune {
    pub id: i64,
    pub session: i64,
    pub dropped: usize,
    pub note: String,
    /// `cap` — the per-conversation limit evicted them — or `forget` — a person asked for
    /// this conversation's copies to be dropped. The two read differently: one is a limit
    /// the app enforced, the other is a decision the person made, and a card that mixed
    /// them would tell someone their own request was an eviction.
    pub reason: String,
    pub created_at: i64,
}

/// What a conversation can undo, in the shape the transcript reads.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckpointOverview {
    /// Per answer row: how many paths that turn can be reverted to.
    pub per_message: Vec<TurnCheckpoints>,
    pub total: usize,
    /// Copies dropped by the cap, summed over every eviction this conversation suffered.
    pub pruned: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnCheckpoints {
    pub message_id: i64,
    pub files: usize,
}

/// One bounded page of transcript history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessagesPage {
    pub messages: Vec<StoredMessage>,
    pub has_older: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub session_id: i64,
    pub role: String,
    pub content: String,
    pub reasoning: String,
    /// Set for role=tool rows: which call this result answers.
    pub tool_call_id: Option<String>,
    pub tool_calls_json: String,
    /// Anthropic attests thinking blocks with a signature and will reject a
    /// replayed block without one. Losing this makes a resumed session 400.
    pub reasoning_signature: String,
    /// Milliseconds the model spent thinking, 0 when it did not think.
    pub reasoning_ms: u64,
    /// Files this message produced in the workspace, for the UI. Never sent to
    /// the model.
    pub files: Vec<crate::tools::OutputFile>,
    /// What a file tool changed, for the tool card: `+N -M` in the header and the
    /// diff when it is expanded. Persisted so a reopened session shows the same
    /// card as the live one; never sent to the model.
    pub changes: Vec<crate::tools::changes::FileChange>,
    /// Metadata only: mime, filename, decoded size. The payload lives in the
    /// attachments table and is loaded by `history()`, never by transcript reads.
    pub attachments: Vec<crate::provider::AttachmentMeta>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: i64,
    pub message_id: i64,
    pub role: String,
    pub created_at: i64,
    /// FTS5 snippet with the match marked.
    pub snippet: String,
    pub rank: f64,
}

/// One remembered passage, with the conversation it came from attached to it.
///
/// The sidebar's `search` answers a different question: there a hit is a row somebody can
/// click. A model asking what it has already discussed needs the conversation *named*,
/// because "this is from the conversation about the Q3 numbers" is the only basis it has for
/// deciding to ask for the whole thing — and a passage with no owner is a quote from nowhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallHit {
    pub session_id: i64,
    pub title: String,
    pub persona: String,
    pub message_id: i64,
    pub role: String,
    pub created_at: i64,
    /// FTS5 snippet with the match marked.
    pub snippet: String,
    pub rank: f64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ToolCtx` derives Debug and now holds an `Arc<Store>`. A live connection has no
        // useful Debug — printing one would imply the caller can see SQL state it cannot.
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::setup(conn)
    }

    /// Which file this record lives in, as SQLite itself says. `:memory:` for an in-memory store.
    ///
    /// Asked of the database rather than recomputed from the environment, because the About page
    /// exists to answer "is my history at that path?" — and a path derived from `CWOFFEE_DATA` by the
    /// page could name a file this store never opened.
    pub fn db_path(&self) -> String {
        let conn = self.conn.lock().unwrap();
        conn.query_row("PRAGMA database_list", [], |r| r.get::<_, String>(2))
            .unwrap_or_default()
    }

    /// Raw connection, for this crate's own tests only.
    ///
    /// A pre-v16 database has to be *built* by hand to test the migration — a store that has lineage
    /// columns but no lineage written into them is exactly what an upgraded file looks like, and no
    /// shipped code path can produce that state on purpose. Keeping this `cfg(test)` is what stops
    /// a feature from reaching through the same hole to hand-write rows.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::setup(conn)
    }

    fn setup(conn: Connection) -> Result<Self, StoreError> {
        // WAL: the sidebar reads while the agent writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Wait for the write lock rather than failing on it. There is a moment where two
        // Cwoffees legitimately hold this file at once: closing the window while "keep devices
        // working" is on starts the standalone gateway, and that process opens the same store a
        // few hundred milliseconds before the window finishes letting go. A busy error there is a
        // gateway that dies at boot, which is indistinguishable from a setting that was ignored.
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if current > SCHEMA_VERSION {
            return Err(StoreError::Migration { from: current, to: SCHEMA_VERSION });
        }
        migrate(&conn, current)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- sessions ---------------------------------------------------------

    // ---- workspaces -------------------------------------------------------

    /// Register a workspace so it shows in the sidebar even with no sessions in
    /// it. Idempotent: the sidebar calls this on every session move.
    pub fn add_workspace(&self, path: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO workspaces (path, label, added_at) VALUES (?1, '', ?2)",
            params![path, now_ts()],
        )?;
        Ok(())
    }

    /// Forget registered folders that are gone and hold nothing, and say how many went.
    ///
    /// A folder row is kept for its label and colour, which means a row survives the folder
    /// disappearing — a drive that is simply unmounted today should not lose either. But a run that
    /// registers a scratch folder and then deletes it leaves a row the person never added and can
    /// never open: the probes did exactly that, and the sidebar ended up with a screen of `ws` rows
    /// holding zero conversations each. Two things have to be true to forget a row: the folder is not
    /// there to open, *and* nothing in the database still says it lives there. A workspace with
    /// sessions is never dropped, because history is worth more than a missing mount.
    pub fn prune_dead_workspaces(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT w.path, (SELECT COUNT(*) FROM sessions s WHERE s.workspace = w.path) \
             FROM workspaces w",
        )?;
        let dead: Vec<String> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .filter_map(|row| match row {
                Ok((path, n)) if n == 0 && !std::path::Path::new(&path).is_dir() => Some(path),
                _ => None,
            })
            .collect();
        drop(stmt);
        let mut gone = 0usize;
        for path in &dead {
            gone += conn.execute("DELETE FROM workspaces WHERE path = ?1", params![path])?;
        }
        Ok(gone)
    }

    /// Drop the sidebar entry. Sessions that live in the workspace are untouched -
    /// removing a folder from the list must not delete anybody's work, and the
    /// sessions still say where they live, so it reappears if they do.
    pub fn remove_workspace(&self, path: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM workspaces WHERE path = ?1", params![path])?)
    }

    /// One display preference, as stored. `None` means nobody set it, which is not the
    /// same thing as its default: the default belongs to `crate::prefs`, not to this table.
    pub fn ui_pref(&self, key: &str) -> Result<Option<String>, StoreError> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT value FROM ui_prefs WHERE key = ?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// What every client has set, as written. Only `crate::prefs` knows what a value means.
    pub fn ui_prefs_all(&self) -> Result<Vec<(String, String)>, StoreError> {
        let c = self.conn.lock().unwrap();
        let mut st = c
            .prepare("SELECT key, value FROM ui_prefs ORDER BY key")
            ?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            ?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?)
        }
        Ok(out)
    }

    pub fn set_ui_pref(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO ui_prefs (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            params![key, value, now_ts()],
        )
        ?;
        Ok(())
    }

    /// Back to unset, which is not the same as writing the default: an unset preference
    /// follows a future change of default, and a written one does not.
    pub fn clear_ui_pref(&self, key: &str) -> Result<(), StoreError> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM ui_prefs WHERE key = ?1", params![key])
            ?;
        Ok(())
    }

    /// Name a workspace for the sidebar. Empty means "show the folder name",
    /// which is what people want nine times out of ten.
    pub fn set_workspace_label(&self, path: &str, label: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE workspaces SET label = ?2 WHERE path = ?1",
            params![path, label.trim()],
        )?;
        Ok(n)
    }

    /// Colour a folder's mark in the sidebar. Empty clears it. Returns the rows touched, which is 0
    /// when the folder has never been added — the sidebar can show a group for a folder that has no
    /// row of its own, and colouring that would silently write nothing.
    pub fn set_workspace_color(&self, path: &str, color: &str) -> Result<usize, StoreError> {
        let color = color.trim().to_ascii_lowercase();
        if !color.is_empty() && !is_hex_color(&color) {
            return Err(StoreError::Invalid(format!(
                "{color} is not a folder colour: use empty to clear it, or #rrggbb like #c08a2e"
            )));
        }
        // The row is created on demand, like a label: a folder the person can see is a folder the
        // record can hold a colour for, and requiring them to "add" it first is a dance.
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO workspaces (path, label, added_at) VALUES (?1, '', ?2)",
            params![path, now_ts()],
        )?;
        let n = conn.execute(
            "UPDATE workspaces SET color = ?2 WHERE path = ?1",
            params![path, color],
        )?;
        Ok(n)
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceEntry>, StoreError> {
        let conn = self.conn.lock().unwrap();
        // Registered paths unioned with paths that actually hold sessions: the
        // list must reflect reality, so a workspace you never added but have
        // sessions in still shows up, and one you added and emptied still shows
        // too.
        let mut stmt = conn.prepare(
            "SELECT p.path,
                    COALESCE((SELECT label FROM workspaces w WHERE w.path = p.path), '') AS label,
                    COALESCE((SELECT color FROM workspaces w WHERE w.path = p.path), '') AS color,
                    (SELECT COUNT(*) FROM sessions s WHERE s.workspace = p.path) AS n
             FROM (
                 SELECT DISTINCT workspace AS path FROM sessions WHERE workspace <> ''
                 UNION
                 SELECT path FROM workspaces
             ) p
             ORDER BY COALESCE((SELECT added_at FROM workspaces w WHERE w.path = p.path), 0) ASC,
                      p.path ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkspaceEntry {
                path: r.get(0)?,
                label: r.get(1)?,
                // A value written by hand or by an older build that is not a colour reads as empty
                // here rather than reaching a `style` attribute: the record may be old, but what it
                // hands the UI still has to be safe to interpolate.
                color: match r.get::<_, String>(2)? {
                    c if c.is_empty() || is_hex_color(&c) => c,
                    _ => String::new(),
                },
                sessions: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn create_session(&self, title: &str, workspace: &str, model: &str) -> Result<i64, StoreError> {
        let title = title_or_default(title);
        let now = now_ts();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (title, workspace, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![title, workspace, model, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn rename_session(&self, id: i64, title: &str) -> Result<(), StoreError> {
        // Clearing a name must not blank the row out of usefulness; it falls back
        // to the default rather than storing "".
        let title = title_or_default(title);
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1", params![id, title, now_ts()])?;
        Ok(())
    }

    /// Record which directory a session's tools operate in.
    pub fn set_session_workspace(
        &self,
        id: i64,
        path: &std::path::Path,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET workspace = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![id, path.to_string_lossy().to_string(), now_ts()],
        )?;
        Ok(())
    }

    /// Record the mode a conversation belongs to. The default is "cowork", and anything
    /// that is not "chat" normalises to it: an unknown value from a stale client must not
    /// make a row vanish from both lists.
    pub fn set_session_persona(&self, id: i64, persona: &str) -> Result<(), StoreError> {
        let p = if persona == "chat" { "chat" } else { "cowork" };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET persona = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, p, now_ts()],
        )?;
        Ok(())
    }

    /// Choose whether this conversation may reach the web. See `Session::web_on`.
    pub fn set_session_web(&self, id: i64, on: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET web_on = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, on as i64, now_ts()],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: i64) -> Result<(), StoreError> {
        // Read the copy keys before the rows go: the cascade deletes them, and a blob whose
        // last index row is gone has to be deleted from the disk too, or the workspace keeps
        // growing copies for conversations that no longer exist.
        let blobs = self.checkpoint_blob_keys(id)?;
        let workspace = self.session_workspace(id).unwrap_or_default();
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM messages_fts WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM turns WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM checkpoints WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM checkpoint_prunes WHERE session_id = ?1", params![id])?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        }
        self.drop_unreferenced_blobs(&workspace, &blobs);
        Ok(())
    }

    /// The folder a conversation works in, as recorded. Checkpoints need it to find the
    /// snapshot store, and revert needs it to resolve stored paths — both without asking a
    /// caller to repeat a path the database already knows.
    pub fn session_workspace(&self, session_id: i64) -> Result<String, StoreError> {
        let conn = self.conn.lock().unwrap();
        let w: String = conn.query_row("SELECT workspace FROM sessions WHERE id = ?1", params![session_id], |r| r.get(0))?;
        Ok(w)
    }

    /// Record what a turn's file calls overwrote. Returns how many rows were written.
    ///
    /// Marks come from the tool layer (`ToolResult::checkpoints`) rather than being written
    /// by the tool itself: only the host knows which turn is running, and the turn number is
    /// what makes "revert files to before turn N" mean anything. Both cores hand marks in
    /// from their own turn counter, which is why one implementation covers both.
    pub fn record_checkpoints(
        &self,
        session_id: i64,
        turn: i64,
        marks: &[crate::checkpoint::Mark],
    ) -> Result<usize, StoreError> {
        let now = chrono::Utc::now().timestamp();
        let mut written = 0usize;
        {
            let conn = self.conn.lock().unwrap();
            for m in marks.iter() {
                conn.execute(
                    "INSERT INTO checkpoints (session_id, turn, message_id, path, kind, hash, after_hash, bytes, created_at)
                     VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        session_id,
                        turn,
                        m.path,
                        m.kind.as_str(),
                        m.hash,
                        m.after_hash,
                        m.bytes as i64,
                        now
                    ],
                )?;
                written += 1;
            }
        }
        if written > 0 {
            self.prune_checkpoints(session_id)?;
        }
        Ok(written)
    }

    /// Keep the conversation's index inside the cap, oldest first, and record that it
    /// happened. Deleting index rows is not enough: a blob nothing points at is a copy of
    /// somebody's file kept for a change nobody can revert any more.
    fn prune_checkpoints(&self, session_id: i64) -> Result<(), StoreError> {
        let cap = crate::checkpoint::CHECKPOINT_CAP as i64;
        let mut dropped: Vec<String> = Vec::new();
        let n: i64;
        {
            let conn = self.conn.lock().unwrap();
            let held: i64 = conn.query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            if held <= cap {
                return Ok(());
            }
            // Drop past the cap by a batch rather than to the cap exactly: evicting one
            // row per call would log forty separate notices for one long conversation and
            // touch the disk forty times. One notice per batch is a sentence a person can
            // read, and the copies they would have used go together.
            let excess = held - cap + (crate::checkpoint::CHECKPOINT_BATCH as i64 - 1);
            n = excess;
            let mut stmt = conn.prepare(
                "SELECT hash FROM checkpoints WHERE session_id = ?1 AND hash <> '' ORDER BY id ASC LIMIT ?2",
            )?;
            for row in stmt.query_map(params![session_id, excess], |r| r.get::<_, String>(0))? {
                dropped.push(row?);
            }
            drop(stmt);
            conn.execute(
                "DELETE FROM checkpoints WHERE session_id = ?1 AND id IN (
                     SELECT id FROM checkpoints WHERE session_id = ?1 ORDER BY id ASC LIMIT ?2
                 )",
                params![session_id, excess],
            )?;
        }
        let workspace = self.session_workspace(session_id).unwrap_or_default();
        self.drop_unreferenced_blobs(&workspace, &dropped);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO checkpoint_prunes (session_id, dropped, note, reason, created_at) VALUES (?1, ?2, ?3, 'cap', ?4)",
            params![
                session_id,
                n,
                format!(
                    "{n} older cop{} pruned: this conversation has reached {cap} checkpoints, so the oldest ones went first",
                    if n == 1 { "y was" } else { "ies were" }
                ),
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(())
    }

    /// Keys this conversation holds. Read before a delete, because the cascade takes the
    /// rows and the disk copy is a separate thing to clean up.
    pub fn checkpoint_blob_keys(&self, session_id: i64) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT hash FROM checkpoints WHERE session_id = ?1 AND hash <> ''",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![session_id], |r| r.get::<_, String>(0))? {
            out.push(row?);
        }
        Ok(out)
    }

    /// Delete copies no index row points at any more.
    ///
    /// Checked across every conversation, not just the one being pruned: content-addressing
    /// means two conversations that overwrote the same bytes share one copy, and taking it
    /// out because one of them ended would leave the other unable to revert.
    pub fn drop_unreferenced_blobs(&self, workspace: &str, hashes: &[String]) {
        if workspace.is_empty() || hashes.is_empty() {
            return;
        }
        let dir = crate::checkpoint::snapshot_dir(std::path::Path::new(workspace));
        let conn = self.conn.lock().unwrap();
        for h in hashes {
            let still: i64 = conn
                .query_row("SELECT COUNT(*) FROM checkpoints WHERE hash = ?1", params![h], |r| r.get(0))
                .unwrap_or(0);
            if still > 0 {
                continue;
            }
            let _ = std::fs::remove_file(crate::checkpoint::blob_path(&dir, h));
        }
    }

    fn checkpoint_map(&self, sql: &str, params_list: &[&dyn rusqlite::ToSql]) -> Result<Vec<CheckpointRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params_list, |r| {
            let kind_s: String = r.get(5)?;
            Ok(CheckpointRow {
                id: r.get(0)?,
                session: r.get(1)?,
                turn: r.get(2)?,
                message_id: r.get(3)?,
                path: r.get(4)?,
                kind: crate::checkpoint::Kind::parse(&kind_s).unwrap_or(crate::checkpoint::Kind::Modified),
                hash: r.get(6)?,
                after_hash: r.get(7)?,
                bytes: r.get(8)?,
                created_at: r.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every copy this conversation holds, oldest first.
    pub fn checkpoints_for_session(&self, session_id: i64) -> Result<Vec<CheckpointRow>, StoreError> {
        self.checkpoint_map(
            "SELECT id, session_id, turn, message_id, path, kind, hash, after_hash, bytes, created_at
             FROM checkpoints WHERE session_id = ?1 ORDER BY id ASC",
            &[&session_id],
        )
    }

    /// The copies belonging to one answer row — "revert files from this turn".
    pub fn checkpoints_for_message(&self, session_id: i64, message_id: i64) -> Result<Vec<CheckpointRow>, StoreError> {
        self.checkpoint_map(
            "SELECT id, session_id, turn, message_id, path, kind, hash, after_hash, bytes, created_at
             FROM checkpoints WHERE session_id = ?1 AND message_id = ?2 ORDER BY id ASC",
            &[&session_id, &message_id],
        )
    }

    /// That turn's copies and every later turn's — "revert files to before turn N". The
    /// anchor is the turn the answer row was written in, so a row whose own turn is unknown
    /// cannot anchor a rewind of the disk.
    pub fn checkpoints_from_message(&self, session_id: i64, message_id: i64) -> Result<Vec<CheckpointRow>, StoreError> {
        self.checkpoint_map(
            "SELECT id, session_id, turn, message_id, path, kind, hash, after_hash, bytes, created_at
             FROM checkpoints
             WHERE session_id = ?1
               AND turn >= (SELECT turn FROM checkpoints WHERE session_id = ?1 AND message_id = ?2 LIMIT 1)
             ORDER BY id ASC",
            &[&session_id, &message_id],
        )
    }

    /// Name the answer row a turn's copies belong to, once that row exists. Called at turn
    /// end, next to `set_message_files`, for the same reason: the row is not written until
    /// the answer is finished, and the revert control is addressed by row id.
    pub fn stamp_checkpoints_turn(
        &self,
        session_id: i64,
        turn: i64,
        message_id: i64,
    ) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE checkpoints SET message_id = ?3 WHERE session_id = ?1 AND turn = ?2 AND message_id IS NULL",
            params![session_id, turn, message_id],
        )?;
        Ok(n)
    }

    /// Which turns can be undone, and how much was pruned. The transcript draws a revert
    /// control only where this says there is one, and says "older snapshots pruned" where
    /// the cap has been reached — a control that would do nothing is worse than none.
    pub fn checkpoint_overview(&self, session_id: i64) -> Result<CheckpointOverview, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT message_id, COUNT(*) FROM checkpoints
             WHERE session_id = ?1 AND message_id IS NOT NULL
             GROUP BY message_id ORDER BY message_id ASC",
        )?;
        let mut per_message = Vec::new();
        for row in stmt.query_map(params![session_id], |r| {
            Ok(TurnCheckpoints {
                message_id: r.get(0)?,
                files: r.get::<_, i64>(1)? as usize,
            })
        })? {
            per_message.push(row?);
        }
        drop(stmt);
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare("SELECT id, session_id, dropped, note, reason, created_at FROM checkpoint_prunes WHERE session_id = ?1 AND reason = 'cap' ORDER BY id ASC")?;
        let mut pruned = 0usize;
        for row in stmt.query_map(params![session_id], |r| r.get::<_, i64>(2))? {
            pruned += row? as usize;
        }
        Ok(CheckpointOverview {
            per_message,
            total: total as usize,
            pruned,
        })
    }

    pub fn checkpoint_prunes(&self, session_id: i64) -> Result<Vec<CheckpointPrune>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, dropped, note, reason, created_at FROM checkpoint_prunes WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![session_id], |r| {
            Ok(CheckpointPrune {
                id: r.get(0)?,
                session: r.get(1)?,
                dropped: r.get::<_, i64>(2)? as usize,
                note: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
            })
        })? {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every blob key any conversation's index points at. The snapshot folder is a separate
    /// thing from the index, and the gap between the two is orphan bytes — files on disk that
    /// nothing can find again. Saying that number is how a person can tell a store is being
    /// tidy rather than quietly full.
    pub fn referenced_hashes(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT hash FROM checkpoints WHERE hash <> ''")?;
        let mut out = Vec::new();
        for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
            out.push(row?);
        }
        Ok(out)
    }

    /// Rows for `hash` in conversations *other than* `session`. Content-addressing means two
    /// conversations that wrote the same bytes share one blob, so this is the question behind
    /// "would dropping this one free it?" — and it is not answered by this conversation's rows.
    pub fn rows_pointing_at_outside(&self, hash: &str, session_id: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE hash = ?1 AND session_id <> ?2",
            params![hash, session_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// The conversations that share a folder's snapshot store. `neighbours` in the stats line,
    /// because "whose copies are these" is answered per folder and not per conversation.
    pub fn sessions_sharing_workspace(&self, workspace: &str) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM sessions WHERE workspace = ?1 ORDER BY id ASC")?;
        let mut out = Vec::new();
        for row in stmt.query_map(params![workspace], |r| r.get::<_, i64>(0))? {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drop one conversation's copies: its index rows, and the blobs no other conversation
    /// points at. Returns `(rows, bytes freed, blobs left behind)`.
    ///
    /// Reachability is checked before anything is deleted, and across every conversation:
    /// two conversations that overwrote the same file share one blob, and freeing it because
    /// one of them asked would leave the other unable to revert. What is left is reported
    /// rather than mentioned in passing — a person who asked to free space and freed none
    /// deserves to know why.
    pub fn forget_checkpoints(&self, session_id: i64, workspace: &str) -> Result<(usize, u64, usize), StoreError> {
        if workspace.trim().is_empty() {
            return Err(StoreError::Invalid(format!(
                "conversation {session_id} has no folder, so it keeps no copies"
            )));
        }
        let dir = crate::checkpoint::snapshot_dir(std::path::Path::new(workspace));
        let conn = self.conn.lock().unwrap();
        let mut hashes: Vec<String> = {
            let mut stmt = conn.prepare("SELECT hash FROM checkpoints WHERE session_id = ?1 AND hash <> ''")?;
            let it = stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?;
            it.filter_map(|x| x.ok()).collect()
        };
        // One row per copy, and copies are content-addressed: two turns that wrote the same bytes
        // name one file. Summing the rows here would report twice the bytes that actually leave the
        // disk on a verb whose whole receipt is about what it deleted.
        hashes.sort();
        hashes.dedup();
        let rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        let mut freed = 0u64;
        let mut left = 0usize;
        for h in &hashes {
            let still: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM checkpoints WHERE hash = ?1 AND session_id <> ?2",
                    params![h, session_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if still > 0 {
                left += 1;
                continue;
            }
            freed += std::fs::metadata(crate::checkpoint::blob_path(&dir, h))
                .map(|m| m.len())
                .unwrap_or(0);
        }
        conn.execute(
            "DELETE FROM checkpoints WHERE session_id = ?1",
            params![session_id],
        )?;
        drop(conn);
        self.drop_unreferenced_blobs(workspace, &hashes);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO checkpoint_prunes (session_id, dropped, note, reason, created_at) VALUES (?1, ?2, ?3, 'forget', ?4)",
            params![
                session_id,
                rows,
                format!(
                    "{rows} cop{} forgotten at the person's request: {} freed{}, {} blob{} pointed at by another \
                     conversation and left in the store",
                    if rows == 1 { "y was" } else { "ies were" },
                    crate::checkpoint::human_bytes(freed),
                    if freed == 0 { " (nothing was on disk to free)" } else { "" },
                    left,
                    if left == 1 { " was" } else { "s were" }
                ),
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok((rows as usize, freed, left))
    }

    /// How many conversations the record holds, and how many messages are in them. Two counts for
    /// the About page: "where do my things live" is only half the question, the other half is
    /// "is the thing at that path the thing I expected", and a number answers that in a glance.
    pub fn conversation_count(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn message_total(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn list_sessions(&self, limit: i64) -> Result<Vec<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sessions_query("ORDER BY s.updated_at DESC, s.id DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit], |r| session_from_row(r))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Every conversation filed under one folder — **including** chat rows and empty
    /// drafts, which the sidebar files elsewhere or not at all.
    ///
    /// A destructive folder action cannot trust either list the sidebar shows: `list_sessions`
    /// is a window (the newest 50), and the folder group hides chats and drafts. Meanwhile
    /// `list_workspaces` unions registered paths with paths that still hold sessions, so a
    /// folder with anything left inside it resurrects itself the moment it is removed. The
    /// user sees that as "I deleted the folder and it came back", and the only honest fix is
    /// to ask the store what is actually in the folder before deciding what deleting it means.
    ///
    /// Bounded at 500 so a pathological folder cannot flood a client; the caller compares the
    /// count with `SESSIONS_IN_CAP` and says so rather than deleting a silent subset.
    pub fn sessions_in(&self, workspace: &str) -> Result<Vec<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sessions_query(
            "WHERE s.workspace = ?1 ORDER BY s.updated_at DESC, s.id DESC LIMIT ?2",
        ))?;
        let rows = stmt.query_map(params![workspace, SESSIONS_IN_CAP], |r| session_from_row(r))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// The conversation's row tree, as `(id, parent)` pairs in file order plus its leaf.
    ///
    /// `parent_row` is the lineage a leaf switch wrote. A row with none was written before v16,
    /// or is the conversation's first row: for the first, `None` means "root"; for the rest, the
    /// row *before it in this conversation* is what its lineage meant, because that is exactly what
    /// every read of a pre-v16 store did. Reading a missing parent as "no parent" would collapse
    /// an old database to its last row, which is a transcript quietly lost to a migration.
    fn rows_locked(&self, conn: &rusqlite::Connection, session_id: i64) -> Result<(Vec<i64>, Vec<Option<i64>>, Option<i64>), StoreError> {
        let leaf: Option<i64> = conn
            .query_row(
                "SELECT leaf_row FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let mut ids: Vec<i64> = Vec::new();
        let mut parents: Vec<Option<i64>> = Vec::new();
        let mut stmt = conn.prepare("SELECT id, parent_row FROM messages WHERE session_id = ?1 ORDER BY id ASC")?;
        for row in stmt.query_map(params![session_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))? {
            let (id, p) = row?;
            // Resolve before returning: a row with no recorded parent continues from the row that
            // was already there, which is what its lineage meant before the column existed.
            parents.push(if ids.is_empty() { None } else { p.or(Some(*ids.last().unwrap())) });
            ids.push(id);
        }
        Ok((ids, parents, leaf))
    }

    /// The rows the next answer reads, in reading order: the path from the leaf up to the root.
    ///
    /// Rows that hang off another branch stay in the store — a leaf switch deletes nothing — and
    /// are what [`Store::off_branch_rows`] counts, so the tree can say how much of a conversation
    /// is not currently being read rather than leaving the person to wonder where the rest went.
    pub fn branch_chain(&self, session_id: i64) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        self.chain_locked(&conn, session_id)
    }

    /// Where the next answer reads through. `None` is "the newest row", which is the state every
    /// conversation had before leaf switching existed and the state a send returns it to.
    pub fn leaf_row(&self, session_id: i64) -> Result<Option<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT leaf_row FROM sessions WHERE id = ?1", params![session_id], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten())
    }

    /// Move the leaf. Refuses a row that belongs to another conversation, because a leaf pointing
    /// outside its own conversation is not a branch to read — it is a prompt assembled from a
    /// conversation nobody named.
    ///
    /// `None` means the newest row. `Some(0)` means *before the first message*, the node pi's own
    /// navigation leaves you at when you take back a conversation's opening ask: nothing is read, and
    /// every row of the conversation is then reported as off the path. It needs its own value because
    /// "no leaf recorded" and "deliberately at the root" are different facts.
    pub fn set_leaf(&self, session_id: i64, row: Option<i64>) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        if let Some(r) = row.filter(|r| *r != 0) {
            let own: Option<i64> = conn
                .query_row("SELECT session_id FROM messages WHERE id = ?1", params![r], |q| q.get(0))
                .optional()?;
            match own {
                Some(s) if s == session_id => {}
                Some(s) => {
                    return Err(StoreError::Invalid(format!(
                        "message {r} belongs to conversation {s}, not {session_id}, so the leaf was \
                         not moved: the next answer would read a path out of the wrong conversation"
                    )));
                }
                None => {
                    return Err(StoreError::Invalid(format!(
                        "message {r} is not in the record any more, so the leaf was not moved"
                    )));
                }
            }
        }
        conn.execute(
            "UPDATE sessions SET leaf_row = ?2, updated_at = ?3 WHERE id = ?1",
            params![session_id, row, now_ts()],
        )?;
        Ok(())
    }

    /// Record who a conversation was cut out of, and where. Written by the branch path, read by
    /// the tree.
    pub fn set_lineage(&self, id: i64, parent: i64, role: &str, row: Option<i64>) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET parent = ?2, branch_role = ?3, branch_row = ?4 WHERE id = ?1",
            params![id, parent, role, row],
        )?;
        Ok(())
    }

    /// The conversations cut out of this one, oldest first — the children the tree draws.
    pub fn children_of(&self, id: i64) -> Result<Vec<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sessions_query("WHERE s.parent = ?1 ORDER BY s.id ASC"))?;
        let rows = stmt.query_map(params![id], |r| session_from_row(r))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// How many rows this conversation holds that are **not** on the branch the leaf sits on.
    /// Nothing was deleted to make them that way; they are simply not what the next answer reads.
    pub fn off_branch_rows(&self, session_id: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let (ids, parents, leaf) = self.rows_locked(&conn, session_id)?;
        Ok(ids.len().saturating_sub(chain_from(&ids, &parents, leaf).len()))
    }

    pub fn session(&self, id: i64) -> Result<Option<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                SESSION_SELECT,
                params![id],
                |r| session_from_row(r),
            )
            .optional()?;
        Ok(row)
    }

    // ---- messages ---------------------------------------------------------

    /// Append one message. Returns its id.
    ///
    /// Append-only by construction: there is no update_message in this API.
    /// That is not a limitation, it is the same invariant PromptBuilder relies
    /// on — rewriting history invalidates the prefix cache and makes the audit
    /// trail useless.
    pub fn append_message(&self, session_id: i64, m: &Message) -> Result<i64, StoreError> {
        // A message typed mid-turn carries the time it was typed (`Message::sent_at`), and that is
        // the time the row gets: a steer that lands two minutes later must not read as though the
        // person asked it after the answer they were reacting to. Everything else is stamped now.
        let now = m.sent_at.unwrap_or_else(now_ts);
        let tool_calls = if m.tool_calls.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&m.tool_calls).unwrap_or_default()
        };
        let tool_meta = if m.changes.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&m.changes).unwrap_or_default()
        };
        let conn = self.conn.lock().unwrap();
        // The new row hangs off the leaf, not off the newest row: that is what makes a message sent
        // after a leaf switch join *that* branch instead of whichever path happened to be last in
        // the file. Same rule pi's tree follows when its leaf is moved and a prompt is sent.
        let tip: Option<i64> = conn
            .query_row(
                "SELECT COALESCE(leaf_row, (SELECT MAX(id) FROM messages WHERE session_id = ?1)) \
                 FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning, reasoning_signature, reasoning_ms, tool_call_id, tool_calls_json, tool_meta_json, parent_row, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session_id,
                role_str(&m.role),
                m.content,
                m.reasoning.clone().unwrap_or_default(),
                m.reasoning_signature.clone().unwrap_or_default(),
                m.reasoning_ms.unwrap_or(0) as i64,
                m.tool_call_id,
                tool_calls,
                tool_meta,
                tip,
                now
            ],
        )?;
        let id = conn.last_insert_rowid();

        // Naming is not done here any more. This used to cut the opening line into
        // the title the moment it was stored, which meant a conversation was named
        // before it had been understood - and it put words on the row that the
        // reader watched change under an animation that was trying to type something
        // else. `crate::titling` asks the model for a name, in its own context,
        // before the first answer; the placeholder survives until that answer or its
        // fallback arrives.

        conn.execute(
            "INSERT INTO messages_fts (rowid, content, session_id, message_id, role)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, m.content, session_id, id, role_str(&m.role)],
        )?;
        for a in &m.attachments {
            conn.execute(
                "INSERT INTO attachments (message_id, mime, name, data_b64, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, a.mime, a.name, a.data_b64, now],
            )?;
        }
        conn.execute(
            "UPDATE sessions SET updated_at = ?2, leaf_row = ?3 WHERE id = ?1",
            params![session_id, now, id],
        )?;
        Ok(id)
    }

    /// Newest `limit` messages with id < `before_id`, returned in ascending order.
    ///
    /// The UI needs a bounded window: a session can hold thousands of messages,
    /// and inserting all of them costs memory and scroll performance for content
    /// nobody is looking at. The agent still reads the full history through
    /// `history()` - this is a display concern, not a context concern.
    ///
    /// "Full history" and "this window" both mean **the branch the leaf sits on**, not every row
    /// the conversation ever held. A row kept on another branch is still in the store and still in
    /// the tree; it is not what the reader is looking at, and it is not what the next answer reads.
    pub fn messages_before(
        &self,
        session_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let chain = self.chain_locked(&conn, session_id)?;
        let mut keep: Vec<i64> = chain
            .into_iter()
            .filter(|id| before_id.map(|b| *id < b).unwrap_or(true))
            .collect();
        if keep.len() > limit {
            keep = keep[keep.len() - limit..].to_vec();
        }
        if keep.is_empty() {
            return Ok(Vec::new());
        }
        let ph = keep.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, session_id, role, content, reasoning, tool_call_id, tool_calls_json,
                    reasoning_signature, reasoning_ms, files, tool_meta_json, created_at
             FROM messages WHERE id IN ({ph}) ORDER BY id ASC"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(keep.iter()), |r| {
            Ok(StoredMessage {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                reasoning: r.get(4)?,
                tool_call_id: r.get(5)?,
                tool_calls_json: r.get(6)?,
                reasoning_signature: r.get(7)?,
                reasoning_ms: r.get(8)?,
                files: decode_files(r.get(9)?),
                changes: decode_changes(r.get(10)?),
                attachments: Vec::new(),
                created_at: r.get(11)?,
            })
        })?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        attach_meta(&conn, &mut out)?;
        Ok(out)
    }

    /// The conversation's row tree, read under a lock the caller already holds. Split out so
    /// `messages_before` does not lock twice — and twice would deadlock, because this store's
    /// mutex is not reentrant.
    fn chain_locked(&self, conn: &rusqlite::Connection, session_id: i64) -> Result<Vec<i64>, StoreError> {
        let (ids, parents, leaf) = self.rows_locked(conn, session_id)?;
        Ok(chain_from(&ids, &parents, leaf))
    }

    /// One bounded page of transcript history, newest window in reading order.
    ///
    /// Lives in the store rather than in each shell so the desktop and the
    /// gateway cannot drift on what "the newest 60 messages" means.
    pub fn messages_page(
        &self,
        session_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<MessagesPage, StoreError> {
        let limit = limit.clamp(1, 400);
        // Fetch one extra row as a probe for `has_older` rather than a second COUNT.
        let mut page = self.messages_before(session_id, before_id, limit + 1)?;
        let has_older = page.len() > limit;
        if has_older {
            page.remove(0); // drop the probe, keeping the newest `limit`
        }
        Ok(MessagesPage { messages: page, has_older })
    }

    pub fn message_count(&self, session_id: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Change one message's words in place. Returns false when no row answers `message_id`, so a
    /// caller can tell "saved" from "that message is gone" instead of being told both.
    ///
    /// The search index is rewritten in the same lock. `messages_fts` is a separate copy of the
    /// text (no `content=` link, no triggers — see the note in the schema), so a correction that
    /// left it alone would mean the search box still answered with the sentence the person has
    /// just crossed out, in a transcript where that sentence no longer appears.
    pub fn edit_message(&self, message_id: i64, content: &str) -> Result<bool, StoreError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE messages SET content = ?2 WHERE id = ?1",
            params![message_id, content],
        )?;
        if n == 0 {
            return Ok(false);
        }
        // The fts row carries the message id both as its rowid and as an unindexed column; the
        // second form is what `truncate_from` deletes by, so both have to be kept in step.
        conn.execute(
            "UPDATE messages_fts SET content = ?2 WHERE rowid = ?1",
            params![message_id, content],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = (SELECT session_id FROM messages WHERE id = ?1)",
            params![message_id, now_ts()],
        )?;
        Ok(true)
    }

    /// Drop everything stored from `from_id` onward in this conversation. The row it names goes
    /// with the rest, so handing it the id of a message means "back to just before that one"; 0
    /// drops the whole transcript. Returns how many rows went.
    ///
    /// The named row is included on purpose. A person correcting a message they already sent expects
    /// that message to be gone from where it stood — the corrected ask goes back in at the end, with
    /// the answer it now gets. Leaving the original standing and adding a second copy of it one row
    /// lower is the bug this choice prevents.
    ///
    /// This is the half of "edit a message and send it again" the person sees: the conversation
    /// goes back to that message. Attachments go with their rows (`ON DELETE CASCADE`); the search
    /// index has no such link and is deleted by hand, same as everywhere else in this file.
    ///
    /// What does *not* go is the `turns` telemetry for the lost stretch. Those turns happened, the
    /// panel's averages are computed from the live event stream rather than from this table, and
    /// quietly deleting part of a person's usage history is not what "undo that message" asks for.
    pub fn truncate_from(&self, session_id: i64, from_id: i64) -> Result<usize, StoreError> {
        let conn = self.conn.lock().unwrap();
        // Resolved **before** the delete, because the walk needs the rows that are about to go. A leaf
        // inside the cut moves to the first surviving row of *its own* path — where the person is
        // standing when the cut lands. `MAX(id)` would be wrong and quietly worse: that is the newest
        // row in the file, and in a conversation with two branches it belongs to a path nobody was
        // reading. A leaf earlier than the cut is left alone: it marks a branch the cut did not reach,
        // and moving it would change what the next answer reads without anybody asking.
        let (ids, parents, leaf) = self.rows_locked(&conn, session_id)?;
        let pos: std::collections::HashMap<i64, usize> =
            ids.iter().copied().enumerate().map(|(i, id)| (id, i)).collect();
        let mut new_leaf = leaf;
        if let Some(l) = leaf {
            if l >= from_id {
                let mut cur: Option<i64> = Some(l);
                let mut guard = 0usize;
                while let Some(id) = cur {
                    guard += 1;
                    if guard > ids.len() + 1 {
                        break;
                    }
                    if id < from_id {
                        new_leaf = Some(id);
                        break;
                    }
                    cur = pos.get(&id).and_then(|i| parents[*i]);
                }
                if new_leaf == leaf {
                    // Nothing on this path survived the cut. `None` is the honest answer: the newest
                    // row that is left, and when the whole transcript went, nothing at all.
                    new_leaf = None;
                }
            }
        }
        let n = conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND id >= ?2",
            params![session_id, from_id],
        )?;
        conn.execute(
            "DELETE FROM messages_fts WHERE session_id = ?1 AND message_id >= ?2",
            params![session_id, from_id],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2, leaf_row = ?3 WHERE id = ?1",
            params![session_id, now_ts(), new_leaf],
        )?;
        Ok(n)
    }

    /// Copy the rows the source conversation is **reading**, up to and including `through_id`, into
    /// another conversation.
    ///
    /// This is what a branch is made of: the new conversation has to hold the same words, the same
    /// thinking (including the signature an Anthropic replay needs), the same tool calls and results,
    /// the same file badges and the same attachments as the stretch it came from. Copying through
    /// [`Store::history`] instead would rebuild rows from the *model's* view of them and quietly drop
    /// the columns that only the UI reads — a branch whose tool cards and picture chips had vanished,
    /// which is the same loss v8 and v9 were added to prevent.
    ///
    /// The path, not the file. Once a leaf has been moved, a conversation holds rows on more than one
    /// branch, and `id <= through_id` alone would copy rows the person never saw in that conversation:
    /// the transcript of a conversation is the branch its leaf sits on, so a branch cut at a node
    /// carries that node's path and leaves the rest behind. `crate::tree` is where the rows left behind
    /// are still shown.
    ///
    /// Row by row, keeping each source row's `created_at`: the transcript's order is its meaning, and
    /// a branch that read as a scrambled version of the original is a conversation nobody can trust.
    /// The search index is written in the same call, under the new ids — a branch that cannot be
    /// searched is a conversation the search panel says does not exist.
    pub fn copy_through(&self, from: i64, to: i64, through_id: i64) -> Result<usize, StoreError> {
        if from == to {
            return Err(StoreError::Invalid(
                "a branch cannot copy a conversation into itself".to_string(),
            ));
        }
        let now = now_ts();
        let conn = self.conn.lock().unwrap();
        // The branch the source's leaf sits on. Everything the source holds is `all_rows`; what it
        // *is* — the transcript a person looking at it would recognise — is this path.
        let path: std::collections::HashSet<i64> = self.chain_locked(&conn, from)?.into_iter().collect();
        let mut stmt = conn.prepare(
            "SELECT id, role, content, reasoning, tool_call_id, tool_calls_json,
                    reasoning_signature, reasoning_ms, files, tool_meta_json, created_at
             FROM messages WHERE session_id = ?1 AND id <= ?2 ORDER BY id ASC",
        )?;
        let src: Vec<(i64, String, String, String, Option<String>, String, String, i64, String, String, i64)> = stmt
            .query_map(params![from, through_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut copied = 0usize;
        let mut prev: Option<i64> = None;
        for (sid, role, content, reasoning, tool_call_id, tool_calls, sig, ms, files, meta, created) in src {
            if !path.contains(&sid) {
                continue;
            }
            // The copy is one branch: each row continues from the one before it, and the branch
            // point itself (`prev = None` for the first row) is what the tree draws as the cut.
            conn.execute(
                "INSERT INTO messages (session_id, role, content, reasoning, reasoning_signature, reasoning_ms,
                                       tool_call_id, tool_calls_json, tool_meta_json, files, parent_row, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![to, role, content, reasoning, sig, ms, tool_call_id, tool_calls, meta, files, prev, created],
            )?;
            let new_id = conn.last_insert_rowid();
            prev = Some(new_id);
            conn.execute(
                "INSERT INTO messages_fts (rowid, content, session_id, message_id, role)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![new_id, content, to, new_id, role],
            )?;
            conn.execute(
                "INSERT INTO attachments (message_id, mime, name, data_b64, created_at)
                 SELECT ?1, mime, name, data_b64, ?2 FROM attachments WHERE message_id = ?3",
                params![new_id, now, sid],
            )?;
            copied += 1;
        }
        // The branch is a conversation with content in it, so it belongs at "now": a fresh row that
        // keeps the source's old `updated_at` sorts behind everything in the sidebar list, which reads
        // as the branch never happened.
        conn.execute(
            "UPDATE sessions SET updated_at = ?2, leaf_row = ?3 WHERE id = ?1",
            params![to, now, prev],
        )?;
        Ok(copied)
    }

    pub fn messages(&self, session_id: i64) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        // The branch the leaf sits on. See `messages_before` for why a row on another branch is
        // not part of the transcript, and `crate::tree` for where it is still shown.
        let chain = self.chain_locked(&conn, session_id)?;
        if chain.is_empty() {
            return Ok(Vec::new());
        }
        let ph = chain.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, session_id, role, content, reasoning, tool_call_id, tool_calls_json,
                    reasoning_signature, reasoning_ms, files, tool_meta_json, created_at
             FROM messages WHERE id IN ({ph}) ORDER BY id ASC"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chain.iter()), |r| {
            Ok(StoredMessage {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                reasoning: r.get(4)?,
                tool_call_id: r.get(5)?,
                tool_calls_json: r.get(6)?,
                reasoning_signature: r.get(7)?,
                reasoning_ms: r.get(8)?,
                files: decode_files(r.get(9)?),
                changes: decode_changes(r.get(10)?),
                attachments: Vec::new(),
                created_at: r.get(11)?,
            })
        })?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        attach_meta(&conn, &mut out)?;
        Ok(out)
    }

    /// Every row this conversation holds, on any branch, oldest first.
    ///
    /// The tree needs the rows that are *not* on the current branch — they are the ones a leaf
    /// switch left behind, and a tree that only draws the path being read cannot say what it left
    /// out. Nothing else reads this: the transcript, the prompt and the share path all want the
    /// branch, not the whole tree.
    pub fn all_rows(&self, session_id: i64) -> Result<Vec<(i64, Option<i64>, String, String)>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, parent_row, role, content FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Full payloads for a set of messages, keyed by message id.
    fn attachment_payloads(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<crate::provider::Attachment>>, StoreError> {
        let mut out: std::collections::HashMap<i64, Vec<crate::provider::Attachment>> =
            std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(out)
        }
        let conn = self.conn.lock().unwrap();
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT message_id, mime, name, data_b64
             FROM attachments WHERE message_id IN ({ph}) ORDER BY id ASC"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                crate::provider::Attachment {
                    mime: r.get(1)?,
                    data_b64: r.get(3)?,
                    name: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            let (mid, a) = row?;
            out.entry(mid).or_default().push(a);
        }
        Ok(out)
    }

    /// The attachments of one stored row, payloads included.
    ///
    /// "Send it again" needs these and not the metadata the transcript reads: the person is
    /// re-sending the picture they sent, not a description of it.
    pub fn message_attachments(
        &self,
        message_id: i64,
    ) -> Result<Vec<crate::provider::Attachment>, StoreError> {
        Ok(self.attachment_payloads(&[message_id])?.remove(&message_id).unwrap_or_default())
    }

    /// Rehydrate a PromptBuilder's history. Tool calls are restored with their
    /// ids, because a tool_result whose call is missing is a hard 400.
    /// Attach produced files to a message row. Called after the turn, because
    /// the tool result is only known once the turn has finished.
    pub fn set_message_files(&self, message_id: i64, files: &[crate::tools::OutputFile]) -> Result<(), StoreError> {
        let json = serde_json::to_string(files).unwrap_or_else(|_| "[]".into());
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE messages SET files = ?2 WHERE id = ?1", params![message_id, json])?;
        Ok(())
    }

    pub fn history(&self, session_id: i64) -> Result<Vec<Message>, StoreError> {
        let stored = self.messages(session_id)?;
        // Full payloads only here. A transcript page gets metadata; the agent
        // prompt is the only thing that needs the bytes back.
        let mut payloads = self.attachment_payloads(&stored.iter().map(|m| m.id).collect::<Vec<_>>())?;
        let mut out = Vec::with_capacity(stored.len());
        for s in stored {
            let role = match s.role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                // Unknown role: skip rather than feed a provider something wrong.
                _ => continue,
            };
            let tool_calls = if s.tool_calls_json.is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&s.tool_calls_json).unwrap_or_default()
            };
            out.push(Message {
                role,
                content: s.content,
                attachments: payloads.remove(&s.id).unwrap_or_default(),
                tool_calls,
                // Must survive the round trip: a tool_result whose call id is
                // missing is rejected outright by every provider.
                tool_call_id: s.tool_call_id,
                // Change metadata is UI-only and is deliberately not replayed
                // into the prompt; the model already has the tool's own text.
                changes: Vec::new(),
                reasoning: if s.reasoning.is_empty() { None } else { Some(s.reasoning) },
                reasoning_ms: if s.reasoning_ms == 0 { None } else { Some(s.reasoning_ms) },
                // A stored row's own time is not a mid-turn stamp: these rows are already in the
                // record and nothing re-persists them. `sent_at` is set only by a press that lands
                // at a step boundary later in the conversation.
                sent_at: None,
                reasoning_signature: if s.reasoning_signature.is_empty() {
                    None
                } else {
                    Some(s.reasoning_signature)
                },
            });
        }
        Ok(out)
    }

    // ---- telemetry --------------------------------------------------------

    pub fn record_turn(&self, session_id: i64, t: &TurnTelemetry) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO turns (session_id, created_at, prompt_tokens, cached_tokens, completion_tokens,
                                 ttft_ms, prefill_tps, decode_tps, draft_n, draft_accepted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                now_ts(),
                t.prompt_tokens as i64,
                t.cached_tokens as i64,
                t.completion_tokens as i64,
                t.ttft_ms as i64,
                t.server_prefill_tps,
                t.server_decode_tps,
                t.draft_n as i64,
                t.draft_accepted as i64,
            ],
        )?;
        Ok(())
    }

    /// Aggregate stats for the telemetry bar's "average" columns.
    pub fn session_totals(&self, session_id: i64) -> Result<Totals, StoreError> {
        let conn = self.conn.lock().unwrap();
        let t = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(prompt_tokens),0),
                    COALESCE(SUM(cached_tokens),0),
                    COALESCE(SUM(completion_tokens),0),
                    COALESCE(AVG(ttft_ms),0),
                    COALESCE(AVG(prefill_tps),0),
                    COALESCE(AVG(decode_tps),0),
                    COALESCE(SUM(draft_n),0),
                    COALESCE(SUM(draft_accepted),0)
             FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| {
                Ok(Totals {
                    turns: r.get(0)?,
                    prompt_tokens: r.get::<_, i64>(1)? as u64,
                    cached_tokens: r.get::<_, i64>(2)? as u64,
                    completion_tokens: r.get::<_, i64>(3)? as u64,
                    avg_ttft_ms: r.get(4)?,
                    avg_prefill_tps: r.get(5)?,
                    avg_decode_tps: r.get(6)?,
                    draft_n: r.get::<_, i64>(7)? as u64,
                    draft_accepted: r.get::<_, i64>(8)? as u64,
                })
            },
        )?;
        Ok(t)
    }

    // ---- search -----------------------------------------------------------

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>, StoreError> {
        let q = fts_escape(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            // Column 0 is `content`; 1-3 are the UNINDEXED metadata columns.
            // Asking for a snippet of an UNINDEXED column returns an empty
            // string rather than an error, which is why this is asserted below.
            "SELECT f.session_id, f.message_id, f.role, m.created_at,
                    snippet(messages_fts, 0, '[', ']', '…', 18),
                    bm25(messages_fts)
             FROM messages_fts f
             JOIN messages m ON m.id = f.message_id
             WHERE messages_fts MATCH ?1
             ORDER BY bm25(messages_fts)
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => return Err(StoreError::FtsUnavailable(e.to_string())),
        };
        let rows = stmt.query_map(params![q, limit], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                message_id: r.get(1)?,
                role: r.get(2)?,
                created_at: r.get(3)?,
                snippet: r.get(4)?,
                rank: r.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Search what the **other** conversations said.
    ///
    /// `except` is the conversation that is asking. Its own text is already in front of the
    /// model, so returning it costs prompt bytes to say nothing new — and a model that keeps
    /// finding its own last message tends to conclude it has remembered something useful.
    /// The join to `sessions` is the point of this query rather than `search`: a passage
    /// without a conversation attached is a quote from nowhere.
    pub fn recall(
        &self,
        query: &str,
        limit: i64,
        except: Option<i64>,
    ) -> Result<Vec<RecallHit>, StoreError> {
        let q = fts_escape(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT f.session_id, s.title, s.persona, f.message_id, f.role, m.created_at,
                    snippet(messages_fts, 0, '[', ']', '…', 18),
                    bm25(messages_fts)
             FROM messages_fts f
             JOIN messages m ON m.id = f.message_id
             JOIN sessions s ON s.id = f.session_id
             WHERE messages_fts MATCH ?1
               AND (?2 IS NULL OR f.session_id <> ?2)
             ORDER BY bm25(messages_fts)
             LIMIT ?3",
        ) {
            Ok(st) => st,
            Err(e) => return Err(StoreError::FtsUnavailable(e.to_string())),
        };
        let rows = stmt.query_map(params![q, except, limit], |r| {
            Ok(RecallHit {
                session_id: r.get(0)?,
                title: r.get(1)?,
                persona: r.get(2)?,
                message_id: r.get(3)?,
                role: r.get(4)?,
                created_at: r.get(5)?,
                snippet: r.get(6)?,
                rank: r.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Totals {
    pub turns: i64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub avg_ttft_ms: f64,
    pub avg_prefill_tps: f64,
    pub avg_decode_tps: f64,
    pub draft_n: u64,
    pub draft_accepted: u64,
}

impl Totals {
    pub fn cache_hit_pct(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 * 100.0 / self.prompt_tokens as f64
        }
    }
}

fn role_str(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Make arbitrary user text safe for FTS5 MATCH.
///
/// FTS5 has its own query syntax (`AND`, `OR`, `NEAR`, `"`, `*`, `:`), and raw
/// user input will produce syntax errors — `"AND"` alone is enough to break a
/// search box. Quote each term; that turns the whole thing into a phrase list.
fn fts_escape(q: &str) -> String {
    q.split_whitespace()
        .map(|t| {
            let t = t.replace('"', "\"\"");
            format!("\"{t}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A session row with no title renders as a blank line in the sidebar, which reads
/// as a corrupted or half-deleted entry. There is no useful meaning in an empty
/// name, so it is normalised at the store boundary instead of at every caller.
fn title_or_default(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        UNTITLED.to_string()
    } else {
        t.to_string()
    }
}

/// The name a conversation carries until the model names it.
///
/// This is the one placeholder the app promises: the row exists, it says "New
/// Session", and the titler (`crate::titling`) replaces it with the model's own
/// words before the first answer streams. Anything that asks "has this
/// conversation been named yet?" compares against this, and against the legacy
/// spellings older databases carry, rather than inventing its own guess.
pub const UNTITLED: &str = "New Session";

/// Metadata for the transcript, without dragging base64 across IPC.
fn attach_meta(conn: &Connection, msgs: &mut [StoredMessage]) -> Result<(), StoreError> {
    if msgs.is_empty() {
        return Ok(())
    }
    let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
    let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT message_id, mime, name, length(data_b64) * 3 / 4
         FROM attachments WHERE message_id IN ({ph}) ORDER BY id ASC"
    ))?;
    let mut by_id: std::collections::HashMap<i64, Vec<crate::provider::AttachmentMeta>> =
        std::collections::HashMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
        let mid: i64 = r.get(0)?;
        Ok((
            mid,
            crate::provider::AttachmentMeta {
                mime: r.get(1)?,
                name: r.get(2)?,
                bytes: r.get::<_, i64>(3)? as usize,
            },
        ))
    })?;
    for row in rows {
        let (mid, meta) = row?;
        by_id.entry(mid).or_default().push(meta);
    }
    for m in msgs.iter_mut() {
        m.attachments = by_id.remove(&m.id).unwrap_or_default();
    }
    Ok(())
}

/// Whether a string is `#rrggbb`. Deliberately strict and deliberately small: the colour goes
/// straight into a `style` attribute in the sidebar, so "validated" has to mean a shape, not an
/// intention. Named colours, `#rgb`, and anything with a `url(` in it are all refused.
fn is_hex_color(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[0] == b'#'
        && b[1..].iter().all(|c| c.is_ascii_hexdigit())
}

/// A short, single-line label from the first user message.
///
/// Sessions otherwise stay titled "new session" forever, which turns a list of
/// real conversations into a list of identical rows.
/// The automatic title: the opening line, whitespace-collapsed, cut at forty-eight
/// characters with an ellipsis. Public so the eligibility test for a *model* title can
/// be "is this still the title we made up?", rather than a guess about who named it.
pub fn derive_title(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = String::new();
    for ch in one_line.chars() {
        if out.chars().count() >= 48 {
            out.push('\u{2026}');
            break;
        }
        out.push(ch);
    }
    out
}

/// Add a column only if it is missing.
///
/// The older migrations use a bare `ALTER TABLE`, which is fine while the
/// version counter is the only way in. But a column add is the one migration
/// step that is not naturally repeatable, and re-running it turns a recoverable
/// situation into a hard open failure. Check first.
/// A corrupt or legacy value degrades to "no files" rather than failing the
/// whole transcript read.
fn decode_changes(raw: String) -> Vec<crate::tools::changes::FileChange> {
    if raw.is_empty() {
        return Vec::new();
    }
    // A row written by an older build, or a corrupt blob, degrades to "no change
    // metadata" rather than failing the whole transcript read.
    serde_json::from_str(&raw).unwrap_or_default()
}

fn decode_files(raw: String) -> Vec<crate::tools::OutputFile> {
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Walk leaf→root and hand back the path in reading order.
///
/// `ids` is ascending by id and `parents[i]` is row `ids[i]`'s recorded parent, already resolved
/// for rows that pre-date the column (see [`Store::row_tree`]). A parent id that is not in this
/// conversation ends the walk rather than jumping to another conversation's rows, and a repeated
/// id means the file contradicts itself: the walk stops at the repeat instead of spinning.
fn chain_from(ids: &[i64], parents: &[Option<i64>], leaf: Option<i64>) -> Vec<i64> {
    // `0` is not a row id. It is the leaf that means *before the first message*: taking back a
    // conversation's opening ask leaves the model reading nothing but its prompt, which is where pi's
    // own navigation puts you at that node. `None` cannot stand for it — `None` means "the newest
    // row", which is what a store with no leaf column at all says.
    if leaf == Some(0) {
        return Vec::new();
    }
    let pos: std::collections::HashMap<i64, usize> = ids.iter().copied().enumerate().map(|(i, id)| (id, i)).collect();
    let mut start = leaf.and_then(|l| pos.get(&l).copied()).or_else(|| ids.len().checked_sub(1));
    let mut out: Vec<i64> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    while let Some(i) = start {
        if !seen.insert(i) {
            break;
        }
        out.push(ids[i]);
        match parents[i] {
            // A parent outside this conversation (another conversation's row, or one that has been
            // deleted) is the end of the path, not a link to follow.
            Some(p) => start = pos.get(&p).copied(),
            None => start = None,
        }
    }
    out.reverse();
    out
}

fn add_column_if_missing(conn: &Connection, table: &str, col: &str, ddl: &str) -> rusqlite::Result<()> {
    let names: Vec<String> = {
        let mut s = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = s.query_map([], |r| r.get::<_, String>(1))?;
        let out: Vec<String> = rows.filter_map(|x| x.ok()).collect();
        out
    };
    let exists = names.iter().any(|name| name == col);
    if exists {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl};"))
}

fn migrate(conn: &Connection, from: i64) -> Result<(), StoreError> {
    if from < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS sessions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL,
                 workspace TEXT NOT NULL,
                 model TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 reasoning TEXT NOT NULL DEFAULT '',
                 tool_call_id TEXT,
                 tool_calls_json TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                 content,
                 session_id UNINDEXED,
                 message_id UNINDEXED,
                 role UNINDEXED,
                 tokenize = 'porter unicode61 remove_diacritics 2'
             );

             CREATE TABLE IF NOT EXISTS turns (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 created_at INTEGER NOT NULL,
                 prompt_tokens INTEGER NOT NULL,
                 cached_tokens INTEGER NOT NULL,
                 completion_tokens INTEGER NOT NULL,
                 ttft_ms INTEGER NOT NULL,
                 prefill_tps REAL NOT NULL,
                 decode_tps REAL NOT NULL,
                 draft_n INTEGER NOT NULL,
                 draft_accepted INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);

             CREATE TABLE IF NOT EXISTS checkpoints (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 turn INTEGER NOT NULL,
                 message_id INTEGER,
                 path TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 hash TEXT NOT NULL,
                 after_hash TEXT NOT NULL DEFAULT '',
                 bytes INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON checkpoints(session_id, turn);
             CREATE INDEX IF NOT EXISTS idx_checkpoints_message ON checkpoints(session_id, message_id);

             CREATE TABLE IF NOT EXISTS checkpoint_prunes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 dropped INTEGER NOT NULL,
                 note TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ckprune_session ON checkpoint_prunes(session_id);
             COMMIT;",
        )?;
    }
    if from < 2 {
        // v2: rendered HTML cache. The UI re-renders a finished turn once and
        // stores the result, so reopening a long session does not re-parse
        // markdown for every message (PLAN.md §9 rule 3).
        conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN rendered_html TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if from < 3 {
        // v3: signed-thinking attestation. Without it, resuming an Anthropic
        // session that used tools sends back thinking blocks the API rejects.
        conn.execute_batch(
            "ALTER TABLE messages ADD COLUMN reasoning_signature TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if from < 5 {
        // v5: image attachments. Separate table, not a column: a 2 MB image is
        // ~2.7 MB of base64, and putting that in `messages` makes every
        // transcript page and every FTS maintenance step carry it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS attachments (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                 mime TEXT NOT NULL,
                 name TEXT,
                 data_b64 TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_attachments_msg ON attachments(message_id);",
        )?;
    }
    if from < 6 {
        // v6a: the env block used to be baked into the stored user message, so
        // the transcript rendered a raw <env>...</env> block above every
        // question. Strip it from rows that were written the old way.
        let rows: Vec<(i64, String)> = {
            let mut s = conn.prepare("SELECT id, content FROM messages WHERE role = 'user' AND content LIKE '<env>%'")?;
            let it = s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            it.filter_map(|x| x.ok()).collect()
        };
        for (id, content) in rows {
            match content.find("</env>") {
                Some(end) => {
                    let rest = &content[end + 6..];
                    let rest = rest.trim_start_matches(['\n', '\r']);
                    let mut c = conn.prepare("UPDATE messages SET content = ?2 WHERE id = ?1")?;
                    c.execute(rusqlite::params![id, rest])?;
                }
                None => {
                    // Malformed: no closing tag. Leaving it is better than
                    // guessing where the user's text begins.
                }
            }
        }

        // v6b: sessions were titled "new session" forever, so a list of real
        // conversations was a list of identical rows. Derive a title from the
        // first user message for any session still carrying the placeholder.
        let untitled: Vec<i64> = {
            let mut s = conn.prepare("SELECT id FROM sessions WHERE title = 'new session'")?;
            let it = s.query_map([], |r| r.get::<_, i64>(0))?;
            it.filter_map(|x| x.ok()).collect()
        };
        for sid in untitled {
            let first: Option<String> = conn
                .query_row(
                    "SELECT content FROM messages WHERE session_id = ?1 AND role = 'user' ORDER BY id LIMIT 1",
                    rusqlite::params![sid],
                    |r| r.get(0),
                )
                .ok();
            if let Some(t) = first.as_deref().map(derive_title) {
                if !t.is_empty() {
                    conn.execute(
                        "UPDATE sessions SET title = ?2 WHERE id = ?1",
                        rusqlite::params![sid, t],
                    )?;
                }
            }
        }
    }
    if from < 7 {
        // v7: how long the model spent thinking. The transcript says "thought
        // for 2m34s"; if the number is not stored, that fact is lost the moment
        // the session is reloaded.
        add_column_if_missing(
            conn,
            "messages",
            "reasoning_ms",
            "reasoning_ms INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if from < 11 {
        // v11: which mode made the conversation. Chat is not a label on the same list:
        // a chat has no folder and no tools, so filing it under a workspace makes the
        // sidebar claim something about it that is not true.
        add_column_if_missing(
            conn,
            "sessions",
            "persona",
            "persona TEXT NOT NULL DEFAULT 'cowork'",
        )?;
    }
    if from < 12 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ui_prefs (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
    }
    if from < 14 {
        // v14: whether a conversation may reach the web. Chat mode starts with no tools at all, and
        // the person can grant it the two network tools. That grant belongs on the row: it is a
        // decision about one conversation, it has to survive closing the app, and the model process
        // has to be mounted to match it when the conversation is reopened.
        add_column_if_missing(
            conn,
            "sessions",
            "web_on",
            "web_on INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if from < 15 {
        // v15: the checkpoint index. Same statements as the fresh-database block above:
        // `IF NOT EXISTS` makes them correct for a database that was created after this
        // version, and the block is what an older one needs.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 turn INTEGER NOT NULL,
                 message_id INTEGER,
                 path TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 hash TEXT NOT NULL,
                 after_hash TEXT NOT NULL DEFAULT '',
                 bytes INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON checkpoints(session_id, turn);
             CREATE INDEX IF NOT EXISTS idx_checkpoints_message ON checkpoints(session_id, message_id);

             CREATE TABLE IF NOT EXISTS checkpoint_prunes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 dropped INTEGER NOT NULL,
                 note TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_ckprune_session ON checkpoint_prunes(session_id);",
        )?;
    }
    if from < 16 {
        // v16: lineage. A branch made a second conversation and recorded nothing about where it
        // came from, so the sidebar showed a branch as an unrelated row and the tree pi owns was
        // read for nothing but a leaf id. A session gains its parent, the role and row id of the
        // cut it was made at, and a leaf; a message gains the row it continues from.
        //
        // NULL is a value, not a gap, in three of these four places: a session with no parent is a
        // **root** (and the tree must draw it as one, not as "unknown"), a session with no leaf is
        // reading its newest row, and a message with no `parent_row` continues from the row before
        // it — which is the rule every pre-v16 database already lived by, so an old store reads
        // exactly as it did before this migration.
        add_column_if_missing(conn, "sessions", "parent", "parent INTEGER")?;
        add_column_if_missing(
            conn,
            "sessions",
            "branch_role",
            "branch_role TEXT NOT NULL DEFAULT ''",
        )?;
        add_column_if_missing(conn, "sessions", "branch_row", "branch_row INTEGER")?;
        add_column_if_missing(conn, "sessions", "leaf_row", "leaf_row INTEGER")?;
        add_column_if_missing(conn, "messages", "parent_row", "parent_row INTEGER")?;
        // Rows the older build wrote keep `parent_row` NULL on purpose — see the note above — but
        // every conversation had a newest row, and that row is where a leaf switch starts from.
        // Writing it here would make `leaf_row` mean "newest" in two ways at once.
    }
    if from < 17 {
        // v17: `reason` on the prune notices. Old rows are all cap evictions, because that was
        // the only thing that ever wrote one, and the default says so rather than leaving the
        // history blank.
        add_column_if_missing(
            conn,
            "checkpoint_prunes",
            "reason",
            "reason TEXT NOT NULL DEFAULT 'cap'",
        )?;
    }
    if from < 10 {
        // v10: workspaces became things you can add, not just side effects of the
        // last path you typed. The sidebar groups sessions by workspace and lets
        // you attach a folder before it has any sessions in it, which needs a row
        // of its own. Sessions keep their own workspace column: this table is the
        // list, that column is the fact.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspaces (
                 path TEXT PRIMARY KEY,
                 label TEXT NOT NULL DEFAULT '',
                 added_at INTEGER NOT NULL
             );",
        )?;
    }
    // Placed after the v10 block that creates `workspaces`, not up with the other newest blocks:
    // these blocks run in the order they are written, not in numeric order, and asking
    // `add_column_if_missing` for a table that does not exist yet looks exactly like asking for a
    // column it already has — its PRAGMA reads "no columns", which is not what it cannot tell from
    // "no such table". A fresh database only reaches this once its table exists.
    if from < 13 {
        // v13: a colour on a folder row. The sidebar lists folders by name, and names of the same
        // shape ("notes", "notes-old", "notes-new") are the ones a person distinguishes by eye, not
        // by reading. The colour is decoration on the record, never a fact about the folder: the
        // path stays the fact.
        add_column_if_missing(
            conn,
            "workspaces",
            "color",
            "color TEXT NOT NULL DEFAULT ''",
        )?;
    }
    if from < 9 {
        // v9: what a file tool changed. The +N -M badge and the diff view are
        // UI metadata about a tool call, exactly like the files a turn produced,
        // and they belong in the row: a reopened session that lost them is a
        // transcript that quietly got less informative.
        add_column_if_missing(
            conn,
            "messages",
            "tool_meta_json",
            "tool_meta_json TEXT NOT NULL DEFAULT ''",
        )?;
    }
    if from < 8 {
        // v8: files a turn produced. Without this the attachment chips under an
        // answer exist only while the app is open and vanish on reload.
        add_column_if_missing(
            conn,
            "messages",
            "files",
            "files TEXT NOT NULL DEFAULT '[]'",
        )?;
    }
    if from < 4 {
        // v4: sessions with an empty title. `delete_session` used to move the host
        // onto a freshly created session titled "", which rendered as a blank row
        // in the sidebar - the deleted session was gone, but it left a title-less
        // orphan behind. Backfill the rows that already exist.
        conn.execute_batch(
            "UPDATE sessions SET title = 'new session' WHERE trim(title) = '';",
        )?;
    }
    Ok(())
}

/// The slice of the store that a **client** reads and writes.
///
/// Paired with [`crate::host::HostOps`]: together they are the whole contract a
/// front end needs, and the contract a second core has to satisfy. The gateway uses
/// all **fourteen** of these, and none of them is an engine internal.
///
/// Note for the pi-backed implementation: pi keeps sessions as JSONL trees, so its
/// implementation of this trait is a **derived index** over pi's session directory,
/// not the source of truth. `HostOps` decides what a conversation *is*; this trait
/// only answers "what is in it, and what is in the sidebar".
pub trait StoreOps: Send + Sync {
    fn add_workspace(&self, path: &str) -> Result<(), StoreError>;
    fn remove_workspace(&self, path: &str) -> Result<usize, StoreError>;
    fn list_workspaces(&self) -> Result<Vec<WorkspaceEntry>, StoreError>;
    fn create_session(&self, title: &str, workspace: &str, model: &str) -> Result<i64, StoreError>;
    fn delete_session(&self, id: i64) -> Result<(), StoreError>;
    fn list_sessions(&self, limit: i64) -> Result<Vec<Session>, StoreError>;

    /// Every conversation in one folder, chats and empty drafts included. See the inherent
    /// method on `Store` for what this is for.
    fn sessions_in(&self, workspace: &str) -> Result<Vec<Session>, StoreError>;
    fn append_message(&self, session_id: i64, m: &Message) -> Result<i64, StoreError>;
    fn messages_page(&self, session_id: i64, before_id: Option<i64>, limit: usize)
        -> Result<MessagesPage, StoreError>;
    fn messages(&self, session_id: i64) -> Result<Vec<StoredMessage>, StoreError>;
    /// How many rows a conversation holds. Cheap on purpose: a caller that has to notice a row
    /// arriving polls this, and polling `messages()` would re-read the whole conversation each time.
    fn message_count(&self, session_id: i64) -> Result<usize, StoreError>;
    /// See `Store::edit_message` / `Store::truncate_from`. Both cores need them: a conversation
    /// the person corrects has to read corrected on every screen, from every device.
    fn edit_message(&self, message_id: i64, content: &str) -> Result<bool, StoreError>;
    fn truncate_from(&self, session_id: i64, from_id: i64) -> Result<usize, StoreError>;
    /// See [`Store::copy_through`] — the row copy a branch is built from. Both cores need it: on the
    /// Rust core it *is* the branch, and on the pi core it writes the record that sits beside the
    /// thread pi forks.
    ///
    /// The checkpoint verbs below are on this trait for the same reason: the pi core's file
    /// calls run in `cwtool`, a separate process that takes the copies and hands the marks
    /// back, and the bridge then indexes them — so the store a turn writes checkpoints into
    /// is reached through this trait on one core and directly on the other. A revert that
    /// only worked on the Rust core would be a feature the phone cannot use.
    fn copy_through(&self, from: i64, to: i64, through_id: i64) -> Result<usize, StoreError>;

    // ---- lineage and the leaf ----------------------------------------------------------
    //
    // On this trait rather than only on `Store` for the same reason `copy_through` is: the pi core
    // writes the record beside the thread it forks, and both cores need the same four facts —
    // which conversation a conversation was cut out of, where it was cut, which node it reads
    // through now, and what is left on the branch it is not reading.

    /// Record the parent and the cut. Written by the branch path on both cores.
    fn set_lineage(&self, id: i64, parent: i64, role: &str, row: Option<i64>) -> Result<(), StoreError>;
    /// Move the leaf — the node the next answer reads through. `None` means the newest row.
    ///
    /// This is not a rewind: it deletes no row and copies none. Rows it leaves behind stay in the
    /// record and are reported by [`StoreOps::off_branch_rows`].
    fn set_leaf(&self, session_id: i64, row: Option<i64>) -> Result<(), StoreError>;
    /// Where the leaf is. `None` is "the newest row", the state every conversation starts in.
    fn leaf_row(&self, session_id: i64) -> Result<Option<i64>, StoreError>;
    /// The conversations cut out of this one, oldest first.
    fn children_of(&self, id: i64) -> Result<Vec<Session>, StoreError>;
    /// Rows this conversation holds that are not on the branch the leaf sits on.
    fn off_branch_rows(&self, session_id: i64) -> Result<usize, StoreError>;
    /// The row ids the next answer reads, in reading order.
    fn branch_chain(&self, session_id: i64) -> Result<Vec<i64>, StoreError>;
    /// Every row on any branch: `(id, parent_row, role, content)`, oldest first. The tree reads
    /// this; the transcript, the prompt and the share path read the branch.
    fn all_rows(&self, session_id: i64) -> Result<Vec<(i64, Option<i64>, String, String)>, StoreError>;

    /// The folder a conversation works in: where its snapshot store lives, and what
    /// stored checkpoint paths are relative to.
    fn session_workspace(&self, session_id: i64) -> Result<String, StoreError>;
    /// Index the copies one turn took. See [`crate::checkpoint::capture`].
    fn record_checkpoints(
        &self,
        session_id: i64,
        turn: i64,
        marks: &[crate::checkpoint::Mark],
    ) -> Result<usize, StoreError>;
    /// Name the answer row a turn's copies belong to, once that row exists.
    fn stamp_checkpoints_turn(
        &self,
        session_id: i64,
        turn: i64,
        message_id: i64,
    ) -> Result<usize, StoreError>;
    /// The copies that turn made — "revert files from this turn".
    fn checkpoints_for_message(
        &self,
        session_id: i64,
        message_id: i64,
    ) -> Result<Vec<CheckpointRow>, StoreError>;
    /// That turn's copies and every later turn's — "revert files to before turn N".
    fn checkpoints_from_message(
        &self,
        session_id: i64,
        message_id: i64,
    ) -> Result<Vec<CheckpointRow>, StoreError>;
    /// Which turns can be undone, and how much was pruned. Drives whether the transcript
    /// offers a revert at all, and whether it says older copies were pruned.
    fn checkpoint_overview(
        &self,
        session_id: i64,
    ) -> Result<CheckpointOverview, StoreError>;
    /// Every copy this conversation holds, oldest first — the file ledger reads the whole
    /// conversation, where a revert card reads one turn.
    fn checkpoints_for_session(
        &self,
        session_id: i64,
    ) -> Result<Vec<CheckpointRow>, StoreError>;
    /// The notices saying copies went away, each carrying the reason it says so.
    fn checkpoint_prunes(&self, session_id: i64) -> Result<Vec<CheckpointPrune>, StoreError>;
    /// Blob keys any conversation points at. Anything in the snapshot folder that is not
    /// here is orphan bytes.
    fn referenced_hashes(&self) -> Result<Vec<String>, StoreError>;
    /// Rows for `hash` outside `session_id` — what "would dropping this conversation free it?"
    /// has to ask, because a blob two conversations share is not this one's to free.
    fn rows_pointing_at_outside(&self, hash: &str, session_id: i64) -> Result<usize, StoreError>;
    /// Conversations sharing one folder's snapshot store.
    fn sessions_sharing_workspace(&self, workspace: &str) -> Result<Vec<i64>, StoreError>;
    /// Drop one conversation's copies and the blobs nothing else points at. See
    /// [`Store::forget_checkpoints`] for what is checked before anything is deleted.
    fn forget_checkpoints(&self, session_id: i64, workspace: &str) -> Result<(usize, u64, usize), StoreError>;
    /// See `Store::message_attachments`.
    fn message_attachments(&self, message_id: i64) -> Result<Vec<crate::provider::Attachment>, StoreError>;
    fn history(&self, session_id: i64) -> Result<Vec<Message>, StoreError>;

    /// Full-text search across conversations — the search panel, not the transcript.
    fn search(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>, StoreError>;
    /// One sidebar row, by id: the share path needs its title and workspace before
    /// it renders anything.
    fn session(&self, id: i64) -> Result<Option<Session>, StoreError>;
    fn rename_session(&self, id: i64, title: &str) -> Result<(), StoreError>;
    fn set_workspace_label(&self, path: &str, label: &str) -> Result<usize, StoreError>;
    fn set_workspace_color(&self, path: &str, color: &str) -> Result<usize, StoreError>;
    /// Say which mode a conversation belongs to. Not part of `create_session` because the
    /// store creates rows in several places (boot, resume, import) that know nothing about
    /// personas; the default is the honest one, and the client that *does* know says so.
    fn set_session_persona(&self, id: i64, persona: &str) -> Result<(), StoreError>;
    fn set_session_web(&self, id: i64, on: bool) -> Result<(), StoreError>;
    fn conversation_count(&self) -> Result<usize, StoreError>;

    /// Where this store's file is, for the About page. See [`Store::db_path`].
    fn db_path(&self) -> String {
        String::new()
    }
    fn message_total(&self) -> Result<usize, StoreError>;
    /// Display preferences a client set for itself. The keys are not free-form — see
    /// `crate::prefs` — because a table every client can invent keys in is a drawer, not
    /// a settings store.
    fn ui_pref(&self, key: &str) -> Result<Option<String>, StoreError>;
    fn ui_prefs_all(&self) -> Result<Vec<(String, String)>, StoreError>;
    fn set_ui_pref(&self, key: &str, value: &str) -> Result<(), StoreError>;
    fn clear_ui_pref(&self, key: &str) -> Result<(), StoreError>;
    /// Attach the files a tool produced to the row that produced them. Not a nicety:
    /// the file chips under an answer, and "what did this run make?", are read from
    /// this column. Both cores learn the files only after the row is written, so both
    /// need the same verb.
    fn set_message_files(&self, message_id: i64, files: &[crate::tools::OutputFile]) -> Result<(), StoreError>;
}

impl StoreOps for Store {
    fn add_workspace(&self, path: &str) -> Result<(), StoreError> {
        Store::add_workspace(self, path)
    }
    fn remove_workspace(&self, path: &str) -> Result<usize, StoreError> {
        Store::remove_workspace(self, path)
    }
    fn list_workspaces(&self) -> Result<Vec<WorkspaceEntry>, StoreError> {
        Store::list_workspaces(self)
    }
    fn create_session(&self, title: &str, workspace: &str, model: &str) -> Result<i64, StoreError> {
        Store::create_session(self, title, workspace, model)
    }
    fn delete_session(&self, id: i64) -> Result<(), StoreError> {
        Store::delete_session(self, id)
    }
    fn set_session_persona(&self, id: i64, persona: &str) -> Result<(), StoreError> {
        Store::set_session_persona(self, id, persona)
    }
    fn set_session_web(&self, id: i64, on: bool) -> Result<(), StoreError> {
        Store::set_session_web(self, id, on)
    }
    fn copy_through(&self, from: i64, to: i64, through_id: i64) -> Result<usize, StoreError> {
        Store::copy_through(self, from, to, through_id)
    }

    fn set_lineage(&self, id: i64, parent: i64, role: &str, row: Option<i64>) -> Result<(), StoreError> {
        Store::set_lineage(self, id, parent, role, row)
    }
    fn set_leaf(&self, session_id: i64, row: Option<i64>) -> Result<(), StoreError> {
        Store::set_leaf(self, session_id, row)
    }
    fn leaf_row(&self, session_id: i64) -> Result<Option<i64>, StoreError> {
        Store::leaf_row(self, session_id)
    }
    fn children_of(&self, id: i64) -> Result<Vec<Session>, StoreError> {
        Store::children_of(self, id)
    }
    fn off_branch_rows(&self, session_id: i64) -> Result<usize, StoreError> {
        Store::off_branch_rows(self, session_id)
    }
    fn branch_chain(&self, session_id: i64) -> Result<Vec<i64>, StoreError> {
        Store::branch_chain(self, session_id)
    }
    fn all_rows(&self, session_id: i64) -> Result<Vec<(i64, Option<i64>, String, String)>, StoreError> {
        Store::all_rows(self, session_id)
    }

    fn session_workspace(&self, session_id: i64) -> Result<String, StoreError> {
        Store::session_workspace(self, session_id)
    }

    fn record_checkpoints(
        &self,
        session_id: i64,
        turn: i64,
        marks: &[crate::checkpoint::Mark],
    ) -> Result<usize, StoreError> {
        Store::record_checkpoints(self, session_id, turn, marks)
    }

    fn stamp_checkpoints_turn(
        &self,
        session_id: i64,
        turn: i64,
        message_id: i64,
    ) -> Result<usize, StoreError> {
        Store::stamp_checkpoints_turn(self, session_id, turn, message_id)
    }

    fn checkpoints_for_message(
        &self,
        session_id: i64,
        message_id: i64,
    ) -> Result<Vec<CheckpointRow>, StoreError> {
        Store::checkpoints_for_message(self, session_id, message_id)
    }

    fn checkpoints_from_message(
        &self,
        session_id: i64,
        message_id: i64,
    ) -> Result<Vec<CheckpointRow>, StoreError> {
        Store::checkpoints_from_message(self, session_id, message_id)
    }

    fn checkpoint_overview(&self, session_id: i64) -> Result<CheckpointOverview, StoreError> {
        Store::checkpoint_overview(self, session_id)
    }
    fn checkpoints_for_session(&self, session_id: i64) -> Result<Vec<CheckpointRow>, StoreError> {
        Store::checkpoints_for_session(self, session_id)
    }
    fn checkpoint_prunes(&self, session_id: i64) -> Result<Vec<CheckpointPrune>, StoreError> {
        Store::checkpoint_prunes(self, session_id)
    }
    fn referenced_hashes(&self) -> Result<Vec<String>, StoreError> {
        Store::referenced_hashes(self)
    }
    fn rows_pointing_at_outside(&self, hash: &str, session_id: i64) -> Result<usize, StoreError> {
        Store::rows_pointing_at_outside(self, hash, session_id)
    }
    fn sessions_sharing_workspace(&self, workspace: &str) -> Result<Vec<i64>, StoreError> {
        Store::sessions_sharing_workspace(self, workspace)
    }
    fn forget_checkpoints(&self, session_id: i64, workspace: &str) -> Result<(usize, u64, usize), StoreError> {
        Store::forget_checkpoints(self, session_id, workspace)
    }
    fn db_path(&self) -> String {
        Store::db_path(self)
    }

    fn conversation_count(&self) -> Result<usize, StoreError> {
        Store::conversation_count(self)
    }
    fn message_total(&self) -> Result<usize, StoreError> {
        Store::message_total(self)
    }
    fn ui_pref(&self, key: &str) -> Result<Option<String>, StoreError> {
        Store::ui_pref(self, key)
    }
    fn ui_prefs_all(&self) -> Result<Vec<(String, String)>, StoreError> {
        Store::ui_prefs_all(self)
    }
    fn set_ui_pref(&self, key: &str, value: &str) -> Result<(), StoreError> {
        Store::set_ui_pref(self, key, value)
    }
    fn clear_ui_pref(&self, key: &str) -> Result<(), StoreError> {
        Store::clear_ui_pref(self, key)
    }
    fn set_message_files(&self, message_id: i64, files: &[crate::tools::OutputFile]) -> Result<(), StoreError> {
        Store::set_message_files(self, message_id, files)
    }
    fn list_sessions(&self, limit: i64) -> Result<Vec<Session>, StoreError> {
        Store::list_sessions(self, limit)
    }

    fn sessions_in(&self, workspace: &str) -> Result<Vec<Session>, StoreError> {
        Store::sessions_in(self, workspace)
    }
    fn append_message(&self, session_id: i64, m: &Message) -> Result<i64, StoreError> {
        Store::append_message(self, session_id, m)
    }
    fn messages_page(
        &self,
        session_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<MessagesPage, StoreError> {
        Store::messages_page(self, session_id, before_id, limit)
    }
    fn messages(&self, session_id: i64) -> Result<Vec<StoredMessage>, StoreError> {
        Store::messages(self, session_id)
    }
    fn message_count(&self, session_id: i64) -> Result<usize, StoreError> {
        Store::message_count(self, session_id)
    }
    fn edit_message(&self, message_id: i64, content: &str) -> Result<bool, StoreError> {
        Store::edit_message(self, message_id, content)
    }
    fn truncate_from(&self, session_id: i64, from_id: i64) -> Result<usize, StoreError> {
        Store::truncate_from(self, session_id, from_id)
    }
    fn message_attachments(&self, message_id: i64) -> Result<Vec<crate::provider::Attachment>, StoreError> {
        Store::message_attachments(self, message_id)
    }
    fn history(&self, session_id: i64) -> Result<Vec<Message>, StoreError> {
        Store::history(self, session_id)
    }
    fn search(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>, StoreError> {
        Store::search(self, query, limit)
    }
    fn session(&self, id: i64) -> Result<Option<Session>, StoreError> {
        Store::session(self, id)
    }
    fn rename_session(&self, id: i64, title: &str) -> Result<(), StoreError> {
        Store::rename_session(self, id, title)
    }
    fn set_workspace_label(&self, path: &str, label: &str) -> Result<usize, StoreError> {
        Store::set_workspace_label(self, path, label)
    }
    fn set_workspace_color(&self, path: &str, color: &str) -> Result<usize, StoreError> {
        Store::set_workspace_color(self, path, color)
    }
}

#[cfg(test)]
mod tests {
    /// What "edit this message" has to guarantee: the corrected sentence is the one every reader
    /// sees — the transcript, the model's replayed history, and the search box — and the row the
    /// person did not name is left alone rather than silently rewritten next door.
    #[test]
    fn a_corrected_message_reads_corrected_everywhere() {
        let s = store();
        let c = sid(&s);
        let first = s.append_message(c, &Message::user("the old sentence")).unwrap();
        let second = s.append_message(c, &Message::user("the next one")).unwrap();
        assert!(
            s.search("old sentence", 10).unwrap().iter().any(|h| h.message_id == first),
            "the search index has to hold the row before the edit"
        );

        assert!(s.edit_message(first, "the fixed sentence").unwrap());

        assert_eq!(
            s.messages(c).unwrap().iter().find(|m| m.id == first).unwrap().content,
            "the fixed sentence"
        );
        assert_eq!(
            s.history(c).unwrap().iter().find(|m| m.content == "the fixed sentence").map(|m| m.role),
            Some(Role::User),
            "the model has to be told the corrected words, not the ones in the old draft"
        );
        assert_eq!(
            s.messages(c).unwrap().iter().find(|m| m.id == second).unwrap().content,
            "the next one",
            "only the named row changes"
        );
        // The search copy moved with it: the corrected words answer, the crossed-out ones do not.
        assert!(s.search("fixed sentence", 10).unwrap().iter().any(|h| h.message_id == first));
        assert!(
            s.search("old sentence", 10).unwrap().is_empty(),
            "a search that still answers the sentence the person crossed out is a second copy \
             of the transcript they can no longer find"
        );
        // A row that is not there says so, instead of reporting a save that saved nothing.
        assert!(!s.edit_message(999_999, "x").unwrap());
    }

    /// "Take the conversation back to just before that message" — the row it names goes with the
    /// rest, and the search index goes with it.
    #[test]
    fn a_conversation_can_be_taken_back_to_a_message() {
        let s = store();
        let c = sid(&s);
        let other = sid(&s);
        let a = s.append_message(c, &Message::user("one")).unwrap();
        let b = s.append_message(c, &Message::assistant("two")).unwrap();
        s.append_message(c, &Message::user("three")).unwrap();
        s.append_message(other, &Message::user("keep me")).unwrap();

        assert_eq!(s.truncate_from(c, b).unwrap(), 2, "`two` and everything after it goes");
        assert_eq!(
            s.messages(c).unwrap().into_iter().map(|m| m.content).collect::<Vec<_>>(),
            vec!["one"],
            "the named row goes with the stretch after it"
        );
        assert_eq!(s.messages(other).unwrap().len(), 1, "another conversation is not involved");

        assert_eq!(s.truncate_from(c, 0).unwrap(), 1, "0 means the whole transcript");
        assert_eq!(s.message_count(c).unwrap(), 0);
        assert!(s.search("three", 10).unwrap().is_empty(), "the search copy goes with the rows");
        assert_eq!(s.messages(other).unwrap()[0].content, "keep me");
        // Back to nothing is a legal answer, not an error: the next send starts the conversation
        // over, which is exactly what editing the very first message asks for.
        assert_eq!(s.truncate_from(c, 0).unwrap(), 0);
        let _ = a;
    }

    /// The failure this query exists for: a folder that looks empty in the sidebar is not
    /// empty in the store, and deleting the folder then reads as a UI that will not let you
    /// remove it. Chat rows carry the folder path (they were started in it) while the sidebar
    /// files them in their own list; an empty draft carries it too.
    #[test]
    fn a_folder_holds_what_the_sidebar_does_not_show_under_it() {
        let s = store();
        let cowork = s.create_session("Read the quarterly numbers", "/tmp/folder-a", "m").unwrap();
        let chat = s.create_session("Chat in the same folder", "/tmp/folder-a", "m").unwrap();
        s.set_session_persona(chat, "chat").unwrap();
        let draft = s.create_session("new session", "/tmp/folder-a", "m").unwrap();
        s.create_session("Somewhere else", "/tmp/folder-b", "m").unwrap();
        s.append_message(cowork, &Message::user("mangosteen")).unwrap();

        // What the folder row claims: all three.
        let listed = s.list_workspaces().unwrap();
        let a = listed.iter().find(|w| w.path == "/tmp/folder-a").expect("folder listed");
        assert_eq!(a.sessions, 3, "the folder count must include chats and drafts");

        // What is actually inside, which is what a delete must act on.
        let mut ids: Vec<i64> = s.sessions_in("/tmp/folder-a").unwrap().into_iter().map(|x| x.id).collect();
        ids.sort();
        assert_eq!(ids, vec![cowork, chat, draft], "sessions_in missed rows the folder holds");

        // A row is not filed in two folders at once.
        assert_eq!(s.sessions_in("/tmp/folder-b").unwrap().len(), 1);
        // An unknown folder is an empty answer, not an error: the caller has to be able to
        // ask "is anything left inside?" after a delete without bracing for a failure.
        assert!(s.sessions_in("/tmp/no-such-folder").unwrap().is_empty());
    }

    use super::*;
    use crate::provider::{Message, Role, ToolCall};

    fn store() -> Store {
        Store::in_memory().expect("in-memory store")
    }

    fn sid(s: &Store) -> i64 {
        s.create_session("t", "/tmp", "m").unwrap()
    }

    #[test]
    fn attachments_round_trip_but_transcript_reads_stay_light() {
        let s = store();
        let id = sid(&s);
        let msg = Message::user_with(
            "what is in this picture?",
            vec![
                crate::provider::Attachment {
                    mime: "image/png".into(),
                    data_b64: "aGVsbG8=".into(),
                    name: Some("shot.png".into()),
                },
                crate::provider::Attachment {
                    mime: "image/jpeg".into(),
                    data_b64: "aGk=".into(),
                    name: None,
                },
            ],
        );
        s.append_message(id, &msg).unwrap();

        // The agent prompt needs the bytes back, in order.
        let h = s.history(id).unwrap();
        assert_eq!(h[0].attachments.len(), 2);
        assert_eq!(h[0].attachments[0].data_b64, "aGVsbG8=");
        assert_eq!(h[0].attachments[0].name.as_deref(), Some("shot.png"));
        assert_eq!(h[0].attachments[1].mime, "image/jpeg");

        // A transcript page must not carry megabytes of base64 over IPC.
        let page = s.messages_before(id, None, 10).unwrap();
        assert_eq!(page[0].attachments.len(), 2);
        assert_eq!(page[0].attachments[0].name.as_deref(), Some("shot.png"));
        assert_eq!(page[0].attachments[0].bytes, 6);
        assert_eq!(page[0].content, "what is in this picture?");
    }

    #[test]
    fn deleting_a_session_takes_its_attachments_with_it() {
        let s = store();
        let id = sid(&s);
        s.append_message(
            id,
            &Message::user_with(
                "x",
                vec![crate::provider::Attachment {
                    mime: "image/png".into(),
                    data_b64: "aGVsbG8=".into(),
                    name: None,
                }],
            ),
        )
        .unwrap();
        s.delete_session(id).unwrap();
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "orphaned blobs would outlive the session that owns them");
    }

        /// Naming is the titler's job now, not the store's.
    ///
    /// This used to cut the opening line into the title the moment the first message
    /// was stored. That is gone on purpose: a row is created as "New Session",
    /// `crate::titling` asks the model for a name in its own context before the first
    /// answer, and the name then arrives once, animated, from the engine - rather than
    /// silently from a write path the reader never sees.
    #[test]
    fn a_new_row_keeps_its_placeholder_until_someone_names_it() {
        let s = store();
        let id = s.create_session("", "/w", "m").unwrap();
        assert_eq!(s.session(id).unwrap().unwrap().title, UNTITLED);

        let msg = Message {
            role: Role::User,
            content: "  can you\n\nmake me a styled   excel sheet please\n".into(),
            ..Default::default()
        };
        s.append_message(id, &msg).unwrap();
        assert_eq!(
            s.session(id).unwrap().unwrap().title,
            UNTITLED,
            "storing the first message must not rename the row behind the titler's back"
        );

        // A rename is the only way a row's name changes, and it sticks.
        s.rename_session(id, "quarterly numbers").unwrap();
        s.append_message(
            id,
            &Message { role: Role::User, content: "second question".into(), ..Default::default() },
        )
        .unwrap();
        assert_eq!(s.session(id).unwrap().unwrap().title, "quarterly numbers");
    }

    #[test]
    fn titles_are_single_line_and_bounded() {
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let t = derive_title(&format!("\n\t  {long}  \n\n"));
        assert!(!t.contains('\n') && !t.contains('\t'));
        assert!(t.chars().count() <= 49, "too long: {} chars", t.chars().count());
        assert!(t.ends_with('\u{2026}'));
        assert_eq!(derive_title("   "), "");
    }

    #[test]
    fn migration_v6_strips_env_baked_into_old_rows() {
        // Rows written before the fix carry the block in `content`. Re-open them
        // through the migrator and check the transcript text is clean while the
        // question survives.
        let dir = std::env::temp_dir().join(format!("cwoffee_v6_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            migrate(&conn, 0).unwrap();
            conn.execute(
                "INSERT INTO sessions (id,title,workspace,model,created_at,updated_at) VALUES (1,'new session','/w','m',1,1)",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO messages (id,session_id,role,content,reasoning,reasoning_signature,tool_call_id,tool_calls_json,created_at) VALUES (1,1,'user','<env>\nos: linux\nworkspace: /w\n</env>\n\nHello there!','','','', '',1)",
                [],
            ).unwrap();
            conn.pragma_update(None, "user_version", 5).unwrap();
        }
        let s = Store::open(&path).unwrap();
        let rows = s.messages(1).unwrap();
        assert_eq!(rows[0].content, "Hello there!");
        // The title backfill also runs, so pre-existing conversations are findable.
        assert_eq!(s.session(1).unwrap().unwrap().title, "Hello there!");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_duration_survives_a_reload() {
        let s = store();
        let sid = s.create_session("t", "/w", "m").unwrap();
        s.append_message(sid, &Message {
            role: Role::Assistant,
            content: "answer".into(),
            reasoning: Some("considered a, then b".into()),
            reasoning_ms: Some(154_000),
            ..Default::default()
        })
        .unwrap();

        let rows = s.messages(sid).unwrap();
        assert_eq!(rows[0].reasoning_ms, 154_000);

        // The prompt rebuild path, which is what a resumed session uses.
        let hist = s.history(sid).unwrap();
        let a = hist.iter().find(|m| matches!(m.role, Role::Assistant)).unwrap();
        assert_eq!(a.reasoning.as_deref(), Some("considered a, then b"));
        assert_eq!(a.reasoning_ms, Some(154_000));
    }

    /// A message typed during a turn is written with the time it was typed, not the time the loop
    /// got round to it. That stamp is the difference between a transcript that reads in the order
    /// the person typed and one that says they asked something after the answer they were reacting
    /// to — and it is the row's own `created_at`, so every read path gets it for free.
    #[test]
    fn a_message_typed_mid_turn_is_written_with_the_time_it_was_typed() {
        let s = store();
        let sid = s.create_session("t", "/w", "m").unwrap();
        s.append_message(sid, &Message::user("the question")).unwrap();
        s.append_message(
            sid,
            &Message {
                role: Role::User,
                content: "steered in later".into(),
                sent_at: Some(1_700_000_000),
                ..Default::default()
            },
        )
        .unwrap();

        let rows = s.messages(sid).unwrap();
        assert_eq!(rows[1].content, "steered in later");
        assert_eq!(rows[1].created_at, 1_700_000_000, "the row was stamped when it landed, not when it was typed");
        // An ordinary send still gets the clock, and the two must not be confused.
        assert!(rows[0].created_at > 1_700_000_000, "a plain row inherited the mid-turn stamp");
    }

    #[test]
    fn no_thinking_stores_zero_not_a_null_guess() {
        let s = store();
        let sid = s.create_session("t", "/w", "m").unwrap();
        s.append_message(sid, &Message { role: Role::Assistant, content: "x".into(), ..Default::default() })
            .unwrap();
        let rows = s.messages(sid).unwrap();
        assert_eq!(rows[0].reasoning_ms, 0);
        assert_eq!(s.history(sid).unwrap()[0].reasoning_ms, None);
    }

    #[test]
    fn empty_titles_are_normalised_at_the_store_boundary() {
        let s = store();
        let a = s.create_session("", "/w", "m").unwrap();
        let b = s.create_session("   ", "/w", "m").unwrap();
        let c = s.create_session("real title", "/w", "m").unwrap();
        assert_eq!(s.session(a).unwrap().unwrap().title, UNTITLED);
        assert_eq!(s.session(b).unwrap().unwrap().title, UNTITLED);
        assert_eq!(s.session(c).unwrap().unwrap().title, "real title");

        // Renaming to blank must not blank the row out of usefulness either.
        s.rename_session(c, "").unwrap();
        assert_eq!(s.session(c).unwrap().unwrap().title, UNTITLED);
    }

    #[test]
    fn a_row_remembers_which_mode_made_it() {
        let s = store();
        let id = s.create_session("chat with no folder", "/tmp", "m").unwrap();
        // The default is the one that describes most conversations, and it is set by the
        // schema rather than by every caller remembering to pass it.
        assert_eq!(s.session(id).unwrap().unwrap().persona, "cowork");
        s.set_session_persona(id, "chat").unwrap();
        assert_eq!(s.list_sessions(10).unwrap()[0].persona, "chat");
        // An unknown value from a stale client normalises; it must not make the row
        // disappear from both lists.
        s.set_session_persona(id, "sideways").unwrap();
        assert_eq!(s.session(id).unwrap().unwrap().persona, "cowork");
        // An old database (no column at all) still comes up with the default on it.
        let path = std::env::temp_dir().join(format!("cw-persona-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let st = Store::open(&path).unwrap();
            st.create_session("one", "/tmp", "m").unwrap();
            st.conn.lock().unwrap().execute_batch("ALTER TABLE sessions DROP COLUMN persona;").ok();
            st.conn.lock().unwrap().pragma_update(None, "user_version", 10).unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.list_sessions(10).unwrap()[0].persona,
            "cowork",
            "a pre-v11 store gains the column and the default, it does not fail to open"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A chat's network grant rides with the conversation, not with the window that granted it.
    #[test]
    fn a_conversation_remembers_whether_it_may_reach_the_web() {
        let s = store();
        let a = sid(&s);
        let b = sid(&s);
        assert!(!s.session(a).unwrap().unwrap().web_on, "off until somebody grants it");
        s.set_session_web(a, true).unwrap();
        assert!(s.session(a).unwrap().unwrap().web_on);
        assert!(!s.session(b).unwrap().unwrap().web_on, "the other conversation is untouched");
        // Read back through the list query too: the sidebar builds its own idea of a chat from that
        // row, and a grant that survives `session()` but not `list_sessions()` is half a setting.
        let listed = s.list_sessions(10).unwrap();
        assert!(listed.iter().any(|x| x.id == a && x.web_on));
        assert!(listed.iter().any(|x| x.id == b && !x.web_on));
        s.set_session_web(a, false).unwrap();
        assert!(!s.session(a).unwrap().unwrap().web_on, "taking it back is a write, not a delete");
        // A conversation filed in a folder reads its grant there as well: `sessions_in` is the third
        // query that builds a `Session`, and the third one is where a missed column hides.
        let s2 = Store::in_memory().unwrap();
        let c = s2.create_session("in a folder", "/home/me/ws", "m").unwrap();
        s2.set_session_web(c, true).unwrap();
        assert!(s2.sessions_in("/home/me/ws").unwrap()[0].web_on);
    }

    /// The counts the About page prints.
    #[test]
    fn the_record_can_say_how_much_is_in_it() {
        let s = store();
        let a = sid(&s);
        s.append_message(a, &Message::user("one")).unwrap();
        s.append_message(a, &Message::assistant("two")).unwrap();
        let b = sid(&s);
        s.append_message(b, &Message::user("three")).unwrap();
        s.delete_session(b).unwrap();
        assert_eq!(s.conversation_count().unwrap(), 1, "a deleted conversation stops counting");
        assert_eq!(s.message_total().unwrap(), 2, "and its rows went with it");
    }

    #[test]
    fn schema_is_idempotent_and_versioned() {
        let s = store();
        assert_eq!(
            s.conn.lock().unwrap().query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn append_and_load_round_trip_preserves_tool_calls() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("hello")).unwrap();
        s.append_message(
            id,
            &Message {
                role: Role::Assistant,
                content: "calling".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "exec".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        s.append_message(id, &Message::tool_result("call_1", "a.txt")).unwrap();

        let h = s.history(id).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[1].tool_calls.len(), 1);
        assert_eq!(h[1].tool_calls[0].id, "call_1");
        assert_eq!(h[1].tool_calls[0].name, "exec");
        assert_eq!(h[2].role, Role::Tool);
        // the pairing must survive, or replaying this session 400s
        assert_eq!(h[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn history_is_ordered_and_append_only() {
        let s = store();
        let id = sid(&s);
        for i in 0..20 {
            s.append_message(id, &Message::user(format!("q{i}"))).unwrap();
        }
        let h = s.history(id).unwrap();
        assert_eq!(h.first().unwrap().content, "q0");
        assert_eq!(h.last().unwrap().content, "q19");
        // ids strictly increase: nothing rewrote an earlier row
        let ids: Vec<i64> = s.messages(id).unwrap().iter().map(|m| m.id).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn paged_history_is_bounded_and_ordered() {
        let s = Store::in_memory().unwrap();
        let id = s.create_session("long", "/w", "m").unwrap();
        for i in 0..50 {
            s.append_message(id, &Message::user(format!("message {i}"))).unwrap();
        }
        assert_eq!(s.message_count(id).unwrap(), 50);

        let page = s.messages_before(id, None, 10).unwrap();
        assert_eq!(page.len(), 10, "window is bounded");
        assert_eq!(page[0].content, "message 40", "newest window");
        assert_eq!(page[9].content, "message 49");
        // Ascending despite the DESC query - the UI appends in reading order.
        let ids: Vec<i64> = page.iter().map(|m| m.id).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));

        let older = s.messages_before(id, Some(page[0].id), 10).unwrap();
        // The page immediately before the first one, still in reading order.
        assert_eq!(older.first().unwrap().content, "message 30", "walks backwards");
        assert_eq!(older.last().unwrap().content, "message 39");
        assert!(
            older.iter().all(|m| m.id < page[0].id),
            "no overlap between pages"
        );

        // Asking for more than exists returns everything, not an error.
        assert_eq!(s.messages_before(id, None, 500).unwrap().len(), 50);
        let rest = s.messages_before(id, Some(older[0].id), 500).unwrap();
        assert_eq!(rest.len(), 30, "the remainder walks back to the start");
        assert_eq!(rest.first().unwrap().content, "message 0");
        // An exhausted window is empty rather than a repeat of the last page.
        assert!(s.messages_before(id, Some(rest[0].id), 500).unwrap().is_empty());
    }

    #[test]
    fn fts_finds_content_and_ranks_it() {
        let s = store();
        let a = sid(&s);
        let b = sid(&s);
        s.append_message(a, &Message::user("how do I configure the llama.cpp endpoint")).unwrap();
        s.append_message(b, &Message::user("totally unrelated about cooking pasta")).unwrap();

        let hits = s.search("llama endpoint", 10).unwrap();
        assert!(!hits.is_empty(), "search found nothing");
        assert_eq!(hits[0].session_id, a);
        assert!(hits[0].snippet.contains("llama") || hits[0].snippet.contains("endpoint"));
        // The match must be marked, or the search UI has nothing to show.
        assert!(hits[0].snippet.contains('['), "snippet has no match markers: {:?}", hits[0].snippet);
    }

    #[test]
    fn search_snippet_is_not_empty_for_any_match() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("the quick brown fox jumps over the lazy dog")).unwrap();
        let hits = s.search("fox", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.is_empty());
        assert!(hits[0].snippet.contains("[fox]"), "got {:?}", hits[0].snippet);
    }

    /// Recall answers a different question from the sidebar's search: a passage has to come
    /// back with its conversation attached, and the conversation that is *asking* has to be
    /// left out — its text is already in front of the model, so returning it costs prompt
    /// bytes to say nothing new.
    #[test]
    fn recall_names_the_conversation_and_leaves_the_asking_one_out() {
        let s = store();
        let a = sid(&s);
        let b = s.create_session("trip plan", "/tmp/ws2", "m").unwrap();
        s.set_session_persona(b, "chat").unwrap();
        s.append_message(a, &Message::user("the quarterly cashflow total is 946010")).unwrap();
        s.append_message(b, &Message::assistant("we leave before the quarterly meeting")).unwrap();

        let hits = s.recall("quarterly", 8, Some(a)).unwrap();
        assert_eq!(hits.len(), 1, "the asking conversation must not come back: {hits:?}");
        assert_eq!(hits[0].session_id, b);
        assert_eq!(hits[0].title, "trip plan", "a passage with no conversation attached is a quote from nowhere");
        assert_eq!(hits[0].persona, "chat");
        assert!(hits[0].snippet.contains("[quarterly]"), "{:?}", hits[0].snippet);

        // Nothing to exclude: both conversations answer.
        assert_eq!(s.recall("quarterly", 8, None).unwrap().len(), 2);
        // Hostile input behaves the same way it does for the sidebar search.
        assert!(s.recall("NOT AND OR \"x", 5, None).is_ok());
    }

    /// FTS5 has query syntax. Raw user input must not be able to break it.
    #[test]
    fn search_never_errors_on_hostile_input() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("the AND OR NOT NEAR operators")).unwrap();
        s.append_message(id, &Message::user("quotes \" and * and : colons")).unwrap();
        for q in [
            "AND", "OR", "NOT", "NEAR", "\"unbalanced", "a*", "a:", "()", "SELECT * FROM x",
            ";;", "🦪", "", "   ", "one \"two\" three",
        ] {
            let r = s.search(q, 5);
            assert!(r.is_ok(), "query {q:?} errored: {:?}", r.err());
        }
    }

    #[test]
    fn porter_stemming_matches_word_forms() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("I am configuring the sandbox")).unwrap();
        assert!(!s.search("configure", 5).unwrap().is_empty());
        assert!(!s.search("configured", 5).unwrap().is_empty());
    }

    #[test]
    fn telemetry_totals_aggregate() {
        let s = store();
        let id = sid(&s);
        let t = |p: u64, c: u64, o: u64| TurnTelemetry {
            prompt_tokens: p,
            cached_tokens: c,
            completion_tokens: o,
            ttft_ms: 500,
            server_prefill_tps: 60.0,
            server_decode_tps: 40.0,
            draft_n: 10,
            draft_accepted: 7,
        };
        s.record_turn(id, &t(1000, 800, 50)).unwrap();
        s.record_turn(id, &t(2000, 1500, 90)).unwrap();
        let tot = s.session_totals(id).unwrap();
        assert_eq!(tot.turns, 2);
        assert_eq!(tot.prompt_tokens, 3000);
        assert_eq!(tot.cached_tokens, 2300);
        assert!((tot.cache_hit_pct() - 76.67).abs() < 0.1);
        assert_eq!(tot.draft_accepted, 14);
    }

    #[test]
    fn delete_session_leaves_no_orphans() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("x")).unwrap();
        s.record_turn(id, &TurnTelemetry::default()).unwrap();
        s.delete_session(id).unwrap();
        assert!(s.session(id).unwrap().is_none());
        assert!(s.messages(id).unwrap().is_empty());
        assert!(s.search("x", 5).unwrap().is_empty(), "fts row survived deletion");
    }

    #[test]
    fn a_workspace_can_be_registered_before_it_has_sessions() {
        let s = Store::in_memory().unwrap();
        s.add_workspace("/home/me/projects/thing").unwrap();
        s.add_workspace("/home/me/projects/thing").unwrap(); // idempotent
        let ws = s.list_workspaces().unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].path, "/home/me/projects/thing");
        assert_eq!(ws[0].sessions, 0, "an empty folder is still a workspace");
    }

    /// A workspace you never added but have sessions in must show anyway: the
    /// sidebar that hides where your work lives is worse than no sidebar.
    #[test]
    fn a_workspace_holding_sessions_lists_itself() {
        let s = Store::in_memory().unwrap();
        s.create_session("a", "/home/me/ws-a", "m").unwrap();
        s.create_session("b", "/home/me/ws-a", "m").unwrap();
        s.create_session("c", "/home/me/ws-b", "m").unwrap();
        let mut ws = s.list_workspaces().unwrap();
        ws.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            ws.iter().map(|w| (w.path.as_str(), w.sessions)).collect::<Vec<_>>(),
            vec![("/home/me/ws-a", 2), ("/home/me/ws-b", 1)]
        );
    }

    /// A scratch folder that was registered and then deleted must not stay in the sidebar. This
    /// happened for real: every probe run left a `/tmp/....../ws` row, and the person's sidebar
    /// ended up a screen of folders named `ws` holding nothing.
    #[test]
    fn a_gone_folder_holding_nothing_stops_showing_and_a_folder_holding_history_does_not() {
        let s = Store::in_memory().unwrap();
        let base = std::env::temp_dir().join(format!("cwoffee-prune-{}", std::process::id()));
        let scratch = base.join("ws");
        let alive = base.join("real");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(&alive).unwrap();
        s.add_workspace(scratch.to_str().unwrap()).unwrap();
        s.add_workspace(alive.to_str().unwrap()).unwrap();
        s.create_session("history outlives the mount", "/mnt/gone/ws", "m").unwrap();
        assert_eq!(s.list_workspaces().unwrap().len(), 3, "two folders plus one from sessions");
        std::fs::remove_dir(&scratch).unwrap();
        assert_eq!(
            s.prune_dead_workspaces().unwrap(),
            1,
            "only the deleted scratch folder: `/mnt/gone/ws` holds a conversation, and a folder that exists is nobody's dead row"
        );
        let left: Vec<String> = s.list_workspaces().unwrap().into_iter().map(|w| w.path).collect();
        assert!(!left.contains(&scratch.to_str().unwrap().to_string()), "gone and empty: gone from the list");
        assert!(left.contains(&alive.to_str().unwrap().to_string()), "a folder that exists is kept");
        assert!(left.contains(&"/mnt/gone/ws".to_string()), "and the conversation still says where it lives");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Removing a folder from the list is not deleting anyone's work.
    #[test]
    fn removing_a_workspace_takes_the_entry_not_the_sessions() {
        let s = Store::in_memory().unwrap();
        let id = s.create_session("keep me", "/home/me/ws-a", "m").unwrap();
        s.remove_workspace("/home/me/ws-a").unwrap();
        // The entry was never explicitly added, so removing it changes nothing
        // about the fact that sessions live there.
        assert_eq!(s.list_workspaces().unwrap().len(), 1);
        s.add_workspace("/home/me/empty").unwrap();
        assert_eq!(s.remove_workspace("/home/me/empty").unwrap(), 1);
        assert_eq!(s.list_workspaces().unwrap().len(), 1);
        assert!(s.session(id).unwrap().is_some(), "the session survived, as it must");
    }

    /// A label names the folder in the sidebar without touching the path the
    /// tools are confined to - the path is the fact, the label is decoration.
    #[test]
    fn a_workspace_label_renames_the_row_and_not_the_folder() {
        let s = Store::in_memory().unwrap();
        let id = s.create_session("a", "/home/me/projects/inventory-app", "m").unwrap();
        s.add_workspace("/home/me/projects/inventory-app").unwrap();
        assert_eq!(s.set_workspace_label("/home/me/projects/inventory-app", "  inventory  ").unwrap(), 1);
        let ws = s.list_workspaces().unwrap();
        assert_eq!(ws[0].label, "inventory", "surrounding spaces are not part of a name");
        assert_eq!(ws[0].path, "/home/me/projects/inventory-app");
        assert_eq!(s.session(id).unwrap().unwrap().workspace, "/home/me/projects/inventory-app");
        // An unknown path renames nothing, rather than creating a stray row.
        assert_eq!(s.set_workspace_label("/home/me/nope", "ghost").unwrap(), 0);
    }

    /// A colour belongs to the folder row, not to the window that set it: the point of colouring a
    /// folder is that every client reading the list can tell two same-shaped names apart.
    #[test]
    fn a_folder_colour_travels_with_the_folder() {
        let s = Store::in_memory().unwrap();
        // A folder with sessions in it but no row of its own still appears in the sidebar, so
        // colouring it has to create the row it needs. This is the difference from a rename, which
        // refuses an unknown path: a colour that silently does not stick on the folder the person
        // is looking at is the failure this path exists to avoid.
        s.create_session("a", "/home/me/projects/inventory-app", "m").unwrap();
        assert_eq!(s.list_workspaces().unwrap()[0].color, "", "no colour until one is chosen");
        assert_eq!(
            s.set_workspace_color("/home/me/projects/inventory-app", "#3D8F6F").unwrap(),
            1
        );
        assert_eq!(
            s.list_workspaces().unwrap()[0].color,
            "#3d8f6f",
            "stored lower-case, so the client comparing it against a palette agrees with itself"
        );
        // Clearing it is a value, not a delete: the label and the count survive.
        assert_eq!(s.set_workspace_color("/home/me/projects/inventory-app", "").unwrap(), 1);
        assert_eq!(s.list_workspaces().unwrap()[0].color, "");
        assert_eq!(s.list_workspaces().unwrap()[0].sessions, 1);
        // A value that is not a colour is refused where it is written, not filtered on the way
        // out: the row is what every client is handed, and one of them puts it in a style.
        for bad in ["red", "#3d8f6", "#gg8f6f", "#3d8f6f; url(x)", "rgb(1,2,3)", "#3d8f6ff"] {
            assert!(
                s.set_workspace_color("/home/me/projects/inventory-app", bad).is_err(),
                "{bad} is not a colour"
            );
        }
        assert_eq!(s.list_workspaces().unwrap()[0].color, "", "a refused write changed nothing");
    }

    /// A database from before the column exists opens, and its folders read as uncoloured.
    #[test]
    fn an_old_store_gains_the_colour_column_without_losing_the_folder() {
        let path = std::env::temp_dir().join(format!("cw-wscolour-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let st = Store::open(&path).unwrap();
            st.add_workspace("/home/me/notes").unwrap();
            st.set_workspace_label("/home/me/notes", "old notes").unwrap();
            st.conn.lock().unwrap().execute_batch("ALTER TABLE workspaces DROP COLUMN color;").ok();
            st.conn.lock().unwrap().pragma_update(None, "user_version", 12).unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        let ws = reopened.list_workspaces().unwrap();
        assert_eq!(ws[0].label, "old notes", "the folder and its name survived the upgrade");
        assert_eq!(ws[0].color, "", "a pre-v13 store reads as no colour, it does not fail to open");
        assert_eq!(reopened.set_workspace_color("/home/me/notes", "#c08a2e").unwrap(), 1);
        assert_eq!(reopened.list_workspaces().unwrap()[0].color, "#c08a2e");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_sessions_is_newest_first_and_counted() {
        let s = store();
        let a = sid(&s);
        let b = sid(&s);
        s.append_message(b, &Message::user("hi")).unwrap();
        let list = s.list_sessions(10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, b, "most recently touched should be first");
        assert_eq!(list[0].message_count, 1);
        assert_eq!(list[1].id, a);
    }

    /// updated_at has one-second resolution, so sessions touched in the same
    /// second tie. Without a deterministic tiebreak the sidebar reorders itself
    /// between renders, which looks like a flickering list.
    #[test]
    fn session_order_is_deterministic_within_the_same_second() {
        let s = store();
        let ids: Vec<i64> = (0..15).map(|_| sid(&s)).collect();
        let first = s.list_sessions(50).unwrap().iter().map(|x| x.id).collect::<Vec<_>>();
        for _ in 0..5 {
            let again = s.list_sessions(50).unwrap().iter().map(|x| x.id).collect::<Vec<_>>();
            assert_eq!(first, again, "session order changed between identical queries");
        }
        // newest id first
        assert_eq!(first[0], *ids.last().unwrap());
    }

    #[test]
    fn rename_updates_the_row_not_the_history() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("keep me")).unwrap();
        s.rename_session(id, "new title").unwrap();
        assert_eq!(s.session(id).unwrap().unwrap().title, "new title");
        assert_eq!(s.history(id).unwrap()[0].content, "keep me");
    }

    #[test]
    fn empty_search_returns_nothing_instead_of_everything() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("a")).unwrap();
        assert!(s.search("", 10).unwrap().is_empty());
        assert!(s.search("   ", 10).unwrap().is_empty());
    }

    #[test]
    fn signed_thinking_survives_a_round_trip() {
        let s = store();
        let id = sid(&s);
        s.append_message(
            id,
            &Message {
                role: Role::Assistant,
                content: "answered".into(),
                reasoning: Some("deliberations".into()),
                reasoning_signature: Some("EqMC7Q2...opaque...abc".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let h = s.history(id).unwrap();
        assert_eq!(h[0].reasoning.as_deref(), Some("deliberations"));
        assert_eq!(h[0].reasoning_signature.as_deref(), Some("EqMC7Q2...opaque...abc"));
    }

    /// In-memory tests never touch WAL, never create files, and never reopen.
    /// Resuming a session across process restarts is the whole point of the
    /// store, so it gets tested as a file on disk.
    #[test]
    fn file_store_persists_across_reopen() {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cwoffee_store_test_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cwoffee.db");

        {
            let s = Store::open(&path).unwrap();
            let id = s.create_session("resume me", "/tmp", "model-x").unwrap();
            s.append_message(id, &Message::user("first question")).unwrap();
            s.append_message(
                id,
                &Message {
                    role: Role::Assistant,
                    content: "called".into(),
                    tool_calls: vec![ToolCall { id: "c1".into(), name: "exec".into(), arguments: "{}".into() }],
                    ..Default::default()
                },
            )
            .unwrap();
            s.append_message(id, &Message::tool_result("c1", "output")).unwrap();
            s.record_turn(id, &TurnTelemetry { prompt_tokens: 100, cached_tokens: 80, completion_tokens: 5, ..Default::default() }).unwrap();
        }

        // Fresh handle, as if the app had quit and started again.
        let s = Store::open(&path).unwrap();
        let list = s.list_sessions(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "resume me");
        assert_eq!(list[0].model, "model-x");

        let h = s.history(list[0].id).unwrap();
        assert_eq!(h.len(), 3, "history did not survive reopen");
        assert_eq!(h[1].tool_calls[0].id, "c1");
        assert_eq!(h[2].tool_call_id.as_deref(), Some("c1"));

        // FTS index is persistent, not rebuilt at open.
        assert!(!s.search("question", 5).unwrap().is_empty(), "fts index lost on reopen");
        assert_eq!(s.session_totals(list[0].id).unwrap().turns, 1);

        // WAL sidecar files should exist while open; that is what lets the UI
        // read during a write. Assert the mode rather than the files.
        let mode = s.conn.lock().unwrap().query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "expected WAL, got {mode}");

        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cjk_content_round_trips() {
        let s = store();
        let id = sid(&s);
        s.append_message(id, &Message::user("这是一个关于性能的问题")).unwrap();
        let h = s.history(id).unwrap();
        assert_eq!(h[0].content, "这是一个关于性能的问题");
    }

    /// The hand-off at window close has two processes on one file for a few hundred
    /// milliseconds: the window that is closing, and the standalone gateway it started. With no
    /// busy timeout the newcomer's first write is SQLITE_BUSY and it exits at boot, so the
    /// setting reads as "did not work" rather than as a lock it should have waited out.
    #[test]
    fn two_stores_on_one_file_wait_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("cw-store-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shared.db");
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        let timeout: i64 = b
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert!(timeout >= 5000, "busy_timeout was {timeout} ms; a hand-off will race");
        let sa = a.create_session("closing window", "/tmp/ws", "m").unwrap();
        let sb = b.create_session("starting gateway", "/tmp/ws", "m").unwrap();
        assert_ne!(sa, sb, "one of the two writes was lost");
        a.append_message(sa, &Message::user("from the window")).unwrap();
        b.append_message(sb, &Message::user("from the gateway")).unwrap();
        assert_eq!(b.history(sa).unwrap().len(), 1, "the other process's write is not visible");
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
