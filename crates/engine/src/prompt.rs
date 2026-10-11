//! Append-only prompt construction.
//!
//! This is the module that decides whether Cwoffee feels fast or feels broken on a
//! local model. Prefill on this box runs at 30–70 tok/s *uncached*; a 4k-token
//! prompt is a minute of staring at nothing. llama.cpp's prefix cache is the only
//! reason that is tolerable, and it only hits when the prompt's **leading bytes
//! are unchanged**. Live measurement in M1a: 92% of a 347-token prompt came from
//! cache, and effective prefill went 38/s → 452/s.
//!
//! Three rules follow from that, and they are not stylistic:
//!
//! 1. **Nothing volatile goes in the system prompt.** A timestamp, a git status,
//!    a cwd — any of them changes the leading bytes and the whole prefix misses
//!    on every single turn. Volatile environment context is appended to the
//!    *newest user message*, which is new bytes anyway, so it costs nothing.
//! 2. **History is append-only.** We never rewrite a message that has already
//!    been sent. Editing one byte anywhere in the middle invalidates everything
//!    after it.
//! 3. **Compaction is explicit, rare, and logged.** Truncating history is the one
//!    operation that deliberately throws the cache away. It must be a decision,
//!    not something that happens incidentally mid-turn.

use serde::{Deserialize, Serialize};

use crate::provider::{ChatRequest, Message, Role, ToolCall};

/// A prompt under construction.
#[derive(Debug, Clone, Default)]
pub struct PromptBuilder {
    /// Byte-stable across the whole session. Bump `SYSTEM_VERSION` on change so
    /// the cost is at least attributable.
    system: String,
    /// Thinking level the prompt argues for. Part of the system bytes.
    reasoning: crate::provider::Reasoning,
    history: Vec<Message>,
    /// Environment block appended to the newest user message only.
    env_block: String,
    /// Approximate token budget for the whole prompt.
    pub token_budget: usize,
    /// The context window that budget was sized against, when the model stated
    /// one. The compaction message quotes it: "context full" means nothing until
    /// the user can see the number the model can actually hold.
    #[allow(clippy::struct_field_names)]
    pub context_window: Option<usize>,
    /// Incremented every time we drop history. Surfaced in the UI: a compaction
    /// is a visible latency spike, and users notice the spike.
    pub compactions: u32,
    /// Sequence number of `history[0]`.
    ///
    /// Compaction removes from the front, so index-based cursors break. A
    /// monotonic sequence lets a consumer (the session host) ask "which
    /// messages have I not persisted yet?" without caring that the window
    /// shifted. The database keeps everything; the prompt is a window over it.
    first_seq: u64,
}

/// Bump when the system prompt text changes, so cache-miss regressions can be
/// attributed to a specific change rather than appearing as mysterious slowness.
pub const SYSTEM_VERSION: &str = "cwoffee-v1";

/// What the prompt costs, as the thing that builds it measured it.
///
/// Shared (behind a mutex) between the agent and the host so a client reading
/// host state *mid-turn* sees the number the next prefill will pay. Before this,
/// `context_tokens` was refreshed only at the end of a turn, so a session that
/// had just been compacted from 53k to 4k kept reporting 53k in the toolbar for
/// the whole turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextGauge {
    pub tokens: usize,
    pub budget: usize,
    pub window: Option<usize>,
    pub compactions: u32,
}

/// The estimate's own constants, named because `compact_to_budget` works in
/// characters and must convert the token budget with the *same* numbers, not a
/// copy of them that can drift.
const CHARS_PER_TOKEN: f64 = 3.5;
const FRAMING_TOKENS: usize = 64;
const MSG_FRAMING_CHARS: usize = 12;
const CALL_FRAMING_CHARS: usize = 16;
/// Tool results newer than this stay whole during compaction.
pub const KEEP_TAIL: usize = 12;
/// Shorter than this, and replacing a tool result with a note about it saves
/// nothing.
const STUB_MIN_CHARS: usize = 400;

fn budget_chars(token_budget: usize) -> usize {
    (token_budget.saturating_sub(FRAMING_TOKENS) as f64 * CHARS_PER_TOKEN) as usize
}

/// call id -> tool name, so a trimmed tool result can say which tool it came
/// from. A stub that says "tool output trimmed" tells the model nothing it can
/// act on; one that says `sheet_read output trimmed` tells it exactly what to
/// re-run.
fn tool_names(history: &[Message]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for m in history {
        for c in &m.tool_calls {
            map.insert(c.id.clone(), c.name.clone());
        }
    }
    map
}

/// What one compaction did, in the two units the note has to report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Compaction {
    /// Tool results replaced by a one-line note about their size.
    pub stubbed: usize,
    /// Whole oldest turns removed.
    pub dropped: usize,
    /// Rough tokens this bought back.
    pub reclaimed: usize,
}

impl Compaction {
    pub fn did_anything(&self) -> bool {
        self.stubbed > 0 || self.dropped > 0
    }
}

/// The stable system prompt. Deliberately contains no clock, no path, no git
/// state — see the module comment.
pub fn system_prompt() -> String {
    system_prompt_for(crate::provider::Reasoning::default())
}

/// The system prompt for a given thinking level.
///
/// The level is part of the prompt bytes, not just a request parameter, and
/// that is a deliberate cost: changing effort invalidates the llama.cpp prefix
/// cache for the session. It is a cost we were already paying, because this
/// server's chat template injects its own effort-dependent instruction into the
/// system message, so the prefix changes on any level switch whether we write
/// anything or not. docs/MODELS.md carries the measurements.
pub fn system_prompt_for(reasoning: crate::provider::Reasoning) -> String {
    match deliberation_line(reasoning) {
        Some(line) => format!("{SYSTEM_VERSION}\n{}\n{}", system_body(), line),
        None => format!("{SYSTEM_VERSION}\n{}", system_body()),
    }
}

/// What the top thinking levels actually ask for.
///
/// Measured on qwen3.8-flash-next with this prompt: reasoning came out at 603
/// characters without this line and 1426 with it, on an identical question. The
/// template's own "please think carefully through the task" wording produced no
/// measurable change, so if the user asked for xhigh we say it in words this
/// model responds to. Low and medium are left alone: the template already
/// covers those, and adding a "think briefly" line would argue with the user.
pub fn deliberation_line(reasoning: crate::provider::Reasoning) -> Option<&'static str> {
    use crate::provider::Reasoning as R;
    match reasoning {
        R::High | R::XHigh => Some(
            "Deliberation: reason at length. Use as much thinking as the problem needs; do not stop early.",
        ),
        _ => None,
    }
}

/// The instructions, kept separate from the version tag so the "no volatile
/// data" test can assert on the body without tripping on the version digit.
fn system_body() -> String {
    r#"You are Cwoffee, a capable local assistant that works on real files and runs real commands.

Working style:
- Be concise. Prefer doing over explaining.
- Use tools when they are faster than guessing. Never claim to have run something you did not run.
- After tool output, report the result; do not repeat the whole output back.
- If a command fails, read the error, adjust, and retry once before asking.

Tool use:
- Call tools by name with JSON arguments exactly matching the schema.
- Emit exactly one JSON object per tool call.
- If you cannot do something, say so plainly rather than inventing an answer.

Environment: commands run in a sandbox with no network access; only the workspace is writable."#
        .to_string()
}

impl PromptBuilder {
    pub fn new() -> Self {
        Self {
            system: system_prompt(),
            reasoning: crate::provider::Reasoning::default(),
            history: Vec::new(),
            env_block: String::new(),
            // A fallback, not a policy: the host replaces this with a budget
            // derived from the loaded model's window before the first turn.
            token_budget: 8_000,
            context_window: None,
            compactions: 0,
            first_seq: 0,
        }
    }

    /// Set the thinking level the prompt should argue for.
    ///
    /// Only call this when the level actually changes: it rewrites the system
    /// message, and a rewritten system message drops the whole prefix.
    pub fn set_reasoning(&mut self, r: crate::provider::Reasoning) {
        if self.reasoning == r {
            return;
        }
        self.reasoning = r;
        self.system = system_prompt_for(r);
    }

    /// Volatile context (date, cwd, model). Appended to the newest user message,
    /// never to the system prompt.
    pub fn set_env(&mut self, block: impl Into<String>) {
        self.env_block = block.into();
    }

    /// Append a user turn. `env_block` is attached here because this message is
    /// new bytes regardless, so including it cannot invalidate the prefix.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_user_with(text, Vec::new())
    }

    /// Append a user turn that carries attachments (images in v1).
    pub fn push_user_with(
        &mut self,
        text: impl Into<String>,
        attachments: Vec<crate::provider::Attachment>,
    ) {
        // The env block is NOT baked into the stored content. It is injected at
        // build() time instead, because this history is also what gets persisted
        // and rendered: baking it in put a raw <env>...</env> block in front of
        // every user message in the transcript.
        //
        // The bytes on the wire are unchanged - every user message still carries
        // the block, so older messages never change and the prefix cache is
        // unaffected.
        self.history.push(Message {
            role: Role::User,
            content: text.into(),
            attachments,
            ..Default::default()
        });
    }

    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.push_assistant_with(text, None, None);
    }

    /// As `push_assistant`, but carrying the reasoning text and how long it
    /// took. Reasoning is not replayed to the model unless the provider asks
    /// for it (`Provider::wants_reasoning`); it is kept so the transcript can
    /// be rebuilt after a reload.
    pub fn push_assistant_with(
        &mut self,
        text: impl Into<String>,
        reasoning: Option<String>,
        reasoning_ms: Option<u64>,
    ) {
        self.history.push(Message {
            role: Role::Assistant,
            content: text.into(),
            reasoning,
            reasoning_ms,
            ..Default::default()
        });
    }

    /// An assistant turn that requested tool calls. The calls must be replayed
    /// verbatim: providers match tool_result back to these ids.
    pub fn push_assistant_calls(&mut self, text: &str, calls: &[ToolCall]) {
        self.push_assistant_calls_with(text, calls, None, None);
    }

    pub fn push_assistant_calls_with(
        &mut self,
        text: &str,
        calls: &[ToolCall],
        reasoning: Option<String>,
        reasoning_ms: Option<u64>,
    ) {
        self.history.push(Message {
            role: Role::Assistant,
            content: text.to_string(),
            reasoning,
            reasoning_ms,
            tool_calls: calls.to_vec(),
            ..Default::default()
        });
    }

    pub fn push_tool_result(&mut self, call_id: &str, output: impl Into<String>) {
        self.push_tool_result_with(call_id, output, Vec::new())
    }

    /// `changes` is UI-only (see `Message::changes`): it rides along to the store
    /// and never enters the bytes sent to the model.
    pub fn push_tool_result_with(
        &mut self,
        call_id: &str,
        output: impl Into<String>,
        changes: Vec<crate::tools::changes::FileChange>,
    ) {
        self.history.push(Message {
            role: Role::Tool,
            content: output.into(),
            tool_call_id: Some(call_id.to_string()),
            changes,
            ..Default::default()
        });
    }

    /// Number of messages currently held.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Build the request. Does not mutate.
    pub fn build(&self, model: &str) -> ChatRequest {
        let mut msgs = Vec::with_capacity(self.history.len() + 1);
        msgs.push(Message::system(&self.system));
        for m in self.history.iter() {
            if m.role == Role::User && !self.env_block.is_empty() {
                // Env first, question last: models weight the tail of the prompt
                // more heavily, and the question is what must be answered.
                let mut wired = m.clone();
                wired.content = format!("{}\n\n{}", self.env_block, m.content);
                msgs.push(wired);
            } else {
                msgs.push(m.clone());
            }
        }
        ChatRequest::new(model, msgs)
    }

        /// Rough token estimate. We cannot get an exact count before prefill, and
    /// asking for one would cost a prefill — so the estimate only has to be good
    /// enough to decide *when* to compact, and it must never be so optimistic
    /// that we overflow the window. 1 token per ~3.5 chars plus per-message
    /// overhead is conservative for CJK-heavy text, which tokenizes worse.
    pub fn estimate_tokens(&self, tool_schemas: &str) -> usize {
        (self.estimate_chars(tool_schemas) as f64 / CHARS_PER_TOKEN).ceil() as usize
            + FRAMING_TOKENS
    }

    /// The same estimate in the unit the compaction loop actually works in.
    ///
    /// Compaction re-measures after every edit; re-scanning every message each
    /// time is O(n²) over a 600-message session. Tracking the character total
    /// and subtracting what each edit reclaimed is exact under this formula and
    /// keeps the loop linear.
    fn estimate_chars(&self, tool_schemas: &str) -> usize {
        let mut chars = self.system.chars().count() + tool_schemas.chars().count();
        for m in &self.history {
            chars += m.content.chars().count() + MSG_FRAMING_CHARS;
            for c in &m.tool_calls {
                chars += c.name.len() + c.arguments.len() + CALL_FRAMING_CHARS;
            }
        }
        chars
    }

    /// Trim the window until the estimate fits, in the cheapest order.
    ///
    /// This is the one cache-destroying operation, which is why it is a named
    /// call rather than something build() does silently.
    ///
    /// Two stages, cheapest first, because the two kinds of bytes are not equally
    /// valuable:
    ///
    /// 1. **Old tool output.** A 40k-cell spreadsheet dump from nine turns ago is
    ///    the fattest and least useful thing in the window. Replace it with one
    ///    line saying what was there. The call, the answer it produced and the
    ///    model's own reasoning all survive.
    /// 2. **Whole oldest turns.** Only reached if stage 1 was not enough.
    ///
    /// Doing it the other way round is how a session loses its reasoning while a
    /// stale file dump is still sitting in the prompt.
    pub fn compact_to_budget(&mut self, tool_schemas: &str) -> Compaction {
        let before = self.estimate_tokens(tool_schemas);
        let need = budget_chars(self.token_budget);
        let mut chars = self.estimate_chars(tool_schemas);
        let mut out = Compaction::default();
        if chars <= need {
            return out;
        }

        // Stage 1. The newest KEEP_TAIL messages stay whole: those are the ones
        // the model is reading right now, and a stub there would be a stub the
        // model actually needs.
        let names = tool_names(&self.history);
        let floor = self.history.len().saturating_sub(KEEP_TAIL);
        for i in 0..floor {
            if chars <= need {
                break;
            }
            let m = &mut self.history[i];
            if m.role != Role::Tool {
                continue;
            }
            let full = m.content.chars().count();
            // Below this there is nothing to win, and a stub longer than what it
            // replaces would make the window bigger.
            if full <= STUB_MIN_CHARS {
                continue;
            }
            let name = m
                .tool_call_id
                .as_deref()
                .and_then(|id| names.get(id))
                .cloned()
                .unwrap_or_else(|| "tool".into());
            let stub = format!(
                "[{name} output trimmed to fit the context window: {full} chars. \
                 Still in the session file - re-run the tool if you need it again.]"
            );
            let after = stub.chars().count();
            m.content = stub;
            chars = chars.saturating_sub(full - after);
            out.stubbed += 1;
        }

        // Stage 2. Never drop the system prompt or the newest turn. Drop in
        // whole turns, and keep tool_call/tool_result pairs together: an
        // orphaned tool_result with no preceding call is a hard 400 from every
        // provider.
        while chars > need && self.history.len() > 2 {
            let remove = self.first_safe_prefix();
            if remove == 0 {
                break;
            }
            for m in &self.history[..remove] {
                chars = chars.saturating_sub(m.content.chars().count() + MSG_FRAMING_CHARS);
                for c in &m.tool_calls {
                    chars = chars.saturating_sub(c.name.len() + c.arguments.len() + CALL_FRAMING_CHARS);
                }
            }
            self.history.drain(0..remove);
            self.first_seq += remove as u64;
            out.dropped += remove;
        }

        if out.did_anything() {
            self.compactions += 1;
            out.reclaimed = before.saturating_sub(self.estimate_tokens(tool_schemas));
        }
        out
    }

    /// Index of the leading run of messages we may drop without orphaning a
    /// tool_result from a call we kept.
    fn first_safe_prefix(&self) -> usize {
        if self.history.is_empty() {
            return 0;
        }
        let mut n = 1;
        // extend past any tool results belonging to the assistant turn we drop
        while n < self.history.len() && self.history[n].role == Role::Tool {
            n += 1;
        }
        // if the leading message is itself a tool result, the shape is already
        // broken; drop it so we can recover rather than 400 forever
        if self.history[0].role == Role::Tool {
            return n;
        }
        // keep at least the last exchange
        if self.history.len() - n < 2 {
            return 0;
        }
        n
    }

    /// History for persistence (excludes the system prompt, which is stored
    /// separately by version).
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Sequence number of the oldest message still in the window.
    pub fn first_seq(&self) -> u64 {
        self.first_seq
    }

    /// Sequence number the next pushed message will get.
    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.history.len() as u64
    }

    /// Sequence of the message at `idx` within the current window.
    pub fn seq_at(&self, idx: usize) -> u64 {
        self.first_seq + idx as u64
    }

    /// Rehydrate from stored history.
    pub fn with_history(&mut self, msgs: Vec<Message>) {
        self.history = msgs;
    }

    /// Continue sequence numbering after rehydrating a stored session, so the
    /// host does not re-persist messages it already has.
    pub fn set_first_seq(&mut self, s: u64) {
        self.first_seq = s;
    }
}

/// Serializable record of one turn, for the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub id: i64,
    pub session_id: i64,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub tool_calls_json: String,
    #[serde(default)]
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built() -> PromptBuilder {
        let mut p = PromptBuilder::new();
        p.set_env("<env>\ncwd: /home/u/proj\ndate: 2026-07-01\n</env>");
        p.push_user("first question");
        p.push_assistant("first answer");
        p
    }

    /// The central invariant of the project.
    #[test]
    fn appending_never_changes_existing_bytes() {
        let mut p = built();
        let before = serde_json::to_string(&p.build("m").messages).unwrap();

        for i in 0..40 {
            p.push_assistant(format!("extra {i}"));
            p.push_user(format!("q {i}"));
            let after = serde_json::to_string(&p.build("m").messages).unwrap();
            let prefix = before.strip_suffix(']').expect("array");
            assert!(
                after.starts_with(prefix),
                "prompt prefix bytes changed at append {i}; every later turn \
                 would miss the llama.cpp prefix cache"
            );
        }
    }

    /// A clock in the system prompt is a performance bug, not a style choice.
    #[test]
    fn thinking_level_changes_the_prompt_only_when_it_should() {
        use crate::provider::Reasoning as R;
        let base = system_prompt_for(R::Off);
        assert_eq!(system_prompt_for(R::Medium), base);
        assert_eq!(system_prompt_for(R::Low), base);
        assert_ne!(system_prompt_for(R::XHigh), base);
        assert!(system_prompt_for(R::XHigh).contains("reason at length"));

        let mut p = PromptBuilder::new();
        let before = p.build("m").messages[0].content.clone();
        p.set_reasoning(R::XHigh);
        let after = p.build("m").messages[0].content.clone();
        assert_ne!(before, after);
        // Setting the same level twice must not churn the bytes.
        p.set_reasoning(R::XHigh);
        assert_eq!(after, p.build("m").messages[0].content);
    }

    #[test]
    fn system_prompt_body_contains_no_volatile_data() {
        let body = system_body();
        // no digits at all: rules out dates, times, and anything session-specific
        assert!(
            !body.contains(|c: char| c.is_ascii_digit()),
            "system prompt body contains a digit; likely a date or path that \
             would churn the prefix and miss the cache every turn"
        );
        assert!(!body.contains("/home"), "absolute path in the system prompt");
        assert_eq!(body, system_body(), "system prompt must be deterministic");
        assert_eq!(
            serde_json::to_string(&PromptBuilder::new().build("m").messages).unwrap(),
            serde_json::to_string(&PromptBuilder::new().build("m").messages).unwrap()
        );
    }

    #[test]
    fn env_reaches_the_wire_but_never_the_stored_history() {
        // The transcript and the DB read `history()`. If env leaked in there,
        // every user message would render a raw <env> block - which is exactly
        // the bug this guards.
        let mut p = PromptBuilder::new();
        p.set_env("<env>\nworkspace: /w\n</env>");
        p.push_user("first question");
        p.push_assistant("answer");
        p.push_user("second question");

        for m in p.history() {
            if m.role == Role::User {
                assert!(
                    !m.content.contains("<env>"),
                    "env leaked into stored history: {:?}",
                    m.content
                );
            }
        }

        // The wire form must still carry it, on every user message, or the
        // prefix-cache property (older messages byte-identical) is lost.
        let req = p.build("m");
        let users: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(users.len(), 2);
        for u in &users {
            assert!(u.starts_with("<env>"), "env missing on the wire: {u}");
            assert!(u.ends_with("question"), "question must come last: {u}");
        }

        // Older turns must not change bytes when a new turn is appended.
        let before = p.build("m");
        p.push_assistant("later");
        p.push_user("third");
        let after = p.build("m");
        assert_eq!(
            before.messages[1].content, after.messages[1].content,
            "an older user message changed bytes, which invalidates the prefix cache"
        );
    }

    #[test]
    fn env_block_lands_on_the_newest_user_message_not_the_system_prompt() {
        let mut p = PromptBuilder::new();
        p.set_env("<env>cwd=/x</env>");
        p.push_user("hello");
        let req = p.build("m");
        assert_eq!(req.messages[0].role, Role::System);
        assert!(!req.messages[0].content.contains("cwd=/x"));
        assert!(req.messages[1].content.contains("cwd=/x"));
        assert!(req.messages[1].content.ends_with("hello"));
    }

    #[test]
    fn tool_call_and_result_stay_paired() {
        let mut p = PromptBuilder::new();
        for _ in 0..200 {
            p.push_user("q");
            p.push_assistant_calls("thinking", &[ToolCall {
                id: "c1".into(),
                name: "exec".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }]);
            p.push_tool_result("c1", "a file");
        }
        p.token_budget = 100;
        let c = p.compact_to_budget("");
        assert!(c.did_anything(), "should have compacted");
        // no message may be a tool result whose parent call was dropped
        let h = p.history();
        for (i, m) in h.iter().enumerate() {
            if m.role == Role::Tool {
                assert!(
                    i > 0 && h[i - 1].role == Role::Assistant && !h[i - 1].tool_calls.is_empty(),
                    "orphaned tool_result at index {i}: providers reject this"
                );
            }
        }
    }

    /// The complaint: 43k of 262k used and the app said the context was full.
    /// The budget was a constant, so it was comparing the history against 8k.
    #[test]
    fn a_short_history_on_a_big_window_does_not_compact() {
        let mut p = PromptBuilder::new();
        let para = "x".repeat(5_000);
        for _ in 0..15 {
            p.push_user(&para);
            p.push_assistant(&para);
        }
        let est = p.estimate_tokens("");
        assert!(est > 40_000 && est < 46_000, "fixture is ~43k tokens, got {est}");

        p.token_budget = 186_695; // 72% of 262 144, minus headroom
        p.context_window = Some(262_144);
        assert_eq!(
            p.compact_to_budget(""),
            Compaction::default(),
            "{est} tokens was dropped against a 262 144 token window"
        );

        // The number this used to be compared against, kept so the fix cannot
        // quietly regress back into a constant.
        p.token_budget = 8_000;
        assert!(
            p.compact_to_budget("").did_anything(),
            "an 8k budget no longer compacts this, so it no longer proves the bug"
        );
    }

    #[test]
    fn compaction_is_counted_and_only_happens_when_needed() {
        let mut p = built();
        assert_eq!(p.compact_to_budget(""), Compaction::default());
        assert_eq!(p.compactions, 0);

        // A two-message history cannot be compacted further without deleting the
        // live exchange, so nothing happens and nothing is claimed.
        p.token_budget = 10;
        assert_eq!(p.compact_to_budget(""), Compaction::default());
        assert_eq!(p.compactions, 0);

        // A real history does compact, and the count reflects it.
        let mut q = PromptBuilder::new();
        for i in 0..60 {
            q.push_user(format!("question number {i} with a reasonable amount of text in it"));
            q.push_assistant(format!("answer number {i} with a reasonable amount of text in it"));
        }
        let before = q.len();
        q.token_budget = 200;
        let c = q.compact_to_budget("");
        assert!(c.did_anything(), "60 messages should compact at a 200 token budget");
        assert_eq!(q.len(), before - c.dropped);
        assert_eq!(q.compactions, 1);
        // the newest turn must survive
        assert!(q.history().last().unwrap().content.contains("answer number 59"));
    }

    /// The whole point of the two stages: a big old tool dump is worth more
    /// window than it is worth keeping, and paying for it by deleting turns is
    /// how a session loses the reasoning the user cared about.
    #[test]
    fn old_tool_output_is_trimmed_before_any_turn_is_deleted() {
        let mut p = PromptBuilder::new();
        for i in 0..12 {
            p.push_user(format!("question {i}: read the sheet again"));
            p.push_assistant_calls(
                "looking",
                &[ToolCall {
                    id: format!("c{i}"),
                    name: "sheet_read".into(),
                    arguments: "{\"path\":\"book.xlsx\"}".into(),
                }],
            );
            p.push_tool_result(&format!("c{i}"), "1:\tRegion\tUnits\n".repeat(2_000));
        }
        p.push_user("now total the Units column");
        let before = p.history().len();
        let est = p.estimate_tokens("");
        // Enough to need the dump, not enough to need the conversation.
        p.token_budget = est - 4_000;
        let c = p.compact_to_budget("");
        assert!(c.stubbed > 0, "the dump should be trimmed, not the chat: {c:?}");
        assert_eq!(c.dropped, 0, "no turn was deleted while 40k of old tool output was still in the window");
        assert_eq!(p.history().len(), before, "trimming is not deletion");
        // Trimmed results say which tool they came from and how big they were,
        // which is the one thing the model needs to decide whether to re-run it.
        let stub = p
            .history()
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content
            .clone();
        assert!(stub.contains("sheet_read"), "stub does not name the tool: {stub}");
        assert!(stub.contains("chars"), "stub does not say what it removed: {stub}");
        // The newest tool result is the one the model is reading now.
        let last_tool = p
            .history()
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(last_tool.content.len() > 1_000, "the live tool result was trimmed");
        assert!(p.estimate_tokens("") <= p.token_budget, "still over budget after trimming");
    }

    /// A stub shorter than what it replaces would make the window bigger.
    #[test]
    fn small_tool_results_are_left_alone() {
        let mut p = PromptBuilder::new();
        for i in 0..30 {
            p.push_user(format!("question {i} with some length to it, enough to matter"));
            p.push_assistant_calls(
                "thinking",
                &[ToolCall {
                    id: format!("s{i}"),
                    name: "exec".into(),
                    arguments: "{\"cmd\":\"ls\"}".into(),
                }],
            );
            p.push_tool_result(&format!("s{i}"), "ok");
        }
        p.token_budget = 300;
        let c = p.compact_to_budget("");
        assert_eq!(c.stubbed, 0, "`ok` is not worth a 120 character note about it");
        assert!(c.dropped > 0, "the history still has to fit somehow: {c:?}");
    }

    #[test]
    fn estimate_is_conservative_and_monotonic() {
        let mut p = PromptBuilder::new();
        let a = p.estimate_tokens("");
        p.push_user("a moderately long question about something specific");
        let b = p.estimate_tokens("");
        assert!(b > a);
        // CJK tokenizes worse than latin; the estimate must not go down
        p.push_user("这是一个关于系统性能的问题，需要仔细分析");
        let c = p.estimate_tokens("");
        assert!(c > b);
        // tool schemas count
        assert!(p.estimate_tokens("{\"name\":\"exec\"}") > p.estimate_tokens(""));
    }

    #[test]
    fn build_does_not_mutate() {
        let p = built();
        let n = p.len();
        for _ in 0..5 {
            p.build("m");
        }
        assert_eq!(p.len(), n);
    }
}
