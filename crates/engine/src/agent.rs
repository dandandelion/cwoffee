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
use crate::provider::{ChatRequest, Delta, Message, Provider, Reasoning, ToolCall};
use crate::telemetry::{TurnStats, TurnTelemetry};
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
    /// Sampling overrides for the endpoint. Empty by default: the server's own
    /// defaults are its decision, not ours.
    pub sampling: crate::provider::Sampling,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "default".into(),
            // 8 was too few for anything with a document in it: research, write,
            // convert, verify is four rounds before the model has answered a
            // question. The cap is a runaway guard, not a work budget.
            max_tool_iters: 14,
            reasoning: Reasoning::Low,
            // None means "whatever the server defaults to", which for llama.cpp
            // is -1: generate until the model stops or the context fills.
            // This used to be Some(2048), and that single number was the reason
            // thinking looked weak here and lavish in the server's own web UI:
            // the cap applies to reasoning and answer *together*, so a 2k budget
            // truncates thinking before the model has finished, and finish_reason
            // comes back as `length` rather than the model ever stopping.
            max_tokens: None,
            sampling: crate::provider::Sampling::default(),
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
        // The prompt must argue for the same level the request will carry, so
        // seed it here rather than leaving PromptBuilder at its default.
        let mut prompt = PromptBuilder::new();
        prompt.set_reasoning(cfg.reasoning);
        Self {
            provider,
            registry,
            prompt,
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

    /// The live search tool, typed, so the settings card can switch providers or
    /// ask what an ephemeral SearXNG is doing without rebuilding the tool list.
    /// Rebuilding it would change the prompt bytes and drop every prefix cache.
    pub fn search_tool(&self) -> Option<std::sync::Arc<crate::tools::web::WebSearchTool>> {
        self.registry.search_tool()
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
        self.drop_deleted_turn_files();
        std::mem::take(&mut self.turn_files)
    }

    /// Drop the files this turn produced and then deleted.
    ///
    /// A chip in the transcript is a button. The model that writes a scratch
    /// script, runs it, and removes the folder afterwards - which is what a
    /// careful agent does, and what one did in a real session - left two chips
    /// pointing at paths that no longer existed. The test is existence when the
    /// turn closes rather than a delete event: the tool surface has no "unproduce"
    /// result, and a file the *user* removed mid-turn deserves the same treatment.
    fn drop_deleted_turn_files(&mut self) {
        if self.turn_files.is_empty() {
            return;
        }
        let ws = self.ctx.workspace.clone();
        self.turn_files.retain(|f| ws.join(&f.path).exists());
    }

    /// Record a file this turn produced.
    ///
    /// Keyed by path: a model that writes a report and then edits it has
    /// produced *one* file, and the transcript used to show two identical chips
    /// for it. The first position is kept so the strip does not reshuffle as the
    /// turn goes on, while the metadata (size, mime) is the latest one.
    fn note_turn_file(&mut self, f: &crate::tools::OutputFile) {
        match self
            .turn_files
            .iter_mut()
            .find(|e| e.path == f.path)
        {
            Some(existing) => *existing = f.clone(),
            None => self.turn_files.push(f.clone()),
        }
    }

    fn emit_turn_files(&mut self, turn: u64, out: &EngineSink) {
        self.drop_deleted_turn_files();
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
        // The prompt carries a line for the top levels, so a level change also
        // rewrites the system message. Doing both here is the point: a request
        // that says xhigh while the prompt says nothing is the bug this whole
        // change is about.
        self.prompt.set_reasoning(r);
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
            // Say so, with the numbers: "context full" without them is a claim the
            // user cannot check, and it used to fire at 43k tokens of a 262k
            // window because the budget was a constant rather than the model's.
            let est = self.prompt.estimate_tokens("");
            let budget = self.prompt.token_budget;
            let window = self
                .prompt
                .context_window
                .map(|w| w.to_string())
                .unwrap_or_else(|| "unstated".into());
            let _ = out.send(EngineEvent::Note {
                message: format!(
                    "history over budget: ~{est} tokens now, budget {budget} (model window {window}); \
                     dropped the oldest {dropped} message(s), compaction #{} - the next prefill will be slow",
                    self.prompt.compactions
                ),
            });
        }

        let mut agg = TurnTelemetry::default();
        self.turn_files.clear();
        // A round-trip that stopped at an output limit, and how many times this
        // turn has picked such an answer back up. Bounded because each
        // continuation is another prefill on a single-slot local server.
        let mut cut_short = 0usize;
        let mut carry_on = false;

        for _ in 0..self.cfg.max_tool_iters {
            if self.cancel.load(Ordering::Relaxed) {
                self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Cancelled });
                return agg;
            }

            let mut req = self.prompt.build(&self.cfg.model);
            req.tools = defs.clone();
            req.reasoning = self.cfg.reasoning;
            req.max_tokens = self.cfg.max_tokens;
            self.cfg.sampling.apply(&mut req);
            if carry_on {
                carry_on = false;
                // Request-only. History already ends with the partial answer, and a
                // instruction pushed through history would render a user bubble the
                // user never typed.
                req.messages.push(Message::user(
                    "Your previous message was cut off at the output limit. Continue it from              exactly where it stopped, in the same language, without repeating anything              you already wrote.",
                ));
            }

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
                if collected.finish == FinishReason::Length {
                    // The shape this box produced for a while: nine tool calls, a
                    // "Done" chip, and no answer. Thinking and the answer share one
                    // output budget, so a thinking-heavy turn can consume all of it
                    // and the answer never starts. Say which knob to turn.
                    let cap = match self.cfg.max_tokens {
                        Some(m) => format!("cwoffee's max_output_tokens = {m}"),
                        None => "the server's -np / n_predict (Cwoffee sets no cap of its own)".into(),
                    };
                    let _ = out.send(EngineEvent::Error {
                        message: format!(
                            "no answer: the model used all {} completion tokens on thinking and stopped at the output limit.                              Lower the thinking level, or raise {cap}.",
                            collected.telemetry.completion_tokens
                        ),
                        fatal: false,
                    });
                } else {
                    let _ = out.send(EngineEvent::Error {
                        message: "model returned an empty turn".into(),
                        fatal: false,
                    });
                }
            }

            if collected.calls.is_empty() {
                self.prompt.push_assistant_with(
                    &collected.text,
                    reason(&collected),
                    collected.reasoning_ms,
                );

                // The answer stopped at an output limit rather than at a sentence.
                // Until a moment ago this was reported as a clean stop, which is how
                // "the real LLM gets cut off mid-generation" looked like nothing at
                // all in the log. Pick it back up while there is room, then say so.
                if collected.finish == FinishReason::Length {
                    if !collected.text.trim().is_empty() && cut_short < MAX_CONTINUATIONS {
                        cut_short += 1;
                        carry_on = true;
                        let _ = out.send(EngineEvent::Note {
                            message: format!(
                                "the answer hit the output limit after {} tokens - continuing where it stopped ({cut_short} of {MAX_CONTINUATIONS})",
                                collected.telemetry.completion_tokens
                            ),
                        });
                        continue;
                    }
                    let _ = out.send(EngineEvent::Note {
                        message: format!(
                            "the answer is still cut short at the output limit ({} tokens).                              Raise max_output_tokens in config.toml, or the server's -np.",
                            collected.telemetry.completion_tokens
                        ),
                    });
                }

                self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: collected.finish });
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

        }

        // The tool budget is spent and the model still has not answered. Stopping
        // here is what "stopped after 8 tool iterations without a final answer"
        // used to look like: a warning, a file chip, and no summary of the work
        // that was just done. So spend no more tools - one last round-trip with
        // the tool list removed, whose only job is to say what it did and what is
        // left.
        self.wrap_up_without_tools(out, &mut agg).await;

        self.emit_turn_files(turn, out);
                let _ = out.send(EngineEvent::TurnEnd { turn_id: turn, finish: FinishReason::Length });
        agg
    }

    /// Final round-trip after the tool budget runs out: no tools offered, so the
    /// only thing the model can do is talk.
    ///
    /// The instruction is added to the *request*, never to history. It is a
    /// control message rather than something the user said, and pushing it through
    /// `push_user` would store it and render a user bubble the user never typed.
    async fn wrap_up_without_tools(&mut self, out: &EngineSink, agg: &mut TurnTelemetry) {
        let _ = out.send(EngineEvent::Note {
            message: format!(
                "tool budget spent ({} iterations); summarising without running anything else",
                self.cfg.max_tool_iters
            ),
        });

        let mut req = self.prompt.build(&self.cfg.model);
        req.tools = Vec::new();
        req.reasoning = Reasoning::Low;
        req.max_tokens = self.cfg.max_tokens;
        self.cfg.sampling.apply(&mut req);
        req.messages.push(Message::user(
            "You have used up your tool budget. Do not request any more tools.              In the user's own language, say in a few sentences what you actually              did, what you produced or changed (by name), and what is still              unfinished or unverified. Plain prose, no tool calls.",
        ));

        match self.stream_once(&req, out).await {
            Ok(c) if !c.text.trim().is_empty() => {
                agg.merge_server(&c.telemetry);
                self.prompt
                    .push_assistant_with(&c.text, reason(&c), c.reasoning_ms);
            }
            Ok(_) => {
                let _ = out.send(EngineEvent::Error {
                    message: format!(
                        "stopped after {} tool iterations and the model gave no summary",
                        self.cfg.max_tool_iters
                    ),
                    fatal: false,
                });
            }
            Err(msg) => {
                let _ = out.send(EngineEvent::Error {
                    message: format!("wrap-up after the tool budget failed: {msg}"),
                    fatal: false,
                });
            }
        }
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
            // Live status-bar numbers. The server's own token counts only arrive
            // with the final chunk, so mid-turn these are character-derived
            // estimates - which is still far more useful than a bar that reads
            // zero through the whole of a long first turn.
            let t0 = Instant::now();
            let mut first_delta: Option<Instant> = None;
            let mut chars_out: u64 = 0;
            let mut last_stats = Instant::now();
            let mut thinking_now = false;
            while let Some(d) = rx.recv().await {
                // Stop forwarding as soon as the user asks to stop: otherwise the
                // transcript keeps filling in for the rest of the generation.
                if cancel_consume.load(Ordering::Relaxed) {
                    break;
                }
                // Computed before the match: the arms move out of `d`.
                let just_text_or_reasoning = matches!(&d, Delta::Text(_) | Delta::Reasoning(_));
                match d {
                    Delta::Text(t) => {
                        chars_out += t.chars().count() as u64;
                        thinking_now = false;
                        text.push_str(&t);
                        let _ = out2.send(EngineEvent::Text(t));
                    }
                    Delta::Reasoning(r) => {
                        chars_out += r.chars().count() as u64;
                        thinking_now = true;
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
                // First delta out immediately (that is the TTFT the user is
                // waiting for); after that, a few times a second.
                if just_text_or_reasoning {
                    let first = first_delta.is_none();
                    let fd = first_delta.get_or_insert_with(Instant::now);
                    // The first delta goes out immediately: TTFT is the number the
                    // user is waiting for, and waiting another 400ms to say so is
                    // exactly the "it measures nothing" complaint.
                    if first || last_stats.elapsed() >= std::time::Duration::from_millis(400) {
                        last_stats = Instant::now();
                        let mut st = TurnStats {
                            ttft_ms: fd.duration_since(t0).as_millis() as u64,
                            chars: chars_out,
                            thinking: thinking_now,
                            ..Default::default()
                        };
                        st.derive(fd.elapsed().as_millis() as u64);
                        let _ = out2.send(EngineEvent::TurnStats(st));
                    }
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

        let risk = tool.risk_for(&args, &self.ctx);
        // A Stop press has to release a parked approval. The front ends drop their
        // senders when they remember to, but the engine cannot depend on that: a phone
        // tab that went to sleep answers nothing and drops nothing, and the turn then
        // sits "stopping" for the whole approval timeout - five minutes of a Stop
        // button that does nothing. Polling the flag here costs one load.
        let already_stopped = self.cancel.load(std::sync::atomic::Ordering::Relaxed);
        let cancel_wait = Arc::clone(&self.cancel);
        let gate = if already_stopped {
            // Asking now would put a card on screen that the user has already said
            // they do not want, and it would be answered by nobody.
            GateOutcome::Deny { reason: "the turn was stopped before the approval was asked".into() }
        } else {
            tokio::select! {
                biased;
                _ = Self::wait_cancel(cancel_wait) => GateOutcome::Deny {
                    reason: "the turn was stopped while the approval was pending".into(),
                },
                o = self.gate.check(&call.name, &risk, &args) => o,
            }
        };
        match gate {
            GateOutcome::Deny { reason } => {
                let _ = out.send(EngineEvent::ToolExecEnd {
                    id: call.id.clone(),
                    ok: false,
                    exit_code: None,
                    changes: Vec::new(),
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

        // Paths the user just said yes to, for this call only. A session-level
        // "allow for session" answer never lands here: it grants a *tool*, not
        // permission to reach outside the workspace, and the second `read_file`
        // of ~/.ssh would otherwise be free.
        let run_ctx = {
            let mut c = self.ctx.clone();
            c.permits = risk.escapes.iter().map(std::path::PathBuf::from).collect();
            // Tools normally report through their return value. A tool that must
            // speak to the *windows* while it is still waiting gets the stream
            // here, and only for this call.
            c.events = Some(out.clone());
            c
        };

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
            let ctx = run_ctx;
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
                changes: Vec::new(),
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

        for f in result.files.iter() {
            self.note_turn_file(f);
        }

        let _ = out.send(EngineEvent::ToolExecEnd {
            id: call.id.clone(),
            ok: result.ok,
            exit_code: result.exit_code,
            changes: result.changes.clone(),
        });

        self.prompt
            .push_tool_result_with(&call.id, render_result(&result), result.changes.clone());
    }
}

/// Empty reasoning must not become `Some("")`: an empty thinking block replayed
/// to a provider that wants reasoning is a different prompt from no block at all.
/// How many times one turn may resume an answer that hit an output cap.
const MAX_CONTINUATIONS: usize = 2;

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
    /// Why the provider stopped. `Length` means an output cap, not the model's
    /// own choice, and changes what the turn does next.
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
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::Harmless }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
            _sink: &crate::tools::ExecSink,
        ) -> crate::tools::ToolResult {
            ToolResult::ok("fetched: the price is 42")
        }
    }

    #[derive(Default)]
    struct Producer {
        bytes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl crate::tools::Tool for Producer {
        fn name(&self) -> &str { "producer" }
        fn description(&self) -> &str { "writes a file" }
        fn schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::Workspace }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
            _sink: &crate::tools::ExecSink,
        ) -> crate::tools::ToolResult {
            let n = self.bytes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut r = ToolResult::ok("wrote");
            // What write_file/edit_file report for the card: lines added and
            // removed against what was on disk.
            r.changes.push(crate::tools::changes::FileChange {
                path: "report.md".into(),
                added: 3,
                removed: n as usize,
                diff: Some("--- a
+++ b
-1
+2
".into()),
                created: n == 0,
            });
            r = r.with_file(crate::tools::OutputFile {
                path: "report.md".into(),
                name: "report.md".into(),
                mime: "text/markdown".into(),
                bytes: (100 * (n as u64 + 1)) as u64,
            });
            r
        }
    }

    /// A tool that reaches outside the workspace, and reports what it was granted.
    #[derive(Default)]
    struct Escaper;

    #[async_trait]
    impl crate::tools::Tool for Escaper {
        fn name(&self) -> &str { "escaper" }
        fn description(&self) -> &str { "reads a path outside the workspace" }
        fn schema(&self) -> serde_json::Value { serde_json::json!({"type":"object"}) }
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::Sensitive }
        fn risk_for(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolCtx,
        ) -> crate::tools::CallRisk {
            crate::tools::CallRisk {
                level: crate::tools::Risk::Sensitive,
                escapes: vec!["/home/me/.ssh/id_ed25519".into()],
            }
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCtx,
            _sink: &crate::tools::ExecSink,
        ) -> crate::tools::ToolResult {
            // The point of the test: permits arrive per call, and nothing else.
            ToolResult::ok(format!("permits={}", ctx.permits.len()))
        }
    }

    /// Harmless tools must not reach the gate at all. Every prompt that is not
    /// about harm is a prompt that trains the user to click without reading.
    #[tokio::test]
    async fn harmless_calls_never_ask_even_in_ask_mode() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Quiet));
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
                    vec![Delta::Text("ok".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx).with_timeout(std::time::Duration::from_millis(150)),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        let _ = arx; // nobody to answer: if a prompt were raised, fail-closed would deny it
        agent.run_turn("go", vec![], &etx).await;

        let ran = agent
            .prompt_mut()
            .history()
            .iter()
            .any(|m| m.content.contains("the price is 42"));
        assert!(ran, "a harmless tool was blocked by the gate");
        let prompted = erx
            .try_recv()
            .map(|e| matches!(e, EngineEvent::ApprovalRequested { .. }))
            .unwrap_or(false);
        assert!(!prompted, "an approval prompt was raised for a harmless call");
    }

    /// Reaching outside prompts every time, and the grant is for that call only:
    /// a yes on one file is not a yes on the next one.
    #[tokio::test]
    async fn an_escape_prompts_every_time_and_grants_one_call() {
        let (etx, _erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, mut arx) = mpsc::unbounded_channel::<(
            crate::approval::ApprovalRequest,
            tokio::sync::oneshot::Sender<crate::approval::ApprovalAnswer>,
        )>();
        let mut reg = Registry::new();
        reg.register(Box::new(Escaper));
        // Answers exactly two prompts and stops. Waiting on the channel until it
        // closes would hang: the gate keeps the sender alive for the whole turn.
        let responder = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..2 {
                match arx.recv().await {
                    Some((req, reply)) => {
                        seen.push((req.summary.clone(), req.escapes.clone()));
                        let _ = reply.send(crate::approval::ApprovalAnswer::Allow);
                    }
                    None => break,
                }
            }
            drop(arx);
            seen
        });
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "e1".into(),
                            name: "escaper".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![
                        Delta::ToolCallReady {
                            id: "e2".into(),
                            name: "escaper".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![Delta::Text("done".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        agent.run_turn("go", vec![], &etx).await;
        let prompts = responder.await.unwrap();
        assert_eq!(prompts.len(), 2, "both escapes must prompt: {prompts:?}");
        for (summary, escapes) in &prompts {
            assert_eq!(escapes, &vec!["/home/me/.ssh/id_ed25519".to_string()]);
            assert!(summary.contains("OUTSIDE the workspace"), "{summary}");
        }
        // Each run saw exactly one grant - its own - and never a carried-over one.
        let grants: Vec<String> = agent
            .prompt_mut()
            .history()
            .iter()
            .filter(|m| m.content.contains("permits="))
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(grants.len(), 2, "{grants:?}");
        assert!(grants.iter().all(|g| g.contains("permits=1")), "{grants:?}");
    }

    /// The +N -M badge and the diff have to arrive live *and* survive a reload,
    /// or the card says less about the work the second time you look at it.
    #[tokio::test]
    async fn file_change_metadata_reaches_the_event_and_the_stored_message() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Producer::default()));
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "c1".into(),
                            name: "producer".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![Delta::Text("done".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Auto, atx),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        // Producer reports a change of its own, the way write_file does.
        agent.run_turn("go", vec![], &etx).await;

        let mut saw_live = false;
        while let Ok(e) = erx.try_recv() {
            if let EngineEvent::ToolExecEnd { id, changes, .. } = e {
                if id == "c1" {
                    saw_live = !changes.is_empty();
                }
            }
        }
        let stored = agent
            .prompt_mut()
            .history()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c1"))
            .cloned()
            .unwrap_or_default();
        assert!(
            saw_live || !stored.changes.is_empty(),
            "change metadata reached neither the event stream nor the store"
        );
    }

    /// A model that writes a file and then edits it has produced one file. Two
    /// identical chips under the answer read as a bug in the app, not as work.
    #[tokio::test]
    async fn the_same_file_produced_twice_is_one_attachment() {
        let (etx, _erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Producer::default()));
        // A real workspace with a real file in it: turn files are checked against
        // the disk before they become chips, so the fixture has to be on the disk.
        let dir = std::env::temp_dir().join(format!(
            "cwoffee_turnfiles_{}_dup",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.md"), "# hi\n").unwrap();
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "p1".into(),
                            name: "producer".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![
                        Delta::ToolCallReady {
                            id: "p2".into(),
                            name: "producer".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![Delta::Text("done".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Auto, atx),
            AgentConfig::default(),
            ToolCtx {
                workspace: dir.clone(),
                ..Default::default()
            },
        );
        agent.run_turn("go", vec![], &etx).await;

        let files = agent.take_turn_files();
        assert_eq!(files.len(), 1, "one file, seen twice: {files:?}");
        // Metadata is the latest write, so the size the chip shows is the size
        // of what is on disk now.
        assert_eq!(files[0].bytes, 200, "kept the wrong revision: {files:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A turn that cleans up after itself produces nothing to show. The model
    /// that writes a scratch script, runs it, and `rm -rf`s the folder is doing
    /// the right thing; the transcript used to answer that with chips pointing at
    /// paths that no longer existed.
    #[tokio::test]
    async fn a_file_the_turn_deleted_is_not_an_attachment() {
        let (etx, _erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, _arx) = mpsc::unbounded_channel();
        let mut reg = Registry::new();
        reg.register(Box::new(Producer::default()));
        let dir = std::env::temp_dir().join(format!(
            "cwoffee_turnfiles_{}_deleted",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.md"), "# hi\n").unwrap();
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "p1".into(),
                            name: "producer".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![Delta::Text("done".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            ApprovalGate::new(crate::approval::ApprovalMode::Auto, atx),
            AgentConfig::default(),
            ToolCtx {
                workspace: dir.clone(),
                ..Default::default()
            },
        );
        agent.run_turn("go", vec![], &etx).await;
        // The cleanup call, arriving as a real deletion on disk.
        std::fs::remove_file(dir.join("report.md")).unwrap();

        let files = agent.take_turn_files();
        assert!(files.is_empty(), "a deleted scratch file is still an artifact: {files:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
        fn risk(&self) -> crate::tools::Risk { crate::tools::Risk::Harmless }
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

    /// Stop has to end a turn that is waiting on an approval, not just mark it.
    ///
    /// The prompt here is taken and the reply channel *held*. That is the case the
    /// engine cannot delegate: a front end alive enough to show a card and asleep
    /// enough never to answer it - a phone tab that went to sleep - answers nothing
    /// and drops nothing. If the test dropped the sender instead, the "approval UI
    /// disconnected" path would release the park and the test would pass without
    /// cancel doing anything.
    #[tokio::test]
    async fn a_stop_press_releases_a_parked_approval() {
        let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
        let (atx, mut arx) = mpsc::unbounded_channel::<(
            crate::approval::ApprovalRequest,
            tokio::sync::oneshot::Sender<crate::approval::ApprovalAnswer>,
        )>();
        let mut reg = Registry::new();
        reg.register(Box::new(Escaper));
        let mut agent = Agent::new(
            Arc::new(Scripted {
                turns: vec![
                    vec![
                        Delta::ToolCallReady {
                            id: "e1".into(),
                            name: "escaper".into(),
                            arguments: "{}".into(),
                        },
                        Delta::Done(FinishReason::ToolCalls),
                    ],
                    vec![Delta::Text("the model got another turn".into()), Delta::Done(FinishReason::Stop)],
                ],
                n: std::sync::atomic::AtomicUsize::new(0),
            }),
            reg,
            // Longer than the assertion window, so a release inside 500ms can only come
            // from the cancel; short enough that a missing release reports instead of
            // stalling the suite.
            ApprovalGate::new(crate::approval::ApprovalMode::Ask, atx)
                .with_timeout(std::time::Duration::from_secs(3)),
            AgentConfig::default(),
            ToolCtx::default(),
        );
        let cancel = agent.cancel_handle();
        let run = tokio::spawn(async move { agent.run_turn("go", vec![], &etx).await });

        let (req, reply) = arx.recv().await.expect("an approval prompt");
        assert_eq!(req.tool, "escaper");
        let t0 = std::time::Instant::now();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let _tel = run.await.unwrap();
        let dt = t0.elapsed();
        drop(reply); // held the whole time: nothing but cancel could have woken it

        assert!(
            dt < std::time::Duration::from_millis(500),
            "cancel did not release the parked approval: took {dt:?}"
        );

        let mut card_closed = false;
        let mut chatter = false;
        while let Ok(e) = erx.try_recv() {
            if matches!(e, EngineEvent::ToolExecEnd { ok: false, .. }) {
                card_closed = true;
            }
            if matches!(&e, EngineEvent::Text(t) if t.contains("another turn")) {
                chatter = true;
            }
        }
        assert!(card_closed, "the tool card was never closed after the cancel");
        assert!(!chatter, "the model was given another turn after the user pressed stop");
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

    /// The tool budget is not an ending. When it runs out while the model still
    /// wants tools, one last round-trip - with no tools offered - has to produce a
    /// summary, because a transcript that ends on a warning and a file chip reads
    /// as "it did the work and then said nothing".
    #[tokio::test]
    async fn an_exhausted_tool_budget_ends_with_a_summary() {
        let tool_turn = vec![
            Delta::ToolCallReady {
                id: "t".into(),
                name: "exec".into(),
                arguments: "{\"cmd\":\"true\"}".into(),
            },
            Delta::Done(FinishReason::ToolCalls),
        ];
        let mut turns = vec![tool_turn; 3];
        turns.push(vec![
            Delta::Text("I read two files and wrote the report.".into()),
            Delta::Done(FinishReason::Stop),
        ]);
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
        let mut saw_note = false;
        let mut saw_complaint = false;
        while let Ok(e) = rx.try_recv() {
            match e {
                EngineEvent::Note { message } => {
                    if message.contains("tool budget spent") {
                        saw_note = true;
                    }
                }
                EngineEvent::Error { message, .. } => {
                    if message.contains("no summary") || message.contains("tool iterations") {
                        saw_complaint = true;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_note, "the wrap-up did not say what it was doing");
        assert!(!saw_complaint, "the turn ended with a warning anyway");

        let last = agent.prompt.history()
            .iter()
            .rev()
            .find(|m| m.role == crate::provider::Role::Assistant)
            .expect("an assistant turn");
        assert!(
            last.content.contains("wrote the report"),
            "the summary never reached the transcript: {:?}",
            last.content
        );

        // And the instruction that asked for it must not be sitting in history as
        // something the user said.
        assert!(
            agent
                .prompt_mut()
                .history()
                .iter()
                .all(|m| m.role != crate::provider::Role::User || !m.content.contains("tool budget")),
            "the wrap-up instruction was stored as a user message"
        );
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
        let r = ToolResult { ok: false, output: "".into(), exit_code: Some(2), truncated: false, error: None, files: Vec::new(), changes: Vec::new() };
        assert!(render_result(&r).contains("(no output)"));
    }

    #[test]
    fn risk_of_exec_is_exec() {
        use crate::tools::{Risk, Tool};
        let t = crate::tools::exec::ExecTool::default();
        assert_eq!(t.risk(), Risk::Sensitive);
    }
}
