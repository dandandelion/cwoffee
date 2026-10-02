//! Anthropic Messages API adapter.
//!
//! Different enough from OpenAI that sharing a parser would be a lie:
//!   * SSE carries a meaningful `event:` field; the payload's `type` mirrors it
//!   * `system` is a top-level parameter, not a message
//!   * roles must strictly alternate; consecutive same-role turns must merge
//!   * tool results are `tool_result` blocks inside a **user** message
//!   * `max_tokens` is required
//!   * thinking blocks are signed, and when tools are used the signed block must
//!     be replayed verbatim or the API rejects the request
//!
//! Wire shapes below were written against the documented v1 streaming format;
//! `tests` replay a recorded event sequence so regressions are caught without a
//! network call.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::event::FinishReason;
use crate::provider::{
    map_finish, ChatRequest, Delta, DeltaSink, Message, Provider, ProviderError, Role, ToolCall,
};
use crate::sse::SseParser;
use crate::telemetry::TurnTelemetry;

pub struct AnthropicProvider {
    label: String,
    base_url: url::Url,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(label: impl Into<String>, base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let mut base: url::Url = base_url.into().parse()?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            label: label.into(),
            base_url: base,
            api_key: std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()),
            http: reqwest::Client::builder()
                .tcp_nodelay(true)
                .build()
                .map_err(ProviderError::Transport)?,
        })
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    fn models_url(&self) -> String {
        self.base_url
            .join("models")
            .map(|u| u.into())
            .unwrap_or_else(|_| self.base_url.as_str().to_string())
    }

    fn endpoint(&self) -> String {
        self.base_url
            .join("messages")
            .map(|u| u.into())
            .unwrap_or_else(|_| self.base_url.as_str().to_string())
    }
}

// ---- request construction --------------------------------------------------

/// Split system turns out, then merge consecutive same-role turns.
/// Anthropic rejects a request whose messages do not alternate.
pub fn to_anthropic_messages(msgs: &[Message]) -> (Vec<Value>, Vec<String>) {
    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for m in msgs {
        match m.role {
            Role::System => {
                if !m.content.is_empty() {
                    system.push(m.content.clone());
                }
                continue;
            }
            Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                });
                push_block(&mut out, "user", block);
                continue;
            }
            Role::User => {
                let block = json!({ "type": "text", "text": m.content });
                push_block(&mut out, "user", block);
            }
            Role::Assistant => {
                // Signed thinking must come first in the block list, and only
                // when we actually have a signature to go with it.
                if let (Some(thinking), true) = (
                    m.reasoning.clone(),
                    m.reasoning_signature.is_some(),
                ) {
                    if !thinking.is_empty() {
                        push_block(
                            &mut out,
                            "assistant",
                            json!({
                                "type": "thinking",
                                "thinking": thinking,
                                "signature": m.reasoning_signature.clone().unwrap(),
                            }),
                        );
                    }
                }
                if !m.content.is_empty() {
                    push_block(&mut out, "assistant", json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    // input must be an object, not the raw string the model sent
                    let input: Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or_else(|_| json!({}));
                    push_block(
                        &mut out,
                        "assistant",
                        json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": input,
                        }),
                    );
                }
            }
        }
    }

    (out, system)
}

/// Append `block` to the last message if the role matches, else start a new one.
/// This is the alternation fix.
fn push_block(out: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                arr.push(block);
                return;
            }
        }
    }
    out.push(json!({ "role": role, "content": [block] }));
}

impl AnthropicProvider {
    pub fn build_body(req: &ChatRequest) -> Value {
        let (messages, system) = to_anthropic_messages(&req.messages);

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            // required by the API
            "max_tokens": req.max_tokens.unwrap_or(4096),
            "stream": true,
        });

        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = req.top_p {
            body["top_p"] = json!(p);
        }

        if req.reasoning.enabled() {
            // budget_tokens must be < max_tokens or the API 400s.
            let max = req.max_tokens.unwrap_or(4096);
            let budget = match req.reasoning {
                crate::provider::Reasoning::Low => (max / 4).max(1024),
                crate::provider::Reasoning::High => (max * 3 / 4).max(1024),
                _ => (max / 2).max(1024),
            };
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget.min(max - 1) });
        }

        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })).collect::<Vec<_>>());
        }

        body
    }
}

// ---- wire shapes -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Ev {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    #[serde(default)]
    delta: Option<DeltaBlk>,
    #[serde(default)]
    message: Option<StartMessage>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaBlk {
    /// Not always present: the `message_delta` delta carries only `stop_reason`
    /// and `stop_sequence`, with no `type` field at all.
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Which block index is a tool call, and what its id/name were.
#[derive(Default)]
struct BlockMap {
    tools: std::collections::HashMap<u32, (String, String)>,
}

#[async_trait]


impl Provider for AnthropicProvider {
    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = self.models_url();
        let mut req = self.http.get(&url);
        if let Some(k) = &self.api_key {
            req = req.header("x-api-key", k);
        }
        req = req.header("anthropic-version", "2023-06-01");
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                url,
                body: body.chars().take(400).collect(),
            });
        }
        let v: serde_json::Value = resp.json().await?;
        let arr = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::Shape("models response has no `data` array".into()))?;
        Ok(arr
            .iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect())
    }
    fn id(&self) -> &str {
        "anthropic"
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn wants_reasoning(&self) -> bool {
        true
    }

    async fn stream(&self, req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError> {
        let url = self.endpoint();
        let body = AnthropicProvider::build_body(req);

        let mut call = self
            .http
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        if let Some(k) = &self.api_key {
            call = call.header("x-api-key", k);
        }

        let started = std::time::Instant::now();
        let resp = call.send().await.map_err(ProviderError::Transport)?;
        let status = resp.status();
        if !status.is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                url,
                body: b.chars().take(2000).collect(),
            });
        }

        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut blocks = BlockMap::default();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cached_tokens = 0u64;
        let mut ttft_ms: Option<u64> = None;
        let mut finish: Option<String> = None;
        // tool args by block index, so we can emit Ready at content_block_stop
        let mut args_by_block: std::collections::HashMap<u32, String> = Default::default();

        let mut stop = false;
        while !stop {
            let Some(chunk) = stream.next().await else { break };
            let bytes = chunk.map_err(ProviderError::Transport)?;
            for sse in parser.feed(&bytes) {
                let ev: Ev = match serde_json::from_str(&sse.data) {
                    Ok(e) => e,
                    // `ping` and unknown event types must not kill the stream
                    Err(_) => continue,
                };
                match ev.kind.as_str() {
                    "message_start" => {
                        if let Some(m) = ev.message {
                            if let Some(u) = m.usage {
                                input_tokens = u.input_tokens.unwrap_or(0);
                                cached_tokens = u.cache_read_input_tokens.unwrap_or(0);
                            }
                        }
                    }
                    "content_block_start" => {
                        let idx = ev.index.unwrap_or(0);
                        if let Some(cb) = ev.content_block {
                            match cb.kind.as_str() {
                                "tool_use" => {
                                    let id = cb.id.unwrap_or_default();
                                    let name = cb.name.unwrap_or_default();
                                    let _ = sink.send(Delta::ToolCallBegin {
                                        id: id.clone(),
                                        name: name.clone(),
                                    });
                                    blocks.tools.insert(idx, (id, name));
                                    args_by_block.entry(idx).or_default();
                                }
                                "text" => {
                                    if let Some(t) = cb.text.filter(|t| !t.is_empty()) {
                                        ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                                        let _ = sink.send(Delta::Text(t));
                                    }
                                }
                                "thinking" => {
                                    if let Some(t) = cb.thinking.filter(|t| !t.is_empty()) {
                                        ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                                        let _ = sink.send(Delta::Reasoning(t));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_delta" => {
                        let idx = ev.index.unwrap_or(0);
                        let Some(d) = ev.delta else { continue };
                        match d.kind.as_str() {
                            "text_delta" => {
                                if let Some(t) = d.text.filter(|t| !t.is_empty()) {
                                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                                    let _ = sink.send(Delta::Text(t));
                                }
                            }
                            "thinking_delta" => {
                                if let Some(t) = d.thinking.filter(|t| !t.is_empty()) {
                                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                                    let _ = sink.send(Delta::Reasoning(t));
                                }
                            }
                            "signature_delta" => {
                                let _ = sink.send(Delta::ReasoningEnd {
                                    signature: d.signature.filter(|s| !s.is_empty()),
                                });
                            }
                            "input_json_delta" => {
                                if let Some(j) = d.partial_json.filter(|j| !j.is_empty()) {
                                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                                    args_by_block.entry(idx).or_default().push_str(&j);
                                    let _ = sink.send(Delta::ToolCallArgs(j));
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let idx = ev.index.unwrap_or(0);
                        if let Some((id, name)) = blocks.tools.get(&idx) {
                            let args = args_by_block.remove(&idx).unwrap_or_default();
                            let _ = sink.send(Delta::ToolCallReady {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: args,
                            });
                        }
                    }
                    "message_delta" => {
                        if let Some(d) = &ev.delta {
                            if let Some(sr) = &d.stop_reason {
                                finish = Some(sr.clone());
                            }
                        }
                        if let Some(u) = ev.usage {
                            output_tokens = u.output_tokens.unwrap_or(output_tokens);
                        }
                    }
                    "message_stop" => stop = true,
                    "error" => {
                        return Err(ProviderError::Shape(sse.data));
                    }
                    // ping, content_block_stop on non-tools, etc.
                    _ => {}
                }
            }
        }

        let mut tel = TurnTelemetry::default();
        tel.prompt_tokens = input_tokens;
        tel.completion_tokens = output_tokens;
        tel.cached_tokens = cached_tokens;
        tel.ttft_ms = ttft_ms.unwrap_or(0);
        // Anthropic exposes no rates; the client-measured window is all we have.
        let _ = sink.send(Delta::Telemetry(tel));

        let reason = match finish.as_deref() {
            Some(f) => map_finish(f),
            None => FinishReason::Stop,
        };
        let _ = sink.send(Delta::Done(reason));
        Ok(())
    }
}

/// Rebuild an assistant Message from collected deltas, keeping the signature.
pub fn assistant_message(
    text: String,
    reasoning: Option<String>,
    signature: Option<String>,
    tool_calls: Vec<ToolCall>,
) -> Message {
    Message {
        role: Role::Assistant,
        content: text,
        reasoning,
        reasoning_signature: signature,
        tool_calls,
        tool_call_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Reasoning, Role, ToolCall};

    fn replay(events: &[(&str, &str)]) -> Vec<Delta> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut parser = SseParser::new();
        let mut blocks = BlockMap::default();
        let mut args: std::collections::HashMap<u32, String> = Default::default();

        for (name, data) in events {
            let raw = format!("event: {name}\ndata: {data}\n\n");
            for sse in parser.feed(raw.as_bytes()) {
                assert_eq!(sse.name, *name);
                let ev: Ev = serde_json::from_str(&sse.data).expect("frame parses");
                match ev.kind.as_str() {
                    "content_block_start" => {
                        let idx = ev.index.unwrap_or(0);
                        if let Some(cb) = ev.content_block {
                            if cb.kind == "tool_use" {
                                let id = cb.id.unwrap_or_default();
                                let nm = cb.name.unwrap_or_default();
                                let _ = tx.send(Delta::ToolCallBegin { id: id.clone(), name: nm.clone() });
                                blocks.tools.insert(idx, (id, nm));
                            }
                        }
                    }
                    "content_block_delta" => {
                        let idx = ev.index.unwrap_or(0);
                        let d = ev.delta.unwrap();
                        match d.kind.as_str() {
                            "text_delta" => { let _ = tx.send(Delta::Text(d.text.unwrap())); }
                            "thinking_delta" => { let _ = tx.send(Delta::Reasoning(d.thinking.unwrap())); }
                            "signature_delta" => {
                                let _ = tx.send(Delta::ReasoningEnd { signature: d.signature });
                            }
                            "input_json_delta" => {
                                args.entry(idx).or_default().push_str(d.partial_json.as_deref().unwrap_or(""));
                                let _ = tx.send(Delta::ToolCallArgs(d.partial_json.unwrap()));
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let idx = ev.index.unwrap_or(0);
                        if let Some((id, nm)) = blocks.tools.get(&idx) {
                            let a = args.remove(&idx).unwrap_or_default();
                            let _ = tx.send(Delta::ToolCallReady {
                                id: id.clone(), name: nm.clone(), arguments: a,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut out = Vec::new();
        while let Ok(d) = rx.try_recv() {
            out.push(d);
        }
        out
    }

    #[test]
    fn thinking_then_signed_tool_use_stream() {
        let deltas = replay(&[
            ("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":20}}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me run it"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"AbC=="}}"#),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"exec"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#),
            ("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ]);

        let begins: Vec<_> = deltas.iter().filter(|d| matches!(d, Delta::ToolCallBegin { .. })).collect();
        assert_eq!(begins.len(), 1);

        let ready = deltas.iter().find_map(|d| match d {
            Delta::ToolCallReady { id, name, arguments } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        });
        assert_eq!(ready.unwrap(), ("toolu_9".into(), "exec".into(), "{\"cmd\":\"ls\"}".into()));

        let sig = deltas.iter().find_map(|d| match d {
            Delta::ReasoningEnd { signature } => signature.clone(),
            _ => None,
        });
        assert_eq!(sig.as_deref(), Some("AbC=="));
    }

    #[test]
    fn system_is_extracted_and_roles_alternate() {
        let msgs = vec![
            Message::system("be nice"),
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::tool_result("t1", "result"),
        ];
        let (out, system) = to_anthropic_messages(&msgs);
        assert_eq!(system, vec!["be nice".to_string()]);

        // user(q1) | assistant(a1) | user(q2 + tool_result merged)
        assert_eq!(out.len(), 3);
        let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(out[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[2]["content"][1]["type"], json!("tool_result"));
    }

    #[test]
    fn tool_use_input_is_an_object_not_a_string() {
        let m = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "exec".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }],
            ..Default::default()
        };
        let (out, _) = to_anthropic_messages(&[m]);
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], json!("tool_use"));
        assert!(block["input"].is_object());
        assert_eq!(block["input"]["cmd"], json!("ls"));
    }

    #[test]
    fn thinking_budget_stays_below_max_tokens() {
        let mut r = ChatRequest::new("claude-x", vec![Message::user("hi")]);
        r.max_tokens = Some(2000);
        r.reasoning = Reasoning::High;
        let b = AnthropicProvider::build_body(&r);
        let budget = b["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(budget < 2000, "budget {budget} must be < max_tokens");
    }

    #[test]
    fn max_tokens_always_present() {
        let r = ChatRequest::new("claude-x", vec![Message::user("hi")]);
        let b = AnthropicProvider::build_body(&r);
        assert!(b["max_tokens"].is_number());
    }

    #[test]
    fn signed_thinking_is_replayed_when_signature_exists() {
        let m = Message {
            role: Role::Assistant,
            content: "done".into(),
            reasoning: Some("I thought hard".into()),
            reasoning_signature: Some("SIG==".into()),
            ..Default::default()
        };
        let (out, _) = to_anthropic_messages(&[m]);
        assert_eq!(out[0]["content"][0]["type"], json!("thinking"));
        assert_eq!(out[0]["content"][0]["signature"], json!("SIG=="));
    }

    #[test]
    fn unsigned_thinking_is_never_replayed() {
        // Replaying an unsigned thinking block is an API error, so it must be dropped.
        let m = Message {
            role: Role::Assistant,
            content: "done".into(),
            reasoning: Some("I thought hard".into()),
            reasoning_signature: None,
            ..Default::default()
        };
        let (out, _) = to_anthropic_messages(&[m]);
        assert_eq!(out[0]["content"][0]["type"], json!("text"));
    }
}
