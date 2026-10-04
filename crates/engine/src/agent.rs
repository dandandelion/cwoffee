//! The agent loop: provider → tool calls → approval → execute → feed back → repeat.
//!
//! This is the only place that knows the ordering rules. The front end must be
//! able to render `EngineEvent` passively and never drive the loop.
//!
//! Two invariants worth stating because violating them is how agent harnesses
//! become unreliable:
//!
//!   * **Every tool call gets a tool_result**, including denied and failed ones.
//!     Providers reject an assistant turn with tool_calls that has no matching
//!     results, and a model that never learns a call was refused will keep
//!     retrying it forever.
//!   * **The loop is bounded.** A small model can call a tool, misread the
//!     output, and call it again identically. `max_tool_iters` is a hard stop,
//!     reported as such rather than silently continuing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, GateOutcome};
use crate::event::{EngineEvent, ExecStream, FinishReason};
use crate::prompt::PromptBuilder;
use crate::provider::{ChatRequest, Delta, Provider, Reasoning, ToolCall};
use crate::telemetry::TurnTelemetry;
use crate::tools::repair::{self, RepairError};
use crate::tools::{ExecChunk, Registry, ToolCtx, ToolResult};

/// Where engine events go. The desktop app forwards these over Tauri IPC; the
/// TUI renders them directly.
pub type EngineSink = mpsc::UnboundedSender<EngineEvent>;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// Hard cap on provider round-trips inside one user turn.
    pub max_tool_iters: usize,
    pub reasoning: Reasoning,
    pub max_tokens: Option<u32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "default".into(),
            max_tool_iters: 8,
            reasoning: Reasoning::Low,
            max_tokens: Some(2048),
        }
    }
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Registry,
    prompt: PromptBuilder,
    gate: ApprovalGate,
    cfg: AgentConfig,
    ctx: ToolCtx,
    turn_id: u64,
    cancel: Arc<AtomicBool>,
    /// Files produced by tools during the current turn. UI metadata only: these
    /// never enter the prompt, so they cost no prefill and cannot invalidate the
    /// prefix cache.
    turn_files: Vec<crate::tools::OutputFile>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Registry,
        gate: ApprovalGate,
        cfg: AgentConfig,
        ctx: ToolCtx,
    ) -> Self {
        Self {
            provider,
            registry,
            prompt: PromptBuilder::new(),
            gate,
            cfg,
            ctx,
            turn_id: 0,
            turn_files: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn prompt_mut(&mut self) -> &mut PromptBuilder {
        &mut self.prompt
    }

    /// Point the tools at a different workspace.
    ///
    /// Only safe between turns: a running tool already holds the old `ToolCtx`,
    /// and the sandbox bind is fixed for the lifetime of that process.
    pub fn set_workspace(&mut self, path: std::path::PathBuf) {
        self.ctx.workspace = path
    }

    pub fn workspace(&self) -> &std::path::Path {
        &self.ctx.workspace
    }

    /// Swap the whole prompt out, e.g. when moving to a different session.
    pub fn replace_prompt(&mut self, p: crate::prompt::PromptBuilder) {
        self.prompt = p;
        self.turn_id = 0;
    }

    /// Turn numbering is per conversation, not per process. Without this a
    /// brand-new session opened at "turn 4" because the counter belonged to the
    /// Agent, which outlives every session switch.
    pub fn reset_turn_id(&mut self) {
        self.turn_id = 0;
    }

    /// Files the last turn produced. The host persists these onto the assistant
    /// row so they survive a reload; the event is only the live path.
    pub fn take_turn_files(&mut self) -> Vec<crate::tools::OutputFile> {
        std::mem::take(&mut self.turn_files)
    }

    fn emit_turn_files(&mut self, turn: u64, out: &EngineSink) {
        if self.turn_files.is_empty() {
            return;
        }
        let _ = out.send(EngineEvent::TurnFiles {
            turn_id: turn,
            files: self.turn_files.clone(),
        });
    }

    pub fn set_mode(&mut self, m: crate::approval::ApprovalMode) {
        self.gate.set_mode(m);
    }

    /// For the composer picker's current-state label.
    pub fn mode(&self) -> crate::approval::ApprovalMode {
        self.gate.mode()
    }

    /// Switching model mid-session is allowed: the transcript is provider
    /// agnostic. It does invalidate the server-side prefix cache, since the
    /// new model tokenises the same bytes differently.
    pub fn set_model(&mut self, m: String) {
        self.cfg.model = m;
    }

    pub fn model(&self) -> String {
        self.cfg.model.clone()
    }

    pub fn set_reasoning(&mut self, r: crate::provider::Reasoning) {
        self.cfg.reasoning = r;
    }

    /// Handle for cancelling from another thread (the UI's Stop button).
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Run one user turn to completion. Returns aggregated telemetry.
    pub async fn run_turn(
        &mut self,
        user_text: &str,
        attachments: Vec<crate::provider::Attachment>,
        out: &EngineSink,
    ) -> TurnTelemetry {
        self.turn_id += 1;
        let turn = self.turn_id;
        self.cancel.store(false, Ordering::Relaxed);

        let _ = out.send(EngineEvent::TurnStart {
            turn_id: turn,
            model: self.cfg.model.clone(),
        });

        self.prompt.push_user_with(user_text, attachments);

        let defs = self.registry.defs();
        let schemas = serde_json::to_string(&defs).unwrap_or_default();
        let dropped = self.prompt.compact_to_budget(&schemas);
        if dropped > 0 {
            // Say so: compaction is a visible latency spike (the prefix cache is
            // thrown away) and an unexplained one looks like a bug.
            let _ = out.send(EngineEvent::Error {
                message: format!(
                    "context full: dropped {dropped} oldest message(s) \
                     (compaction #{}); the next prefill will be slow",
                    self.prompt.compactions
                ),
                fatal: false,
            });
        }

        let mut agg = TurnTelemetry::default();
        self.turn_files.clear();

        for iter in 0..self.cfg.max_tool_iters {
            if self.cancel.load(Ordering::Relaxed) {
                self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Cancelled });
                return agg;
            }

            let mut req = self.prompt.build(&self.cfg.model);
            req.tools = defs.clone();
            req.reasoning = self.cfg.reasoning;
            req.max_tokens = self.cfg.max_tokens;

            let collected = match self.stream_once(&req, out).await {
                Ok(c) => c,
                Err(msg) => {
                    let _ = out.send(EngineEvent::Error { message: msg, fatal: true });
                    self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Error });
                    return agg;
                }
            };

            agg.merge_server(&collected.telemetry);

            if collected.text.is_empty() && collected.calls.is_empty() {
                let _ = out.send(EngineEvent::Error {
                    message: "model returned an empty turn".into(),
                    fatal: false,
                });
            }

            if collected.calls.is_empty() {
                self.prompt.push_assistant_with(
                    &collected.text,
                    reason(&collected),
                    collected.reasoning_ms,
                );
                self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Stop });
                return agg;
            }

            // The model asked for tools. Record the assistant turn *with* its
            // calls, then answer every one of them.
            self.prompt.push_assistant_calls_with(
                &collected.text,
                &collected.calls,
                reason(&collected),
                collected.reasoning_ms,
            );

            for call in &collected.calls {
                self.run_tool(call, out).await;
            }

            if iter + 1 == self.cfg.max_tool_iters {
                let _ = out.send(EngineEvent::Error {
                    message: format!(
                        "stopped after {} tool iterations without a final answer",
                        self.cfg.max_tool_iters
                    ),
                    fatal: false,
                });
            }
        }

        self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Length });
        agg
    }

    /// One provider round-trip. Streams deltas out and collects the tool calls.
    /// Resolves once `flag` is set. Polled rather than signalled: the flag is also
    /// read synchronously by the tool loop, and a channel would not be.
    async fn wait_cancel(flag: Arc<AtomicBool>) {
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn stream_once(&mut self, req: &ChatRequest, out: &EngineSink) -> Result<Collected, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Delta>();

        // The consumer runs as a task, not inline. `Provider::stream` borrows
        // `tx`, so the channel cannot close while that call is live, and an
        // inline `while rx.recv()` would block forever once the provider returns
        // (this deadlocked every agent test the first time it was written).
        // Dropping `tx` after the call is what ends the loop.
        let out2 = out.clone();
        let cancel_consume = Arc::clone(&self.cancel);
        let consume = tokio::spawn(async move {
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut tel = TurnTelemetry::default();
            let mut finish = FinishReason::Stop;
            // Thinking duration is measured here, at the deltas, because this
            // is the only place that sees when reasoning actually started and
            // stopped. A UI-side timer would be wrong across a reload or a
            // session switch mid-turn.
            let mut think: String = String::new();
            let mut think_start: Option<Instant> = None;
            let mut think_end: Option<Instant> = None;
            while let Some(d) = rx.recv().await {
                // Stop forwarding as soon as the user asks to stop: otherwise the
                // transcript keeps filling in for the rest of the generation.
                if cancel_consume.load(Ordering::Relaxed) {
                    break;
                }
                match d {
                    Delta::Text(t) => {
                        text.push_str(&t);
                        let _ = out2.send(EngineEvent::Text(t));
                    }
                    Delta::Reasoning(r) => {
                        think_start.get_or_insert_with(Instant::now);
                        think_end = Some(Instant::now());
                        think.push_str(&r);
                        let _ = out2.send(EngineEvent::Reasoning(r));
                    }
                    Delta::ToolCallBegin { id, name } => {
                        let _ = out2.send(EngineEvent::ToolCallBegin { id, name });
                    }
                    Delta::ToolCallArgs(a) => {
                        let _ = out2.send(EngineEvent::ToolCallArgs(a));
                    }
                    Delta::ReasoningEnd { .. } => {
                        // Some providers close the thinking block explicitly.
                        // Trust that over the last delta's arrival time.
                        if think_start.is_some() {
                            think_end = Some(Instant::now());
                        }
                    }
                    Delta::ToolCallReady { id, name, arguments } => {
                        calls.push(ToolCall { id, name, arguments });
                    }
                    Delta::Telemetry(t) => tel = t,
                    Delta::Done(f) => finish = f,
                }
            }
            let reasoning_ms = match (think_start, think_end) {
                (Some(s), Some(e)) => Some(e.duration_since(s).as_millis() as u64),
                _ => None,
            };
            (text, calls, tel, finish, think, reasoning_ms)
        });

        // A Stop press has to interrupt the generation, not just the gap between
        // tool iterations: a single streamed answer can run for a minute. Polling
        // the flag is cheap and avoids threading a notification channel through
        // every provider.
        let cancel_wait = Arc::clone(&self.cancel);
        let mut cancelled = false;
        let res = tokio::select! {
            biased;
            _ = Self::wait_cancel(cancel_wait) => {
                cancelled = true;
                Ok(())
            }
            r = self.provider.stream(req, &tx) => r,
        };
        // Dropping tx is what ends the consumer loop, cancelled or not.
        drop(tx);

        let (text, calls, tel, finish, think, reasoning_ms) = consume
            .await
            .map_err(|e| format!("event consumer task failed: {e}"))?;
        if cancelled {
            return Ok(Collected {
                text,
                calls,
                telemetry: tel,
                finish: FinishReason::Cancelled,
                reasoning: think,
                reasoning_ms,
            });
        }
        if let Err(e) = res {
            return Err(format!("provider error: {e}"));
        }

        let _ = out.send(EngineEvent::Usage(tel.clone()));
        Ok(Collected { text, calls, telemetry: tel, finish, reasoning: think, reasoning_ms })
    }

    /// Repair args, gate, execute, and always produce a tool_result.
    async fn run_tool(&mut self, call: &ToolCall, out: &EngineSink) {
        let Some(tool) = self.registry.get_shared(&call.name) else {
            let msg = format!(
                "unknown tool {:?}. Available: {}",
                call.name,
                self.registry.defs().iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
            );
            let _ = out.send(EngineEvent::Error { message: msg.clone(), fatal: false });
            self.prompt.push_tool_result(&call.id, format!("ERROR: {msg}"));
            return;
        };

        // Repair before gating: the approval prompt must show the *parsed*
        // command, and a gate that cannot read the arguments cannot judge it.
        let args = match repair::parse_or_repair(&call.arguments) {
            Ok(o) => {
                if o.repaired {
                    let _ = out.send(EngineEvent::Error {
                        message: format!(
                            "repaired malformed arguments for {}: {}",
                            call.name,
                            o.notes.join("; ")
                        ),
                        fatal: false,
                    });
                }
                o.value
            }
            Err(RepairError::Unrepairable { input, notes }) => {
                let _ = out.send(EngineEvent::ToolCallParseError {
                    id: call.id.clone(),
                    raw: input.clone(),
                    message: notes.join("; "),
                });
                // Tell the model its JSON was bad. This is the single most
                // effective fix for small-model tool loops: without the error it
                // repeats the identical malformed call forever.
                self.prompt.push_tool_result(
                    &call.id,
                    format!(
                        "ERROR: your tool arguments were not valid JSON and could not be repaired.\n\
                         Raw: {input}\n\
                         Re-emit the call with a single valid JSON object matching the schema."
                    ),
                );
                return;
            }
        };

        let _ = out.send(EngineEvent::ToolCallReady {
            id: call.id.clone(),
            args: args.clone(),
        });

        let risk = tool.risk();
        match self.gate.check(&call.name, risk, &args).await {
            GateOutcome::Deny { reason } => {
                let _ = out.send(EngineEvent::ToolExecEnd {
                    id: call.id.clone(),
                    ok: false,
                    exit_code: None,
                });
                self.prompt.push_tool_result(
                    &call.id,
                    format!("DENIED BY USER ({reason}). Do not retry this call."),
                );
                return;
            }
            GateOutcome::Allow => {}
            GateOutcome::Ask => unreachable!("check() resolves Ask by prompting"),
        }

        let _ = out.send(EngineEvent::ToolExecStart { id: call.id.clone() });

        // Bridge the tool's output channel onto the engine event stream. Same
        // rule as stream_once: the task owns the receiver, and we drop the
        // sender once the tool returns, or the bridge never finishes.
        let (etchunk, mut erx) = mpsc::unbounded_channel::<ExecChunk>();
        // Did this tool stream anything of its own? exec does; web_fetch,
        // web_search, read_file and friends do not - they only return a result.
        // Without this, those cards stay blank until the session is reopened and
        // the stored tool message is rendered from the store.
        let streamed = Arc::new(AtomicUsize::new(0));
        let bridge = tokio::spawn({
            let out = out.clone();
            let id = call.id.clone();
            let streamed = Arc::clone(&streamed);
            async move {
                while let Some(c) = erx.recv().await {
                    streamed.fetch_add(1, Ordering::Relaxed);
                    let _ = out.send(EngineEvent::ToolExecOutput {
                        id: id.clone(),
                        stream: c.stream,
                        chunk: c.chunk,
                    });
                }
            }
        });

        // The tool runs on its own task so a panic inside it becomes a
        // tool_result the model can see rather than a dead app. That is not
        // hypothetical: a byte-index slice in the HTML stripper walked into a
        // multi-byte character and closed the whole process mid-session.
        let running = tokio::spawn({
            let tool = tool.clone();
            let ctx = self.ctx.clone();
            let sink = etchunk.clone();
            let args = args.clone();
            async move { tool.run(args, &ctx, &sink).await }
        });
        let result = match running.await {
            Ok(r) => r,
            Err(e) => ToolResult {
                ok: false,
                output: String::new(),
                exit_code: None,
                truncated: false,
                files: Vec::new(),
                error: Some(if e.is_panic() {
                    format!(
                        "tool '{}' panicked and was isolated; the session continues",
                        call.name
                    )
                } else {
                    format!("tool '{}' task failed: {e}", call.name)
                }),
            },
        };
        drop(etchunk);
        let _ = bridge.await;

        // A tool that streamed nothing still has to show its outcome. Sent after
        // the bridge is joined so the ordering on the event channel is: chunks,
        // then this, then ToolExecEnd.
        if streamed.load(Ordering::Relaxed) == 0 {
            let shown = render_result(&result);
            if !shown.is_empty() {
                let _ = out.send(EngineEvent::ToolExecOutput {
                    id: call.id.clone(),
                    stream: ExecStream::Stdout,
                    chunk: shown,
                });
            }
        }

        if !result.files.is_empty() {
            self.turn_files.extend(result.files.iter().cloned());
        }

        let _ = out.send(EngineEvent::ToolExecEnd {
            id: call.id.clone(),
            ok: result.ok,
            exit_code: result.exit_code,
        });

        self.prompt.push_tool_result(&call.id, render_result(&result));
    }
}

/// Empty reasoning must not become `Some("")`: an empty thinking block replayed
/// to a provider that wants reasoning is a different prompt from no block at all.
fn reason(c: &Collected) -> Option<String> {
    if c.reasoning.is_empty() {
        None
    } else {
        Some(c.reasoning.clone())
    }
}

struct Collected {
    text: String,
    calls: Vec<ToolCall>,
    telemetry: TurnTelemetry,
    #[allow(dead_code)]
    finish: FinishReason,
    /// Raw thinking text from this round-trip, kept for the transcript.
    reasoning: String,
    /// Wall time the model spent thinking, when it thought at all.
    reasoning_ms: Option<u64>,
}

/// What the model sees for a tool result. Shaped for a small model: lead with
/// the status, keep it short, and never let an empty result be ambiguous.
fn render_result(r: &ToolResult) -> String {
    if let Some(e) = &r.error {
        return format!("ERROR (exit {:?}): {e}\n{}", r.exit_code.unwrap_or(-1), r.output);
    }
    if !r.ok {
        let out = if r.output.trim().is_empty() { "(no output)" } else { r.output.as_str() };
        return format!("FAILED exit={}\n{out}", r.exit_code.unwrap_or(-1));
    }
    if r.output.trim().is_empty() {
        return "(succeeded, no output)".to_string();
    }
    r.output.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalAnswer;
    use crate::provider::{DeltaSink, ProviderError};
    use async_trait::async_trait;

    /// Scripted provider: replays a fixed sequence of Deltas.
    struct Scripted {
        turns: Vec<Vec<Delta>>,
        n: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Provider for Scripted {
        fn id(&self) -> &str { "scripted" }
        fn label(&self) -> &str { "scripted" }
        async fn stream(&self, _req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError> {
            let i = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let seq = self.turns.get(i).cloned().unwrap_or_else(|| vec![
                Delta::Text("done".into()),
                Delta::Done(FinishReason::Stop),
            ]);
            for d in seq {
                sink.send(d).ok();
            }
            Ok(())
        }
        fn wants_reasoning(&self) -> bool { false }
    }

    fn harness(
        turns: Vec<Vec<Delta>>,
        answer: ApprovalAnswer,
    ) -> (
        Agent,
        mpsc::UnboundedReceiver<EngineEvent>,
        mpsc::UnboundedReceiver<(crate::approval::ApprovalRequest, tokio::sync::oneshot::Sender<ApprovalAnswer>)>,
    ) {
        let (_etx, erx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::<crate::tools::exec::ExecTool>::default());
        let gate = ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx);
        let agent = Agent::new(
            std::sync::Arc::new(Scripted { turns, n: std::sync::atomic::AtomicUsize::new(0) }),
            reg,
            gate,
            AgentConfig::default(),
            ToolCtx::default(),
        );
        let _ = answer;
        (agent, erx, arx)
    }

    /// Streams forever. Used to prove Stop interrupts a live generation rather
    /// than only being observed between tool iterations.
    struct Endless;

    #[async_trait]
    impl Provider for Endless {
        fn id(&self) -> &str { "endless" }
        fn label(&self) -> &str { "endless" }
        async fn stream(&self, _req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError> {
            for i in 0..1000u32 {
                if sink.send(Delta::Text(format!("tok{i} "))).is_err() {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Ok(())
        }
        fn wants_reasoning(&self) -> bool { false }
    }

    /// The transcript label says "thought for 2m34s". That number has to be
    /// measured where the deltas arrive, and stored, or it is gone on reload.
    #[tokio::test]
    async fn thinking_is_captured_with_its_duration() {
        let (etx, _erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(
            std::sync::Arc::new(Scripted {
                turns: vec![vec![
                    Delta::Reasoning("step one. ".into()),
                    Delta::Reasoning("step two.".into()),
                    Delta::ReasoningEnd { signature: None },
                    Delta::Text("42".into()),
                    Delta::Done(FinishReason::Stop),
                ]],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            Registry::new(),
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        agent.run_turn("q", vec![], &etx).await;

        let h = agent.prompt_mut().history();
        let a = h.iter().find(|m| matches!(m.role, crate::provider::Role::Assistant)).expect("assistant message");
        assert_eq!(a.reasoning.as_deref(), Some("step one. step two."));
        assert!(a.reasoning_ms.is_some(), "thinking duration was not measured");
    }

    /// A turn that never thinks must not produce an empty thinking block: an
    /// empty block replayed to a provider that wants reasoning is a different
    /// prompt from no block at all.
    #[tokio::test]
    async fn a_turn_without_thinking_stores_no_reasoning_block() {
        let (etx, _erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(
            std::sync::Arc::new(Scripted { turns: vec![], n: std::sync::atomic::AtomicUsize::new(0) }),
            Registry::new(),
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        agent.run_turn("q", vec![], &etx).await;
        let h = agent.prompt_mut().history();
        let a = h.iter().find(|m| matches!(m.role, crate::provider::Role::Assistant)).expect("assistant message");
        assert_eq!(a.reasoning, None);
        assert_eq!(a.reasoning_ms, None);
    }

    /// A tool that returns a result but streams nothing, which is what every
    /// non-exec tool does. The transcript rendered only streamed chunks, so a
    /// web_fetch card stayed blank until the session was closed and reopened -
    /// the result had been in the store the whole time.
    #[derive(Default)]
    struct Quiet;

    #[async_trait]
    impl crate::tools::Tool for Quiet {
        fn name(&self) -> &str { "quiet" }
        fn description(&self) -> &str { "returns output without streaming it" }
        fn schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::ReadOnly }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
            _sink: &crate::tools::ExecSink,
        ) -> crate::tools::ToolResult {
            ToolResult::ok("fetched: the price is 42")
        }
    }

    #[tokio::test]
    async fn a_non_streaming_tool_still_shows_its_result_live() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Quiet::default()));
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "q1".into(),
                            name: "quiet".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![
                        Delta::Text("done".into()),
                        Delta::Done(FinishReason::Stop),
                    ],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Auto, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        agent.run_turn("go", vec![], &etx).await;

        let mut seen: Vec<String> = Vec::new();
        while let Ok(ev) = erx.try_recv() {
            if let EngineEvent::ToolExecOutput { id, chunk, .. } = ev {
                seen.push(format!("{id}:{chunk}"));
            }
        }
        assert!(
            seen.iter().any(|s| s.contains("fetched: the price is 42")),
            "the result never reached the UI: {seen:?}"
        );
    }

    /// A tool that panics. Real example: the HTML stripper sliced a string at a
    /// non-char boundary and the panic took the whole app down mid-session.
    #[derive(Default)]
    struct Exploding;

    #[async_trait]
    impl crate::tools::Tool for Exploding {
        fn name(&self) -> &str { "explode" }
        fn description(&self) -> &str { "always panics" }
        fn schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::ReadOnly }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
            _sink: &crate::tools::ExecSink,
        ) -> crate::tools::ToolResult {
            panic!("boom from inside a tool")
        }
    }

    /// The panic must be contained: the model gets an error result, the turn
    /// finishes, and the process survives.
    #[tokio::test]
    async fn a_panicking_tool_is_isolated_not_fatal() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Exploding::default()));
        let mut agent = Agent::new(
            std::sync::Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "c1".into(),
                            name: "explode".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![
                        Delta::Text("recovered".into()),
                        Delta::Done(FinishReason::Stop),
                    ],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Auto, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        agent.run_turn("go", vec![], &etx).await;

        let h = agent.prompt_mut().history();
        let tr = h
            .iter()
            .find(|m| matches!(m.role, crate::provider::Role::Tool))
            .expect("the failed call still needs a tool_result");
        assert!(tr.content.contains("panicked"), "result was: {}", tr.content);

        let mut saw_end = false;
        while let Ok(e) = erx.try_recv() {
            if let EngineEvent::ToolExecEnd { ok, .. } = e {
                saw_end = true;
                assert!(!ok, "a panicked tool must not report ok");
            }
        }
        assert!(saw_end, "tool exec end was never emitted");
    }

    #[tokio::test]
    async fn cancel_interrupts_an_in_flight_stream() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(
            std::sync::Arc::new(Endless),
            Registry::new(),
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        let cancel = agent.cancel_handle();
        let run = tokio::spawn(async move { agent.run_turn("go", vec![], &etx).await });

        // Let a few tokens arrive, then stop.
        let mut got = 0usize;
        while let Some(e) = erx.recv().await {
            if matches!(e, EngineEvent::Text(_)) {
                got += 1;
                if got >= 3 {
                    break;
                }
            }
        }
        let t0 = std::time::Instant::now();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let _tel = run.await.unwrap();
        let dt = t0.elapsed();

        // 1000 tokens at 20ms is 20s. Returning in well under that is the proof.
        assert!(
            dt < std::time::Duration::from_millis(800),
            "cancel did not interrupt the stream: took {dt:?}"
        );
        assert!(got < 1000, "provider ran to completion despite cancel");
    }

    /// The invariant that keeps providers from 400ing: every call gets a result.
    #[tokio::test]
    async fn every_tool_call_receives_a_tool_result() {
        let turns = vec![
            vec![
                Delta::ToolCallBegin { id: "c1".into(), name: "exec".into() },
                Delta::ToolCallArgs("{\"cmd\":\"echo hi\"}".into()),
                Delta::ToolCallReady { id: "c1".into(), name: "exec".into(), arguments: "{\"cmd\":\"echo hi\"}".into() },
                Delta::Done(FinishReason::ToolCalls),
            ],
            vec![Delta::Text("final".into()), Delta::Done(FinishReason::Stop)],
        ];
        let (mut agent, _ev, mut arx) = harness(turns, ApprovalAnswer::Allow);
        tokio::spawn(async move {
            while let Some((_, reply)) = arx.recv().await {
                reply.send(ApprovalAnswer::Allow).ok();
            }
        });
        let (tx, rx) = mpsc::unbounded_channel();
        agent.run_turn("go", vec![], &tx).await;
        drop(tx);
        let mut rx = rx;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        let ready = events.iter().filter(|e| matches!(e, EngineEvent::ToolCallReady { .. })).count();
        let end = events.iter().filter(|e| matches!(e, EngineEvent::ToolExecEnd { .. })).count();
        assert_eq!(ready, end, "a tool call reached no terminal state");
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Text(t) if t == "final")));

        // and the prompt must contain a tool_result for c1
        let h = agent.prompt.history();
        assert!(h.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")));
    }

    /// A malformed call must produce an error result, not a silent skip.
    #[tokio::test]
    async fn malformed_args_feed_an_error_back_to_the_model() {
        let turns = vec![
            vec![
                Delta::ToolCallReady { id: "b1".into(), name: "exec".into(), arguments: "not json at all".into() },
                Delta::Done(FinishReason::ToolCalls),
            ],
            vec![Delta::Text("recovered".into()), Delta::Done(FinishReason::Stop)],
        ];
        let (mut agent, _ev, _arx) = harness(turns, ApprovalAnswer::Allow);
        let (tx, rx) = mpsc::unbounded_channel();
        agent.run_turn("go", vec![], &tx).await;
        drop(rx);
        let h = agent.prompt.history();
        let tr = h.iter().find(|m| m.tool_call_id.as_deref() == Some("b1")).expect("tool_result exists");
        assert!(tr.content.contains("not valid JSON"));
    }

    #[tokio::test]
    async fn denied_calls_are_answered_and_not_retried() {
        let turns = vec![
            vec![
                Delta::ToolCallReady { id: "d1".into(), name: "exec".into(), arguments: "{\"cmd\":\"rm a.txt\"}".into() },
                Delta::Done(FinishReason::ToolCalls),
            ],
            vec![Delta::Text("ok skipping".into()), Delta::Done(FinishReason::Stop)],
        ];
        let (mut agent, _ev, mut arx) = harness(turns, ApprovalAnswer::Deny);
        tokio::spawn(async move {
            while let Some((_, reply)) = arx.recv().await {
                reply.send(ApprovalAnswer::Deny).ok();
            }
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        agent.run_turn("go", vec![], &tx).await;
        let h = agent.prompt.history();
        let tr = h.iter().find(|m| m.tool_call_id.as_deref() == Some("d1")).expect("result exists");
        assert!(tr.content.contains("DENIED BY USER"));
        assert!(tr.content.contains("Do not retry"));
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_panicked() {
        let turns = vec![
            vec![
                Delta::ToolCallReady { id: "u1".into(), name: "teleport".into(), arguments: "{}".into() },
                Delta::Done(FinishReason::ToolCalls),
            ],
            vec![Delta::Text("sorry".into()), Delta::Done(FinishReason::Stop)],
        ];
        let (mut agent, _ev, _arx) = harness(turns, ApprovalAnswer::Allow);
        let (tx, _rx) = mpsc::unbounded_channel();
        agent.run_turn("go", vec![], &tx).await;
        let h = agent.prompt.history();
        let tr = h.iter().find(|m| m.tool_call_id.as_deref() == Some("u1")).expect("result exists");
        assert!(tr.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn loop_is_bounded() {
        // provider always asks for a tool -> must stop at max_tool_iters
        let loop_turn = vec![
            Delta::ToolCallReady { id: "x".into(), name: "exec".into(), arguments: "{\"cmd\":\"true\"}".into() },
            Delta::Done(FinishReason::ToolCalls),
        ];
        let turns = vec![loop_turn.clone(); 50];
        let (mut agent, _ev, mut arx) = harness(turns, ApprovalAnswer::Allow);
        tokio::spawn(async move {
            while let Some((_, reply)) = arx.recv().await {
                reply.send(ApprovalAnswer::Allow).ok();
            }
        });
        agent.cfg.max_tool_iters = 3;
        let (tx, rx) = mpsc::unbounded_channel();
        agent.run_turn("go", vec![], &tx).await;
        drop(tx);
        let mut rx = rx;
        let mut saw_cap = false;
        while let Ok(e) = rx.try_recv() {
            if let EngineEvent::Error { message, .. } = e {
                if message.contains("tool iterations") {
                    saw_cap = true;
                }
            }
        }
        assert!(saw_cap, "runaway loop was not bounded");
    }

    #[test]
    fn render_result_never_returns_empty_to_the_model() {
        assert_eq!(render_result(&ToolResult::ok("")), "(succeeded, no output)");
        assert!(render_result(&ToolResult::err("boom")).contains("ERROR"));
        let r = ToolResult { ok: false, output: "".into(), exit_code: Some(2), truncated: false, error: None, files: Vec::new() };
        assert!(render_result(&r).contains("(no output)"));
    }

    #[test]
    fn risk_of_exec_is_exec() {
        use crate::tools::{Risk, Tool};
        let t = crate::tools::exec::ExecTool::default();
        assert_eq!(t.risk(), Risk::Exec);
    }
}
