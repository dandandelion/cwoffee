//! The event contract between the engine and *any* front end.
//!
//! This enum is the single most important interface in the project. The desktop
//! app, the TUI, and a future headless daemon all consume exactly this. Nothing
//! in here may reference a UI type.
//!
//! Serde representation is **adjacently tagged** (`{ "kind": ..., "d": ... }`)
//! because internally-tagged enums cannot represent newtype/tuple variants, and
//! untagged enums make malformed payloads ambiguous to decode. Adjacent tagging
//! is compact, cheap to serialise, and unambiguous on the JS side.
//!
//! Wire shape, exactly as mirrored by `ui/src/lib/events.ts`:
//!   variant names are camelCase (via `rename_all` below)
//!   struct-variant **fields stay snake_case** (container rename_all does not
//!   touch them) — e.g. `{"kind":"turnStart","d":{"turn_id":1,"model":"..."}}`

use serde::{Deserialize, Serialize};

use crate::telemetry::{TurnStats, TurnTelemetry};
use crate::tools::Risk;

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishReason {
    /// model produced its answer and stopped
    Stop,
    /// model asked for tool calls; the loop will continue after execution
    ToolCalls,
    /// context window exhausted
    Length,
    /// user cancelled
    Cancelled,
    /// provider or engine error
    Error,
    /// a vendor stop reason we do not model; the raw string is not carried
    /// because this enum is Copy and crosses the IPC boundary on every turn
    Other,
}

/// One event in a turn's stream. The engine emits these in order; a front end
/// renders them incrementally and must never need to re-read history to paint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "d", rename_all = "camelCase")]
pub enum EngineEvent {
    /// A turn began. Carries the resolved model id so the UI can label the reply.
    /// The active session changed. The UI reloads its transcript in response to
    /// this event rather than assuming the switch already landed: the host owns
    /// that ordering, and reading the store before it moves shows the previous
    /// session's messages.
    SessionChanged {
        session_id: i64,
        title: String,
    },
    TurnStart {
        turn_id: u64,
        model: String,
    },

    /// Visible answer text, one delta at a time.
    ///
    /// CONTRACT: the front end appends these as **plain text** while streaming.
    /// Markdown parsing happens once, on `TurnEnd` — never per delta
    /// (PLAN.md §9 rule 3).
    Text(String),

    /// Chain-of-thought / `reasoning_content` from llama.cpp-style servers.
    /// Rendered in a collapsible block, and never mixed into `Text`.
    Reasoning(String),

    /// A tool call began. `name` is the tool, `args` is not yet available.
    ToolCallBegin {
        id: String,
        name: String,
    },

    /// Fragment of the tool-call JSON arguments, arriving incrementally.
    /// Accumulate, then repair-and-parse (PLAN.md §4: a 3-bit quant will
    /// occasionally emit malformed JSON).
    ToolCallArgs(String),

    /// The accumulated arguments parsed (or repaired) successfully.
    ToolCallReady {
        id: String,
        args: serde_json::Value,
    },

    /// Arguments could not be parsed even after repair.
    ToolCallParseError {
        id: String,
        raw: String,
        message: String,
    },

    /// Files the turn produced inside the workspace. Rendered as clickable
    /// attachments under the answer; never sent to the model.
    TurnFiles {
        turn_id: u64,
        files: Vec<crate::tools::OutputFile>,
    },

    /// A tool began executing (used for the live "running…" card state).
    ToolExecStart {
        id: String,
    },

    /// Chunk of a tool's stdout/stderr, for live display.
    ToolExecOutput {
        id: String,
        stream: ExecStream,
        chunk: String,
    },

    /// Tool finished.
    ToolExecEnd {
        id: String,
        ok: bool,
        exit_code: Option<i32>,
        /// What file tools changed, for the `+N -M` badge and the diff view.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<crate::tools::changes::FileChange>,
    },

    /// The gate wants permission before a tool runs.
    ///
    /// CONTRACT: the front end must answer with `respond_approval(id, answer)`.
    /// If it never does — closed webview, crashed renderer — the gate fails
    /// closed and the tool does not run. A UI that disappears must never turn
    /// into an agent that runs things unattended.
    ApprovalRequested {
        id: u64,
        tool: String,
        risk: Risk,
        summary: String,
        args: serde_json::Value,
    },

    /// The model asked the user to choose something, and the turn is parked
    /// until someone answers.
    ///
    /// CONTRACT: the front end answers with `respond_question(id, text, options)`,
    /// where `options` is the list of chosen indices (empty when the user typed
    /// instead). Not answering is allowed - the tool times out and tells the model
    /// to decide for itself and say so - but a front end that *can* answer should,
    /// because the turn is doing nothing until it does.
    QuestionRequested {
        id: u64,
        question: String,
        options: Vec<crate::tools::ask::QuestionOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recommended: Option<usize>,
        allow_custom: bool,
    },

    /// A question was answered or cleared, from any client. Every view gets
    /// this so a card cannot sit there offering a choice that is already made.
    QuestionAnswered {
        id: u64,
    },

    /// A question closed *without* an answer: the timeout expired, or the turn
    /// was cancelled while it was parked. Without this the card stays live in
    /// every window - and clicking Send on it then aims an answer at a tool call
    /// that has already returned, which is how a five-minute-old question ends up
    /// answering nothing.
    QuestionExpired {
        id: u64,
    },

    /// Live progress, emitted a few times a second while a turn streams.
    ///
    /// These are estimates: the server reports token counts only at the end of
    /// the turn, so mid-turn rates come from character counts. `Usage` follows
    /// with the real numbers, and front ends should prefer those when present.
    TurnStats(TurnStats),

    /// Per-turn telemetry. Emitted once, just before `TurnEnd`.
    Usage(TurnTelemetry),

    /// The turn is over. Front ends may now do the expensive render pass.
    TurnEnd {
        turn_id: u64,
        finish: FinishReason,
    },

    /// A non-actionable state change worth showing in the transcript, e.g. the
    /// workspace moving. Not an error: nothing went wrong.
    Note {
        message: String,
    },

    /// A non-fatal or fatal problem. `fatal` means the turn is over.
    Error {
        message: String,
        fatal: bool,
    },
}

/// Which pane a tool output chunk belongs to. The chunk itself travels on
/// `EngineEvent::ToolExecOutput`, so this stays a plain Copy tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecStream {
    Stdout,
    Stderr,
}

impl EngineEvent {
    /// True for events that carry user-visible answer text.
    pub fn is_answer_delta(&self) -> bool {
        matches!(self, EngineEvent::Text(_))
    }
}
