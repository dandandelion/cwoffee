//! The session host: one task that owns the Agent, the Store, and the approval
//! gate, driven entirely through channels.
//!
//! This exists so no UI owns product logic. The TUI and the Tauri shell both
//! talk to this over the same three channels, which is also the seam where a
//! future remote/daemon mode plugs in without touching either frontend.
//!
//! Deliberate limitation, stated rather than hidden: while `run_turn` is in
//! flight the host does not poll the command channel, so `NewSession` and
//! `SwitchSession` queue until the turn finishes. Cancellation and approvals do
//! work mid-turn, because they travel on their own channels. Making everything
//! preemptible would mean restructuring the agent loop around `select!` at every
//! await point; for v1 that complexity is not worth it, but it is worth naming.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, AgentConfig};
use crate::approval::{ApprovalAnswer, ApprovalGate, ApprovalMode, ApprovalRequest};
use crate::event::EngineEvent;
use crate::prompt::PromptBuilder;
use crate::provider::{Provider, Reasoning};
use crate::store::Store;
use crate::telemetry::TurnTelemetry;
use crate::tools::{Registry, ToolCtx};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub model: String,
    pub workspace: PathBuf,
    pub mode: ApprovalMode,
    pub reasoning: Reasoning,
    pub max_tool_iters: usize,
    pub max_tokens: Option<u32>,
    /// Prompt token budget. Compaction is expensive (it throws away the prefix
    /// cache), so this should sit near the server's context window, not below it.
    pub context_budget: usize,
    /// Extra volatile context appended to the newest user message.
    pub env_block: String,
}

impl HostConfig {
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            workspace: workspace.into(),
            mode: ApprovalMode::Ask,
            reasoning: Reasoning::Low,
            max_tool_iters: 8,
            max_tokens: None,
            context_budget: 8_000,
            env_block: String::new(),
        }
    }
}

/// Commands the UI sends. Everything the agent does on its own goes out over
/// `events`, so the UI never has to poll for state.
pub enum HostCmd {
    Send { text: String },
    SetMode { mode: ApprovalMode },
    SetReasoning { reasoning: Reasoning },
    NewSession { title: String },
    SwitchSession { id: i64 },
    Cancel,
    Shutdown,
}

/// Snapshot for status lines / the composer's current-state labels. Cheap to
/// clone, safe to read from any thread.
#[derive(Debug, Clone)]
pub struct HostState {
    pub session_id: i64,
    pub mode: ApprovalMode,
    pub reasoning: Reasoning,
    pub busy: bool,
    /// Turns completed so far. A UI uses this to know a turn finished without
    /// inferring it from the event stream; it also makes tests deterministic,
    /// since `busy` going false is not observable if the turn was fast.
    pub turns: u64,
    pub context_tokens: usize,
    pub compactions: u32,
}

pub struct SessionHost {
    cmd: mpsc::UnboundedSender<HostCmd>,
    pub events: mpsc::UnboundedReceiver<EngineEvent>,
    pub approvals: mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<ApprovalAnswer>)>,
    state: Arc<Mutex<HostState>>,
    cancel: Arc<AtomicBool>,
}

impl SessionHost {
    pub fn spawn(
        provider: Box<dyn Provider>,
        registry: Registry,
        store: Arc<Store>,
        cfg: HostConfig,
    ) -> SessionHost {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<HostCmd>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<EngineEvent>();
        let (ap_tx, ap_rx) =
            mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<ApprovalAnswer>)>();
        let cancel = Arc::new(AtomicBool::new(false));

        let state = Arc::new(Mutex::new(HostState {
            session_id: 0,
            mode: cfg.mode,
            reasoning: cfg.reasoning,
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
        }));

        let state_bg = Arc::clone(&state);

        tokio::spawn(async move {
            let gate = ApprovalGate::new(cfg.mode, ap_tx);
            let ctx = ToolCtx {
                workspace: cfg.workspace.clone(),
                max_output_bytes: 24 * 1024,
            };
            let mut agent = Agent::new(
                provider,
                registry,
                gate,
                AgentConfig {
                    model: cfg.model.clone(),
                    max_tool_iters: cfg.max_tool_iters,
                    reasoning: cfg.reasoning,
                    max_tokens: cfg.max_tokens,
                },
                ctx,
            );
            agent.prompt_mut().token_budget = cfg.context_budget;
            agent.prompt_mut().set_env(cfg.env_block.clone());
            let agent_cancel = agent.cancel_handle();

            // Sequence number the next unpersisted message will have. Sequences
            // start at 0, so this must not be initialised to "the highest
            // persisted seq" or the first message of every session is skipped.
            let mut persisted_next: u64 = 0;
            let mut session_id: i64 = match store.create_session(
                "new session",
                &cfg.workspace.display().to_string(),
                &cfg.model,
            ) {
                Ok(id) => id,
                Err(e) => {
                    let _ = ev_tx.send(EngineEvent::Error {
                        message: format!("cannot open session store: {e}"),
                        fatal: true,
                    });
                    return;
                }
            };

            {
                let mut s = state_bg.lock().unwrap();
                s.session_id = session_id;
            }

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    HostCmd::Shutdown => break,

                    HostCmd::Cancel => {
                        agent_cancel.store(true, Ordering::Relaxed);
                    }

                    HostCmd::SetMode { mode } => {
                        agent.set_mode(mode);
                        state_bg.lock().unwrap().mode = mode;
                    }

                    HostCmd::SetReasoning { reasoning } => {
                        agent.set_reasoning(reasoning);
                        state_bg.lock().unwrap().reasoning = reasoning;
                    }

                    HostCmd::NewSession { title } => {
                        match store.create_session(&title, &cfg.workspace.display().to_string(), &cfg.model) {
                            Ok(id) => {
                                session_id = id;
                                persisted_next = 0;
                                *agent.prompt_mut() = {
                                    let mut p = PromptBuilder::new();
                                    p.token_budget = cfg.context_budget;
                                    p.set_env(cfg.env_block.clone());
                                    p
                                };
                                let mut s = state_bg.lock().unwrap();
                                s.session_id = id;
                                s.context_tokens = 0;
                                s.compactions = 0;
                            }
                            Err(e) => {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("cannot create session: {e}"),
                                    fatal: false,
                                });
                            }
                        }
                    }

                    HostCmd::SwitchSession { id } => {
                        match store.history(id) {
                            Ok(hist) => {
                                // Replayed messages keep sequence 0..n-1, which
                                // lines up 1:1 with the rows already in the DB,
                                // so resuming cannot re-persist them.
                                let n = hist.len() as u64;
                                agent.prompt_mut().with_history(hist);
                                agent.prompt_mut().set_first_seq(0);
                                persisted_next = n;
                                // Must rebind the persistence target too, not just
                                // the reported state, or the resumed session's new
                                // messages land in whichever session was active.
                                session_id = id;
                                let mut s = state_bg.lock().unwrap();
                                s.session_id = id;
                                s.context_tokens = agent.prompt_mut().estimate_tokens("");
                                s.compactions = 0;
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("resumed session {id} with {n} messages; \
                                                      the prefix cache is cold, first turn will be slow"),
                                    fatal: false,
                                });
                            }
                            Err(e) => {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("cannot load session {id}: {e}"),
                                    fatal: false,
                                });
                            }
                        }
                    }

                    HostCmd::Send { text } => {
                        set_busy(&state_bg, true);
                        let tel: TurnTelemetry = agent.run_turn(&text, &ev_tx).await;

                        // Persist whatever the turn added. Sequence numbers make
                        // this correct even if compaction shifted the window.
                        let hist = agent.prompt_mut().history().to_vec();
                        for (i, m) in hist.iter().enumerate() {
                            let seq = agent.prompt_mut().seq_at(i);
                            if seq < persisted_next {
                                continue;
                            }
                            if let Err(e) = store.append_message(session_id, m) {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("failed to persist message: {e}"),
                                    fatal: false,
                                });
                                break;
                            }
                        }
                        persisted_next = agent.prompt_mut().next_seq();

                        if tel.prompt_tokens > 0 {
                            let _ = store.record_turn(session_id, &tel);
                        }

                        {
                            let mut s = state_bg.lock().unwrap();
                            s.context_tokens = agent.prompt_mut().estimate_tokens("");
                            s.compactions = agent.prompt_mut().compactions;
                            s.busy = false;
                            s.turns += 1;
                        }
                    }
                }
            }
        });

        SessionHost { cmd: cmd_tx, events: ev_rx, approvals: ap_rx, state, cancel }
    }

    pub fn send(&self, cmd: HostCmd) {
        let _ = self.cmd.send(cmd);
    }

    pub fn submit(&self, text: impl Into<String>) {
        self.send(HostCmd::Send { text: text.into() });
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn state(&self) -> HostState {
        self.state.lock().unwrap().clone()
    }
}

fn set_busy(state: &Arc<Mutex<HostState>>, busy: bool) {
    state.lock().unwrap().busy = busy;
}

/// Build the volatile environment block. Kept out of the system prompt on
/// purpose: putting a date there changes the leading bytes of every prompt and
/// guarantees a prefix-cache miss on every single turn.
pub fn default_env_block(workspace: &std::path::Path, os: &str) -> String {
    format!(
        "<env>\nos: {os}\nworkspace: {}\nshell: sh\n\
         commands run sandboxed: no network, only the workspace is writable\n\
         </env>",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FinishReason;
    use crate::provider::{Delta, ProviderError};
    use crate::tools::exec::ExecTool;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    /// Scripted provider: fixed replies, no network.
    struct Scripted {
        replies: std::sync::Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for Scripted {
        fn id(&self) -> &str {
            "scripted"
        }
        fn label(&self) -> &str {
            "scripted"
        }
        fn wants_reasoning(&self) -> bool {
            false
        }
        async fn stream(
            &self,
            _req: &crate::provider::ChatRequest,
            sink: &crate::provider::DeltaSink,
        ) -> Result<(), ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            let reply = self
                .replies
                .lock()
                .unwrap()
                .get(n)
                .cloned()
                .unwrap_or_else(|| "done".into());
            for chunk in reply.split(' ') {
                sink.send(Delta::Text(format!("{chunk} ")))
                    .map_err(|_| ProviderError::Cancelled)?;
            }
            sink.send(Delta::Telemetry(TurnTelemetry {
                prompt_tokens: 40,
                cached_tokens: 20,
                completion_tokens: 8,
                ..Default::default()
            }))
            .map_err(|_| ProviderError::Cancelled)?;
            sink.send(Delta::Done(FinishReason::Stop))
                .map_err(|_| ProviderError::Cancelled)?;
            Ok(())
        }
    }

    fn host(replies: Vec<String>) -> (SessionHost, Arc<Store>) {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Box::new(Scripted {
            replies: std::sync::Mutex::new(replies),
            calls: AtomicUsize::new(0),
        });
        let h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        (h, store)
    }

    /// The host runs on its own task, so reading `state()` straight after
    /// `send()` races it. Every test waits on an observable condition instead
    /// of sleeping.
    async fn wait_for(what: String, f: impl Fn() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn ready(h: &SessionHost) {
        wait_for("initial session id".into(), || h.state().session_id > 0).await;
    }

    /// Waiting on `busy` is wrong: a fast turn can finish before the first
    /// poll, so `busy` is never observed as true.
    async fn turns_done(h: &SessionHost, n: u64) {
        wait_for(format!("{n} turns"), move || h.state().turns >= n).await;
    }

    async fn drain_events(h: &mut SessionHost) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        while let Ok(e) = h.events.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn a_turn_is_streamed_and_persisted() {
        let (mut h, store) = host(vec!["hello there".into()]);
        ready(&h).await;
        h.submit("first question");
        turns_done(&h, 1).await;

        let evs = drain_events(&mut h).await;
        assert!(evs
            .iter()
            .any(|e| matches!(e, EngineEvent::TurnEnd { finish: FinishReason::Stop, .. })));
        assert!(evs.iter().any(|e| matches!(e, EngineEvent::Text(_))));

        let sid = h.state().session_id;
        assert!(sid > 0, "no session was created");
        let hist = store.history(sid).unwrap();
        assert_eq!(hist.len(), 2, "history: {hist:?}");
        assert_eq!(hist[0].role, crate::provider::Role::User);
        assert_eq!(hist[0].content, "first question");
        assert!(hist[1].content.contains("hello"));

        // telemetry recorded: this is what the averages columns read
        assert_eq!(store.session_totals(sid).unwrap().turns, 1);
    }

    #[tokio::test]
    async fn history_accumulates_across_turns() {
        let (h, store) = host(vec!["one".into(), "two".into(), "three".into()]);
        ready(&h).await;
        for (i, q) in ["q1", "q2", "q3"].iter().enumerate() {
            h.submit(*q);
            turns_done(&h, (i + 1) as u64).await;
        }
        let hist = store.history(h.state().session_id).unwrap();
        assert_eq!(hist.len(), 6, "expected 3 user + 3 assistant, got {hist:?}");
        assert_eq!(hist[0].content, "q1");
        assert_eq!(hist[4].content, "q3");
        assert_eq!(hist[5].content, "three ");
    }

    #[tokio::test]
    async fn new_session_starts_a_clean_prompt_but_keeps_the_old_one() {
        let (h, store) = host(vec!["a".into(), "b".into()]);
        ready(&h).await;
        h.submit("in session one");
        turns_done(&h, 1).await;
        let first = h.state().session_id;

        h.send(HostCmd::NewSession { title: "second".into() });
        wait_for("new session id".into(), || h.state().session_id != first).await;
        let second = h.state().session_id;
        assert_ne!(first, second);

        h.submit("in session two");
        turns_done(&h, 2).await;

        assert_eq!(store.history(first).unwrap().len(), 2);
        assert_eq!(store.history(second).unwrap().len(), 2);
        assert_eq!(store.history(first).unwrap()[0].content, "in session one");
        assert_eq!(store.history(second).unwrap()[0].content, "in session two");
    }

    #[tokio::test]
    async fn switching_to_a_stored_session_replays_its_history() {
        let (h, store) = host(vec!["x".into(), "y".into()]);
        ready(&h).await;
        h.submit("remember this");
        turns_done(&h, 1).await;
        let sid = h.state().session_id;

        h.send(HostCmd::NewSession { title: "fresh".into() });
        wait_for("new session id".into(), || h.state().session_id != sid).await;
        let fresh = h.state().session_id;
        assert_eq!(store.history(fresh).unwrap().len(), 0);

        h.send(HostCmd::SwitchSession { id: sid });
        wait_for("switched back".into(), || h.state().session_id == sid).await;
        h.submit("and now continue");
        turns_done(&h, 2).await;

        // Replayed history plus the new exchange, with no duplicate of the
        // replayed part - that is what the sequence cursor is for.
        let hist = store.history(sid).unwrap();
        assert_eq!(hist.len(), 4, "replayed messages duplicated or lost: {hist:?}");
        assert_eq!(hist[0].content, "remember this");
        assert_eq!(hist[2].content, "and now continue");
        assert_eq!(hist[3].content, "y ");
    }

    #[tokio::test]
    async fn mode_changes_take_effect() {
        let (h, _store) = host(vec!["ok".into()]);
        ready(&h).await;
        assert_eq!(h.state().mode, ApprovalMode::Ask);
        h.send(HostCmd::SetMode { mode: ApprovalMode::Auto });
        wait_for("mode change".into(), || h.state().mode == ApprovalMode::Auto).await;
    }

    #[tokio::test]
    async fn approval_channel_stays_open_and_unused_when_no_tools_are_called() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Box::new(Scripted {
            replies: std::sync::Mutex::new(vec!["tool".into()]),
            calls: AtomicUsize::new(0),
        });
        let mut reg = Registry::new();
        reg.register(Box::new(ExecTool { timeout_secs: 5, allow_unsandboxed: false }));

        let mut h = SessionHost::spawn(
            p,
            reg,
            Arc::clone(&store),
            HostConfig { mode: ApprovalMode::Ask, ..HostConfig::new("m", std::env::temp_dir()) },
        );
        ready(&h).await;
        h.submit("say hi");
        turns_done(&h, 1).await;
        assert!(h.approvals.try_recv().is_err(), "unexpected approval request");
    }

    #[tokio::test]
    async fn shutdown_stops_the_host() {
        let (mut h, _store) = host(vec!["ok".into()]);
        h.send(HostCmd::Shutdown);
        // the events channel closes once the host task returns
        assert!(h.events.recv().await.is_none());
    }

    #[test]
    fn env_block_names_the_sandbox_so_the_model_does_not_guess() {
        let b = default_env_block(std::path::Path::new("/home/u/ws"), "linux");
        assert!(b.contains("/home/u/ws"));
        assert!(b.contains("no network"));
        assert!(b.contains("sandbox"));
    }
}
