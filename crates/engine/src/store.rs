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
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Migration { from, to } => {
                write!(f, "database schema v{from} is newer than this build supports (v{to})")
            }
            StoreError::FtsUnavailable(m) => write!(f, "full-text search unavailable: {m}"),
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
const SCHEMA_VERSION: i64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub title: String,
    pub workspace: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::setup(conn)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::setup(conn)
    }

    fn setup(conn: Connection) -> Result<Self, StoreError> {
        // WAL: the sidebar reads while the agent writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
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

    pub fn create_session(&self, title: &str, workspace: &str, model: &str) -> Result<i64, StoreError> {
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
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1", params![id, title, now_ts()])?;
        Ok(())
    }

    pub fn delete_session(&self, id: i64) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        conn.execute("DELETE FROM messages_fts WHERE session_id = ?1", params![id])?;
        conn.execute("DELETE FROM turns WHERE session_id = ?1", params![id])?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: i64) -> Result<Vec<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.title, s.workspace, s.model, s.created_at, s.updated_at,
                    (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s
             ORDER BY s.updated_at DESC, s.id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                workspace: r.get(2)?,
                model: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
                message_count: r.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn session(&self, id: i64) -> Result<Option<Session>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT s.id, s.title, s.workspace, s.model, s.created_at, s.updated_at,
                        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
                 FROM sessions s WHERE s.id = ?1",
                params![id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        workspace: r.get(2)?,
                        model: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        message_count: r.get(6)?,
                    })
                },
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
        let now = now_ts();
        let tool_calls = if m.tool_calls.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&m.tool_calls).unwrap_or_default()
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning, reasoning_signature, tool_call_id, tool_calls_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                role_str(&m.role),
                m.content,
                m.reasoning.clone().unwrap_or_default(),
                m.reasoning_signature.clone().unwrap_or_default(),
                m.tool_call_id,
                tool_calls,
                now
            ],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages_fts (rowid, content, session_id, message_id, role)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, m.content, session_id, id, role_str(&m.role)],
        )?;
        conn.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, now])?;
        Ok(id)
    }

    pub fn messages(&self, session_id: i64) -> Result<Vec<StoredMessage>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content, reasoning, tool_call_id, tool_calls_json,
                    reasoning_signature, created_at
             FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(StoredMessage {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                reasoning: r.get(4)?,
                tool_call_id: r.get(5)?,
                tool_calls_json: r.get(6)?,
                reasoning_signature: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Rehydrate a PromptBuilder's history. Tool calls are restored with their
    /// ids, because a tool_result whose call is missing is a hard 400.
    pub fn history(&self, session_id: i64) -> Result<Vec<Message>, StoreError> {
        let stored = self.messages(session_id)?;
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
                tool_calls,
                // Must survive the round trip: a tool_result whose call id is
                // missing is rejected outright by every provider.
                tool_call_id: s.tool_call_id,
                reasoning: if s.reasoning.is_empty() { None } else { Some(s.reasoning) },
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn store() -> Store {
        Store::in_memory().expect("in-memory store")
    }

    fn sid(s: &Store) -> i64 {
        s.create_session("t", "/tmp", "m").unwrap()
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
            "cowry_store_test_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cowry.db");

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
}
