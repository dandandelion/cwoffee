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
use crate::provider::{Provider, Reasoning, Role};
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
    /// How long an unanswered approval prompt waits before refusing.
    pub approval_timeout: std::time::Duration,
    /// Attachment guardrails. Enforced at the host boundary rather than in the
    /// UI, because the UI is not the only client and never will be again.
    pub max_attachments: usize,
    /// Decoded bytes per attachment.
    pub max_attachment_bytes: usize,
}

impl HostConfig {
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            workspace: workspace.into(),
            mode: ApprovalMode::Ask,
            reasoning: Reasoning::XHigh,
            max_tool_iters: 8,
            max_tokens: None,
            context_budget: 8_000,
            env_block: String::new(),
            approval_timeout: std::time::Duration::from_secs(300),
            max_attachments: 4,
            max_attachment_bytes: 2 * 1024 * 1024,
        }
    }
}

/// The engine's one definition of "a workspace this app will run tools in": an
/// existing, writable directory. Every client needs the same answer, because a
/// sidebar that accepts a path the engine later refuses is a UI that lies about
/// where your files are about to be written.
pub fn check_workspace(raw: &str) -> Result<std::path::PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("a workspace path cannot be empty".into());
    }
    let expanded = crate::config::Config::expand_tilde(trimmed);
    let p = std::path::PathBuf::from(&expanded);
    if !p.is_dir() {
        return Err("is not a directory".into());
    }
    // The sandbox confines writes to this directory, so an unwritable one is a
    // configuration mistake worth saying out loud rather than discovering on the
    // first exec.
    let probe = p.join(".cwoffee-write-test");
    if std::fs::File::create(&probe).is_err() {
        return Err("is not writable; the tools could not create a file there".into());
    }
    let _ = std::fs::remove_file(&probe);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Commands the UI sends. Everything the agent does on its own goes out over
/// `events`, so the UI never has to poll for state.
pub enum HostCmd {
    /// `attachments` are validated at this boundary, before anything reaches the
    /// provider: an unbounded or arbitrary-mime payload from a UI would be a
    /// memory and SSRF-adjacent problem downstream.
    Send {
        text: String,
        attachments: Vec<crate::provider::Attachment>,
    },
    SetMode { mode: ApprovalMode },
    SetReasoning { reasoning: Reasoning },
    /// Change the model id for subsequent turns. Does not rewrite history.
    SetModel { model: String },
    /// Move the tools to a different workspace. Refused while a turn is running.
    SetWorkspace { path: String },
    /// Drop to draft state: no session row, empty prompt. Used after deleting
    /// the active session, where creating a replacement row just produced
    /// another blank entry in the sidebar.
    ClearSession,
    NewSession { title: String },
    SwitchSession { id: i64 },
    Cancel,
    Shutdown,
}

/// Snapshot for status lines / the composer's current-state labels. Cheap to
/// clone, safe to read from any thread.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostState {
    pub session_id: i64,
    pub workspace: String,
    pub model: String,
    pub mode: ApprovalMode,
    pub reasoning: Reasoning,
    pub busy: bool,
    /// Turns completed so far. A UI uses this to know a turn finished without
    /// inferring it from the event stream; it also makes tests deterministic,
    /// since `busy` going false is not observable if the turn was fast.
    pub turns: u64,
    pub context_tokens: usize,
    pub compactions: u32,
    /// Seconds before an unanswered approval fails closed. The UI shows this as a
    /// countdown: a turn blocked on a human looks identical to a turn still
    /// working, and that difference is the entire message.
    pub approval_timeout_secs: u64,
}

pub struct SessionHost {
    cmd: mpsc::UnboundedSender<HostCmd>,
    /// Receivers are `Option` because `recv()` needs `&mut`, which shared UI
    /// state (Tauri's `State<AppState>`) cannot give out. A shell calls
    /// `take_receivers` once and owns them; the TUI does the same.
    events: Option<mpsc::UnboundedReceiver<EngineEvent>>,
    approvals: Option<mpsc::UnboundedReceiver<ApprovalRx>>,
    questions: Option<mpsc::UnboundedReceiver<crate::tools::ask::QuestionRx>>,
    state: Arc<Mutex<HostState>>,
    cancel: Arc<AtomicBool>,
    /// The host keeps a handle so the shell can ask the endpoint questions that
    /// are not turns - the composer's model picker being the obvious one.
    provider: Arc<dyn Provider>,
}

pub type EventRx = mpsc::UnboundedReceiver<EngineEvent>;
pub type ApprovalRx = (ApprovalRequest, oneshot::Sender<ApprovalAnswer>);

impl SessionHost {
    pub fn spawn(
        provider: Arc<dyn Provider>,
        registry: Registry,
        store: Arc<Store>,
        cfg: HostConfig,
    ) -> SessionHost {
        let provider = Arc::clone(&provider);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<HostCmd>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<EngineEvent>();
        let (ap_tx, ap_rx) =
            mpsc::unbounded_channel::<(ApprovalRequest, oneshot::Sender<ApprovalAnswer>)>();
        // ask_user's round trip. Deliberately a separate channel from
        // approvals: an approval is the gate permitting an action, a question is
        // the model asking for input, and conflating them means a "yes" answers
        // both.
        let (ask_tx, ask_rx) = mpsc::unbounded_channel::<crate::tools::ask::QuestionRx>();
        // The folder you start in is a workspace too. Registering it means the
        // sidebar shows where a brand-new session would write, instead of an
        // empty list until the first message happens to create a row.
        let _ = store.add_workspace(&cfg.workspace.display().to_string());
        let state = Arc::new(Mutex::new(HostState {
            session_id: 0,
            workspace: cfg.workspace.display().to_string(),
            model: cfg.model.clone(),
            mode: cfg.mode,
            reasoning: cfg.reasoning,
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
            approval_timeout_secs: cfg.approval_timeout.as_secs(),
        }));
        let limits = AttachmentLimits {
            max_count: cfg.max_attachments,
            max_bytes: cfg.max_attachment_bytes,
        };

        let state_bg = Arc::clone(&state);
        // The task takes the provider; the host keeps a second handle for
        // non-turn questions like the model list.
        let host_provider = Arc::clone(&provider);

        // Built here, not inside the task, so the host can hold the agent's own
        // cancel flag. `SessionHost::cancel()` used to set a separate AtomicBool
        // that nothing ever read, so the UI's Stop button did nothing at all.
        // It has to be a shared flag rather than a HostCmd: the command loop is
        // sequential and is parked inside run_turn() for the whole turn, so a
        // Cancel command would only be processed after the turn it should stop.
        let gate = ApprovalGate::new(cfg.mode, ap_tx).with_timeout(cfg.approval_timeout);
        let ctx = ToolCtx {
            workspace: cfg.workspace.clone(),
            max_output_bytes: 24 * 1024,
            ask: Some(ask_tx),
            events: None,
            permits: Vec::new(),
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
        let cancel = agent.cancel_handle();
        let agent_cancel = cancel.clone();

        tokio::spawn(async move {

            // Sequence number the next unpersisted message will have. Sequences
            // start at 0, so this must not be initialised to "the highest
            // persisted seq" or the first message of every session is skipped.
            let mut persisted_next: u64 = 0;
            // 0 means "draft": no session row exists yet. Creating one at
            // startup (and again on every NewSession) left an empty row behind
            // for every launch and every click of the button, which accumulated
            // into a sidebar full of identical blanks. A session row now appears
            // when the first message is sent.
            let mut session_id: i64 = 0;

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

                    HostCmd::ClearSession => {
                        session_id = 0;
                        persisted_next = 0;
                        agent.replace_prompt(PromptBuilder::new());
                        agent.prompt_mut().set_env(cfg.env_block.clone());
                        {
                            let mut s = state_bg.lock().unwrap();
                            s.session_id = 0;
                            s.context_tokens = 0;
                        }
                        let _ = ev_tx.send(EngineEvent::SessionChanged {
                            session_id: 0,
                            title: String::new(),
                        });
                    }
                    HostCmd::SetWorkspace { path } => {
                        // No busy check is needed, and one here would be dead
                        // code: `Send` runs the turn inline in this loop, so no
                        // command - including this one - can be processed while
                        // a tool holds the previous ToolCtx.
                        let p = match check_workspace(&path) {
                            Ok(p) => p,
                            Err(reason) => {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("the engine refused {path:?}: {reason}"),
                                    fatal: false,
                                });
                                continue;
                            }
                        };
                        // Moving the tools into a folder registers it, so the
                        // sidebar shows where you now work without you having to
                        // add it by hand.
                        let _ = store.add_workspace(&p.display().to_string());
                        agent.set_workspace(p.clone());
                        // The env block rides on the newest user message only, so
                        // changing it does not disturb the cached prefix.
                        agent.prompt_mut().set_env(default_env_block(&p, "linux"));
                        let _ = store.set_session_workspace(session_id, &p);
                        state_bg.lock().unwrap().workspace = p.display().to_string();
                        let _ = ev_tx.send(EngineEvent::Note {
                            message: format!("workspace is now {}", p.display()),
                        });
                    }
                    HostCmd::SetModel { model } => {
                        // Empty would send `"model": ""` to the endpoint, which
                        // is a confusing 400 rather than an obvious bug.
                        if model.trim().is_empty() {
                            let _ = ev_tx.send(EngineEvent::Error {
                                message: "model id cannot be empty".into(),
                                fatal: false,
                            });
                        } else {
                            agent.set_model(model.clone());
                            state_bg.lock().unwrap().model = model;
                        }
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
                        // The agent's live values, not cfg: cfg is what the config
                        // file said at startup. Using it here meant a workspace you
                        // had just changed was quietly undone for the next session,
                        // which is what "it goes back to cwoffee-workspace" was.
                        let ws = agent.workspace().display().to_string();
                        let model = agent.model();
                        match store.create_session(&title, &ws, &model) {
                            Ok(id) => {
                                session_id = id;
                                persisted_next = 0;
                                *agent.prompt_mut() = {
                                    let mut p = PromptBuilder::new();
                                    p.token_budget = cfg.context_budget;
                                    p.set_env(cfg.env_block.clone());
                                    p
                                };
                                agent.reset_turn_id();
                                let mut s = state_bg.lock().unwrap();
                                s.session_id = id;
                                s.context_tokens = 0;
                                s.compactions = 0;
                                let _ = ev_tx.send(EngineEvent::SessionChanged {
                                    session_id: id,
                                    title: title.clone(),
                                });
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
                                agent.reset_turn_id();
                                persisted_next = n;
                                // Must rebind the persistence target too, not just
                                // the reported state, or the resumed session's new
                                // messages land in whichever session was active.
                                session_id = id;
                                // A session lives where its row says it lives. The
                                // sidebar groups sessions by folder, so the tools
                                // have to follow when one is clicked - and a folder
                                // that has gone missing has to say so rather than
                                // quietly run tools in the last good place, which
                                // is how a file ends up written in the wrong tree.
                                //
                                // Done before taking the state lock: this branch
                                // already holds it further down, and a nested lock
                                // on the same mutex deadlocks the host loop. (It
                                // did, in this very arm.)
                                let row = store.session(id).ok().flatten();
                                let mut follow_ws: Option<std::path::PathBuf> = None;
                                if let Some(row) = row.as_ref() {
                                    if row.workspace != agent.workspace().display().to_string() {
                                        match check_workspace(&row.workspace) {
                                            Ok(p) => {
                                                agent.set_workspace(p.clone());
                                                agent
                                                    .prompt_mut()
                                                    .set_env(default_env_block(&p, "linux"));
                                                let _ = ev_tx.send(EngineEvent::Note {
                                                    message: format!(
                                                        "workspace is now {} (from this session)",
                                                        p.display()
                                                    ),
                                                });
                                                follow_ws = Some(p);
                                            }
                                            Err(reason) => {
                                                let msg = format!(
                                                    "session {id} lives in {:?}, which the engine refused: {reason}. Tools stay in {}",
                                                    row.workspace,
                                                    agent.workspace().display()
                                                );
                                                let _ = ev_tx.send(EngineEvent::Error {
                                                    message: msg,
                                                    fatal: false,
                                                });
                                            }
                                        }
                                    }
                                }
                                let mut s = state_bg.lock().unwrap();
                                s.session_id = id;
                                s.context_tokens = agent.prompt_mut().estimate_tokens("");
                                s.compactions = 0;
                                if let Some(p) = follow_ws {
                                    s.workspace = p.display().to_string();
                                }
                                let title = row.map(|x| x.title).unwrap_or_default();
                                let _ = ev_tx.send(EngineEvent::SessionChanged {
                                    session_id: id,
                                    title,
                                });
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

                    HostCmd::Send { text, attachments } => {
                        let attachments = match check_attachments(&attachments, limits) {
                            Ok(()) => attachments,
                            Err(reason) => {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("attachment rejected: {reason}"),
                                    fatal: false,
                                });
                                continue;
                            }
                        };
                        if text.trim().is_empty() && attachments.is_empty() {
                            continue;
                        }
                        if session_id == 0 {
                            let ws = agent.workspace().display().to_string();
                            let _ = store.add_workspace(&ws);
                            let model = agent.model();
                            match store.create_session("new session", &ws, &model) {
                                Ok(id) => {
                                    session_id = id;
                                    persisted_next = 0;
                                    state_bg.lock().unwrap().session_id = id;
                                }
                                Err(e) => {
                                    let _ = ev_tx.send(EngineEvent::Error {
                                        message: format!("cannot open session store: {e}"),
                                        fatal: true,
                                    });
                                    continue;
                                }
                            }
                        }
                        set_busy(&state_bg, true);
                        let tel: TurnTelemetry =
                            agent.run_turn(&text, attachments, &ev_tx).await;

                        // Persist whatever the turn added. Sequence numbers make
                        // this correct even if compaction shifted the window.
                        let hist = agent.prompt_mut().history().to_vec();
                        // Files are attached to the assistant row that closed the
                        // turn, so a reload shows the same attachments the live
                        // turn showed.
                        let mut last_assistant: Option<i64> = None;
                        for (i, m) in hist.iter().enumerate() {
                            let seq = agent.prompt_mut().seq_at(i);
                            if seq < persisted_next {
                                continue;
                            }
                            match store.append_message(session_id, m) {
                                Ok(id) => {
                                    if matches!(m.role, Role::Assistant) {
                                        last_assistant = Some(id);
                                    }
                                }
                                Err(e) => {
                                    let _ = ev_tx.send(EngineEvent::Error {
                                        message: format!("failed to persist message: {e}"),
                                        fatal: false,
                                    });
                                    break;
                                }
                            }
                        }
                        persisted_next = agent.prompt_mut().next_seq();

                        let files = agent.take_turn_files();
                        if !files.is_empty() {
                            if let Some(id) = last_assistant {
                                if let Err(e) = store.set_message_files(id, &files) {
                                    let _ = ev_tx.send(EngineEvent::Error {
                                        message: format!("failed to persist output files: {e}"),
                                        fatal: false,
                                    });
                                }
                            }
                        }

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

        SessionHost {
            cmd: cmd_tx,
            events: Some(ev_rx),
            approvals: Some(ap_rx),
            questions: Some(ask_rx),
            state,
            cancel,
            provider: host_provider,
        }
    }

    /// Fill in an empty model id from the endpoint's own list.
    ///
    /// A fresh install with no config file has no model. Sending `"model": ""`
    /// produces a confusing 400 from the server, so the shell resolves one
    /// instead. Returns the id adopted, or None if there was nothing to adopt.
    pub async fn adopt_first_model(&self) -> Option<String> {
        if !self.state().model.trim().is_empty() {
            return None;
        }
        let first = self.provider.list_models().await.ok()?.into_iter().next()?;
        self.send(HostCmd::SetModel { model: first.clone() });
        Some(first)
    }

    /// Handle to the endpoint, for shell-level queries that are not turns.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// Move the event and approval receivers out of the host, exactly once.
    /// Returns `None` on a second call: two consumers would silently split one
    /// event stream, which shows up as a UI randomly missing tool calls.
    pub fn take_receivers(&mut self) -> Option<(EventRx, mpsc::UnboundedReceiver<ApprovalRx>)> {
        let ev = self.events.take()?;
        let ap = self.approvals.take()?;
        Some((ev, ap))
    }

    /// Move the question receiver out, once. The shell that owns it turns each
    /// question into a `QuestionRequested` event and parks the reply sender
    /// until a client answers.
    pub fn take_questions(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<crate::tools::ask::QuestionRx>> {
        self.questions.take()
    }

    pub fn send(&self, cmd: HostCmd) {
        let _ = self.cmd.send(cmd);
    }

    pub fn submit(&self, text: impl Into<String>) {
        self.send_with(text, Vec::new())
    }

    /// Send a message that carries attachments.
    pub fn send_with(
        &self,
        text: impl Into<String>,
        attachments: Vec<crate::provider::Attachment>,
    ) {
        self.send(HostCmd::Send { text: text.into(), attachments })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn state(&self) -> HostState {
        self.state.lock().unwrap().clone()
    }

    /// Move the workspace and wait (bounded) for the host to accept it.
    ///
    /// `send()` is fire-and-forget, so the caller had no way to learn that the
    /// engine refused the path. The UI therefore showed a label that quietly
    /// reverted, and the only trace was a warning block in the transcript. This
    /// turns "I picked a folder and nothing happened" into an actual reason.
    ///
    /// Note the command loop is sequential: while a turn is running the change
    /// is queued behind it, which is why the timeout says so rather than
    /// claiming the path was bad.
    pub async fn set_workspace_checked(&self, path: &str) -> Result<String, String> {
        let want = std::path::PathBuf::from(path);
        if path.trim().is_empty() {
            return Err("empty workspace path".into())
        }
        self.send(HostCmd::SetWorkspace { path: path.to_string() });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let st = self.state();
            if std::path::PathBuf::from(&st.workspace) == want {
                return Ok(st.workspace)
            }
            if std::time::Instant::now() >= deadline {
                if st.busy {
                    return Err(format!(
                        "a turn is running, so the workspace change is queued behind it ({path} not applied yet). Stop the turn and try again."
                    ))
                }
                return Err(format!(
                    "the engine refused {path}: a workspace must be an existing, writable directory"
                ))
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

fn set_busy(state: &Arc<Mutex<HostState>>, busy: bool) {
    state.lock().unwrap().busy = busy;
}

/// Convert a gate request into the wire event every front end renders.
///
/// Shared so the TUI and the webview cannot drift into asking different
/// questions about the same call.
pub fn approval_event(req: &ApprovalRequest) -> EngineEvent {
    EngineEvent::ApprovalRequested {
        id: req.id,
        tool: req.tool.clone(),
        risk: req.risk,
        summary: req.summary.clone(),
        args: req.args.clone(),
    }
}

/// Convert a pending question into the wire event every front end renders.
///
/// Like `approval_event`, shared so the desktop, the gateway and the TUI cannot
/// drift into asking the same question in three different shapes.
pub fn question_event(q: &crate::tools::ask::Question) -> EngineEvent {
    EngineEvent::QuestionRequested {
        id: q.id,
        question: q.question.clone(),
        options: q.options.clone(),
        recommended: q.recommended,
        allow_custom: q.allow_custom,
    }
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

/// Attachment guardrails, checked before anything reaches a provider.
#[derive(Debug, Clone, Copy)]
pub struct AttachmentLimits {
    pub max_count: usize,
    pub max_bytes: usize,
}

/// Only these are accepted. The provider will happily forward any mime string,
/// and a UI (or a future gateway client) should not be able to ask the model to
/// interpret an arbitrary data blob.
const ALLOWED_MIME: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

fn b64_ok(s: &str) -> bool {
    if s.is_empty() || s.len() % 4 != 0 {
        return false
    }
    s.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
    })
}

fn check_attachments(atts: &[crate::provider::Attachment], limits: AttachmentLimits) -> Result<(), String> {
    if atts.len() > limits.max_count {
        return Err(format!(
            "{} attachments exceeds the limit of {}",
            atts.len(),
            limits.max_count
        ));
    }
    for (i, a) in atts.iter().enumerate() {
        if !ALLOWED_MIME.contains(&a.mime.as_str()) {
            return Err(format!(
                "attachment {}: mime {:?} is not allowed (allowed: {})",
                i + 1,
                a.mime,
                ALLOWED_MIME.join(", ")
            ));
        }
        let bytes = a.approx_bytes();
        if bytes > limits.max_bytes {
            return Err(format!(
                "attachment {} is {} bytes, over the {} byte limit",
                i + 1,
                bytes,
                limits.max_bytes
            ));
        }
        // Cheap and local: a corrupt payload otherwise surfaces as an opaque
        // provider error several seconds later.
        if !b64_ok(&a.data_b64) {
            return Err(format!(
                "attachment {} is not valid base64 (expected unprefixed base64, got {} chars)",
                i + 1,
                a.data_b64.len()
            ));
        }
    }
    Ok(())
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
        /// What the provider actually received, so a test can prove an
        /// attachment survived the host, the prompt builder and the request.
        seen_attachments: Arc<AtomicUsize>,
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
            req: &crate::provider::ChatRequest,
            sink: &crate::provider::DeltaSink,
        ) -> Result<(), ProviderError> {
            let seen = req
                .messages
                .iter()
                .map(|m| m.attachments.len())
                .sum::<usize>();
            self.seen_attachments.fetch_add(seen, Ordering::Relaxed);
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
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(replies),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
        });
        let h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        (h, store)
    }

    #[tokio::test]
    async fn workspace_switch_validates_the_path() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let (mut h, _store) = host(vec!["a".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        // A path that is not a directory must be refused, not silently adopted:
        // bwrap would bind a non-directory and every tool call would then fail
        // far from the cause.
        h.send(HostCmd::SetWorkspace { path: "/nonexistent/nope".into() });
        assert!(
            await_error(&mut ev, "not a directory").await,
            "a non-directory workspace was accepted"
        );

        // A directory that exists but cannot be written is also worth saying so
        // now: the sandbox makes exactly this directory writable, so a read-only
        // choice guarantees the first write fails.
        let ro = base.join("read-only");
        std::fs::create_dir_all(&ro).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::File::create(ro.join("probe")).is_err() {
            // skipped when running as root, which can write anything
            h.send(HostCmd::SetWorkspace { path: ro.display().to_string() });
            assert!(
                await_error(&mut ev, "not writable").await,
                "an unwritable workspace was accepted"
            );
        }
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700));

        // A real, writable directory is adopted, and the state the UI reads
        // follows it.
        let other = base.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        h.send(HostCmd::SetWorkspace { path: other.display().to_string() });
        wait_for("workspace change".into(), || {
            h.state().workspace == other.display().to_string()
        })
        .await;
        assert!(
            drain_events(&mut ev).await.iter().any(|e| matches!(e, EngineEvent::Note { .. })),
            "a workspace move should be visible in the transcript"
        );

        // Ordering: a change queued behind a turn lands after it, never during
        // it, because the command loop runs turns inline.
        h.submit("go");
        h.send(HostCmd::SetWorkspace { path: base.display().to_string() });
        turns_done(&h, 1).await;
        assert_eq!(h.state().workspace, base.display().to_string());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A session lives where its row says it lives, so clicking one in the
    /// sidebar's grouping has to move the tools with it. Otherwise the folder
    /// label is decoration and `write_file` lands wherever the last session
    /// happened to point.
    #[tokio::test]
    async fn switching_a_session_moves_the_tools_to_that_sessions_folder() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_switch_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (mut h, store) = host(vec!["first reply".into()]);
        let (_ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        let sa = store.create_session("in a", &a.display().to_string(), "m").unwrap();
        let sb = store.create_session("in b", &b.display().to_string(), "m").unwrap();

        h.send(HostCmd::SwitchSession { id: sa });
        wait_for("into a".into(), || h.state().workspace == a.display().to_string()).await;
        h.send(HostCmd::SwitchSession { id: sb });
        wait_for("into b".into(), || h.state().workspace == b.display().to_string()).await;

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A folder that no longer exists must be refused out loud. Falling back to
    /// the previous workspace silently is how a file ends up written somewhere the
    /// user did not intend.
    #[tokio::test]
    async fn a_session_in_a_vanished_folder_says_so_and_stays_put() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_gone_{}", std::process::id()));
        let here = base.join("here");
        let gone = base.join("gone");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&gone).unwrap();

        let (mut h, store) = host(vec!["first reply".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        let keep = h.state().workspace.clone();

        let sid = store.create_session("orphan", &gone.display().to_string(), "m").unwrap();
        let _ = std::fs::remove_dir_all(&gone);

        h.send(HostCmd::SwitchSession { id: sid });
        assert!(
            await_error(&mut ev, "refused").await,
            "the engine stayed silent about a missing workspace"
        );
        assert_eq!(
            h.state().workspace, keep,
            "tools moved to a folder that does not exist"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Moving into a folder registers it, so the sidebar shows where you work
    /// without a second step nobody remembers to take.
    #[tokio::test]
    async fn a_workspace_move_registers_the_folder_for_the_sidebar() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_reg_{}", std::process::id()));
        let target = base.join("project");
        std::fs::create_dir_all(&target).unwrap();
        let (mut h, store) = host(vec!["x".into()]);
        let (_ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        h.send(HostCmd::SetWorkspace { path: target.display().to_string() });
        wait_for("moved".into(), || h.state().workspace == target.display().to_string()).await;
        let listed = store.list_workspaces().unwrap();
        assert!(
            listed.iter().any(|w| w.path == target.display().to_string()),
            "the folder was not registered: {listed:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The status bar must show something *during* a turn.
    ///
    /// Reported as "at the first turn, thinking, the performance stats are not
    /// measuring anything": the server only reports token counts at the end of a
    /// turn, so every number in the bar sat at zero while the model was working -
    /// which is precisely when the user is looking at it.
    #[tokio::test]
    async fn a_streaming_turn_reports_live_stats_before_usage() {
        let (mut h, _store) = host(vec!["alpha beta gamma delta epsilon".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        h.submit("go");
        let mut stats: Vec<crate::telemetry::TurnStats> = Vec::new();
        let mut saw_stats_before_usage = false;
        for _ in 0..200 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), ev.recv()).await {
                Ok(Some(EngineEvent::TurnStats(st))) => stats.push(st),
                Ok(Some(EngineEvent::Usage(_))) => {
                    saw_stats_before_usage = !stats.is_empty();
                    break;
                }
                Ok(Some(EngineEvent::TurnEnd { .. })) => break,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(!stats.is_empty(), "no live stats were emitted during the turn");
        let last = stats.last().unwrap();
        assert!(last.chars > 0, "live stats reported no characters");
        assert!(last.est_tokens > 0, "live stats estimated zero tokens");
        assert!(
            saw_stats_before_usage,
            "live stats must arrive before Usage, not after it"
        );
    }

    /// A workspace move must survive the next session.
    ///
    /// New sessions used to be created with `cfg.workspace` - the value the config
    /// file had at startup - so the move was quietly undone for anything you did
    /// afterwards. That is exactly what "it goes back to cwoffee-workspace" was.
    #[tokio::test]
    async fn a_new_session_inherits_the_current_workspace() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_inherit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let inner = base.join("chosen");
        std::fs::create_dir_all(&inner).unwrap();

        let (mut h, store) = host(vec![]);
        let _rx = h.take_receivers();
        ready(&h).await;

        let got = h
            .set_workspace_checked(&inner.display().to_string())
            .await
            .expect("a real writable directory should be accepted");
        assert_eq!(got, inner.display().to_string());

        h.send(HostCmd::NewSession { title: "after the move".into() });
        wait_for("a session row".into(), || h.state().session_id != 0).await;
        let sid = h.state().session_id;
        let row = store.session(sid).unwrap().expect("session row");
        assert_eq!(
            row.workspace,
            inner.display().to_string(),
            "the new session reverted to the config workspace"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The command channel is fire-and-forget, so a refused path used to be
    /// invisible to the caller: the UI showed a workspace that snapped back with
    /// no reason. `set_workspace_checked` has to name the reason.
    #[tokio::test]
    async fn set_workspace_checked_names_the_refusal() {
        let (mut h, _store) = host(vec![]);
        let _rx = h.take_receivers();
        ready(&h).await;

        let err = h
            .set_workspace_checked("/nonexistent/definitely-not")
            .await
            .expect_err("a non-directory must not be accepted");
        assert!(err.contains("refused"), "unhelpful error: {err}");
        assert!(
            err.contains("/nonexistent/definitely-not"),
            "the error should name the path: {err}"
        );
        assert_eq!(
            h.state().workspace,
            std::env::temp_dir().display().to_string(),
            "a refused change must leave the workspace alone"
        );
    }

    #[tokio::test]
    async fn an_attachment_survives_host_prompt_and_store() {
        let store = Arc::new(Store::in_memory().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["noted".into()]),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::clone(&seen),
        });
        let h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        ready(&h).await;
        h.send(HostCmd::Send {
            text: "what is in this image?".into(),
            attachments: vec![crate::provider::Attachment {
                mime: "image/png".into(),
                data_b64: "aGVsbG8=".into(),
                name: Some("shot.png".into()),
            }],
        });
        turns_done(&h, 1).await;
        wait_for("provider saw the image".into(), || {
            seen.load(Ordering::Relaxed) == 1
        })
        .await;
        // The row only exists once the first message has been sent.
        let sid = h.state().session_id;
        let hist = store.history(sid).unwrap();
        assert_eq!(hist[0].attachments.len(), 1, "payload must persist for replay");
        assert_eq!(hist[0].attachments[0].data_b64, "aGVsbG8=");
    }

    #[test]
    fn attachment_guardrails_reject_bad_input_at_the_host_boundary() {
        let l = AttachmentLimits { max_count: 2, max_bytes: 64 };
        let a = |mime: &str, b64: &str| crate::provider::Attachment {
            mime: mime.into(),
            data_b64: b64.into(),
            name: None,
        };
        assert!(check_attachments(&[a("image/png", "aGVsbG8=")], l).is_ok());
        let e = check_attachments(&[a("application/x-msdownload", "aGk=")], l).unwrap_err();
        assert!(e.contains("not allowed"), "{e}");
        let e = check_attachments(
            &[a("image/png", "aGk="), a("image/png", "aGk="), a("image/png", "aGk=")],
            l,
        )
        .unwrap_err();
        assert!(e.contains("limit of 2"), "{e}");
        // A data: URL passed through verbatim is the most likely UI mistake.
        let e = check_attachments(&[a("image/png", "data:image/png;base64,aGk=")], l).unwrap_err();
        assert!(e.contains("not valid base64"), "{e}");
        let big = "A".repeat(200);
        let e = check_attachments(&[a("image/png", &big)], l).unwrap_err();
        assert!(e.contains("over the 64 byte limit"), "{e}");
    }

    /// The host runs on its own task, so reading `state()` straight after
    /// `send()` races it. Every test waits on an observable condition instead
    /// of sleeping.
    /// `FnMut` rather than `Fn` so a poll can drain the event channel while it
    /// waits. A condition that is already true would otherwise let the caller
    /// inspect the channel before the host has written anything to it.
    async fn wait_for(what: String, mut f: impl FnMut() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The host starts in draft state: no session row exists until the first
    /// message is sent, so there is nothing to wait for here. Kept as a named
    /// call site so tests read as "host is up" rather than a bare no-op.
    async fn ready(_h: &SessionHost) {}


    /// Waiting on `busy` is wrong: a fast turn can finish before the first
    /// poll, so `busy` is never observed as true.
    async fn turns_done(h: &SessionHost, n: u64) {
        wait_for(format!("{n} turns"), move || h.state().turns >= n).await;
    }

    /// Poll for an `Error` whose message contains `needle`. `wait_for` takes a
    /// sync closure, so an async drain needs its own polling loop.
    async fn await_error(rx: &mut EventRx, needle: &str) -> bool {
        for _ in 0..400 {
            if drain_events(rx).await.iter().any(|e| {
                matches!(e, EngineEvent::Error { message, .. } if message.contains(needle))
            }) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    async fn drain_events(rx: &mut EventRx) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn a_turn_is_streamed_and_persisted() {
        let (mut h, store) = host(vec!["hello there".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        h.submit("first question");
        turns_done(&h, 1).await;

        let evs = drain_events(&mut ev).await;
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

    /// Turn numbering is per conversation. Without the reset, a brand-new
    /// session opened at "turn 4" because the counter lived on the Agent.
    #[tokio::test]
    async fn turn_numbering_restarts_in_a_different_session() {
        let (mut h, _store) = host(vec!["one".into(), "two".into()]);
        ready(&h).await;
        let (mut ev, _arx) = h.take_receivers().expect("receivers");
        let ids: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&ids);
        tokio::spawn(async move {
            while let Some(e) = ev.recv().await {
                if let EngineEvent::TurnStart { turn_id, .. } = e {
                    sink.lock().unwrap().push(turn_id);
                }
            }
        });

        let a = h.state().session_id;
        h.submit("first in a");
        turns_done(&h, 1).await;

        h.send(HostCmd::NewSession { title: "b".into() });
        wait_for("new session".into(), || h.state().session_id != a).await;
        h.submit("first in b");
        turns_done(&h, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(*ids.lock().unwrap(), vec![1, 1], "turn ids must restart per session");
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
    async fn set_model_changes_later_turns_but_not_history() {
        let (mut h, store) = host(vec!["a".into(), "b".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        h.send(HostCmd::SetModel { model: "second-model".into() });
        wait_for("model change".into(), || h.state().model == "second-model").await;

        h.submit("after switch");
        turns_done(&h, 1).await;
        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::TurnStart { model, .. } if model == "second-model")),
            "turn should report the new model: {evs:?}"
        );
        // History is provider-agnostic: switching mid-session must not rewrite it.
        assert_eq!(store.history(h.state().session_id).unwrap().len(), 2);

        // An empty id would send `"model": ""` and produce a confusing 400 from
        // the endpoint instead of an obvious local error.
        h.send(HostCmd::SetModel { model: "   ".into() });
        let mut rejected = false;
        for _ in 0..400 {
            if drain_events(&mut ev).await.iter().any(|e| {
                matches!(e, EngineEvent::Error { message, .. } if message.contains("empty"))
            }) {
                rejected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(rejected, "an empty model id must be reported, not ignored");
        assert_eq!(h.state().model, "second-model", "rejection must not clear the model");
    }

    #[tokio::test]
    async fn approval_channel_stays_open_and_unused_when_no_tools_are_called() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["tool".into()]),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
        });
        let mut reg = Registry::new();
        reg.register(Box::new(ExecTool { timeout_secs: 5, allow_unsandboxed: false }));

        let mut h = SessionHost::spawn(
            p,
            reg,
            Arc::clone(&store),
            HostConfig { mode: ApprovalMode::Ask, ..HostConfig::new("m", std::env::temp_dir()) },
        );
        let (mut ap, _ev) = h.take_receivers().map(|(e, a)| (a, e)).expect("receivers");
        ready(&h).await;
        h.submit("say hi");
        turns_done(&h, 1).await;
        assert!(ap.try_recv().is_err(), "unexpected approval request");
    }

    #[tokio::test]
    async fn session_changes_are_announced_so_the_ui_reloads_in_order() {
        // The UI must not read the store on its own initiative after switching:
        // the host owns when the switch lands. Without this event the webview
        // can render the previous session's transcript under the new session's
        // name, which is a confusing and easy-to-miss bug.
        let (mut h, _store) = host(vec![]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        let first = h.state().session_id;

        h.send(HostCmd::NewSession { title: "beta".into() });
        wait_for("new session id".into(), || h.state().session_id != first).await;
        let evs = drain_events(&mut ev).await;
        let got: Vec<(i64, String)> = evs
            .iter()
            .filter_map(|e| match e {
                EngineEvent::SessionChanged { session_id, title } => {
                    Some((*session_id, title.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), 1, "expected exactly one announcement, got {evs:?}");
        assert_eq!(got[0].1, "beta");

        h.send(HostCmd::SwitchSession { id: first });
        wait_for("switch back".into(), || h.state().session_id == first).await;
        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::SessionChanged { session_id, .. } if *session_id == first)),
            "switching must also announce: {evs:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_stops_the_host() {
        let (mut h, _store) = host(vec!["ok".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.send(HostCmd::Shutdown);
        // the events channel closes once the host task returns
        assert!(ev.recv().await.is_none());
    }

    #[test]
    fn env_block_names_the_sandbox_so_the_model_does_not_guess() {
        let b = default_env_block(std::path::Path::new("/home/u/ws"), "linux");
        assert!(b.contains("/home/u/ws"));
        assert!(b.contains("no network"));
        assert!(b.contains("sandbox"));
    }
}
