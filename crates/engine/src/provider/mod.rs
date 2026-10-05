//! Provider abstraction (PLAN.md §5).
//!
//! One trait, three backends (llama.cpp, OpenAI, Anthropic). The whole point is
//! that the tool loop, telemetry and UI never learn which vendor is streaming.
//!
//! Design notes:
//!
//! * `stream` pushes [`Delta`]s into an mpsc sink rather than returning a
//!   `Stream`. Returning a stream from `async_trait` needs pinning gymnastics and
//!   gives the caller no place to measure TTFT. A sink lets the provider own the
//!   request timing (it knows when the request actually went out) and lets the
//!   tool loop stay linear.
//! * Providers emit **raw** tool-call argument fragments. JSON repair is a
//!   policy decision that belongs to the tool layer, not the wire layer.
//! * `TurnTelemetry` is emitted by the provider because only the provider can
//!   see server-side `timings`.

pub mod anthropic;
pub mod openai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::event::FinishReason;
use crate::telemetry::TurnTelemetry;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http {status} from {url}: {body}")]
    Http { status: u16, url: String, body: String },

    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("bad endpoint url: {0}")]
    Url(#[from] url::ParseError),

    #[error("malformed stream frame: {0}")]
    Frame(#[from] serde_json::Error),

    #[error("provider returned an unexpected shape: {0}")]
    Shape(String),

    #[error("{0} is not supported by this provider")]
    Unsupported(String),

    #[error("stream cancelled")]
    Cancelled,
}

/// Where the deltas go. Unbounded on purpose: the producer is network-bound and
/// the consumer is a webview/TUI that must never apply backpressure to a
/// provider mid-stream (that would stall the model's own decode loop).
pub type DeltaSink = mpsc::UnboundedSender<Delta>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    /// A tool result. OpenAI models this as role="tool" + tool_call_id;
    /// Anthropic models it as a user turn containing a tool_result block.
    Tool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON exactly as the model produced it, *before* repair. Keeping the
    /// original is what lets us show the user what actually happened when a
    /// small model emits garbage.
    pub arguments: String,
}

/// A non-text part of a message. Images are carried inline as base64 because
/// every provider's multimodal wire format wants them inline; the store keeps
/// them in their own table so ordinary transcript reads do not drag megabytes
/// across IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// e.g. `image/png`. Validated against an allow-list at the boundary.
    pub mime: String,
    /// base64 payload, no `data:` prefix.
    pub data_b64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Attachment {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data_b64)
    }

    /// Decoded size, ignoring base64 padding.
    pub fn approx_bytes(&self) -> usize {
        self.data_b64.len() * 3 / 4
    }
}

/// What the transcript needs to draw a placeholder without the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub mime: String,
    pub name: Option<String>,
    pub bytes: usize,
}

/// What an endpoint says about one model.
///
/// Everything except `id` is optional because it is whatever the server chose to
/// publish. llama.cpp is unusually forthcoming; a hand-written endpoint may say
/// nothing, and the UI then says "not reported" rather than guessing on the
/// user's behalf.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Context window in tokens, when the server states it.
    pub context_tokens: Option<u64>,
    /// Input modalities as the server spells them: "text", "image", "audio".
    pub input_modalities: Vec<String>,
    /// Tool calling. `None` means the endpoint never said.
    ///
    /// llama.cpp has no flag for this; the value is inferred from whether the
    /// served chat template can render tool calls. That is a detection, not a
    /// promise, and the UI labels it as such.
    pub tools: Option<bool>,
    /// True when the tools value came from the chat template rather than from a
    /// capability the endpoint stated outright.
    pub tools_inferred: bool,
    /// Underlying file or repo name, when the server reveals it.
    pub source: Option<String>,
    /// Server build string, e.g. llama.cpp's `build_info`.
    pub server: Option<String>,
    /// Concurrent slots the endpoint advertises.
    pub slots: Option<u64>,
    /// Reasoning effort values this model accepts, e.g. ["low","medium","xhigh"].
    ///
    /// Read out of the served chat template, because no endpoint states it as
    /// data. Empty means the endpoint does not expose thinking levels at all.
    pub reasoning_efforts: Vec<String>,
    /// The effort the template applies when none is given.
    pub reasoning_default: Option<String>,
    /// True if the endpoint published at least one capability for this model.
    pub advertised: bool,
}

impl ModelInfo {
    pub fn vision(&self) -> Option<bool> {
        if self.input_modalities.is_empty() {
            return None;
        }
        Some(self.input_modalities.iter().any(|m| m == "image"))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Only meaningful on user messages in v1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Reasoning we chose to retain. Whether this round-trips back into the
    /// next prompt is a per-provider decision (see `Provider::wants_reasoning`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Anthropic signs each thinking block and requires the signed block to be
    /// sent back verbatim when tools are used. Only meaningful for that backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    /// How long the model spent thinking, in milliseconds. UI metadata only: it
    /// never reaches the wire, but it is persisted so a reloaded transcript
    /// still says "thought for 2m34s" instead of losing the information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// What a file tool changed, for the tool card: `+N -M` in the header and a
    /// diff when expanded. UI metadata on the same footing as `reasoning_ms` -
    /// persisted so a reopened session still shows it, never sent to the model.
    /// A diff of what the model just wrote is the most expensive possible thing
    /// to put back into a prompt, and it would move the prompt bytes the prefix
    /// cache is keyed on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<crate::tools::changes::FileChange>,
}

impl Message {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: Role::System, content: s.into(), ..Default::default() }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: Role::User, content: s.into(), ..Default::default() }
    }
    pub fn user_with(
        s: impl Into<String>,
        attachments: Vec<Attachment>,
    ) -> Self {
        Self { role: Role::User, content: s.into(), attachments, ..Default::default() }
    }
    /// True when there is nothing to send, text or otherwise.
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty() && self.attachments.is_empty()
    }
    pub fn assistant(s: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: s.into(), ..Default::default() }
    }
    pub fn tool_result(id: impl Into<String>, s: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: s.into(),
            tool_call_id: Some(id.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the parameters.
    pub parameters: serde_json::Value,
}

/// Reasoning / "thinking" control.
///
/// llama.cpp + Qwen3 exposes a boolean `enable_thinking`; OpenAI exposes an
/// enum `reasoning_effort`. We keep one enum and let each adapter degrade it
/// honestly rather than pretend low/medium/high works everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reasoning {
    #[default]
    Off,
    Low,
    Medium,
    High,
    /// The top setting. Named rather than implied by "High" because the UI shows
    /// it as a distinct choice; providers that have no equivalent clamp to their
    /// highest real level rather than inventing a value the API would reject.
    ///
    /// Renamed explicitly: camelCase would emit "xHigh", which the front end and
    /// the config file both spell "xhigh". A missed lookup there renders an empty
    /// label rather than failing, which is how this one hid.
    #[serde(rename = "xhigh")]
    XHigh,
}

#[cfg(test)]
mod reasoning_wire_tests {
    use super::*;

    /// The front end indexes a label table with these exact strings, and the
    /// config parser accepts them. A serde rename on either side renders an empty
    /// chip label instead of erroring, so assert the literal set.
    #[test]
    fn reasoning_wire_names_match_the_front_end() {
        let cases = [
            (Reasoning::Off, "off"),
            (Reasoning::Low, "low"),
            (Reasoning::Medium, "medium"),
            (Reasoning::High, "high"),
            (Reasoning::XHigh, "xhigh"),
        ];
        for (r, want) in cases {
            let v = serde_json::to_value(r).unwrap();
            assert_eq!(v.as_str().unwrap(), want, "wrong wire name for {r:?}");
            let back: Reasoning = serde_json::from_value(v).unwrap();
            assert_eq!(back, r);
        }
    }
}

impl Reasoning {
    pub fn enabled(self) -> bool {
        self != Reasoning::Off
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub reasoning: Reasoning,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            reasoning: Reasoning::Off,
            temperature: None,
            top_p: None,
            max_tokens: None,
        }
    }
}

/// One normalized streaming increment, across every vendor.
#[derive(Debug, Clone)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    /// A tool call started; `name` is known, arguments have not arrived.
    ToolCallBegin { id: String, name: String },
    /// A fragment of tool-call JSON. Concatenate in arrival order.
    ToolCallArgs(String),
    /// End of a reasoning block. `signature` is an opaque attestation that some
    /// vendors (Anthropic) require to be replayed with the block.
    ReasoningEnd { signature: Option<String> },
    /// All fragments for that call have arrived. Arguments are still raw.
    ToolCallReady { id: String, name: String, arguments: String },
    /// Server-side + client-measured telemetry for the turn that just finished.
    Telemetry(TurnTelemetry),
    Done(FinishReason),
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier, e.g. "llamacpp", "openai", "anthropic".
    fn id(&self) -> &str;

    /// Display name for the model picker.
    fn label(&self) -> &str;

    async fn stream(&self, req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError>;

    /// Whether this provider expects prior `reasoning` to be sent back on
    /// assistant messages. llama.cpp/Qwen3 generally should NOT (it inflates the
    /// prompt and breaks prefix caching); Anthropic thinking blocks must be
    /// returned verbatim when tools are used.
    fn wants_reasoning(&self) -> bool {
        false
    }

    /// Model ids offered by the endpoint, for the composer's picker.
    ///
    /// Optional by design: a hand-written custom endpoint may expose nothing, and
    /// "no list" is a normal state rather than an error the user should see as a
    /// failure.
    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Err(ProviderError::Unsupported("model listing".into()))
    }

    /// The model list plus whatever the endpoint advertises about each model.
    ///
    /// The default degrades to bare ids, which is the honest answer for a server
    /// that publishes nothing else.
    async fn list_models_info(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self
            .list_models()
            .await?
            .into_iter()
            .map(|id| ModelInfo {
                id,
                advertised: false,
                ..Default::default()
            })
            .collect())
    }

    /// Cheap local token estimate used for the context-usage bar. Exact only if
    /// the backend offers /tokenize.
    fn estimate_tokens(&self, req: &ChatRequest) -> u64 {
        let mut chars = 0usize;
        for m in &req.messages {
            chars += m.content.len() + m.reasoning.as_ref().map_or(0, |r| r.len());
            for tc in &m.tool_calls {
                chars += tc.name.len() + tc.arguments.len();
            }
        }
        for t in &req.tools {
            chars += t.name.len() + t.description.len() + t.parameters.to_string().len();
        }
        // ~3.6 chars/token is a reasonable multilingual+code average; deliberately
        // an estimate, and labelled as one in the UI.
        (chars as f64 / 3.6).ceil() as u64
    }
}

/// Map a vendor's stop reason onto ours. Unrecognised values collapse to
/// `Other`; the enum is Copy and crosses the IPC boundary, so the raw string is
/// not carried. Every vendor we target emits one of the mapped set.
pub fn map_finish(s: &str) -> FinishReason {
    match s {
        "stop" | "end_turn" | "stop_sequence" => FinishReason::Stop,
        "length" | "max_tokens" => FinishReason::Length,
        "tool_calls" | "tool_use" => FinishReason::ToolCalls,
        "abort" | "cancelled" => FinishReason::Cancelled,
        _ => FinishReason::Other,
    }
}
