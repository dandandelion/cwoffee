//! OpenAI-compatible chat completions adapter.
//!
//! Serves two masters, which is why it is one file:
//!   * llama.cpp `llama-server` — our primary BYOM target
//!   * OpenAI and the swarm of clones that copy its wire format
//!
//! Verified against the live llama.cpp server on this box (see PLAN.md). The
//! parts that differ from vanilla OpenAI:
//!   * `delta.reasoning_content` carries thinking tokens
//!   * `chat_template_kwargs` controls Qwen3-style thinking on/off
//!   * the final chunk carries a `timings` object with cache_n / prompt_per_second
//!     / predicted_per_second / draft_n / draft_n_accepted — a telemetry gift that
//!     vanilla OpenAI does not offer
//!   * tool-call `arguments` arrive in many small fragments, not one blob

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::event::FinishReason;
use crate::provider::{
    map_finish, ChatRequest, Delta, DeltaSink, Provider, ProviderError, Reasoning, Role,
};
use crate::sse::{SseParser, DONE};
use crate::telemetry::TurnTelemetry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Vanilla OpenAI (or a clone): no chat_template_kwargs, has reasoning_effort.
    OpenAi,
    /// llama.cpp llama-server: chat_template_kwargs, timings, reasoning_content.
    LlamaCpp,
}

pub struct OpenAiProvider {
    label: String,
    flavor: Flavor,
    base_url: url::Url,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAiProvider {
    /// `base_url` should be the API root, e.g. `http://127.0.0.1:8090/v1`.
    ///
    /// The trailing slash is not cosmetic: `Url::join` replaces the last path
    /// segment, so joining "chat/completions" onto `.../v1` yields
    /// `.../chat/completions` and a 404. We normalise it away.
    pub fn new(label: impl Into<String>, flavor: Flavor, base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let mut base: url::Url = base_url.into().parse()?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            label: label.into(),
            flavor,
            base_url: base,
            api_key: std::env::var("COWRY_API_KEY").ok().filter(|s| !s.is_empty()),
            http: reqwest::Client::builder()
                .tcp_nodelay(true)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
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
            .join("chat/completions")
            .map(|u| u.into())
            .unwrap_or_else(|_| self.base_url.as_str().to_string())
    }

    pub fn build_body(req: &ChatRequest, flavor: Flavor) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(to_openai_message).collect();

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            // Without this, OpenAI sends no usage at all and llama.cpp may
            // withhold timings; we would lose cache-hit rate and spec-decode
            // acceptance, which are the whole point of the status bar.
            "stream_options": { "include_usage": true },
        });

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = req.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = json!(m);
        }

        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools.iter().map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })).collect::<Vec<_>>()
            );
            // "auto" lets a small model answer plainly instead of forcing a call;
            // "required" is a good way to make a quantized model hallucinate.
            body["tool_choice"] = json!("auto");
        }

        match flavor {
            Flavor::LlamaCpp => {
                // Qwen3-family chat templates read this. There is no low/medium/
                // high on this backend, so we degrade to a boolean rather than
                // silently pretending the level was honoured.
                body["chat_template_kwargs"] = json!({ "enable_thinking": req.reasoning.enabled() });
            }
            Flavor::OpenAi => {
                if req.reasoning != Reasoning::Off {
                    body["reasoning_effort"] = json!(match req.reasoning {
                        Reasoning::Low => "low",
                        Reasoning::High => "high",
                        _ => "medium",
                    });
                }
            }
        }

        body
    }
}

fn to_openai_message(m: &crate::provider::Message) -> Value {
    let role = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), json!(role));

    if m.role == Role::Tool {
        if let Some(id) = &m.tool_call_id {
            obj.insert("tool_call_id".into(), json!(id));
        }
        obj.insert("content".into(), json!(m.content));
        return Value::Object(obj);
    }

    // Do NOT replay reasoning to llama.cpp: it inflates the prompt and, more
    // importantly, breaks the append-only prefix that makes llama.cpp's prefix
    // cache hit. Reasoning is displayed, not re-fed.
    obj.insert("content".into(), json!(m.content));

    if !m.tool_calls.is_empty() {
        obj.insert(
            "tool_calls".into(),
            json!(m.tool_calls.iter().map(|tc| json!({
                "id": tc.id,
                "type": "function",
                "function": { "name": tc.name, "arguments": tc.arguments },
            })).collect::<Vec<_>>()),
        );
    }

    Value::Object(obj)
}

// ---- wire shapes -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    timings: Option<Timings>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<DeltaWire>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaWire {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallWire>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallWire {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnWire>,
}

#[derive(Debug, Deserialize)]
struct FnWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// llama.cpp `timings`. Field names copied from the live server response.
#[derive(Debug, Deserialize)]
struct Timings {
    #[serde(default)]
    cache_n: u64,
    #[serde(default)]
    prompt_n: u64,
    #[serde(default)]
    prompt_ms: f64,
    #[serde(default)]
    prompt_per_second: f64,
    #[serde(default)]
    predicted_n: u64,
    #[serde(default)]
    predicted_ms: f64,
    #[serde(default)]
    predicted_per_second: f64,
    #[serde(default)]
    draft_n: u64,
    #[serde(default)]
    draft_n_accepted: u64,
}

/// Accumulates fragmented tool-call deltas into complete calls.
#[derive(Default)]
struct ToolAccum {
    order: Vec<usize>,
    calls: std::collections::HashMap<usize, Acc>,
}

#[derive(Default)]
struct Acc {
    id: String,
    name: String,
    args: String,
    began: bool,
}

impl ToolAccum {
    fn merge(&mut self, w: ToolCallWire, sink: &DeltaSink) {
        let idx = w.index.unwrap_or(0);
        if !self.calls.contains_key(&idx) {
            self.calls.insert(idx, Acc::default());
            self.order.push(idx);
        }

        // Merge identity fields first, then release the borrow: maybe_begin
        // needs &mut self too, and holding both is why this looks convoluted.
        {
            let entry = self.calls.get_mut(&idx).expect("inserted above");
            if let Some(id) = w.id.filter(|s| !s.is_empty()) {
                entry.id = id;
            }
            if let Some(f) = &w.function {
                if let Some(name) = f.name.as_ref().filter(|s| !s.is_empty()) {
                    if entry.name.is_empty() {
                        entry.name = name.clone();
                    }
                }
            }
        }

        self.maybe_begin(idx, sink);

        if let Some(a) = w.function.and_then(|f| f.arguments).filter(|s| !s.is_empty()) {
            // A server may send arguments before any name; open the card now so
            // the UI is never left without one.
            self.maybe_begin(idx, sink);
            self.calls.get_mut(&idx).expect("inserted above").args.push_str(&a);
            let _ = sink.send(Delta::ToolCallArgs(a));
        }
    }

    /// Emit ToolCallBegin as soon as we know the name, so the UI can draw the
    /// card before any arguments land.
    fn maybe_begin(&mut self, idx: usize, sink: &DeltaSink) {
        let Some(entry) = self.calls.get_mut(&idx) else { return };
        if entry.began || entry.name.is_empty() {
            return;
        }
        entry.began = true;
        let id = if entry.id.is_empty() { format!("call_{idx}") } else { entry.id.clone() };
        let _ = sink.send(Delta::ToolCallBegin { id, name: entry.name.clone() });
    }

    fn finish(self, sink: &DeltaSink) {
        for idx in self.order {
            let Some(c) = self.calls.get(&idx) else { continue };
            let id = if c.id.is_empty() { format!("call_{idx}") } else { c.id.clone() };
            let _ = sink.send(Delta::ToolCallReady {
                id,
                name: c.name.clone(),
                arguments: c.args.clone(),
            });
        }
    }
}

#[async_trait]


impl Provider for OpenAiProvider {
    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = self.models_url();
        let mut req = self.http.get(&url);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
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
        // OpenAI-shaped {"data":[{"id":...}]}; llama.cpp answers the same.
        let arr = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::Shape("models response has no `data` array".into()))?;
        let mut ids: Vec<String> = arr
            .iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
    fn id(&self) -> &str {
        match self.flavor {
            Flavor::OpenAi => "openai",
            Flavor::LlamaCpp => "llamacpp",
        }
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn stream(&self, req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError> {
        let url = self.endpoint();
        let body = Self::build_body(req, self.flavor);

        let mut call = self.http.post(&url).json(&body);
        if let Some(k) = &self.api_key {
            call = call.bearer_auth(k);
        }

        let started = std::time::Instant::now();

        let resp = call.send().await.map_err(ProviderError::Transport)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                url,
                body: body.chars().take(2000).collect(),
            });
        }

        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut tools = ToolAccum::default();
        let mut usage: Option<Usage> = None;
        let mut timings: Option<Timings> = None;
        let mut finish: Option<String> = None;
        let mut ttft_ms: Option<u64> = None;
        let mut saw_done = false;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(ProviderError::Transport)?;
            for ev in parser.feed(&bytes) {
                if ev.data == DONE {
                    saw_done = true;
                    continue;
                }
                let c: Chunk = serde_json::from_str(&ev.data)?;

                if let Some(u) = c.usage {
                    usage = Some(u);
                }
                if let Some(t) = c.timings {
                    timings = Some(t);
                }

                for ch in c.choices {
                    if let Some(fr) = ch.finish_reason {
                        finish = Some(fr);
                    }
                    let Some(d) = ch.delta else { continue };

                    if let Some(r) = d.reasoning_content {
                        if !r.is_empty() {
                            ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                            let _ = sink.send(Delta::Reasoning(r));
                        }
                    }
                    if let Some(t) = d.content {
                        if !t.is_empty() {
                            ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                            let _ = sink.send(Delta::Text(t));
                        }
                    }
                    if let Some(tcs) = d.tool_calls {
                        for tc in tcs {
                            ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as u64);
                            tools.merge(tc, sink);
                        }
                    }
                }
            }
            if saw_done {
                break;
            }
        }
        for ev in parser.finish() {
            if ev.data == DONE {
                continue;
            }
            if let Ok(c) = serde_json::from_str::<Chunk>(&ev.data) {
                if let Some(u) = c.usage {
                    usage.get_or_insert(u);
                }
                if let Some(t) = c.timings {
                    timings.get_or_insert(t);
                }
            }
        }

        tools.finish(sink);

        let mut tel = TurnTelemetry::default();
        if let Some(u) = &usage {
            tel.prompt_tokens = u.prompt_tokens;
            tel.completion_tokens = u.completion_tokens;
            tel.cached_tokens =
                u.prompt_tokens_details.as_ref().map_or(0, |d| d.cached_tokens);
        }
        if let Some(t) = &timings {
            // Prefer timings.cache_n for cache hits when usage omits details.
            if tel.cached_tokens == 0 {
                tel.cached_tokens = t.cache_n;
            }
            if tel.prompt_tokens == 0 {
                tel.prompt_tokens = t.prompt_n;
            }
            if tel.completion_tokens == 0 {
                tel.completion_tokens = t.predicted_n;
            }
            tel.server_prefill_tps = t.prompt_per_second;
            tel.server_decode_tps = t.predicted_per_second;
            // Older llama.cpp builds omit the *_per_second fields; derive them
            // from the raw counters rather than showing a blank.
            if tel.server_prefill_tps == 0.0 && t.prompt_ms > 0.0 {
                tel.server_prefill_tps = t.prompt_n as f64 / (t.prompt_ms / 1000.0);
            }
            if tel.server_decode_tps == 0.0 && t.predicted_ms > 0.0 {
                tel.server_decode_tps = t.predicted_n as f64 / (t.predicted_ms / 1000.0);
            }
            tel.draft_n = t.draft_n;
            tel.draft_accepted = t.draft_n_accepted;
        }
        tel.ttft_ms = ttft_ms.unwrap_or(0);
        let _ = sink.send(Delta::Telemetry(tel));

        let reason = match finish.as_deref() {
            Some(f) => map_finish(f),
            // No finish_reason but the stream closed cleanly: treat as stop
            // rather than erroring. Small local servers do this.
            None => FinishReason::Stop,
        };
        let _ = sink.send(Delta::Done(reason));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Role, ToolCall, ToolDef};

    fn body_of(req: &ChatRequest) -> Value {
        OpenAiProvider::build_body(req, Flavor::LlamaCpp)
    }

    #[test]
    fn thinking_off_sets_enable_thinking_false() {
        let mut r = ChatRequest::new("m", vec![Message::user("hi")]);
        r.reasoning = Reasoning::Off;
        assert_eq!(body_of(&r)["chat_template_kwargs"]["enable_thinking"], json!(false));
        r.reasoning = Reasoning::High;
        assert_eq!(body_of(&r)["chat_template_kwargs"]["enable_thinking"], json!(true));
    }

    #[test]
    fn usage_and_stream_options_always_present() {
        let r = ChatRequest::new("m", vec![Message::user("hi")]);
        let b = body_of(&r);
        assert_eq!(b["stream"], json!(true));
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
    }

    #[test]
    fn tool_arguments_are_a_string_not_an_object() {
        let m = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "exec".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }],
            ..Default::default()
        };
        let v = to_openai_message(&m);
        assert_eq!(v["tool_calls"][0]["function"]["arguments"], json!("{\"cmd\":\"ls\"}"));
        assert!(v["tool_calls"][0]["function"]["arguments"].is_string());
    }

    /// Byte-stability of the request body is what makes llama.cpp prefix caching
    /// work. Same messages must serialize to identical bytes, every time.
    #[test]
    fn body_serialization_is_byte_stable() {
        let mk = || {
            let mut r = ChatRequest::new("m", vec![
                Message::system("sys"),
                Message::user("q1"),
                Message {
                    role: Role::Assistant,
                    content: "a1".into(),
                    tool_calls: vec![
                        ToolCall { id: "b".into(), name: "exec".into(), arguments: "{\"cmd\":\"b\"}".into() },
                        ToolCall { id: "a".into(), name: "read_file".into(), arguments: "{\"p\":\"x\"}".into() },
                    ],
                    ..Default::default()
                },
                Message::tool_result("a", "out"),
            ]);
            r.tools = vec![
                ToolDef { name: "z".into(), description: "d".into(), parameters: json!({"type":"object","properties":{"a":{"type":"string"}}}) },
                ToolDef { name: "a".into(), description: "d".into(), parameters: json!({"type":"object","properties":{"z":{"type":"string"}}}) },
            ];
            r
        };
        let a = serde_json::to_string(&body_of(&mk())).unwrap();
        for _ in 0..32 {
            assert_eq!(serde_json::to_string(&body_of(&mk())).unwrap(), a, "body bytes drifted");
        }
    }

    /// The append-only invariant: extending the conversation must leave the
    /// existing prefix byte-identical, so the server's prefix cache still hits.
    #[test]
    fn extending_a_conversation_preserves_the_byte_prefix() {
        let base = vec![Message::system("sys"), Message::user("q1")];
        let mut r = ChatRequest::new("m", base);
        let msgs_prefix = serde_json::to_string(&body_of(&r)["messages"]).unwrap();

        r.messages.push(Message::assistant("a1"));
        r.messages.push(Message::user("q2"));
        let msgs_grown = serde_json::to_string(&body_of(&r)["messages"]).unwrap();

        // Compare without the array's own closing bracket: the invariant is that
        // every already-sent message keeps its exact bytes.
        let p = msgs_prefix
            .strip_suffix(']')
            .expect("messages array");
        assert!(
            msgs_grown.starts_with(p),
            "existing message bytes changed; llama.cpp prefix cache would miss"
        );
    }

    #[test]
    fn parses_live_llama_cpp_final_chunk() {
        // Captured verbatim from the live llama.cpp server on this box.
        let raw = r#"{"id": "x", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 17, "completion_tokens": 3, "total_tokens": 20, "prompt_tokens_details": {"cached_tokens": 0}}, "timings": {"cache_n": 0, "prompt_n": 17, "prompt_ms": 379.1, "prompt_per_token_ms": 22.3, "prompt_per_second": 44.8, "predicted_n": 3, "predicted_ms": 113.6, "predicted_per_token_ms": 37.867, "predicted_per_second": 26.4, "draft_n": 3, "draft_n_accepted": 3}}"#;
        let c: Chunk = serde_json::from_str(raw).unwrap();
        let t = c.timings.unwrap();
        assert_eq!(t.draft_n, 3);
        assert_eq!(t.draft_n_accepted, 3);
        assert!((t.predicted_per_second - 26.4).abs() < 1e-6);
        assert_eq!(c.usage.unwrap().prompt_tokens, 17);
    }

    #[test]
    fn parses_fragmented_tool_call_stream() {
        let frames = [
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"The"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"exec","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"echo hi\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tools = ToolAccum::default();
        for f in frames {
            let c: Chunk = serde_json::from_str(f).unwrap();
            for ch in c.choices {
                if let Some(d) = ch.delta {
                    if let Some(tcs) = d.tool_calls {
                        for tc in tcs {
                            tools.merge(tc, &tx);
                        }
                    }
                }
            }
        }
        tools.finish(&tx);

        let mut begin = None;
        let mut args = String::new();
        let mut ready = None;
        while let Ok(d) = rx.try_recv() {
            match d {
                Delta::ToolCallBegin { id, name } => begin = Some((id, name)),
                Delta::ToolCallArgs(a) => args.push_str(&a),
                Delta::ToolCallReady { id, name, arguments } => {
                    ready = Some((id, name, arguments))
                }
                _ => {}
            }
        }
        assert_eq!(begin.unwrap(), ("call_1".to_string(), "exec".to_string()));
        assert_eq!(ready.unwrap().2, "{\"cmd\":\"echo hi\"}");
        assert!(!args.is_empty());
    }
}
