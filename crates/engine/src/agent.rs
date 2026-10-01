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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::approval::{ApprovalGate, GateOutcome};
use crate::event::{EngineEvent, FinishReason};
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
    provider: Box<dyn Provider>,
    registry: Registry,
    prompt: PromptBuilder,
    gate: ApprovalGate,
    cfg: AgentConfig,
    ctx: ToolCtx,
    turn_id: u64,
    cancel: Arc<AtomicBool>,
}

impl Agent {
    pub fn new(
        provider: Box<dyn Provider>,
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
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn prompt_mut(&mut self) -> &mut PromptBuilder {
        &mut self.prompt
    }

    pub fn set_mode(&mut self, m: crate::approval::ApprovalMode) {
        self.gate.set_mode(m);
    }

    /// For the composer picker's current-state label.
    pub fn mode(&self) -> crate::approval::ApprovalMode {
        self.gate.mode()
    }

    /// Handle for cancelling from another thread (the UI's Stop button).
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Run one user turn to completion. Returns aggregated telemetry.
    pub async fn run_turn(&mut self, user_text: &str, out: &EngineSink) -> TurnTelemetry {
        self.turn_id += 1;
        let turn = self.turn_id;
        self.cancel.store(false, Ordering::Relaxed);

        let _ = out.send(EngineEvent::TurnStart {
            turn_id: turn,
            model: self.cfg.model.clone(),
        });

        self.prompt.push_user(user_text);

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

        for iter in 0..self.cfg.max_tool_iters {
            if self.cancel.load(Ordering::Relaxed) {
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
                self.prompt.push_assistant(&collected.text);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Stop });
                return agg;
            }

            // The model asked for tools. Record the assistant turn *with* its
            // calls, then answer every one of them.
            self.prompt.push_assistant_calls(&collected.text, &collected.calls);

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

        let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Length });
        agg
    }

    /// One provider round-trip. Streams deltas out and collects the tool calls.
    async fn stream_once(&mut self, req: &ChatRequest, out: &EngineSink) -> Result<Collected, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Delta>();

        // The consumer runs as a task, not inline. `Provider::stream` borrows
        // `tx`, so the channel cannot close while that call is live, and an
        // inline `while rx.recv()` would block forever once the provider returns
        // (this deadlocked every agent test the first time it was written).
        // Dropping `tx` after the call is what ends the loop.
        let out2 = out.clone();
        let consume = tokio::spawn(async move {
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut tel = TurnTelemetry::default();
            let mut finish = FinishReason::Stop;
            while let Some(d) = rx.recv().await {
                match d {
                    Delta::Text(t) => {
                        text.push_str(&t);
                        let _ = out2.send(EngineEvent::Text(t));
                    }
                    Delta::Reasoning(r) => {
                        let _ = out2.send(EngineEvent::Reasoning(r));
                    }
                    Delta::ToolCallBegin { id, name } => {
                        let _ = out2.send(EngineEvent::ToolCallBegin { id, name });
                    }
                    Delta::ToolCallArgs(a) => {
                        let _ = out2.send(EngineEvent::ToolCallArgs(a));
                    }
                    Delta::ReasoningEnd { .. } => {}
                    Delta::ToolCallReady { id, name, arguments } => {
                        calls.push(ToolCall { id, name, arguments });
                    }
                    Delta::Telemetry(t) => tel = t,
                    Delta::Done(f) => finish = f,
                }
            }
            (text, calls, tel, finish)
        });

        let res = self.provider.stream(req, &tx).await;
        drop(tx);

        let (text, calls, tel, finish) = consume
            .await
            .map_err(|e| format!("event consumer task failed: {e}"))?;
        if let Err(e) = res {
            return Err(format!("provider error: {e}"));
        }

        let _ = out.send(EngineEvent::Usage(tel.clone()));
        Ok(Collected { text, calls, telemetry: tel, finish })
    }

    /// Repair args, gate, execute, and always produce a tool_result.
    async fn run_tool(&mut self, call: &ToolCall, out: &EngineSink) {
        let Some(tool) = self.registry.get(&call.name) else {
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
        let bridge = tokio::spawn({
            let out = out.clone();
            let id = call.id.clone();
            async move {
                while let Some(c) = erx.recv().await {
                    let _ = out.send(EngineEvent::ToolExecOutput {
                        id: id.clone(),
                        stream: c.stream,
                        chunk: c.chunk,
                    });
                }
            }
        });

        let result = tool.run(args.clone(), &self.ctx, &etchunk).await;
        drop(etchunk);
        let _ = bridge.await;

        let _ = out.send(EngineEvent::ToolExecEnd {
            id: call.id.clone(),
            ok: result.ok,
            exit_code: result.exit_code,
        });

        self.prompt.push_tool_result(&call.id, render_result(&result));
    }
}

struct Collected {
    text: String,
    calls: Vec<ToolCall>,
    telemetry: TurnTelemetry,
    #[allow(dead_code)]
    finish: FinishReason,
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
    use serde_json::json;

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
            Box::new(Scripted { turns, n: std::sync::atomic::AtomicUsize::new(0) }),
            reg,
            gate,
            AgentConfig::default(),
            ToolCtx::default(),
        );
        let _ = answer;
        (agent, erx, arx)
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
        agent.run_turn("go", &tx).await;
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
        agent.run_turn("go", &tx).await;
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
        agent.run_turn("go", &tx).await;
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
        agent.run_turn("go", &tx).await;
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
        agent.run_turn("go", &tx).await;
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
        let r = ToolResult { ok: false, output: "".into(), exit_code: Some(2), truncated: false, error: None };
        assert!(render_result(&r).contains("(no output)"));
    }

    #[test]
    fn risk_of_exec_is_exec() {
        use crate::tools::{Risk, Tool};
        let t = crate::tools::exec::ExecTool::default();
        assert_eq!(t.risk(), Risk::Exec);
    }
}
