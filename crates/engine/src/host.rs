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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, AgentConfig};
use crate::approval::{ApprovalAnswer, ApprovalGate, ApprovalMode, ApprovalRequest};
use crate::config::Config;
use crate::event::EngineEvent;
use crate::prompt::PromptBuilder;
use crate::provider::{Provider, Reasoning, Role};
use crate::settings::{SearchProbe, SearchView};
use crate::store::Store;
use crate::telemetry::TurnTelemetry;
use crate::tools::{Registry, ToolCtx};

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub model: String,
    pub workspace: PathBuf,
    pub mode: ApprovalMode,
    pub reasoning: Reasoning,
    /// Cap on provider round-trips in one turn; 0 = no cap. See AgentConfig: the
    /// old hardcoded 14 acted as a work budget and cut real tasks short.
    pub max_tool_iters: usize,
    pub max_tokens: Option<u32>,
    /// Prompt token budget. Compaction is expensive (it throws away the prefix
    /// cache), so this should sit near the server's context window, not below it.
    ///
    /// `None` derives it from the model's advertised window. A hard 8 000 sat in
    /// front of a 262 144-token model and compacted 43k-token conversations while
    /// reporting "context full" at 16% occupancy.
    pub context_budget: Option<usize>,
    /// Volatile context appended to the newest user message, **only** if the
    /// caller wants something other than what the host would derive.
    ///
    /// `None` - the normal case - means "describe the folder the tools actually
    /// run in", which the host recomputes before every turn. A startup snapshot
    /// here used to be re-installed on new sessions and clears, so a session in
    /// ~/Projects told the model it was working in ~/cwoffee-workspace while its
    /// `pwd` said otherwise.
    pub env_override: Option<String>,
    /// How long an unanswered approval prompt waits before refusing.
    pub approval_timeout: std::time::Duration,
    /// How long an `ask_user` card stays open. `question_timeout_secs` in
    /// config.toml; three minutes is right for someone at the keyboard and wrong
    /// for a question they have to take to someone else.
    pub question_timeout: std::time::Duration,
    /// Attachment guardrails. Enforced at the host boundary rather than in the
    /// UI, because the UI is not the only client and never will be again.
    pub max_attachments: usize,
    /// Decoded bytes per attachment.
    pub max_attachment_bytes: usize,
    /// Sampling overrides for the endpoint. Empty means the server decides, which
    /// is the default and the honest one: these numbers change what the model
    /// does, and a harness that sets them silently changes the product.
    pub sampling: crate::provider::Sampling,
    /// Where choices made through the UI get written, when there is a file to
    /// write them to.
    ///
    /// `None` (the default, and what every test uses) means the host is ephemeral:
    /// a mode picked in the window is forgotten at the next start. The shells set
    /// this to `config.toml`, because a toggle the user can see is a toggle they
    /// expect to still be there tomorrow - and because two clients of one engine
    /// otherwise disagree with each other about what the app is doing.
    pub cfg_path: Option<PathBuf>,
    /// What the exec sandbox may reach over the network, mirrored from
    /// `[sandbox] network` in config.toml.
    ///
    /// The registry already knows (the exec tool was built from ExecCfg), but the
    /// prompt is written by the agent, and letting the prompt guess produced the
    /// most confidently wrong sentence in the system prompt: "no network access"
    /// on a product that defaults to an allowlist proxy.
    pub egress: crate::tools::net::NetMode,
}

impl HostConfig {
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            workspace: workspace.into(),
            mode: ApprovalMode::Ask,
            reasoning: Reasoning::XHigh,
            // Uncapped by default; set max_tool_iters in config.toml for a hard stop.
            max_tool_iters: 0,
            max_tokens: None,
            context_budget: None,
            env_override: None,
            approval_timeout: std::time::Duration::from_secs(300),
            question_timeout: std::time::Duration::from_secs(180),
            max_attachments: DEFAULT_MAX_ATTACHMENTS,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            sampling: crate::provider::Sampling::default(),
            cfg_path: None,
            egress: crate::tools::net::NetCfg::default().mode,
        }
    }
}

/// Budget when the caller did not pin one **and the endpoint would not say its
/// window**.
///
/// This used to be 8 000, and it was a wrecking ball: a resumed session of 61
/// messages (~53k tokens) got trimmed to 4k against a model that had told the
/// model picker it holds 262 144, because this endpoint had not answered
/// `/models` at the moment the host asked. Compacting against a number nobody
/// measured is how gold gets thrown away, so the fallback is now large enough
/// not to bite a normal conversation, and the host re-asks the endpoint every
/// turn until it answers.
/// What a session's context meter shows when neither the user pinned a budget nor
/// the endpoint told us the model's window. `pub` because the pi core has to agree:
/// two cores showing different context meters on one screen is the same drift the
/// mode and reasoning persistence closed, one dial further along.
pub const FALLBACK_BUDGET: usize = 32_000;

/// The ceiling the engine gives itself for a window: a share of what the endpoint
/// states, never more, so a prompt cannot overflow the model and have the server
/// truncate it from the front with no note attached.
pub fn auto_budget(window: usize) -> usize {
    // 80% of the window. The other 20% is the reply plus the estimate's own
    // error: chars/3.5 is close on prose and short on code. The failure this is
    // guarding against is not "we compacted too early" - it is a prompt that
    // overflows the window, which the server truncates from the front with no
    // note attached to it at all.
    ((window as f64 * 0.80) as usize).max(4_096)
}

/// Read the loaded model's advertised window and set the compaction budget.
///
/// `announce` is false on a re-read: a boot-shaped line printed once per turn
/// while an endpoint is unreachable is noise that hides the one line that matters.
async fn apply_context_budget(
    agent: &mut Agent,
    provider: &Arc<dyn crate::provider::Provider>,
    pinned: Option<usize>,
    state: &Arc<std::sync::Mutex<HostState>>,
    sampling: &crate::provider::Sampling,
    gauge: &Arc<std::sync::Mutex<crate::prompt::ContextGauge>>,
    announce: bool,
) {
    let model = agent.model();
    let info = provider
        .list_models_info()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.id == model);
    let window = info.as_ref().and_then(|m| m.context_tokens).map(|t| t as usize);

    let budget = pinned.unwrap_or_else(|| window.map(auto_budget).unwrap_or(FALLBACK_BUDGET));
    let compactions = {
        let p = agent.prompt_mut();
        p.token_budget = budget;
        p.context_window = window;
        p.compactions
    };
    {
        let mut g = gauge.lock().unwrap();
        g.budget = budget;
        g.window = window;
        g.compactions = compactions;
        g.tokens = agent.prompt_mut().estimate_tokens("");
    }
    let mut s = state.lock().unwrap();
    s.context_budget = budget;
    s.model_window = window;
    drop(s);
    if announce {
        eprintln!(
            "[cwoffee] context budget {budget} tokens (model window {}, budget {})",
            window.map(|w| w.to_string()).unwrap_or_else(|| "unknown".into()),
            if pinned.is_some() { "pinned in config" } else { "derived" }
        );
        // Sampling is a claim about behaviour, so the boot line states both halves:
        // what this app sends and what the endpoint says it uses when nothing is
        // sent. A user who never opens `[sampling]` should still be able to see that
        // their server is running at temperature 1.0.
        eprintln!(
            "[cwoffee] sampling: {}",
            sampling.describe(info.as_ref().map(|m| &m.sampling))
        );
    } else {
        eprintln!(
            "[cwoffee] context budget re-read: {budget} tokens (model window {})",
            window.map(|w| w.to_string()).unwrap_or_else(|| "still unknown".into())
        );
    }
}

/// Write one top-level setting to the config file the shells pointed us at.
///
/// Written *before* the live value is announced, for the same reason the search
/// panel does it that way: if the running app and `config.toml` disagree, then
/// one of them is lying, and the one the user reads is the file.
fn persist_setting(path: &Option<std::path::PathBuf>, key: &str, toml_value: &str) {
    let Some(p) = path.as_ref() else { return };
    // `model` belongs to `[provider]`; everything else here is top-level. Writing a
    // bare `model` at the top level - which is what this used to do - produced a file
    // the next start could not read, because the parser rejects keys it has not
    // declared and it had not declared that one. `Config::set_keys` now re-parses the
    // edited text before replacing the file, so the same mistake is a refused write
    // instead of an app that will not boot.
    let wrote = if key == "model" {
        crate::config::Config::set_provider_keys(p, &[(key, toml_value)])
    } else {
        crate::config::Config::set_top_keys(p, &[(key, toml_value)])
    };
    match wrote {
        Ok(changed) => {
            for c in changed {
                eprintln!("[cwoffee] setting saved: {c} ({})", p.display());
            }
        }
        Err(e) => eprintln!(
            "[cwoffee] setting NOT saved ({key} = {toml_value}): {e}. \
             This start keeps the choice; the next one will not."
        ),
    }
}

fn context_budget_note(state: &Arc<std::sync::Mutex<HostState>>, model: &str) -> String {    let s = state.lock().unwrap();
    match s.model_window {
        Some(w) => format!(
            "{model}: {w} token window, compaction budget {} tokens",
            s.context_budget
        ),
        None => format!(
            "{model} did not state a context window; compaction budget {} tokens",
            s.context_budget
        ),
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
    if is_temp_root(&expanded) {
        return Err(
            "is the system temp folder, which is where sessions with no workspace live; pick a real project folder"
                .into(),
        );
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

/// Is this path the system temp folder itself?
///
/// Not merely a *use* of it: a session under `/tmp/build/output` is fine. The root
/// is the case that matters, because it is the bucket this app calls "no
/// workspace" - the default a session sits in before a folder is chosen, and the
/// place older sessions were left in. Letting it also be a workspace would split
/// those same sessions across two sidebar groups and make the grouping a guess.
pub fn is_temp_root(raw: &str) -> bool {
    let p = std::path::Path::new(raw.trim());
    let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    [std::env::temp_dir(), std::path::PathBuf::from("/tmp"), std::path::PathBuf::from("/var/tmp"), std::path::PathBuf::from("/private/tmp")]
        .iter()
        .any(|t| std::fs::canonicalize(t).map(|c| c == canon).unwrap_or(false))
}

/// Refuse a workspace change in a way the caller can read.
fn refuse_workspace(
    ev_tx: &mpsc::UnboundedSender<EngineEvent>,
    state: &Arc<Mutex<HostState>>,
    path: &str,
    reason: &str,
) {
    {
        let mut st = state.lock().unwrap();
        st.refusal_seq += 1;
        st.workspace_refusal = Some(format!("{path}: {reason}"));
    }
    let _ = ev_tx.send(EngineEvent::Error {
        message: format!("the engine refused {path:?}: {reason}"),
        fatal: false,
    });
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
        /// Which conversation this turn belongs to.
        ///
        /// `None` means "wherever the host is" - correct for the desktop's own
        /// composer, which *is* the host's view. A number is an assertion: "I am
        /// looking at this conversation". `0` means "I am on a draft", so the host
        /// drops to draft state and the send creates the row. Any other number that
        /// does not match the host's session is **refused**.
        ///
        /// The refusal is the point. With one host and many clients - desktop,
        /// phone, harness - an untargeted send lands in whichever conversation the
        /// last switch left active. From the user's chair that is indistinguishable
        /// from the transcript being edited behind their back.
        session: Option<i64>,
    },
    SetMode { mode: ApprovalMode },
    SetReasoning { reasoning: Reasoning },
    /// Change the model id for subsequent turns. Does not rewrite history.
    ///
    /// `persist` is false when the host chose the id itself (an empty config
    /// resolved against the endpoint's list): writing that back would pin whatever
    /// happened to be first in the list, which is not a choice anybody made.
    SetModel { model: String, persist: bool },
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
    /// The compaction budget in force, and the window it came from. Without these
    /// a compaction message is a claim with nothing to check it against.
    #[serde(default)]
    pub context_budget: usize,
    #[serde(default)]
    pub model_window: Option<usize>,
    /// Seconds before an unanswered approval fails closed. The UI shows this as a
    /// countdown: a turn blocked on a human looks identical to a turn still
    /// working, and that difference is the entire message.
    pub approval_timeout_secs: u64,
    /// Seconds an unanswered `ask_user` card stays open, same reason as the approval
    /// countdown: a turn parked on a person and a turn still working look identical.
    #[serde(default)]
    pub question_timeout_secs: u64,
    /// Why the last workspace change was refused, and a counter so a caller that
    /// polls `state()` can tell a fresh refusal from an old one.
    ///
    /// `send()` is fire-and-forget, so without this the only trace of a refusal is
    /// an error block in the transcript, and `set_workspace_checked` has to guess
    /// the reason from the fact that the path did not apply.
    #[serde(default)]
    pub workspace_refusal: Option<String>,
    #[serde(default)]
    pub refusal_seq: u64,
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
    /// Written by the agent, read by `state()`. See `ContextGauge`.
    gauge: Arc<Mutex<crate::prompt::ContextGauge>>,
    cancel: Arc<AtomicBool>,
    /// The host keeps a handle so the shell can ask the endpoint questions that
    /// are not turns - the composer's model picker being the obvious one.
    provider: Arc<dyn Provider>,
    /// And the search tool, typed: the provider can change while the app runs, and
    /// the only safe place to change it is inside the live tool. Rebuilding the
    /// registry would move prompt bytes and drop every prefix cache, and the
    /// settings card would then be a button that costs the next turn a cold prefill
    /// in every session.
    search: Option<Arc<crate::tools::web::WebSearchTool>>,
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
            question_timeout_secs: cfg.question_timeout.as_secs(),
            workspace_refusal: None,
            refusal_seq: 0,
            context_budget: cfg.context_budget.unwrap_or(FALLBACK_BUDGET),
            model_window: None,
        }));
        // What the prompt costs, owned here and written by the agent. `state()`
        // reads from this rather than from the snapshot, so a client that asks
        // mid-turn gets the number the next prefill will pay instead of the one
        // left over from the end of the last turn.
        let gauge = Arc::new(Mutex::new(crate::prompt::ContextGauge {
            budget: cfg.context_budget.unwrap_or(FALLBACK_BUDGET),
            window: None,
            ..Default::default()
        }));
        let limits = AttachmentLimits {
            max_count: cfg.max_attachments,
            max_bytes: cfg.max_attachment_bytes,        };

        let state_bg = Arc::clone(&state);
        let gauge_bg = Arc::clone(&gauge);
        // The task takes the provider; the host keeps a second handle for
        // non-turn questions like the model list.
        let host_provider = Arc::clone(&provider);
        // The loop task needs a handle too: the compaction budget comes from the
        // window the *loaded* model advertises, which is a provider question.
        let budget_provider = Arc::clone(&provider);

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
            question_timeout: Some(cfg.question_timeout),
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
                egress: cfg.egress,
                max_tokens: cfg.max_tokens,
                sampling: cfg.sampling.clone(),
                gauge: gauge.clone(),
            },
            ctx,
        );
        // Taken before the agent is built? No - after: the registry was moved into
        // the agent, and the agent hands out the handle it was built with.
        let search_tool = agent.search_tool();
        agent.prompt_mut().token_budget = cfg.context_budget.unwrap_or(FALLBACK_BUDGET);
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
            // Sessions already told about a resumed history. Saying it once per
            // session per process is enough; the user re-clicking a row in the
            // sidebar does not make the cache warmer.
            let mut resume_announced: std::collections::HashSet<i64> = std::collections::HashSet::new();

            {
                let mut s = state_bg.lock().unwrap();
                s.session_id = session_id;
            }

            // Everything that changes when the host starts working on a different
            // conversation: load its history, follow the folder its row names,
            // rebind the persistence target, and tell every client. Two callers
            // need it - an explicit switch, and a turn addressed to a conversation
            // the host is not on - and a copy of this in each is how the two would
            // drift apart, which is exactly how "my conversation got mutated"
            // happened once already.
            macro_rules! adopt_view {
                ($target:expr) => {{
                    let id = $target;
                    // A row that does not exist is not an empty conversation:
                    // `history()` answers an empty list for either, and switching
                    // to a phantom id leaves the host reporting a session nothing
                    // can load, with the next turn appended into nowhere.
                    let real = matches!(store.session(id), Ok(Some(_)));
                    if !real {
                        let _ = ev_tx.send(EngineEvent::Error {
                            message: format!(
                                "session {id} does not exist: nothing was sent, and the host stays on session {session_id}"
                            ),
                            fatal: false,
                        });
                        false
                    } else {
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
                                            let _ = ev_tx.send(EngineEvent::Note {
                                                session: Some(id),
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
                            // Publish before the cold-cache note reads it: the note
                            // and the toolbar must quote the same number, and the
                            // number has to be the one this conversation actually
                            // costs, not the one the previous one cost.
                            agent.publish_gauge("");
                            if let Some(p) = follow_ws {
                                s.workspace = p.display().to_string();
                            }
                            let title = row.map(|x| x.title).unwrap_or_default();
                            let _ = ev_tx.send(EngineEvent::SessionChanged {
                                session_id: id,
                                title,
                            });
                            // Prefix-cache state is only interesting when it
                            // costs something, so an empty or freshly created
                            // session says nothing at all: the unconditional
                            // warning used to mark every New Session as if
                            // something had gone wrong. It is a note rather
                            // than a warning for the same reason - a cold cache
                            // is the expected state after a restart, not a
                            // fault.
                            if n > 0 && resume_announced.insert(id) {
                                // `s` already holds the state lock; re-entering
                                // it here deadlocks the host task (Trap 30).
                                let est = s.context_tokens;
                                let budget = s.context_budget;
                                let _ = ev_tx.send(EngineEvent::Note {
                                    session: Some(id),
                                    message: format!(
                                        "resumed session {id}: {n} message(s), ~{est} tokens of a \
                                         {budget} token budget. Whether the endpoint still holds this \
                                         session's prefix cache is its own business; the first reply \
                                         settles it in numbers.",
                                    ),
                                });
                            }
                            true
                        }
                        Err(e) => {
                            let _ = ev_tx.send(EngineEvent::Error {
                                message: format!("cannot load session {id}: {e}"),
                                fatal: false,
                            });
                            false
                        }
                        }
                    }
                }};
            }

            // Ask the endpoint what the model can actually hold, once, before the
            // first turn. Everything else about the budget follows from that.
            apply_context_budget(
                &mut agent,
                &budget_provider,
                cfg.context_budget,
                &state_bg,
                &cfg.sampling,
                &gauge_bg,
                true,
            ).await;

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
                        // Clearing the transcript must not reset the sizing: the
                        // budget belongs to the model, not to the conversation.
                        {
                            let (b, w) = {
                                let st = state_bg.lock().unwrap();
                                (st.context_budget, st.model_window)
                            };
                            let p = agent.prompt_mut();
                            p.token_budget = b;
                            p.context_window = w;
                        }
                                        {
                            let mut s = state_bg.lock().unwrap();
                            s.session_id = 0;
                            s.context_tokens = 0;
                            agent.publish_gauge("");
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
                                refuse_workspace(&ev_tx, &state_bg, &path, &reason);
                                continue;
                            }
                        };
                        // A session keeps the folder it was started in. The tools
                        // are confined to that folder and the sidebar groups the
                        // session under it, so moving it mid-session makes the
                        // grouping a decoration and writes files into a tree the
                        // user never chose for this conversation. Folders are
                        // changed by starting a session in one, not by moving one
                        // out from under a session that already has one.
                        if session_id != 0 {
                            let current = store
                                .session(session_id)
                                .ok()
                                .flatten()
                                .map(|s| s.workspace.trim().to_string())
                                .unwrap_or_default();
                            if !current.is_empty() && !is_temp_root(&current) {
                                let same = std::fs::canonicalize(&current)
                                    .map(|c| c == p)
                                    .unwrap_or_else(|_| std::path::PathBuf::from(&current) == p);
                                if !same {
                                    refuse_workspace(
                                        &ev_tx,
                                        &state_bg,
                                        &path,
                                        &format!(
                                            "this session works in {current}; a session keeps its folder, so start a new session to work somewhere else"
                                        ),
                                    );
                                    continue;
                                }
                            }
                        }
                        // Moving the tools into a folder registers it, so the
                        // sidebar shows where you now work without you having to
                        // add it by hand.
                        let _ = store.add_workspace(&p.display().to_string());
                        agent.set_workspace(p.clone());
                        let _ = store.set_session_workspace(session_id, &p);
                        state_bg.lock().unwrap().workspace = p.display().to_string();
                        let _ = ev_tx.send(EngineEvent::Note {
                            session: Some(session_id),
                            message: format!("workspace is now {}", p.display()),
                        });
                    }
                    HostCmd::SetModel { model, persist } => {
                        // Empty would send `"model": ""` to the endpoint, which
                        // is a confusing 400 rather than an obvious bug.
                        if model.trim().is_empty() {
                            let _ = ev_tx.send(EngineEvent::Error {
                                message: "model id cannot be empty".into(),
                                fatal: false,
                            });
                        } else {
                            agent.set_model(model.clone());
                            let model_now = agent.model();
                            state_bg.lock().unwrap().model = model;
                            if persist {
                                persist_setting(&cfg.cfg_path, "model", &Config::toml_str(&model_now));
                            }
                            // Different model, different window. A budget carried
                            // over from the previous model is how a compaction
                            // warning shows up in a session that is nowhere near
                            // full.
                            apply_context_budget(
                                &mut agent,
                                &budget_provider,
                                cfg.context_budget,
                                &state_bg,
                                &cfg.sampling,
                                &gauge_bg,
                                true,
                            ).await;
                            // Read the session id in its own statement. A guard
                            // created inside a call's argument list lives until the
                            // end of the *statement*, so writing it inline above
                            // held the state lock while context_budget_note() asked
                            // for the same lock - and the host task deadlocked,
                            // which is how this test sat for twenty minutes.
                            let sess_now = state_bg.lock().unwrap().session_id;
                            let _ = ev_tx.send(EngineEvent::Note {
                                session: Some(sess_now),
                                message: context_budget_note(&state_bg, &model_now),
                            });
                        }
                    }

                    HostCmd::SetMode { mode } => {
                        agent.set_mode(mode);
                        state_bg.lock().unwrap().mode = mode;
                        persist_setting(&cfg.cfg_path, "mode", &Config::toml_str(mode.as_str()));
                    }

                    HostCmd::SetReasoning { reasoning } => {
                        agent.set_reasoning(reasoning);
                        state_bg.lock().unwrap().reasoning = reasoning;
                        let word = serde_json::to_value(reasoning)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_else(|| "low".into());
                        persist_setting(&cfg.cfg_path, "reasoning", &Config::toml_str(&word));
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
                                    // The budget resolved for the loaded model, not
                                    // the config default: a new session in the same
                                    // process should not re-derive it, and a pinned
                                    // value is already in there.
                                    let (b, w) = {
                                        let st = state_bg.lock().unwrap();
                                        (st.context_budget, st.model_window)
                                    };
                                    p.token_budget = b;
                                    p.context_window = w;
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
                        // An explicit request to work on this conversation. The UI
                        // no longer sends it for looking: viewing a session and
                        // making it the one the engine works on are different acts,
                        // and conflating them meant every click printed a workspace
                        // note and a cache warning into the transcript.
                        adopt_view!(id);
                    }

                    HostCmd::Send { text, attachments, session } => {
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
                        // "I am on a draft" - even though the host is not. Without
                        // this, a client whose view is empty sends into the
                        // conversation the host happens to be on, which is the bug
                        // this whole branch exists to kill.
                        if session == Some(0) && session_id != 0 {
                            session_id = 0;
                            persisted_next = 0;
                            agent.replace_prompt(PromptBuilder::new());
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
                        if let Some(want) = session {
                            if want > 0 && want != session_id {
                                // The turn is addressed to a conversation the host
                                // is not on. Since browsing no longer moves the host,
                                // sending is what moves it: adopt the addressed
                                // conversation, then run the turn there. A turn is
                                // only ever added to the conversation it names, and
                                // if that conversation cannot be loaded, nothing is
                                // sent at all.
                                if !adopt_view!(want) {
                                    continue;
                                }
                            }
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
                        // The env block says where the tools run. Derive it from
                        // the workspace the host settled on a moment ago - after
                        // any adoption, draft creation or workspace move - rather
                        // than from anything captured at startup.
                        let env = cfg.env_override.clone()
                            .unwrap_or_else(|| {
                                default_env_block(agent.workspace(), "linux", cfg.egress)
                            });
                        agent.prompt_mut().set_env(env);

                        set_busy(&state_bg, true);
                        // The window is the one fact the whole budget is derived
                        // from. If the endpoint was busy starting up when the host
                        // asked at boot, asking again here costs two GETs and is
                        // the difference between a 209k budget and a 32k one - and
                        // the smaller number is the one that deletes history.
                        if cfg.context_budget.is_none()
                            && agent.prompt_mut().context_window.is_none()
                        {
                            apply_context_budget(
                                &mut agent,
                                &budget_provider,
                                cfg.context_budget,
                                &state_bg,
                                &cfg.sampling,
                                &gauge_bg,
                                false,
                            ).await;
                        }
                        let tel: TurnTelemetry =
                            agent.run_turn(&text, attachments, &ev_tx, session_id).await;

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
                            agent.publish_gauge("");
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
            gauge,
            cancel,
            provider: host_provider,
            search: search_tool,
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
        self.send(HostCmd::SetModel { model: first.clone(), persist: false });
        Some(first)
    }

    /// Handle to the endpoint, for shell-level queries that are not turns.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// What search is currently doing, in one line ("DuckDuckGo (keyless HTML
    /// endpoint)", "SearXNG managed by Cwoffee on port 8899").
    pub fn search_backend(&self) -> Option<crate::tools::web::SearchBackend> {
        self.search.as_ref().map(|s| s.backend())
    }

    /// Switch the provider on the live tool and answer with what it became, so the
    /// caller confirms a change rather than assuming one (`set_workspace` taught
    /// that lesson: fire-and-forget left the UI showing something that had not
    /// happened). Safe mid-turn: a search reads the backend when it starts.
    pub fn set_search_backend(&self, b: crate::tools::web::SearchBackend) -> Option<String> {
        self.search.as_ref().map(|s| {
            s.set_backend(b);
            s.backend().describe()
        })
    }

    /// The live ephemeral-SearXNG handle, if that backend is selected. The clone
    /// shares the idle clock, so the settings card reports the instance the tool
    /// actually started rather than a copy that has never been used.
    pub fn search_auto(&self) -> Option<crate::tools::searxng::AutoSearXNG> {
        match self.search_backend()? {
            crate::tools::web::SearchBackend::Auto(a) => Some(a),
            _ => None,
        }
    }

    pub fn search_tool(&self) -> Option<Arc<crate::tools::web::WebSearchTool>> {
        self.search.clone()
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

    /// Send a message that carries attachments, into the host's current session.
    pub fn send_with(
        &self,
        text: impl Into<String>,
        attachments: Vec<crate::provider::Attachment>,
    ) {
        self.send(HostCmd::Send { text: text.into(), attachments, session: None })
    }

    /// Send a turn *into a named conversation*. Front ends should use this: it is
    /// what stops a phone tab or a test harness from writing into the conversation
    /// the desktop happens to be looking at.
    pub fn send_turn_to(
        &self,
        text: impl Into<String>,
        attachments: Vec<crate::provider::Attachment>,
        session: Option<i64>,
    ) {
        self.send(HostCmd::Send { text: text.into(), attachments, session })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn state(&self) -> HostState {
        let mut s = self.state.lock().unwrap().clone();
        // The gauge outranks the snapshot for these four: it is written by the
        // thing that actually builds the prompt, and it is written while a turn
        // is running rather than only when one ends.
        let g = self.gauge.lock().unwrap();
        s.context_tokens = g.tokens;
        s.context_budget = g.budget;
        s.model_window = g.window;
        s.compactions = g.compactions;
        s
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
        let seq_before = self.state().refusal_seq;
        self.send(HostCmd::SetWorkspace { path: path.to_string() });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let st = self.state();
            if std::path::PathBuf::from(&st.workspace) == want {
                return Ok(st.workspace)
            }
            // The host refused it, and said why. Reading that beats the old
            // guess, which blamed writability for refusals that were really
            // "this session already has a folder".
            if st.refusal_seq > seq_before {
                return Err(st
                    .workspace_refusal
                    .clone()
                    .unwrap_or_else(|| "the engine refused that folder".into()));
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
/// The slice of a core that a **client** drives.
///
/// This is the seam that lets a second core (a `pi` RPC bridge) sit behind the
/// desktop window and the phone client without either front end learning that the
/// engine underneath changed. The surface is measured, not guessed: the gateway
/// calls **nine** verbs and `crates/tauri-app` calls five of those same nine
/// (`state`, `send`, `send_turn_to`, `cancel`, `set_workspace_checked`), so this
/// trait is that surface written down — not a new abstraction.
///
/// Keep it small on purpose. Every verb added here is a promise that *both* cores
/// must keep, in the same way, forever. In particular, do not widen it with things
/// that are really engine internals (`provider`, `search_tool`, `take_receivers`):
/// the moment a client reaches through this trait for a `dyn Provider`, the
/// pi-backed implementation has to invent one, and the seam stops meaning anything.
#[async_trait]
pub trait HostOps: Send + Sync {
    /// The command mailbox. See [`HostCmd`] — including the session assertion on
    /// `Send`, which this trait cannot and must not weaken: an unaddressed send
    /// lands in whatever conversation the last switch left active.
    fn send(&self, cmd: HostCmd);

    /// A send addressed to a specific conversation. `session: None` means "wherever
    /// the host is"; a number is an assertion, and refusing a mismatched one is what
    /// keeps a stale phone tab from appending to the desktop's chat. Both cores have
    /// to refuse it the same way.
    fn send_turn_to(
        &self,
        text: String,
        attachments: Vec<crate::provider::Attachment>,
        session: Option<i64>,
    );

    /// Stop the running turn and release whatever it is parked on.
    fn cancel(&self);

    /// Snapshot for status lines and the composer's current-state labels.
    fn state(&self) -> HostState;

    /// Move the tools to another workspace, refusing anything the checks reject.
    async fn set_workspace_checked(&self, path: &str) -> Result<String, String>;

    /// Models for the model menu. The error must survive: the gateway renders an
    /// empty list plus an inline reason rather than blanking the app, and "no models"
    /// and "cannot reach the endpoint" are different claims to a user.
    async fn models_info(&self) -> Result<Vec<crate::provider::ModelInfo>, String>;

    /// The web-search panel: which provider is on and what it says about itself.
    ///
    /// Named as a capability deliberately. Handing a client `provider()` or
    /// `search_tool()` would make every future core grow those internals just to
    /// satisfy the trait, and the seam would stop meaning anything.
    async fn search_view(&self, cfg_path: &Path) -> SearchView;

    /// Choose the search provider. Both cores write the *same* config file, so a
    /// provider picked on a phone is still picked after a restart, and the two shells
    /// never disagree.
    async fn set_search_backend(
        &self,
        cfg_path: &Path,
        backend: &str,
        searxng_url: &str,
    ) -> Result<SearchView, String>;

    /// One real search, so "is search working?" gets answered by the host instead of
    /// by asking the model to demonstrate it.
    async fn search_probe(&self, query: Option<String>) -> Result<SearchProbe, String>;
}

#[async_trait]
impl HostOps for SessionHost {
    fn send(&self, cmd: HostCmd) {
        SessionHost::send(self, cmd)
    }
    fn send_turn_to(
        &self,
        text: String,
        attachments: Vec<crate::provider::Attachment>,
        session: Option<i64>,
    ) {
        SessionHost::send_turn_to(self, text, attachments, session)
    }
    fn cancel(&self) {
        SessionHost::cancel(self)
    }
    fn state(&self) -> HostState {
        SessionHost::state(self)
    }
    async fn set_workspace_checked(&self, path: &str) -> Result<String, String> {
        SessionHost::set_workspace_checked(self, path).await
    }
    async fn models_info(&self) -> Result<Vec<crate::provider::ModelInfo>, String> {
        self.provider().list_models_info().await.map_err(|e| e.to_string())
    }
    async fn search_view(&self, cfg_path: &Path) -> SearchView {
        crate::settings::search_view(self, cfg_path).await
    }
    async fn set_search_backend(
        &self,
        cfg_path: &Path,
        backend: &str,
        searxng_url: &str,
    ) -> Result<SearchView, String> {
        crate::settings::set_search_backend(self, cfg_path, backend, searxng_url).await
    }
    async fn search_probe(&self, query: Option<String>) -> Result<SearchProbe, String> {
        crate::settings::search_probe(self, query).await
    }
}

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
        // The set marker has to travel. Dropping it here is what made a two-question
        // `ask_user` render as two separate popups on the desktop while the engine
        // believed it had sent one card.
        set: q.set.clone(),
    }
}

#[cfg(test)]
mod question_wire_tests {
    use super::*;
    use crate::tools::ask::{Question, QuestionOption, QuestionSet};

    /// The set marker is the difference between one card with Back/Next and N
    /// unrelated popups, and the only place it can be lost is here: the desktop and
    /// the gateway both build their question event through this function, so a
    /// forgotten field is invisible inside the engine and unfixable in the UI.
    #[test]
    fn a_question_event_carries_the_set_it_was_asked_in() {
        let q = Question {
            id: 7,
            question: "How long should it be?".into(),
            options: vec![QuestionOption { label: "Half a page".into(), detail: None }],
            recommended: Some(0),
            allow_custom: true,
            multi: false,
            set: Some(QuestionSet { id: 42, index: 1, total: 2 }),
        };
        match question_event(&q) {
            EngineEvent::QuestionRequested { id, set, .. } => {
                assert_eq!(id, 7);
                let s = set.expect("a set question must say which set it belongs to");
                assert_eq!((s.id, s.index, s.total), (42, 1, 2));
            }
            other => panic!("expected a question event, got {other:?}"),
        }
    }

    /// A single question keeps the shape it always had: no `set` key on the wire.
    #[test]
    fn a_single_question_event_omits_the_set_key_entirely() {
        let q = Question {
            id: 8,
            question: "Which format?".into(),
            options: vec![QuestionOption { label: "PDF".into(), detail: None }],
            recommended: None,
            allow_custom: false,
            multi: false,
            set: None,
        };
        let json = serde_json::to_string(&question_event(&q)).unwrap();
        assert!(json.contains(r#""id":8"#), "{json}");
        assert!(!json.contains("set"), "a single question grew a wire field: {json}");
    }
}

/// Build the volatile environment block. Kept out of the system prompt on
/// purpose: putting a date there changes the leading bytes of every prompt and
/// guarantees a prefix-cache miss on every single turn.
pub fn default_env_block(workspace: &std::path::Path, os: &str, egress: crate::tools::net::NetMode) -> String {
    // One line, three truths. It said "no network" whatever the config said, which
    // is a second place (after the system prompt) where the model was told it was
    // offline by an app whose default mode routes package installs through a proxy.
    let egress_line = match egress {
        crate::tools::net::NetMode::Off => "no network",
        crate::tools::net::NetMode::Proxy => "network via an allowlist proxy",
        crate::tools::net::NetMode::Open => "network open",
    };
    format!(
        "<env>\nos: {os}\nworkspace: {}\nshell: sh\n\
         commands run sandboxed: {egress_line}, only the workspace is writable\n\
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

/// One owner for the numbers a shell that has no opinion gets. The pi bridge reads
/// these through `default_attachment_limits` so that an over-large picture is refused
/// with the same sentence whichever core is running.
const DEFAULT_MAX_ATTACHMENTS: usize = 4;
const DEFAULT_MAX_ATTACHMENT_BYTES: usize = 2 * 1024 * 1024;

fn b64_ok(s: &str) -> bool {
    if s.is_empty() || s.len() % 4 != 0 {
        return false
    }
    s.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
    })
}

impl AttachmentLimits {
    /// The limits a default host applies. Shared with the bridge deliberately: two
    /// cores with two different attachment rules feel like flakiness to the user.
    pub fn defaults() -> Self {
        AttachmentLimits {
            max_count: DEFAULT_MAX_ATTACHMENTS,
            max_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
        }
    }
}

/// The one place an attachment is accepted or refused. The pi bridge calls this too,
/// so a blob that is rejected says the same sentence whichever core is running.
pub fn check_attachments(atts: &[crate::provider::Attachment], limits: AttachmentLimits) -> Result<(), String> {
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
        /// What the endpoint advertises as the loaded model's context window.
        /// `None` is the honest "the server never said".
        window: Option<u64>,
        /// Report that window only from this `/models` call onward, counting
        /// `list_models_info` calls. 0 always reports it. This exists to model
        /// the real case: a server that was still loading when the host asked
        /// at boot, and answers on the next ask.
        window_from_call: usize,
        asks: std::sync::atomic::AtomicUsize,
        calls: AtomicUsize,
        /// What the provider actually received, so a test can prove an
        /// attachment survived the host, the prompt builder and the request.
        seen_attachments: Arc<AtomicUsize>,
        /// The user message the endpoint actually received, env block included.
        seen_wire: std::sync::Mutex<Vec<String>>,
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
            if let Some(last_user) = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == crate::provider::Role::User)
            {
                self.seen_wire.lock().unwrap().push(last_user.content.clone());
            }
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

        async fn list_models_info(&self) -> Result<Vec<crate::provider::ModelInfo>, ProviderError> {
            let n = self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let window = if n >= self.window_from_call.max(1) { self.window } else { None };
            Ok(vec![crate::provider::ModelInfo {
                id: "test-model".into(),
                context_tokens: window,
                ..Default::default()
            }])
        }
    }

    fn host(replies: Vec<String>) -> (SessionHost, Arc<Store>) {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(replies),
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
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
    async fn the_env_block_names_the_folder_the_tools_run_in() {
        // The bug this pins: the host re-installed the env block captured at
        // startup, so a session in ~/Projects told the model it was working in
        // the startup folder while `pwd` said otherwise. The model then typed
        // absolute paths for a tree it was not sandboxed in.
        let base = std::env::temp_dir().join(format!("cwoffee_env_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            replies: std::sync::Mutex::new(vec!["ok".into(), "ok".into()]),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let h = SessionHost::spawn(
            Arc::clone(&p) as Arc<dyn Provider>,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", &a),
        );
        ready(&h).await;

        // A session that lives in a different folder, and a turn sent to it.
        let in_b = store
            .create_session("elsewhere", &b.display().to_string(), "test-model")
            .unwrap();
        h.send(HostCmd::Send {
            text: "list the files".into(),
            session: Some(in_b),
            attachments: vec![],
        });
        turns_done(&h, 1).await;

        let wire = p.seen_wire.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(
            wire.contains(&b.display().to_string()),
            "the model was told the wrong workspace; wire was:\n{wire}"
        );
        assert!(
            !wire.contains(&a.display().to_string()),
            "the startup folder leaked into a session that lives elsewhere:\n{wire}"
        );
        assert_eq!(h.state().workspace, b.display().to_string());

        // Back to a draft, then to the other folder: the env block has to follow
        // the folder the next turn actually runs in, not the last one it saw.
        h.send(HostCmd::ClearSession);
        wait_for("draft again".into(), || h.state().session_id == 0).await;
        h.send(HostCmd::SetWorkspace { path: a.display().to_string() });
        wait_for("back in a".into(), || {
            h.state().workspace == a.display().to_string()
        })
        .await;
        h.send(HostCmd::Send {
            text: "again".into(),
            session: None,
            attachments: vec![],
        });
        turns_done(&h, 2).await;
        let wire = p.seen_wire.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(
            wire.contains(&a.display().to_string()),
            "a new session kept the previous session's env block:\n{wire}"
        );
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

        // Ordering and ownership. The command loop runs turns inline, so a change
        // sent during a turn is processed after it, never in the middle of one. It
        // is also refused here for a second, deliberate reason: this turn created a
        // session, the session has a folder, and a session keeps that folder.
        h.submit("go");
        h.send(HostCmd::SetWorkspace { path: base.display().to_string() });
        turns_done(&h, 1).await;
        assert_eq!(
            h.state().workspace,
            other.display().to_string(),
            "a running session's folder moved out from under it"
        );
        assert!(
            h.state()
                .workspace_refusal
                .as_deref()
                .unwrap_or("")
                .contains("keeps its folder"),
            "the refusal was not stated: {:?}",
            h.state().workspace_refusal
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The system temp root is the bucket this app calls "no workspace", so it is
    /// not also offered as a workspace: sessions already sitting there would split
    /// across both sidebar groups, and adding it would appear to do something.
    #[test]
    fn the_system_temp_folder_is_not_a_workspace() {
        let err = check_workspace("/tmp").expect_err("/tmp must be refused as a workspace");
        assert!(err.contains("temp folder"), "unhelpful: {err}");
        assert!(is_temp_root("/tmp"));
        assert!(is_temp_root("/tmp/"), "a trailing slash is the same folder");
        assert!(
            !is_temp_root("/tmp/cwoffee-project"),
            "a project under /tmp is a folder like any other"
        );
        let sub = std::env::temp_dir().join(format!("cwoffee_temp_ok_{}", std::process::id()));
        std::fs::create_dir_all(&sub).unwrap();
        assert!(check_workspace(sub.to_str().unwrap()).is_ok(), "a subfolder must work");
        let _ = std::fs::remove_dir_all(&sub);
    }

    /// A session keeps the folder it started in. The tools are confined to it and
    /// the sidebar files the session under it, so moving it mid-life makes the
    /// grouping a decoration and writes into a tree this conversation never chose.
    #[tokio::test]
    async fn a_session_keeps_the_folder_it_started_in() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_lock_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (mut h, _store) = host(vec!["first reply".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        // Draft: no session row yet, so the folder is still yours to choose.
        h.send(HostCmd::SetWorkspace { path: a.display().to_string() });
        wait_for("into a".into(), || h.state().workspace == a.display().to_string()).await;

        // The turn creates the session, and the session inherits that folder.
        h.submit("go");
        turns_done(&h, 1).await;
        assert_ne!(h.state().session_id, 0, "the turn should have created a session");

        h.send(HostCmd::SetWorkspace { path: b.display().to_string() });
        assert!(
            await_error(&mut ev, "keeps its folder").await,
            "a session's folder was changed without a word about it"
        );
        assert_eq!(h.state().workspace, a.display().to_string(), "it moved anyway");

        // Back to a draft and the choice is open again: the lock belongs to the
        // session, not to the process.
        h.send(HostCmd::ClearSession);
        wait_for("draft".into(), || h.state().session_id == 0).await;
        h.send(HostCmd::SetWorkspace { path: b.display().to_string() });
        wait_for("into b".into(), || h.state().workspace == b.display().to_string()).await;

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A budget is a property of the model, not of the app. The number used to be
    /// a constant, which compacted 43k-token conversations on a model with a
    /// 262 144-token window and then announced "context full".
    #[test]
    fn the_budget_sized_from_a_real_window() {
        assert_eq!(auto_budget(262_144), 209_715);
        assert_eq!(auto_budget(8_192), 6_553, "80% of the window, not 72% minus a constant");
        assert_eq!(auto_budget(4_096), 4_096, "a tiny window cannot go below the floor");
        assert_eq!(auto_budget(0), 4_096);
    }

    #[tokio::test]
    async fn the_context_budget_follows_the_loaded_model() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["ok".into()]),
            window: Some(262_144),
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        ready(&h).await;
        // One provider round-trip happens before the budget lands, so wait for it
        // rather than the first turn.
        wait_for(
            "budget derived".into(),
            || h.state().context_budget == auto_budget(262_144),
        )
        .await;
        assert_eq!(h.state().model_window, Some(262_144));
    }

    /// The report was: the window was on full-auto, the gateway said ask, and
    /// then the window started asking for approvals again. A choice that lives
    /// only in the running process is re-read from the file by the next client,
    /// so the app contradicts the user. This drives the host the way the UI does
    /// and reads the file back.
    #[tokio::test]
    async fn a_mode_chosen_through_the_host_is_written_down() {
        let dir = std::env::temp_dir().join(format!("cwoffee_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "mode = \"ask\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\n",
        )
        .unwrap();

        let mut cfg = HostConfig::new("m", dir.join("ws"));
        cfg.cfg_path = Some(path.clone());
        let h = SessionHost::spawn(
            Arc::new(Scripted {
                replies: std::sync::Mutex::new(vec![]),
                window: Some(262_144),
                window_from_call: 0,
                asks: std::sync::atomic::AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                seen_attachments: Arc::new(AtomicUsize::new(0)),
                seen_wire: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        ready(&h).await;
        h.send(HostCmd::SetMode { mode: ApprovalMode::Auto });

        let mut seen = String::new();
        for _ in 0..80 {
            seen = std::fs::read_to_string(&path).unwrap_or_default();
            if seen.contains("mode = \"auto\"") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(seen.contains("mode = \"auto\""), "the file still said: {seen}");
        assert!(!seen.contains("mode = \"ask\""), "{seen}");
        // The provider section the person already had is untouched.
        assert!(seen.contains("[provider]\nbase_url = \"http://127.0.0.1:8090/v1\""), "{seen}");
        // And the running host agrees with the file.
        assert_eq!(h.state().mode, ApprovalMode::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// "Saved" means the next start can read it back, and the check above only ever
    /// looked at the text. When the writer put `reasoning` and `model` where the
    /// parser declares neither, every text assertion passed and the app still
    /// refused to boot on the next launch. So this test ends by parsing the file the
    /// way a boot does.
    #[tokio::test]
    async fn a_thinking_level_and_model_written_through_the_host_parse_back() {
        let dir = std::env::temp_dir().join(format!("cwoffee_cfg_lr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "mode = \"ask\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"old-model\"\n",
        )
        .unwrap();

        let mut cfg = HostConfig::new("old-model", dir.join("ws"));
        cfg.cfg_path = Some(path.clone());
        let h = SessionHost::spawn(
            Arc::new(Scripted {
                replies: std::sync::Mutex::new(vec![]),
                window: Some(262_144),
                window_from_call: 0,
                asks: std::sync::atomic::AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                seen_attachments: Arc::new(AtomicUsize::new(0)),
                seen_wire: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        ready(&h).await;
        h.send(HostCmd::SetReasoning { reasoning: Reasoning::High });
        h.send(HostCmd::SetModel { model: "picked-model".into(), persist: true });

        let mut seen = String::new();
        for _ in 0..80 {
            seen = std::fs::read_to_string(&path).unwrap_or_default();
            if seen.contains("reasoning = \"high\"") && seen.contains("picked-model") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(seen.contains("reasoning = \"high\""), "the file still said: {seen}");
        // One answer to one question: the model line was replaced in [provider], not
        // duplicated at the top level.
        assert_eq!(seen.matches("model = ").count(), 1, "{seen}");
        assert!(seen.contains("model = \"picked-model\""), "{seen}");

        let parsed: crate::config::Config = toml::from_str(&seen)
            .unwrap_or_else(|e| panic!("a file this app wrote will not boot the next start: {e}\n{seen}"));
        assert_eq!(parsed.reasoning_checked().unwrap(), Some(Reasoning::High), "{seen}");
        assert_eq!(parsed.provider.model, "picked-model", "{seen}");
        assert_eq!(h.state().reasoning, Reasoning::High);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host with no file to write to writes nothing: tests, and any shell that
    /// did not ask for persistence, must not end up editing the user's config.
    #[tokio::test]
    async fn a_host_with_no_config_path_keeps_its_choices_to_itself() {
        let dir = std::env::temp_dir().join(format!("cwoffee_cfg_none_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = HostConfig::new("m", dir.join("ws"));
        cfg.cfg_path = None;
        let h = SessionHost::spawn(
            Arc::new(Scripted {
                replies: std::sync::Mutex::new(vec![]),
                window: Some(262_144),
                window_from_call: 0,
                asks: std::sync::atomic::AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                seen_attachments: Arc::new(AtomicUsize::new(0)),
                seen_wire: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        ready(&h).await;
        h.send(HostCmd::SetMode { mode: ApprovalMode::Auto });
        for _ in 0..20 {
            if h.state().mode == ApprovalMode::Auto {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(h.state().mode, ApprovalMode::Auto);
        assert!(!dir.join("config.toml").exists(), "a host not given a file must not invent one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wrecking-ball case, in one test: the server had not finished loading
    /// when the host asked at boot, so the budget stayed at the fallback and a
    /// session got trimmed against a number nobody measured - while the model
    /// picker was already showing 262 144 for the same model.
    #[tokio::test]
    async fn a_window_the_endpoint_was_quiet_about_is_asked_for_again() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["one".into(), "two".into()]),
            window: Some(262_144),
            // silent at boot, answers from the second ask onward - which is what a
            // server that is still loading does
            window_from_call: 2,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        ready(&h).await;
        assert_eq!(h.state().model_window, None, "the boot ask got nothing, as scripted");
        assert_eq!(
            h.state().context_budget, FALLBACK_BUDGET,
            "an endpoint that said nothing must not invent a window"
        );

        h.submit("first");
        turns_done(&h, 1).await;
        assert_eq!(
            h.state().model_window, Some(262_144),
            "the host never asked the endpoint again, so every later turn kept the fallback"
        );
        assert_eq!(h.state().context_budget, auto_budget(262_144));
    }

    #[tokio::test]
    async fn a_pinned_budget_is_not_overridden_by_the_window() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["ok".into()]),
            window: Some(262_144),
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let mut cfg = HostConfig::new("test-model", std::env::temp_dir());
        cfg.context_budget = Some(12_000);
        let h = SessionHost::spawn(p, Registry::new(), Arc::clone(&store), cfg);
        ready(&h).await;
        // Wait on the provider round-trip, not on the number: the state starts at
        // the pinned value, so the budget alone proves nothing about the window.
        wait_for("window reported".into(), || h.state().model_window == Some(262_144)).await;
        assert_eq!(h.state().context_budget, 12_000, "the window overrode the pin");
    }

    /// Cache information is measured, not asserted, and it belongs to a session
    /// with history to reload: an empty one must say nothing at all. This was the
    /// "warning on every new session" complaint.
    #[tokio::test]
    async fn an_empty_session_is_not_announced_as_a_problem() {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec!["one".into(), "two".into()]),
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let mut h = SessionHost::spawn(
            p,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        // A session with one message, so there is something to resume into.
        h.submit("seed");
        turns_done(&h, 1).await;
        let sid = h.state().session_id;
        assert_ne!(sid, 0);

        h.send(HostCmd::ClearSession);
        wait_for("draft".into(), || h.state().session_id == 0).await;
        let fresh = store.create_session("fresh", "/tmp", "test-model").unwrap();
        h.send(HostCmd::SwitchSession { id: fresh });
        wait_for("switched".into(), || h.state().session_id == fresh).await;
        let cold: Vec<String> = (|| {
            let mut out = Vec::new();
            while let Ok(e) = ev.try_recv() {
                if let EngineEvent::Note { message, .. } = e {
                    if message.contains("resumed session") {
                        out.push(message);
                    }
                }
            }
            out
        })();
        assert!(
            cold.is_empty(),
            "an empty session was told about a resumed history: {cold:?}"
        );

        // Same session again: still nothing. And the seeded session says so once,
        // as a note, with numbers - and without claiming a cache state the engine
        // has no window onto.
        h.send(HostCmd::SwitchSession { id: sid });
        wait_for("back".into(), || h.state().session_id == sid).await;
        let mut saw_resume = false;
        while let Ok(e) = ev.try_recv() {
            if let EngineEvent::Note { message, session } = e {
                if message.contains("resumed session") {
                    saw_resume = true;
                    // The note has to name the conversation it is about. The client
                    // used to stamp notes from what it happened to be showing, and
                    // the note a switch emits arrives while the window is still on
                    // the conversation being left - so it got filed in the wrong
                    // transcript and the repaint of the right one dropped it.
                    assert_eq!(
                        session,
                        Some(sid),
                        "the resume note did not name the conversation it is about: {message:?} / session={session:?}"
                    );
                    assert!(message.contains("token"), "the note had no numbers: {message}");
                    for claim in ["cold", "warm", "re-prefill"] {
                        assert!(
                            !message.to_lowercase().contains(claim),
                            "the resume note asserted {claim:?} about a cache it cannot see: {message}"
                        );
                    }
                }
            }
        }
        assert!(saw_resume, "a resumed history did not say what it restored");
    }

    /// Same lock, session row created the way the desktop command does it: a row in
    /// the store and a switch, never a `HostCmd::NewSession`. If the lock only works
    /// for sessions the host itself opened, then every session created from the
    /// sidebar can have its folder moved under it - which is what the harness hit.
    #[tokio::test]
    async fn a_session_created_outside_the_host_still_keeps_its_folder() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_ext_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (mut h, store) = host(vec!["unused".into()]);
        let (_ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        h.send(HostCmd::SetWorkspace {
            path: a.display().to_string(),
        });
        wait_for("into a".into(), || {
            h.state().workspace == a.display().to_string()
        })
        .await;

        let id = store
            .create_session("made outside", &a.display().to_string(), "m")
            .unwrap();
        h.send(HostCmd::SwitchSession { id });
        wait_for("switched".into(), || h.state().session_id == id).await;

        let err = h
            .set_workspace_checked(&b.display().to_string())
            .await
            .expect_err("a row created outside the host is still a session with a folder");
        assert!(
            err.contains("keeps its folder"),
            "the refusal was: {err}"
        );
        assert_eq!(
            store.session(id).unwrap().unwrap().workspace,
            a.display().to_string(),
            "the row's folder moved"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The desktop does not send the raw command; it calls `set_workspace_checked`
    /// and reads the return value. A lock that only exists as an event leaves that
    /// caller reporting success, which is the failure the harness saw: "the session
    /// moved", with an empty reason attached to it.
    #[tokio::test]
    async fn the_lock_reaches_the_caller_that_asked() {
        let base = std::env::temp_dir().join(format!("cwoffee_wslock_api_{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (mut h, _store) = host(vec!["first reply".into()]);
        let (_ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        h.send(HostCmd::SetWorkspace {
            path: a.display().to_string(),
        });
        wait_for("into a".into(), || {
            h.state().workspace == a.display().to_string()
        })
        .await;
        h.submit("go");
        turns_done(&h, 1).await;

        let err = h
            .set_workspace_checked(&b.display().to_string())
            .await
            .expect_err("a session's folder must not move out from under it");
        assert!(
            err.contains("keeps its folder"),
            "the refusal did not reach the caller that asked: {err}"
        );
        assert_eq!(
            h.state().workspace,
            a.display().to_string(),
            "the folder moved anyway"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Sessions in the "no workspace" bucket - the system temp folder, or nothing
    /// at all - are exactly the ones that should be allowed to pick a folder.
    #[tokio::test]
    async fn a_session_with_no_folder_can_choose_one() {
        let base = std::env::temp_dir().join(format!("cwoffee_ws_free_{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();

        let (mut h, store) = host(vec!["first reply".into()]);
        let (_ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;

        let id = store
            .create_session("orphan", "/tmp", "m")
            .expect("session row in the temp bucket");
        h.send(HostCmd::SwitchSession { id });
        wait_for("switched".into(), || h.state().session_id == id).await;

        h.send(HostCmd::SetWorkspace { path: real.display().to_string() });
        wait_for("adopted a real folder".into(), || {
            h.state().workspace == real.display().to_string()
        })
        .await;
        assert_eq!(store.session(id).unwrap().unwrap().workspace, real.display().to_string());

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
        assert!(err.contains("not a directory"), "unhelpful error: {err}");
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
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            replies: std::sync::Mutex::new(vec!["noted".into()]),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::clone(&seen),
            seen_wire: Default::default(),
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
            session: None,
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
    /// A turn that names a conversation the host is not on is refused.
    ///
    /// The symptom this kills is "my transcript is being mutated". One host serves
    /// several clients - desktop, phone, harness - and an untargeted send lands in
    /// whichever conversation the last switch left active, so a harness turn can
    /// appear in the chat the user is looking at. Naming the target, and refusing a
    /// mismatch, makes that impossible rather than merely unlikely.
    #[tokio::test]
    async fn a_turn_moves_the_host_to_the_conversation_it_names() {
        let ws = std::env::temp_dir().display().to_string();
        let (h, store) = host(vec!["the turn ran".into(), "next one".into()]);
        let a = store.create_session("alpha", &ws, "test-model").unwrap();
        let b = store.create_session("beta", &ws, "test-model").unwrap();
        h.send(HostCmd::SwitchSession { id: a });
        wait_for("on alpha".into(), || h.state().session_id == a).await;

        // Browsing does not move the host, so a turn addressed to another
        // conversation is what moves it - and it lands exactly where it said.
        h.send_turn_to("this belongs to beta", vec![], Some(b));
        wait_for("the addressed turn ran".into(), || h.state().turns == 1).await;
        assert_eq!(h.state().session_id, b, "the host did not follow the addressee");
        assert_eq!(store.history(b).unwrap().len(), 2, "user + assistant in beta");
        assert!(
            store.history(a).unwrap().is_empty(),
            "the conversation that was open before got written into"
        );

        // An addressee that does not exist: nothing is sent, and the host stays
        // put. Falling back to "whatever was open" is the bug this guards.
        h.send_turn_to("nowhere to go", vec![], Some(b + 1000));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(h.state().turns, 1, "a turn with no addressee ran anyway");
        assert_eq!(h.state().session_id, b, "the host moved to chase a missing row");
        assert_eq!(store.history(b).unwrap().len(), 2, "the turn was appended anyway");
    }

    /// `session: Some(0)` means "my view is a draft". The send has to open its own
    /// conversation instead of appending to whatever the host left active - the same
    /// mistake with the opposite sign.
    #[tokio::test]
    async fn a_draft_send_starts_its_own_conversation() {
        let ws = std::env::temp_dir().display().to_string();
        let (h, store) = host(vec!["one".into(), "two".into()]);
        let a = store.create_session("alpha", &ws, "test-model").unwrap();
        h.send(HostCmd::SwitchSession { id: a });
        wait_for("on alpha".into(), || h.state().session_id == a).await;

        h.send_turn_to("from a draft", vec![], Some(0));
        wait_for("a row was created".into(), || {
            let id = h.state().session_id;
            id != 0 && id != a
        })
        .await;
        assert!(
            store.history(a).unwrap().is_empty(),
            "the draft's message joined the session that was already open"
        );
        assert_eq!(store.history(h.state().session_id).unwrap().len(), 2);
    }

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

        h.send(HostCmd::SetModel { model: "second-model".into(), persist: true });
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
        h.send(HostCmd::SetModel { model: "   ".into(), persist: true });
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
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            replies: std::sync::Mutex::new(vec!["tool".into()]),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
        });
        let mut reg = Registry::new();
        reg.register(Box::new(ExecTool::new(&crate::tools::ExecCfg {
            timeout_secs: 5,
            ..Default::default()
        })));

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

        // A second conversation, then back to the first. The draft (session 0) is
        // not a switch target: there is no row to load.
        let beta = h.state().session_id;
        h.send(HostCmd::NewSession { title: "gamma".into() });
        wait_for("a second session".into(), || h.state().session_id != beta).await;
        h.send(HostCmd::SwitchSession { id: beta });
        wait_for("switch back".into(), || h.state().session_id == beta).await;
        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineEvent::SessionChanged { session_id, .. } if *session_id == beta)),
            "switching must also announce: {evs:?}"
        );
    }

    /// An id matching no row is refused. `history()` answers an empty list for a
    /// missing session, so without this the host happily "switches" to a phantom
    /// conversation and the next turn is appended to nothing.
    #[tokio::test]
    async fn switching_to_a_missing_session_says_so() {
        let (mut h, _store) = host(vec![]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        let before = h.state().session_id;
        h.send(HostCmd::SwitchSession { id: 987_654 });
        assert!(
            await_error(&mut ev, "does not exist").await,
            "the refusal never reached the client"
        );
        assert_eq!(h.state().session_id, before, "the host moved to a phantom session");
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
        use crate::tools::net::NetMode as N;
        let b = default_env_block(std::path::Path::new("/home/u/ws"), "linux", N::Off);
        assert!(b.contains("/home/u/ws"));
        assert!(b.contains("no network"));
        assert!(b.contains("sandbox"));
        // The default product mode is a proxy, and this line used to deny it.
        let p = default_env_block(std::path::Path::new("/home/u/ws"), "linux", N::Proxy);
        assert!(p.contains("allowlist proxy"), "{p}");
        assert!(!p.contains("no network"), "the env block called a proxy sandbox offline: {p}");
        let o = default_env_block(std::path::Path::new("/home/u/ws"), "linux", N::Open);
        assert!(o.contains("network open"), "{o}");
    }
}
