//! Recalling what was said in **other** conversations.
//!
//! The store already answers this question for the sidebar — that is what its FTS index is
//! for — but a person clicking a result and a model choosing what to re-read are different
//! jobs. A sidebar hit is a row to navigate to; a model needs the passage *attached to the
//! conversation it came from*, because "that was the conversation about the Q3 numbers" is
//! the only basis it has for deciding to ask for the whole thing. So this tool does three
//! things and says which one it did:
//!
//!   * nothing but `{}` — what conversations exist at all;
//!   * `{"query": "…"}` — passages in the other conversations that match;
//!   * `{"session": 12}` — the recent rows of one conversation.
//!
//! The conversation that is asking is left out of a search when the host can say which one
//! it is: its text is already in front of the model, and feeding it back costs prompt bytes
//! to say nothing new. When the host cannot say — a pi process that serves several
//! conversations, a probe, an example — the result says so instead of quietly making a
//! claim it cannot support.
//!
//! Deliberately read-only, and `Harmless`: the data is the user's own, on their own disk, in
//! the app that already shows it in the sidebar. An approval card here would train a person
//! to click through prompts without reading them, which is worse than no prompt at all.

use async_trait::async_trait;
use chrono::{Local, TimeZone};
use serde_json::{json, Value};

use super::{Risk, Tool, ToolCtx, ToolResult};

/// Rows shown when asked what conversations exist.
const LIST_LIMIT: i64 = 20;
/// Passages returned for a search. Each carries a snippet, not a whole message: the point
/// is to decide whether to open the conversation, and a page of full messages is prompt
/// bytes spent before the model has chosen anything.
const HIT_LIMIT: i64 = 8;
/// Rows returned when reading one conversation back.
const SESSION_ROWS: i64 = 30;
/// Per-row clamp in session mode. The total cap still applies on top of this; this one
/// exists so that one 40 KB tool dump cannot eat the whole reply and hide the other 29 rows.
const ROW_CLAMP: usize = 900;

pub struct RecallTool;

impl RecallTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RecallTool {
    fn default() -> Self {
        Self::new()
    }
}

fn day(ts: i64) -> String {
    TimeZone::timestamp_opt(&Local, ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

/// One line of prose per row, clamped. Roles are named, not styled: a model reading
/// "assistant: …" knows whose words it is re-reading, which matters when it is deciding
/// whether to trust the memory.
fn clamp_row(role: &str, content: &str, cap: usize) -> String {
    let one = content.lines().collect::<Vec<_>>().join(" ");
    let one = one.trim();
    if one.len() <= cap {
        return format!("{role}: {one}");
    }
    let mut end = cap;
    while end > 0 && !one.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}: {}…", role, &one[..end])
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search what the user's *other* conversations here said. {} lists them; \
         {\"query\":\"words\"} finds passages (all words must appear); {\"session\":N} reads \
         one back. Read-only: this app's own history, on this machine, nothing else."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words to look for in the other conversations. All must appear."
                },
                "session": {
                    "type": "integer",
                    "description": "Read one conversation back: recent rows, oldest first."
                },
                "limit": {
                    "type": "integer",
                    "description": "Passages (search) or rows (session) to return. 1-30."
                }
            },
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &super::ExecSink) -> ToolResult {
        let Some(store) = ctx.store.as_ref() else {
            // Not "found nothing". A host that never told the tool where the history lives
            // has a different problem, and a model that hears "nothing matched" will stop
            // looking when the real answer is "nobody looked".
            return ToolResult::err(
                "recall is not available here: this host gave the tool no conversation store",
            );
        };

        let session = args.get("session").and_then(Value::as_i64);
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string);
        // Not `unwrap_or(0).clamp(1, 30)`: that makes "no limit given" mean *one row*, which
        // is what the first version did — a default read came back with a single line and a
        // header claiming rows 2-2 of 2. Absent means this mode's own default.
        let asked = args.get("limit").and_then(Value::as_i64);

        // ---- read one conversation back --------------------------------------------------
        if let Some(id) = session {
            let sess = match store.session(id) {
                Ok(s) => s,
                Err(e) => return ToolResult::err(format!("recall could not read the store: {e}")),
            };
            let Some(sess) = sess else {
                return ToolResult::err(format!(
                    "there is no conversation {id} in this store. Ask for the conversations \
                     that exist with recall{{}}."
                ));
            };
            let rows = match store.messages(id) {
                Ok(m) => m,
                Err(e) => return ToolResult::err(format!("recall could not read the store: {e}")),
            };
            let take = asked.unwrap_or(SESSION_ROWS).clamp(1, 30).min(SESSION_ROWS);
            let skip = rows.len().saturating_sub(take as usize);
            let mut out = format!(
                "\"{}\" ({}), folder {} — rows {}-{} of {}.\n\n",
                sess.title,
                sess.persona,
                sess.workspace,
                skip + 1,
                rows.len(),
                rows.len()
            );
            let mut truncated = false;
            for m in &rows[skip..] {
                let line = clamp_row(&m.role, &m.content, ROW_CLAMP);
                if out.len() + line.len() + 1 > ctx.max_output_bytes {
                    truncated = true;
                    break;
                }
                out.push_str(&line);
                out.push('\n');
            }
            if truncated {
                out.push_str("…(cut off: ask again for the rest, or for a smaller limit)\n");
            }
            return ToolResult {
                truncated,
                ..ToolResult::ok(out)
            };
        }

        // ---- what exists at all ---------------------------------------------------------
        if query.is_none() {
            let list = match store.list_sessions(LIST_LIMIT) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("recall could not read the store: {e}")),
            };
            let mut out = format!("{} conversation(s) on this machine:\n\n", list.len());
            let mut truncated = false;
            for s in list {
                let line = format!(
                    "{}. \"{}\" — {}, {} row(s), last touched {}, folder {}",
                    s.id,
                    s.title,
                    s.persona,
                    s.message_count,
                    day(s.updated_at),
                    s.workspace
                );
                if out.len() + line.len() + 1 > ctx.max_output_bytes {
                    truncated = true;
                    break;
                }
                out.push_str(&line);
                out.push('\n');
            }
            if truncated {
                out.push_str("…(cut off: search with a query instead of listing)\n");
            }
            out.push_str("\nPass one id back as {\"session\": id} to read a conversation, or \
                 search with {\"query\": \"words\"}.");
            return ToolResult {
                truncated,
                ..ToolResult::ok(out)
            };
        }

        // ---- search the other conversations ---------------------------------------------
        let q = query.clone().unwrap_or_default();
        let hits = match store.recall(&q, asked.unwrap_or(HIT_LIMIT).clamp(1, 30), ctx.session) {
            Ok(h) => h,
            Err(e) => return ToolResult::err(format!("recall could not search the store: {e}")),
        };
        let scope = match ctx.session {
            Some(id) => format!("conversation {id} itself is left out of these results."),
            None => "note: this host cannot say which conversation is asking, so the one you \
                     are in may be among these."
                .to_string(),
        };
        if hits.is_empty() {
            // Not an error: the honest answer to "what have I said about X" can be "nothing
            // in the other conversations", and the model should be able to act on that.
            return ToolResult::ok(format!(
                "nothing in the other conversations mentions \"{q}\". {scope}"
            ));
        }
        let mut out = format!(
            "{} passage(s) matching \"{q}\".\n\n",
            hits.len()
        );
        let mut truncated = false;
        let mut last_session: Option<i64> = None;
        for (i, h) in hits.iter().enumerate() {
            if Some(h.session_id) != last_session {
                let head = format!(
                    "from \"{}\" ({}, {}):",
                    h.title,
                    h.persona,
                    day(h.created_at)
                );
                if out.len() + head.len() + 1 > ctx.max_output_bytes {
                    truncated = true;
                    break;
                }
                out.push_str(&head);
                out.push('\n');
                last_session = Some(h.session_id);
            }
            let line = format!("  {}. {}: {}", i + 1, h.role, h.snippet);
            if out.len() + line.len() + 1 > ctx.max_output_bytes {
                truncated = true;
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        if truncated {
            out.push_str("…(cut off)\n");
        }
        out.push_str(&format!(
            "\n{scope} Read one whole conversation with {{\"session\": {}}}.",
            hits[0].session_id
        ));
        return ToolResult {
            truncated,
            ..ToolResult::ok(out)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use crate::store::Store;
    use std::sync::Arc;

    fn store_with(rows: &[(&str, &str, &str, &str)]) -> Store {
        // (session title, persona, role, content)
        let s = Store::in_memory().unwrap();
        let mut cur: Option<(i64, String)> = None;
        for (title, persona, role, content) in rows {
            let sid = match &cur {
                Some((id, t)) if t == title => *id,
                _ => {
                    let id = s.create_session(title, "/tmp/ws", "m").unwrap();
                    s.set_session_persona(id, persona).unwrap();
                    cur = Some((id, title.to_string()));
                    id
                }
            };
            let m = match *role {
                "assistant" => Message::assistant(content.to_string()),
                "tool" => Message::tool_result("c1", content.to_string()),
                _ => Message::user(content.to_string()),
            };
            s.append_message(sid, &m).unwrap();
        }
        s
    }

    fn ctx(store: Store, session: Option<i64>) -> ToolCtx {
        ToolCtx {
            store: Some(Arc::new(store)),
            session,
            max_output_bytes: 8 * 1024,
            ..Default::default()
        }
    }

    async fn run(ctx: &ToolCtx, args: Value) -> ToolResult {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RecallTool::new().run(args, ctx, &tx).await
    }

    #[tokio::test]
    async fn recall_finds_another_conversation_and_names_it() {
        let store = store_with(&[
            ("Q3 numbers", "cowork", "user", "the quarterly cashflow total is 946010"),
            ("Trip plan", "chat", "user", "we should leave before the quarterly budget meeting"),
        ]);
        let c = ctx(store, Some(1));
        let r = run(&c, json!({ "query": "quarterly" })).await;
        assert!(r.ok, "{}", r.error.unwrap_or_default());
        assert!(r.output.contains("Trip plan"), "{}", r.output);
        assert!(r.output.contains("946010") == false, "the excluded conversation must not come back:\n{}", r.output);
        assert!(r.output.contains("left out"), "{}", r.output);
    }

    #[tokio::test]
    async fn recall_reads_one_conversation_back() {
        let store = store_with(&[
            ("Q3 numbers", "cowork", "user", "first thing"),
            ("Q3 numbers", "cowork", "assistant", "second thing"),
        ]);
        let c = ctx(store, None);
        let r = run(&c, json!({ "session": 1 })).await;
        assert!(r.ok, "{}", r.error.unwrap_or_default());
        assert!(r.output.contains("user: first thing"), "{}", r.output);
        assert!(r.output.contains("assistant: second thing"), "{}", r.output);
        assert!(r.output.contains("rows 1-2 of 2"), "{}", r.output);
    }

    #[tokio::test]
    async fn no_store_says_that_rather_than_found_nothing() {
        let c = ToolCtx::default();
        let r = run(&c, json!({ "query": "anything" })).await;
        let e = r.error.unwrap_or_default();
        assert!(!e.is_empty(), "expected a refusal, not silence");
        assert!(e.contains("no conversation store"), "{e}");
    }

    #[tokio::test]
    async fn an_unknown_conversation_is_refused_in_words() {
        let store = store_with(&[("Q3 numbers", "cowork", "user", "hi")]);
        let c = ctx(store, None);
        let r = run(&c, json!({ "session": 4242 })).await;
        let e = r.error.unwrap_or_default();
        assert!(!e.is_empty(), "expected a refusal, not silence");
        assert!(e.contains("no conversation 4242"), "{e}");
    }

    #[tokio::test]
    async fn nothing_found_is_an_answer_not_a_failure() {
        let store = store_with(&[("Q3 numbers", "cowork", "user", "cashflow")]);
        let c = ctx(store, Some(1));
        let r = run(&c, json!({ "query": "flamenco" })).await;
        assert!(r.ok, "expected an honest empty answer, got {:?}", r.error);
        assert!(r.output.contains("nothing in the other conversations"), "{}", r.output);
    }

    #[tokio::test]
    async fn listing_names_the_conversations_that_exist() {
        let store = store_with(&[
            ("Q3 numbers", "cowork", "user", "a"),
            ("Trip plan", "chat", "user", "b"),
        ]);
        let c = ctx(store, None);
        let r = run(&c, json!({})).await;
        assert!(r.ok, "{}", r.error.unwrap_or_default());
        assert!(r.output.contains("\"Q3 numbers\""), "{}", r.output);
        assert!(r.output.contains("\"Trip plan\""), "{}", r.output);
    }

    #[tokio::test]
    async fn a_small_cap_stops_and_says_it_was_cut() {
        let store = store_with(&[
            ("Q3 numbers", "cowork", "user", "cashflow cashflow cashflow cashflow cashflow"),
            ("Trip plan", "chat", "user", "cashflow cashflow cashflow cashflow cashflow"),
        ]);
        let mut c = ctx(store, None);
        c.max_output_bytes = 120;
        let r = run(&c, json!({ "query": "cashflow" })).await;
        assert!(r.truncated, "the cap must be reported, not silently applied:\n{}", r.output);
        assert!(r.output.contains("cut off"), "{}", r.output);
    }
}
