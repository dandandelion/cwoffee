//! pi's stream records → cwoffee's `EngineEvent` vocabulary.
//!
//! The mapping is not one-to-one, and the two places where it is not are the two
//! that would bite hardest if guessed at:
//!
//! **pi's "turn" is not cwoffee's "turn."** pi emits a `turn_start`/`turn_end`
//! pair per *model round trip*: a prompt that calls three tools produces four.
//! cwoffee's turn is one user prompt including its whole tool loop, and front
//! ends lean on that — `TurnEnd` is where markdown is parsed once (PLAN.md §9
//! rule 3), and `state().turns` is how a test knows a turn finished without
//! racing the stream. Translating pi's turns 1:1 would re-render a half-finished
//! answer after every tool call and count one prompt as four turns. So the bridge
//! opens a turn at `agent_start` and closes it at `agent_settled`, and pi's inner
//! turn boundaries emit nothing at all.
//!
//! **pi reports no timings.** cwoffee's live rate bar and its per-turn telemetry
//! come from llama.cpp-style `timings`, which pi does not expose. What pi does
//! give is cumulative `usage` on each streamed record, which is enough to
//! synthesize the same fields: time-to-first-delta from our own clock, decode
//! rate from tokens over streamed milliseconds, and a prompt count that has to
//! add the cache terms back in because pi's `usage.input` *excludes* cached
//! tokens. `server_prefill_tps` and the speculative-decode counters have no
//! source, and stay 0 rather than being guessed at — a number that looks
//! measured and is not is worse than a missing one.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use cwoffee_engine::event::{EngineEvent, ExecStream, FinishReason};
use cwoffee_engine::provider::{Attachment, Message, Role, ToolCall};
use cwoffee_engine::telemetry::{TurnStats, TurnTelemetry};
use cwoffee_engine::tools::changes::FileChange;
use serde_json::Value;

/// A dialog pi wants a human to answer. pi's ids are UUIDs; cwoffee's approval
/// and question ids are `u64`, so the bridge keeps both (see `host.rs`).
#[derive(Debug, Clone)]
pub enum UiRequest {
    Confirm { pi_id: String, title: String, message: String, timeout_ms: u64 },
    Select { pi_id: String, title: String, options: Vec<String>, timeout_ms: u64 },
    Input { pi_id: String, title: String, placeholder: String, timeout_ms: u64 },
    /// A dialog method we do not render (`editor`). Answering `cancelled` is how
    /// we say "nobody is going to answer this" instead of letting the extension
    /// hang until its own timeout.
    Unsupported { pi_id: String, method: String },
}

#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<EngineEvent>,
    /// Transcript rows to persist, in order. pi is the one that echoes the user's
    /// message back, so the user row comes from here too rather than from an
    /// optimistic append that could disagree with what the model actually saw.
    pub rows: Vec<Message>,
    /// True when this record ended the run: the turn is closed and `busy` clears.
    pub settled: bool,
    pub ui: Vec<UiRequest>,
    pub note: Option<String>,
    /// Files a tool produced, keyed by the tool-call id of the row that made them.
    /// The row is written before the tool result is known — pi echoes the row, then
    /// reports the result — so the stamps are applied after the append, which is what
    /// the Rust core does with the same store verb.
    pub files_by_call: Vec<(String, Vec<cwoffee_engine::tools::OutputFile>)>,
    /// True when this record began a compaction, so the host can count them.
    pub compaction_start: bool,
    /// Context size as of the last assistant message, in tokens: input + cached,
    /// because that is what the window is actually holding.
    pub context_tokens: Option<usize>,
}

/// One prompt's tool loop, from `agent_start` to `agent_settled`.
#[derive(Debug)]
struct Run {
    started: Instant,
    first_delta: Option<Instant>,
    last_stats: Option<Instant>,
    /// Summed over the assistant messages of this run.
    prompt_tokens: u64,
    cached_tokens: u64,
    completion_tokens: u64,
    chars: u64,
    thinking: bool,
    finish: FinishReason,
    /// Text and reasoning streamed but not yet closed by a `message_end` — which
    /// is what a killed process leaves behind, and what the transcript keeps.
    open_text: String,
    open_reasoning: String,
    context_tokens: Option<usize>,
    /// Time the run spent waiting on a human, and whether it is waiting right
    /// now. A rate that counts a person thinking as slow generation is a lie, and
    /// a measurable one: the first bridge run reported 2 tok/s for a turn where
    /// the human took four seconds to answer an approval card.
    parked_ms: u64,
    parked_since: Option<Instant>,
}

impl Run {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            first_delta: None,
            last_stats: None,
            prompt_tokens: 0,
            cached_tokens: 0,
            completion_tokens: 0,
            chars: 0,
            thinking: false,
            finish: FinishReason::Stop,
            open_text: String::new(),
            open_reasoning: String::new(),
            context_tokens: None,
            parked_ms: 0,
            parked_since: None,
        }
    }

    /// How long this run has spent waiting on a person, including a card that is
    /// still open: if the turn ends with a dialog outstanding, that wait is over,
    /// and none of it was ever generation.
    fn parked_total(&self, now: Instant) -> u64 {
        match self.parked_since {
            Some(since) => self.parked_ms + now.duration_since(since).as_millis() as u64,
            None => self.parked_ms,
        }
    }
}

#[derive(Debug, Default)]
pub struct Translator {
    /// The conversation the events are stamped with. Set by the host before each
    /// run; `TurnStart { session }` is what stops a second client painting
    /// somebody else's thinking into the transcript it is showing.
    pub session: i64,
    pub model: String,
    turn_id: u64,
    run: Option<Run>,
    /// An assistant message that ended with `stopReason: "error"`, held rather than
    /// shown. pi retries a failed call on its own — up to three attempts, waiting
    /// 2s/4s/8s — and the capture shows four errored messages for one dead endpoint, so
    /// reporting each one would stack four error cards over one half-answer. It is
    /// flushed when the run closes, or replaced by pi's own `auto_retry_end`, which knows
    /// how many attempts it took.
    pending_error: Option<String>,
    retry_attempts: u64,
    /// toolCallId -> the argument JSON streamed so far, for the parse-error report.
    /// Also how we catch a *silent* repair: pi's parser closes truncated JSON without
    /// saying so, and a repaired call looks identical to a clean one on screen.
    /// Accumulated from pi's own `toolcall_delta` fragments — which carry no id, only a
    /// `contentIndex`, so the id comes from the `toolcall_start` that opened the block and
    /// the fragments land in the call that is currently streaming.
    stream_cur: Option<String>,
    args_stream: HashMap<String, String>,
    /// Record types we do not know, counted. A pi bump that adds records shows up
    /// here rather than quietly dropping features on the floor.
    pub unknown: BTreeMap<String, usize>,
    /// What this run sent, so the stored row can carry the client's own name and
    /// size for a picture. pi echoes the bytes back but knows nothing about
    /// "probe.png"; taking only the echo would lose what the transcript shows.
    sent_att: Vec<Attachment>,
    /// Tool calls in the run that is currently open. pi runs the loop, so this is the
    /// only place a long-uncapped-turn note can come from on this core.
    calls_this_run: usize,
    /// A hard stop from the config (`max_tool_iters`). 0 means none, which is the
    /// default, and is exactly why the note exists.
    pub iter_cap: usize,
    /// The note is said once per run, not once per call.
    soft_note_sent: bool,
}

impl Translator {
    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// What the host is about to send. Set before the prompt, consumed by the
    /// echoed user row. A failed send has to clear it, or the next turn's row
    /// would claim somebody else's picture.
    pub fn expect_attachments(&mut self, atts: Vec<Attachment>) {
        self.sent_att = atts;
    }

    pub fn clear_attachments(&mut self) {
        self.sent_att.clear();
    }

    /// A dialog closed — answered, denied, expired, or left unanswered until the
    /// engine's own timeout fired. Everything after this is generation again.
    pub fn dialog_resolved(&mut self, now: Instant) {
        if let Some(r) = self.run.as_mut() {
            if let Some(since) = r.parked_since.take() {
                r.parked_ms += now.duration_since(since).as_millis() as u64;
            }
        }
    }

    /// Park the run on a human. Overlapping dialogs keep the earliest start, so
    /// nested cards are not double counted.
    fn park(&mut self, now: Instant) {
        if let Some(r) = self.run.as_mut() {
            if r.parked_since.is_none() {
                r.parked_since = Some(now);
            }
        }
    }

    pub fn on_record(&mut self, rec: &Value, now: Instant) -> Output {
        let kind = rec.get("type").and_then(|t| t.as_str()).unwrap_or("?").to_string();
        let mut out = Output::default();
        match kind.as_str() {
            "agent_start" => self.start_run(&mut out, now),
            // pi's turn boundaries: deliberately silent. See the module comment.
            "turn_start" | "turn_end" => {}
            // A response to a command we sent is normally taken by `PiChild`;
            // one that reaches here is an unmatched id, which is worth seeing in
            // a trace but is not transcript material.
            "response" | "message_start" => {}
            "message_end" => self.on_message_end(rec, &mut out),
            "message_update" => self.on_message_update(rec, now, &mut out),
            "tool_execution_start" => {
                if let Some(id) = s(rec, "toolCallId") {
                    out.events.push(EngineEvent::ToolExecStart { id });
                }
            }
            "tool_execution_update" => {
                if let (Some(id), Some(chunk)) = (s(rec, "toolCallId"), progress_text(rec)) {
                    out.events.push(EngineEvent::ToolExecOutput { id, stream: ExecStream::Stdout, chunk });
                }
            }
            "tool_execution_end" => {
                let (ev, (call, files)) = tool_end(self.turn_id, rec);
                out.events.extend(ev);
                if !files.is_empty() {
                    out.files_by_call.push((call, files));
                }
                self.count_call(&mut out);
            }
            "agent_end" => {}
            "agent_settled" => self.settle(rec, now, &mut out),
            "extension_error" => {
                let m = s(rec, "error")
                    .or_else(|| s(rec, "message"))
                    .unwrap_or_else(|| "an extension reported an error".into());
                out.events.push(EngineEvent::Error { message: format!("extension: {m}"), fatal: false });
            }
            "auto_retry_start" => {
                // pi is doing this behind the prompt: the answer stopped, and the whole
                // thing is being sent again after a wait the user would otherwise
                // experience as the app sitting there. Say what is happening and how long
                // the wait is, and hold the error itself back until the retries are done.
                let attempt = rec.get("attempt").and_then(|v| v.as_u64()).unwrap_or(1);
                let max = rec.get("maxAttempts").and_then(|v| v.as_u64()).unwrap_or(attempt);
                let delay = rec.get("delayMs").and_then(|v| v.as_u64()).unwrap_or(0);
                let err = rec
                    .get("errorMessage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("the provider failed");
                self.pending_error = None;
                self.retry_attempts = attempt;
                out.note = Some(format!(
                    "{}; pi is sending the prompt again (attempt {} of {}, waiting {}s)",
                    stream_death(err),
                    attempt + 1,
                    max,
                    (delay as f64 / 1000.0).max(0.1).round()
                ));
            }
            "auto_retry_end" => {
                let ok = rec.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                let attempts = rec.get("attempt").and_then(|v| v.as_u64()).unwrap_or(self.retry_attempts);
                if ok {
                    out.note = Some(format!("the retry worked after {attempts} attempt(s)"));
                    self.pending_error = None;
                } else {
                    // Keep pi's final error, and how many tries it cost, so the card that
                    // follows can say both.
                    let last = rec
                        .get("finalError")
                        .and_then(|v| v.as_str())
                        .unwrap_or("the provider failed");
                    self.pending_error = Some(format!("{last} (after {attempts} attempts)"));
                }
            }
            "compaction_start" => {
                out.compaction_start = true;
                out.note = Some("compacting the conversation to stay inside the window".into());
            }
            "compaction_end" => {}
            // Fire-and-forget notifications: worth surfacing, never worth a card.
            "extension_ui_request" => match ui_request(rec) {
                Some(u) => {
                    self.park(now);
                    out.ui.push(u);
                }
                None => {}
            },
            "queue_update" | "ping" | "pong" | "mcp_update" => {}
            _ => {
                *self.unknown.entry(kind).or_insert(0) += 1;
            }
        }
        if let Some(r) = self.run.as_ref() {
            out.context_tokens = r.context_tokens;
        }
        out
    }

    /// Close an open run because the turn will never finish — the process died,
    /// or the prompt was refused before the model saw it. The rows gathered so far
    /// still land in the transcript: half an answer is a fact about the run, and
    /// a front end that never sees `TurnEnd` leaves the composer dimmed forever.
    pub fn abort_run(&mut self, message: &str) -> Output {
        let mut out = Output::default();
        let Some(r) = self.run.take() else { return out };
        let turn_id = self.turn_id;
        // Rows already streamed are stored as they arrive; only the text that
        // never got a `message_end` is still ours to keep.
        if !r.open_text.is_empty() || !r.open_reasoning.is_empty() {
            out.rows.push(Message {
                role: Role::Assistant,
                content: r.open_text.clone(),
                reasoning: (!r.open_reasoning.is_empty()).then(|| r.open_reasoning.clone()),
                ..Default::default()
            });
        }
        out.events.push(EngineEvent::Error { message: message.into(), fatal: true });
        out.events.push(EngineEvent::TurnEnd { turn_id, finish: FinishReason::Error });
        out.context_tokens = r.context_tokens;
        out.settled = true;
        out
    }

    fn start_run(&mut self, out: &mut Output, now: Instant) {
        self.calls_this_run = 0;
        self.soft_note_sent = false;
        self.pending_error = None;
        self.retry_attempts = 0;
        if self.run.is_some() {
            // A steer or follow-up can restart the agent inside one run; the turn
            // is already open and stays open.
            return;
        }
        self.turn_id += 1;
        self.run = Some(Run::new(now));
        out.events.push(EngineEvent::TurnStart {
            turn_id: self.turn_id,
            model: self.model.clone(),
            session_id: self.session,
        });
    }

    /// A long uncapped turn has to say so, once. The number and the sentence come from
    /// the engine, because both cores run the same loop and a claim about the run cannot
    /// be made by one and withheld by the other.
    /// A translator that knows the run's bound: `max_tool_iters` from the config, 0 for
    /// none. The count is the bridge's; the sentence and the number at which it is said
    /// are the engine's, so a turn reads the same whichever core ran it.
    pub fn with_iter_cap(cap: usize) -> Self {
        let mut t = Self::default();
        t.iter_cap = cap;
        t
    }

    fn count_call(&mut self, out: &mut Output) {
        self.calls_this_run += 1;
        if self.iter_cap == 0
            && self.calls_this_run == cwoffee_engine::agent::SOFT_ITERATION_NOTE
            && !self.soft_note_sent
        {
            self.soft_note_sent = true;
            out.events.push(EngineEvent::Note {
                message: cwoffee_engine::agent::uncapped_note(self.calls_this_run),
                session: None,
            });
        }
    }

    fn on_message_end(&mut self, rec: &Value, out: &mut Output) {
        let msg = match rec.get("message") {
            Some(m) if m.is_object() => m,
            _ => return,
        };
        let role = block_role(Some(msg)).unwrap_or_default().to_string();
        // pi's own system prompt and its section breakdown are not transcript
        // content: they are what the model was told. Rendering them would put
        // cwoffee's prompt-injection scaffolding on screen.
        if role == "system" {
            return;
        }
        let text = joined(msg, "text");
        let thinking = joined(msg, "thinking");
        match role.as_str() {
            "user" => {
                let mut m = Message::user(text);
                // pi echoes the picture as its own image block. Reading the mime and
                // bytes back off that block is what puts the image in the stored row,
                // so reopening the conversation shows the same picture on either core.
                // A block whose shape is not recognised is skipped rather than guessed
                // at - an attachment row with the wrong mime is a lie about the file.
                let echoed = msg
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("image"))
                            .count()
                    })
                    .unwrap_or(0);
                if !self.sent_att.is_empty() {
                    // We sent N, pi echoed M. The row keeps what was sent - name and
                    // byte count are ours, and the Rust core stores those - but a
                    // shortfall is reported rather than smoothed over, because an
                    // attachment that never reached the model is the one failure mode
                    // a user cannot see for themselves.
                    if echoed != self.sent_att.len() {
                        out.events.push(EngineEvent::Note {
                            message: format!(
                                "the picture was sent but pi echoed {echoed} of {} image block(s) back; the transcript keeps what was sent",
                                self.sent_att.len()
                            ),
                            session: Some(self.session),
                        });
                    }
                    m.attachments = std::mem::take(&mut self.sent_att);
                } else if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("image") {
                            continue;
                        }
                        let mime = b.get("mimeType").and_then(|v| v.as_str()).unwrap_or_default();
                        let data = b.get("data").and_then(|v| v.as_str()).unwrap_or_default();
                        if mime.is_empty() || data.is_empty() {
                            continue;
                        }
                        m.attachments.push(cwoffee_engine::provider::Attachment {
                            mime: mime.to_string(),
                            data_b64: data.to_string(),
                            name: b.get("name").and_then(|v| v.as_str()).map(str::to_string),
                        });
                    }
                }
                out.rows.push(m);
            }
            "assistant" => {
                let mut calls = Vec::new();
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
                            let id = b.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            // cwoffee keeps the model's raw JSON; pi hands over a
                            // parsed object, so re-serialising is the honest way
                            // to keep the stored shape identical to the Rust core.
                            let arguments = match b.get("arguments") {
                                Some(Value::String(raw)) => raw.clone(),
                                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                                None => String::new(),
                            };
                            calls.push(ToolCall { id, name, arguments });
                        }
                    }
                }
                if let Some(r) = self.run.as_mut() {
                    if !thinking.is_empty() {
                        if !r.open_reasoning.is_empty() {
                            r.open_reasoning.push('\n');
                        }
                        r.open_reasoning.push_str(&thinking);
                    }
                    if !text.is_empty() {
                        if !r.open_text.is_empty() {
                            r.open_text.push('\n');
                        }
                        r.open_text.push_str(&text);
                    }
                    if let Some(u) = msg.get("usage") {
                        add_usage(r, u);
                    }
                    if let Some(stop) = msg.get("stopReason").and_then(|v| v.as_str()) {
                        r.finish = finish_reason(stop);
                        if stop == "error" {
                            // The half-answer is real and stays on screen; what must not
                            // happen is the silence. pi reports the cause in errorMessage —
                            // `terminated` for an endpoint that closed the socket mid-stream
                            // — and the sentence about it is ours, in terms of what the
                            // user can see: an answer that stopped before it was done.
                            self.pending_error = Some(
                                msg.get("errorMessage")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("the model call failed")
                                    .to_string(),
                            );
                        }
                    }
                    // Stored now, so a later crash cannot store it twice.
                    r.open_text.clear();
                    r.open_reasoning.clear();
                }
                let m = Message {
                    role: Role::Assistant,
                    content: text.clone(),
                    reasoning: (!thinking.is_empty()).then_some(thinking.clone()),
                    tool_calls: calls,
                    ..Default::default()
                };
                if let Some(r) = self.run.as_mut() {
                    r.context_tokens = msg.get("usage").and_then(used_tokens);
                }
                out.rows.push(m);
            }
            "toolResult" => {
                let id = msg
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if !text.is_empty() {
                    // The row keeps the envelope the Rust tools emit, byte for byte, so a
                    // reopened transcript shows the same card as the live one. `changes`
                    // ride along: the store persists them as the card's `+N -M`, and a
                    // row without them is a file edit the user cannot see the size of.
                    let changes = envelope(&text)
                        .and_then(|v| {
                            v.get("changes")
                                .and_then(|c| serde_json::from_value::<Vec<cwoffee_engine::tools::changes::FileChange>>(c.clone()).ok())
                        })
                        .unwrap_or_default();
                    let m = Message {
                        role: Role::Tool,
                        content: text,
                        tool_call_id: id,
                        changes,
                        ..Default::default()
                    };
                    out.rows.push(m);
                }
            }
            _ => {}
        }
    }

    fn on_message_update(&mut self, rec: &Value, now: Instant, out: &mut Output) {
        // Usage rides every streamed record, cumulative for the message being
        // streamed; it is committed once, at `message_end`.
        let ame = match rec.get("assistantMessageEvent") {
            Some(a) if a.is_object() => a,
            _ => return,
        };
        let kind = ame.get("type").and_then(|t| t.as_str()).unwrap_or("?").to_string();
        let r = match self.run.as_mut() {
            Some(r) => r,
            None => return,
        };
        match kind.as_str() {
            "text_delta" => {
                let d = ame.get("delta").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if d.is_empty() {
                    return;
                }
                r.open_text.push_str(&d);
                r.chars += d.chars().count() as u64;
                r.thinking = false;
                mark_first(r, now);
                out.events.push(EngineEvent::Text(d));
                if let Some(ev) = stats(r, now) {
                    out.events.push(ev);
                }
            }
            "thinking_delta" => {
                let d = ame.get("delta").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if d.is_empty() {
                    return;
                }
                r.open_reasoning.push_str(&d);
                r.chars += d.chars().count() as u64;
                r.thinking = true;
                mark_first(r, now);
                out.events.push(EngineEvent::Reasoning(d));
                if let Some(ev) = stats(r, now) {
                    out.events.push(ev);
                }
            }
            // `text_end`/`thinking_end` repeat the whole block. The deltas have
            // already been streamed; re-emitting would double the answer on screen.
            "text_start" | "text_end" | "thinking_start" | "thinking_end" => {}
            "toolcall_start" => {
                let id = ame.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let name = ame.get("toolName").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                self.stream_cur = Some(id.clone());
                self.args_stream.entry(id.clone()).or_default();
                out.events.push(EngineEvent::ToolCallBegin { id, name });
            }
            "toolcall_delta" => {
                let d = ame.get("delta").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if let Some(cur) = self.stream_cur.clone() {
                    self.args_stream.entry(cur).or_default().push_str(&d);
                }
                out.events.push(EngineEvent::ToolCallArgs(d));
            }
            "toolcall_end" => {
                let tc = ame.get("toolCall").cloned().unwrap_or_default();
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("a tool").to_string();
                // Was the call repaired on the way in? pi's parser accepts truncated JSON
                // and hands over a finished object without a word; cwoffee repairs the same
                // way but says so, because a call someone else completed for the model is
                // not the call the model wrote. Compare the fragments off the wire with
                // what pi ended up running.
                let streamed = self.args_stream.remove(&id).unwrap_or_default();
                if self.stream_cur.as_deref() == Some(id.as_str()) {
                    self.stream_cur = None;
                }
                let handed = matches!(
                    tc.get("arguments"),
                    Some(Value::Object(_)) | Some(Value::Null) | None
                );
                if handed && !streamed.trim().is_empty() && serde_json::from_str::<Value>(streamed.trim()).is_err() {
                    out.events.push(EngineEvent::Error {
                        message: cwoffee_engine::agent::repaired_args_note(
                            &name,
                            &format!(
                                "the arguments as streamed did not parse as JSON ({} bytes); pi's parser                                  closed them and ran the call anyway — read the card before trusting what                                  it ran",
                                streamed.trim().chars().count()
                            ),
                        ),
                        fatal: false,
                    });
                }
                match tc.get("arguments") {
                    Some(Value::Object(_)) | Some(Value::Null) | None => {
                        let args = tc.get("arguments").cloned().unwrap_or_else(|| Value::Object(Default::default()));
                        out.events.push(EngineEvent::ToolCallReady { id, args });
                    }
                    Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                        Ok(v) => out.events.push(EngineEvent::ToolCallReady { id, args: v }),
                        Err(e) => out.events.push(EngineEvent::ToolCallParseError {
                            id: id.clone(),
                            raw: raw.clone(),
                            message: e.to_string(),
                        }),
                    },
                    Some(other) => out.events.push(EngineEvent::ToolCallParseError {
                        id,
                        raw: other.to_string(),
                        message: "arguments were not a JSON object".into(),
                    }),
                }
            }
            _ => {
                *self.unknown.entry(format!("assistantMessageEvent.{kind}")).or_insert(0) += 1;
            }
        }
    }

    fn settle(&mut self, rec: &Value, now: Instant, out: &mut Output) {
        let turn_id = self.turn_id;
        let r = match self.run.take() {
            Some(r) => r,
            None => return,
        };
        if rec.get("aborted").and_then(|v| v.as_bool()) == Some(true) {
            out.events.push(EngineEvent::Error { message: "turn cancelled".into(), fatal: false });
        }
        // A run that ended with the model call in error must say so inside the same turn:
        // the transcript holds half an answer either way, and without this the user's only
        // clue is that the sentence stops. `finish` already says Error; this says why.
        if let Some(err) = self.pending_error.take() {
            out.events.push(EngineEvent::Error {
                message: stream_death(&err),
                fatal: false,
            });
        }
        let streamed_ms = r
            .first_delta
            .map(|f| {
                let total = now.duration_since(f).as_millis() as u64;
                total.saturating_sub(r.parked_total(now))
            })
            .unwrap_or(0);
        out.events.push(EngineEvent::Usage(TurnTelemetry {
            prompt_tokens: r.prompt_tokens,
            cached_tokens: r.cached_tokens,
            completion_tokens: r.completion_tokens,
    ttft_ms: r
                .first_delta
                .map(|f| f.duration_since(r.started).as_millis() as u64)
                .unwrap_or(0),
            // pi exposes no server timings, and no speculative-decode counters.
            // Zero means "not reported", which is what it looks like in the UI.
            server_prefill_tps: 0.0,
            server_decode_tps: if streamed_ms > 0 {
                r.completion_tokens as f64 / (streamed_ms as f64 / 1000.0)
            } else {
                0.0
            },
            draft_n: 0,
            draft_accepted: 0,
        }));
        out.events.push(EngineEvent::TurnEnd { turn_id, finish: r.finish });
        out.context_tokens = r.context_tokens;
        out.settled = true;
    }
}

/// Lift what our own Rust tools report out of a pi tool result. The envelope is
/// ours (`{"ok":true,"tool":"read_file","engine":"rust",...}`), which is how the
/// `+N -M` badges and the attachment chips survive the core swap: pi reports that
/// a tool returned, cwoffee reports *what it changed*.
/// Turn one pi `tool_execution_end` into what cwoffee says about a tool finishing.
///
/// Three things travel, and each one is a user-visible claim:
///   * the tool's own output text, as a `toolExecOutput` chunk, so the card shows what
///     the tool said. It arrives when the call finished rather than during it — pi gives
///     us the result once, at the end — and the bridge adds nothing to it. Without this
///     the live card is an empty box, while the same session reloaded from the store
///     shows the text: the transcript would disagree with itself.
///   * the file changes, for the card's `+N -M`.
///   * the files the tool produced in the workspace, which the host stamps onto the row.
fn tool_end(turn_id: u64, rec: &Value) -> (Vec<EngineEvent>, (String, Vec<cwoffee_engine::tools::OutputFile>)) {
    let _ = &turn_id;
    let mut ev = Vec::new();
    let id = rec.get("toolCallId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let is_err = rec.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut changes: Vec<FileChange> = Vec::new();
    let mut files: Vec<cwoffee_engine::tools::OutputFile> = Vec::new();
    if let Some(txt) = result_text(rec) {
        if let Some(v) = envelope(&txt) {
            if let Some(c) = v.get("changes").and_then(|c| serde_json::from_value::<Vec<FileChange>>(c.clone()).ok()) {
                changes = c;
            }
            if let Some(f) = v.get("files").and_then(|f| serde_json::from_value::<Vec<cwoffee_engine::tools::OutputFile>>(f.clone()).ok()) {
                files = f;
            }
        }
    }
    // The tool's own words, in its own shape. `output` is what the Rust host streams;
    // `error` is what it says when the tool refused. Anything else in the envelope is
    // metadata the card derives for itself.
    if let Some(txt) = result_text(rec) {
        let shown = envelope(&txt)
            .and_then(|v| {
                v.get("output")
                    .and_then(|o| o.as_str())
                    .map(str::to_string)
                    .or_else(|| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            })
            .unwrap_or_else(|| txt.clone());
        if !shown.trim().is_empty() {
            ev.push(EngineEvent::ToolExecOutput {
                id: id.clone(),
                stream: cwoffee_engine::event::ExecStream::Stdout,
                chunk: shown,
            });
        }
    }
    ev.push(EngineEvent::ToolExecEnd { id: id.clone(), ok: !is_err, exit_code: None, changes });
    if !files.is_empty() {
        ev.push(EngineEvent::TurnFiles { turn_id, files: files.clone() });
    }
    (ev, (id, files))
}

/// Our tools answer with a JSON envelope; pi hands it over as text. Parse it once,
/// tolerate anything that is not ours (a pi-side "tool not found", a shell's own text).
fn envelope(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text).ok()
}

fn result_text(rec: &Value) -> Option<String> {
    let parts = rec.get("result")?.get("content")?.as_array()?;
    let mut s = String::new();
    for p in parts {
        if p.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(t);
            }
        }
    }
    (!s.is_empty()).then_some(s)
}

fn progress_text(rec: &Value) -> Option<String> {
    let p = rec.get("partialResult")?;
    if let Some(t) = p.as_str() {
        return (!t.is_empty()).then(|| t.to_string());
    }
    result_text(rec).or_else(|| {
        p.get("text").and_then(|t| t.as_str()).map(str::to_string)
    })
}

fn ui_request(rec: &Value) -> Option<UiRequest> {
    let method = rec.get("method").and_then(|m| m.as_str())?.to_string();
    let pi_id = rec.get("id").and_then(|i| i.as_str())?.to_string();
    let title = s(rec, "title").unwrap_or_default();
    let timeout_ms = rec.get("timeout").and_then(|t| t.as_u64()).unwrap_or(0);
    match method.as_str() {
        "confirm" => Some(UiRequest::Confirm {
            pi_id,
            title,
            message: s(rec, "message").unwrap_or_default(),
            timeout_ms,
        }),
        "select" => {
            let options = rec
                .get("options")
                .and_then(|o| o.as_array())
                .map(|a| {
                    a.iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .or_else(|| v.get("label").and_then(|l| l.as_str()).map(str::to_string))
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UiRequest::Select { pi_id, title, options, timeout_ms })
        }
        "input" => Some(UiRequest::Input {
            pi_id,
            title,
            placeholder: s(rec, "placeholder").unwrap_or_default(),
            timeout_ms,
        }),
        // `notify` and friends are notifications, not dialogs: no answer expected.
        "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text" => None,
        _ => Some(UiRequest::Unsupported { pi_id, method }),
    }
}

fn stats(r: &mut Run, now: Instant) -> Option<EngineEvent> {
    let first = r.first_delta?;
    let due = r
        .last_stats
        .map(|t| now.duration_since(t) >= Duration::from_millis(250))
        .unwrap_or(true);
    if !due {
        return None;
    }
    r.last_stats = Some(now);
    let streaming = now.duration_since(first).as_millis() as u64;
    let streaming = streaming.saturating_sub(r.parked_total(now));
    let mut s = TurnStats {
        ttft_ms: first.duration_since(r.started).as_millis() as u64,
        streaming_ms: 0,
        chars: r.chars,
        est_tokens: 0,
        est_decode_tps: 0.0,
        thinking: r.thinking,
    };
    s.derive(streaming);
    Some(EngineEvent::TurnStats(s))
}

fn mark_first(r: &mut Run, now: Instant) {
    if r.first_delta.is_none() {
        r.first_delta = Some(now);
    }
}

/// `usage.input` excludes cached tokens, so the window holds input + read + write.
fn add_usage(r: &mut Run, u: &Value) -> Option<()> {
    let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let (input, read, write, out) = (g("input"), g("cacheRead"), g("cacheWrite"), g("output"));
    r.prompt_tokens += input + read + write;
    r.cached_tokens += read;
    r.completion_tokens += out;
    Some(())
}

fn used_tokens(u: &Value) -> Option<usize> {
    let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Some((g("input") + g("cacheRead") + g("cacheWrite")) as usize)
}

/// Put pi's provider error into words a person can act on. The distinction that matters
/// is between an endpoint that stopped talking in the middle of an answer — where the
/// transcript on screen is genuinely incomplete and retrying may help — and a call that
/// was refused before it produced anything, where "the stream dropped" would be a wrong
/// story. pi's own text is kept in the sentence either way.
fn stream_death(raw: &str) -> String {
    let low = raw.to_ascii_lowercase();
    const TRANSPORT: &[&str] = &[
        "terminated",
        "econn",
        "socket hang up",
        "other side closed",
        "fetch failed",
        "network",
        "etimedout",
        "eof",
        "aborted",
    ];
    if TRANSPORT.iter().any(|k| low.contains(k)) {
        format!("the endpoint closed the connection in the middle of the answer: {raw}")
    } else {
        format!("the model call failed: {raw}")
    }
}

fn finish_reason(stop: &str) -> FinishReason {
    match stop {
        "stop" | "endTurn" | "stopSequence" => FinishReason::Stop,
        "toolUse" | "toolCall" | "tool_use" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "aborted" | "cancelled" => FinishReason::Cancelled,
        _ => FinishReason::Error,
    }
}

fn block_role(msg: Option<&Value>) -> Option<&str> {
    msg?.get("role").and_then(|r| r.as_str())
}

/// Concatenate the text (or thinking) blocks of a pi message.
fn joined(msg: &Value, block: &str) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let is_match = match (block, kind) {
                    ("text", "text") => true,
                    ("thinking", "thinking") => true,
                    _ => false,
                };
                if is_match {
                    if let Some(t) = b.get("text").or_else(|| b.get("thinking")).and_then(|v| v.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn s(rec: &Value, key: &str) -> Option<String> {
    rec.get(key).and_then(|v| v.as_str()).map(str::to_string)
}
