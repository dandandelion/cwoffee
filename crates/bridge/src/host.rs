//! `pi --mode rpc` behind cwoffee's own `HostOps`.
//!
//! This is the point of the seam Move 0 opened: the desktop window and the phone
//! client keep talking to `EngineHandle`, and what sits behind it changes. The
//! contract that makes that safe is that this type answers *the same way*, not
//! merely in the same shape:
//!
//! - **Turn accounting.** One prompt is one turn, tool loop included, so
//!   `state().turns` and `TurnEnd` mean the same thing to a front end on either
//!   core (`translate.rs` explains why pi's own turn boundaries are ignored).
//! - **The send address.** A `Send` that names a conversation moves the host to
//!   it, and a name matching no row is refused — the behaviour
//!   `SessionHost::a_turn_moves_the_host_to_the_conversation_it_names` asserts.
//!   A core that "mostly" honours the address is a core that writes into the
//!   wrong transcript.
//! - **Fail-closed approvals.** The gate parks the turn and asks. If nobody
//!   answers, pi's own timeout denies the tool and the bridge says so in the
//!   transcript: a vanished webview must not become an unattended agent.
//! - **Nothing invented.** What pi does not report (server timings, speculative
//!   decode counters, whether search is mounted) is reported as absent.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode, ApprovalRequest};
use cwoffee_engine::config::Config;
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{approval_event, question_event, ApprovalRx, HostCmd, HostOps, HostState};
use cwoffee_engine::provider::{Attachment, ModelInfo, Reasoning};
use cwoffee_engine::settings::{SearchProbe, SearchView};
use cwoffee_engine::store::StoreOps;
use cwoffee_engine::tools::ask::{Answer, Question, QuestionOption, QuestionRx};
use cwoffee_engine::tools::Risk;

use crate::process::{pi_version, PiChild, PiError, PiLaunch};
use crate::sessions::SessionMap;
use crate::translate::{Output, Translator, UiRequest};

pub type EventRx = mpsc::UnboundedReceiver<EngineEvent>;
pub type ApprovalIn = mpsc::UnboundedReceiver<ApprovalRx>;
pub type QuestionIn = mpsc::UnboundedReceiver<QuestionRx>;

#[derive(Clone)]
pub struct PiHostConfig {
    pub launch: PiLaunch,
    pub store: Arc<dyn StoreOps>,
    pub mode: ApprovalMode,
    pub reasoning: Reasoning,
    /// How long an unanswered approval card stays open. The same number goes out
    /// to pi as the dialog's own `timeout`, so the countdown in the UI and the
    /// deny inside the engine cannot disagree.
    pub approval_timeout: Duration,
    pub question_timeout: Duration,
    /// Locked decision 14: no cap on live pi processes; an idle one is closed
    /// after this long and reopened on the next turn against the same pi session
    /// file, so nothing the model knew is lost.
    pub idle_reap: Duration,
    pub context_budget: usize,
    pub model_window: Option<usize>,
    /// cwoffee row id <-> pi session file. See `sessions.rs`.
    pub session_map: PathBuf,
}

/// The handle the gateway and the desktop window hold, deliberately shaped like
/// `SessionHost`: the receivers are taken once, before this goes behind the `Arc`
/// both shells share, because a second consumer would silently split one stream.
pub struct PiHost {
    cmd_tx: mpsc::UnboundedSender<HostCmd>,
    /// A request that needs an answer, run by the model task against the live
    /// child: the model list, and anything else a front end asks and waits on.
    rpc_tx: mpsc::UnboundedSender<(Value, oneshot::Sender<Result<Value, String>>)>,
    state: Arc<Mutex<HostState>>,
    ev_rx: Option<EventRx>,
    ap_rx: Option<ApprovalIn>,
    q_rx: Option<QuestionIn>,
    pub pi_version: String,
}

impl PiHost {
    /// Nothing is started here except a version read. The pi process itself is
    /// started by the first turn that needs it, which is what keeps a desktop
    /// launch from paying ~75 MB and a few hundred milliseconds for a model the
    /// user may not use in this session.
    pub async fn spawn(cfg: PiHostConfig) -> Result<PiHost, PiError> {
        let pi_version = pi_version(&cfg.launch.bin).await?;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ap_tx, ap_rx) = mpsc::unbounded_channel();
        let (q_tx, q_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (rec_tx, rec_rx) = mpsc::unbounded_channel();
        let (rpc_tx, rpc_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(HostState {
            session_id: 0,
            workspace: cfg.launch.workdir.display().to_string(),
            model: cfg.launch.model.clone(),
            mode: cfg.mode.clone(),
            reasoning: cfg.reasoning.clone(),
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
            context_budget: cfg.context_budget,
            model_window: cfg.model_window,
            approval_timeout_secs: cfg.approval_timeout.as_secs(),
            question_timeout_secs: cfg.question_timeout.as_secs(),
            workspace_refusal: None,
            refusal_seq: 0,
        }));
        let map = SessionMap::open(cfg.session_map.clone());
        let engine = Engine {
            cfg,
            state: Arc::clone(&state),
            ev: ev_tx,
            ap: ap_tx,
            q: q_tx,
            ui_tx,
            rec_tx,
            child: None,
            tr: Translator::default(),
            map,
            cmd_tx: cmd_tx.clone(),
            deferred: Vec::new(),
            auto_allow: HashSet::new(),
            waits: HashMap::new(),
            next_id: 1,
            last_activity: Instant::now(),
        };
        // One task owns all mutation: the command mailbox, pi's record stream,
        // the answered dialogs, and the requests that need an answer. State only
        // one task touches cannot race, and there is one core per process.
        tokio::spawn(async move { engine.run(cmd_rx, rec_rx, ui_rx, rpc_rx).await });
        Ok(PiHost {
            cmd_tx,
            rpc_tx,
            state,
            ev_rx: Some(ev_rx),
            ap_rx: Some(ap_rx),
            q_rx: Some(q_rx),
            pi_version,
        })
    }

    /// Mirrors `SessionHost::take_receivers`.
    pub fn take_receivers(&mut self) -> Option<(EventRx, ApprovalIn)> {
        Some((self.ev_rx.take()?, self.ap_rx.take()?))
    }

    pub fn take_questions(&mut self) -> Option<QuestionIn> {
        self.q_rx.take()
    }

    fn push(&self, cmd: HostCmd) {
        if self.cmd_tx.send(cmd).is_err() {
            eprintln!("[bridge] the model task is gone; the command was dropped");
        }
    }

    /// Ask the live child something and wait for its answer. The model task runs
    /// it, because only that task may touch the child.
    async fn rpc(&self, cmd: Value, timeout: Duration) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.rpc_tx
            .send((cmd, tx))
            .map_err(|_| "the model task is gone".to_string())?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the model task dropped the request".into()),
            Err(_) => Err("the model process did not answer in time".into()),
        }
    }
}

#[async_trait]
impl HostOps for PiHost {
    fn send(&self, cmd: HostCmd) {
        self.push(cmd);
    }

    fn send_turn_to(
        &self,
        text: String,
        attachments: Vec<Attachment>,
        session: Option<i64>,
    ) {
        self.push(HostCmd::Send { text, attachments, session });
    }

    fn cancel(&self) {
        self.push(HostCmd::Cancel);
    }

    fn state(&self) -> HostState {
        self.state.lock().unwrap().clone()
    }

    /// Move the tools. The checks here are deliberately narrower than the Rust
    /// core's (which also re-arms the sandbox): busy, exists, is a folder. What
    /// they are not is silent — a move that did not happen says why, in the same
    /// two places the Rust core puts it (`state().workspace_refusal` and an
    /// `Error` event), and a move that happened reports the canonical path.
    async fn set_workspace_checked(&self, path: &str) -> Result<String, String> {
        if self.state().busy {
            return Err("a turn is running; stop it before moving the tools".into());
        }
        // The engine's own probe, not a copy of it: empty, missing, the system temp
        // root, and an unwritable folder are refused with the same words on both
        // cores. A sidebar that accepts a folder the tools then cannot write to is a
        // UI that lies about where your files are about to be written.
        let p = cwoffee_engine::host::check_workspace(path).map_err(|reason| format!("{path}: {reason}"))?;
        let want = p.display().to_string();
        let before = self.state().refusal_seq;
        self.push(HostCmd::SetWorkspace { path: want.clone() });
        // The command mailbox is fire-and-forget by contract, so the answer is
        // read back the way a poller reads it: `state()`, until the folder moved
        // or a refusal landed.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = self.state();
            if st.workspace == want && st.refusal_seq == before {
                return Ok(want);
            }
            if st.refusal_seq > before {
                return Err(st.workspace_refusal.unwrap_or_else(|| "the move was refused".into()));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "the move to {want} was asked for and never confirmed; still on {}",
                    st.workspace
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn models_info(&self) -> Result<Vec<ModelInfo>, String> {
        // pi's own catalogue, asked through the model task. The error has to
        // survive: the gateway renders an empty list plus a reason, and "no
        // models" and "cannot reach the endpoint" are different claims.
        let data = self
            .rpc(json!({ "type": "get_available_models" }), Duration::from_secs(30))
            .await?;
        let models = data.get("models").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        Ok(models
            .iter()
            .map(|m| ModelInfo {
                id: m.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                context_tokens: m.get("contextWindow").and_then(|v| v.as_u64()),
                input_modalities: m
                    .get("input")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                // pi does not report chat-template tool support or a server build.
                tools: None,
                tools_inferred: false,
                source: m.get("provider").and_then(|v| v.as_str()).map(str::to_string),
                server: None,
                slots: None,
                reasoning_efforts: Vec::new(),
                reasoning_default: None,
                advertised: m.get("contextWindow").is_some(),
                ..Default::default()
            })
            .collect())
    }

    async fn search_view(&self, cfg_path: &std::path::Path) -> SearchView {
        let (cfg, _) = Config::load(Some(cfg_path)).unwrap_or_else(|_| (Config::default(), None));
        SearchView {
            backend: cfg.search.backend.clone(),
            searxng_url: cfg.search.searxng_url.clone().unwrap_or_default(),
            auto_port: cfg.search.auto_port,
            auto_idle_secs: cfg.search.auto_idle_secs,
            auto_dir: cfg.search_auto_dir().display().to_string(),
            describing: "search is a cwoffee tool, and this bridge does not mount the tool extensions into pi yet \
                          (PI-CORE-PLAN Phase 2); the panel shows the saved choice, not a live tool"
                .into(),
            runtime: None,
            auto: None,
            env_override: None,
            saved_to: None,
        }
    }

    async fn set_search_backend(
        &self,
        cfg_path: &std::path::Path,
        backend: &str,
        searxng_url: &str,
    ) -> Result<SearchView, String> {
        let (cfg, _) = Config::load(Some(cfg_path)).unwrap_or_else(|_| (Config::default(), None));
        // The engine's own parser, so a typo is an error here exactly as it is in
        // the Rust core, and both cores read the same file afterwards.
        cwoffee_engine::settings::parse_backend(backend, searxng_url, &cfg)?;
        let mut keys = vec![("backend", backend.to_string())];
        if !searxng_url.trim().is_empty() {
            keys.push(("searxng_url", searxng_url.trim().to_string()));
        }
        let refs: Vec<(&str, &str)> = keys.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let written = Config::set_search_keys(cfg_path, &refs)?;
        let mut view = self.search_view(cfg_path).await;
        view.saved_to = Some(written.join(", "));
        Ok(view)
    }

    async fn search_probe(&self, _query: Option<String>) -> Result<SearchProbe, String> {
        Err("search runs as a cwoffee tool through the pi extension, which this bridge does not mount yet \
            (PI-CORE-PLAN Phase 2)"
            .into())
    }
}

/// What the bridge knows about an outstanding dialog, in both id spaces.
enum Wait {
    Approval { pi_id: String, tool: String, deadline: Instant },
    Question { pi_id: String, kind: QKind, deadline: Instant, answered: bool },
}

enum QKind {
    /// pi's `select` takes plain strings and returns the one chosen.
    Select { options: Vec<String> },
    Input,
}

enum UiAnswer {
    /// `None` means the card closed without an answer (closed webview, cancel).
    Approval { id: u64, answer: Option<ApprovalAnswer> },
    Question { id: u64, answer: Option<Answer> },
}

struct Engine {
    cfg: PiHostConfig,
    state: Arc<Mutex<HostState>>,
    ev: mpsc::UnboundedSender<EngineEvent>,
    ap: mpsc::UnboundedSender<ApprovalRx>,
    q: mpsc::UnboundedSender<QuestionRx>,
    ui_tx: mpsc::UnboundedSender<UiAnswer>,
    rec_tx: mpsc::UnboundedSender<Value>,
    child: Option<PiChild>,
    tr: Translator,
    map: SessionMap,
    cmd_tx: mpsc::UnboundedSender<HostCmd>,
    /// Conversation-retargeting verbs that arrived during a turn. The Rust core queues
    /// command verbs until the turn finishes; a `select!` loop owes the same, or the rest
    /// of a running turn lands in whichever conversation the user clicked.
    deferred: Vec<HostCmd>,
    /// Approvals granted with `AllowSession`: the same tool is not asked again.
    auto_allow: HashSet<String>,
    waits: HashMap<u64, Wait>,
    next_id: u64,
    last_activity: Instant,
}

impl Engine {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<HostCmd>,
        mut rec_rx: mpsc::UnboundedReceiver<Value>,
        mut ui_rx: mpsc::UnboundedReceiver<UiAnswer>,
        mut rpc_rx: mpsc::UnboundedReceiver<(Value, oneshot::Sender<Result<Value, String>>)>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                biased;
                cmd = cmd_rx.recv() => match cmd {
                    Some(HostCmd::Shutdown) | None => break,
                    Some(c) => self.on_cmd(c).await,
                },
                req = rpc_rx.recv() => match req {
                    Some((cmd, reply)) => {
                        // A model list costs nothing when a child is up and a whole pi
                        // process when one is not. Bootstrap asks on every client connect
                        // — including a phone that only watches and will never send a
                        // prompt — so with no child the answer comes from this bridge's
                        // own configuration: the one model this conversation can run.
                        // pi's fuller catalogue is used whenever a process exists. What
                        // that costs is a one-entry model menu between reaps, which is a
                        // smaller lie than an empty one.
                        let wants_models =
                            cmd.get("type").and_then(|t| t.as_str()) == Some("get_available_models");
                        if wants_models && self.child.is_none() {
                            let _ = reply.send(Ok(json!({ "models": [{
                                "id": self.cfg.launch.model,
                                "provider": self.cfg.launch.provider,
                                "contextWindow": self.cfg.model_window,
                            }] })));
                        } else {
                            let out = self.request(cmd, Duration::from_secs(30)).await;
                            let _ = reply.send(out);
                        }
                    }
                    None => break,
                },
                rec = rec_rx.recv() => match rec {
                    Some(r) => {
                        let out = self.tr.on_record(&r, Instant::now());
                        self.apply(out);
                    }
                    None => break,
                },
                ans = ui_rx.recv() => match ans {
                    Some(a) => self.on_ui_answer(a).await,
                    None => {}
                },
                _ = tick.tick() => self.on_tick().await,
            }
        }
        if let Some(child) = self.child.take() {
            child.reap(Duration::from_secs(3)).await;
        }
    }

    // ---- addressing -------------------------------------------------------

    fn row(&self) -> i64 {
        self.state.lock().unwrap().session_id
    }

    fn note(&self, message: impl Into<String>) {
        let session = self.state.lock().unwrap().session_id;
        let _ = self.ev.send(EngineEvent::Note { message: message.into(), session: Some(session) });
    }

    fn err(&self, message: impl Into<String>) {
        let _ = self.ev.send(EngineEvent::Error { message: message.into(), fatal: false });
    }

    /// Why a command was refused, in the two places a caller can look: the
    /// transcript (a human reads it) and `state()` (a poller counts it).
    fn refuse(&self, reason: String) {
        {
            let mut st = self.state.lock().unwrap();
            st.refusal_seq += 1;
            st.workspace_refusal = Some(reason.clone());
        }
        self.err(reason);
    }

    // ---- the child process ------------------------------------------------

    async fn request(&self, cmd: Value, timeout: Duration) -> Result<Value, String> {
        match &self.child {
            Some(c) => match c.request(cmd, timeout).await {
                Ok(v) => Ok(v),
                Err(e) => Err(self.explain(&e).await),
            },
            None => Err("no model process is running for this workspace".into()),
        }
    }

    /// Turn a protocol error into something a user can act on. pi's stderr tail
    /// rides along when the process died: "pi exited" explains nothing.
    async fn explain(&self, e: &PiError) -> String {
        match e {
            PiError::Closed { command } | PiError::Timeout { command, .. } => {
                let tail = match &self.child {
                    Some(c) => c.stderr_tail().await,
                    None => String::new(),
                };
                if tail.is_empty() {
                    format!("the model process did not answer {command}")
                } else {
                    format!("the model process did not answer {command}; its last lines:\n{tail}")
                }
            }
            other => other.to_string(),
        }
    }

    async fn ensure_child(&mut self) -> Result<(), String> {
        if let Some(c) = &self.child {
            if c.is_up() {
                return Ok(());
            }
        }
        if let Some(c) = self.child.take() {
            c.reap(Duration::from_secs(2)).await;
        }
        let mut launch = self.cfg.launch.clone();
        launch.session_file = self.map.get(self.row()).map(str::to_string).map(PathBuf::from);
        // Name the root for the tools instead of trusting an inherited cwd. A shim that
        // guesses its confinement from the shell that spawned it is a shim that will be
        // right until someone changes how it is started.
        launch.extra_env.push((
            "CWOFFEE_WORKSPACE".to_string(),
            launch.workdir.to_string_lossy().to_string(),
        ));
        // The tools ask the engine how risky a call is, and the answer depends on the
        // approval mode. Named here so a tool host never guesses it. (Fixed for the life
        // of the child: changing the mode mid-session is not seen by an already-spawned
        // pi — noted in the plan.)
        launch.extra_env.push(("CWOFFEE_MODE".to_string(), self.state.lock().unwrap().mode.as_str().to_string()));
        let (child, mut rx) = PiChild::spawn(&launch).await.map_err(|e| self.explain_now(&e))?;
        let pid = child.pid;
        self.child = Some(child);
        // One hop per record into the bridge's own stream, so a respawn does not
        // change the `select!` arm the main loop reads.
        let rec_tx = self.rec_tx.clone();
        tokio::spawn(async move { while let Some(v) = rx.recv().await {
            if rec_tx.send(v).is_err() {
                break;
            }
        } });
        let t0 = Instant::now();
        let data = self.request(json!({ "type": "get_state" }), Duration::from_secs(30)).await?;
        self.absorb_state(&data);
        if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
            eprintln!(
                "[bridge] pi ready in {} ms (pid {pid}, resume {:?})",
                t0.elapsed().as_millis(),
                launch.session_file.as_ref().map(|p| p.display().to_string())
            );
        }
        Ok(())
    }

    fn explain_now(&self, e: &PiError) -> String {
        // The async path wants stderr lines; this one runs before a child exists.
        e.to_string()
    }

    /// Read what pi says about itself: the model it picked, its window, and the
    /// session file that identifies this conversation on disk.
    fn absorb_state(&mut self, data: &Value) {
        let mut st = self.state.lock().unwrap();
        if let Some(m) = data.get("model") {
            if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                st.model = id.to_string();
            }
            if let Some(w) = m.get("contextWindow").and_then(|v| v.as_u64()) {
                st.model_window = Some(w as usize);
            }
        }
        let row = st.session_id;
        let file = data.get("sessionFile").and_then(|v| v.as_str()).map(str::to_string);
        drop(st);
        if let (row, Some(f)) = (row, file) {
            if row != 0 {
                self.map.set(row, &f);
            }
        }
    }

    // ---- commands ---------------------------------------------------------

    async fn on_cmd(&mut self, cmd: HostCmd) {
        self.last_activity = Instant::now();
        // Retargeting the conversation in the middle of a turn is deferred, not obeyed.
        //
        // The Rust core queues command verbs until the turn finishes (module doc,
        // `crates/engine/src/host.rs`). A pi bridge is a `select!` loop, so without this
        // the click is answered *during* the turn and the rest of that turn — its tool
        // result and its closing answer — is written into the conversation the user just
        // clicked. Recorded reproduction: session A kept only the prompt and an empty
        // assistant row while session B received A's tool result and final answer; the
        // same script against the Rust core left A with all four rows and B empty.
        // `scripts/gateway-midturn-switch.mjs` gates the rule on both cores.
        if matches!(cmd, HostCmd::SwitchSession { .. } | HostCmd::ClearSession | HostCmd::NewSession { .. })
            && (self.tr.running() || self.state.lock().unwrap().busy)
        {
            self.deferred.push(cmd);
            return;
        }
        match cmd {
            HostCmd::Send { text, attachments, session } => self.on_send(text, attachments, session).await,
            HostCmd::Cancel => {
                if let Err(e) = self.request(json!({ "type": "abort" }), Duration::from_secs(10)).await {
                    self.err(format!("could not stop the turn: {e}"));
                }
            }
            HostCmd::SetMode { mode } => {
                if matches!(mode, ApprovalMode::Ask) {
                    self.auto_allow.clear();
                }
                let mut st = self.state.lock().unwrap();
                st.mode = mode;
            }
            HostCmd::SetReasoning { reasoning } => {
                let level = pi_level(&reasoning);
                match self.request(json!({ "type": "set_thinking_level", "level": level }), Duration::from_secs(10)).await {
                    Ok(_) => {
                        let mut st = self.state.lock().unwrap();
                        st.reasoning = reasoning;
                    }
                    Err(e) => self.err(format!("the model refused that reasoning level: {e}")),
                }
            }
            HostCmd::SetModel { model, persist } => {
                let provider = self.cfg.launch.provider.clone();
                match self.request(json!({ "type": "set_model", "provider": provider, "modelId": model }), Duration::from_secs(20)).await {
                    Ok(data) => {
                        {
                            let mut st = self.state.lock().unwrap();
                            st.model = data
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&model)
                                .to_string();
                            if let Some(w) = data.get("contextWindow").and_then(|v| v.as_u64()) {
                                st.model_window = Some(w as usize);
                            }
                        }
                        self.cfg.launch.model = model.clone();
                        if persist {
                            self.note(
                                "the model change applies to this run; writing the default model to the config \
                                 file is not wired on the pi core yet (PI-CORE-PLAN Phase 4)",
                            );
                        }
                    }
                    Err(e) => self.err(format!("could not switch model: {e}")),
                }
            }
            HostCmd::SetWorkspace { path } => {
                if self.state.lock().unwrap().busy {
                    self.refuse(format!(
                        "not moving the tools to {path}: a turn is running. Stop it first — a turn that kept \
                         running after its folder changed would write into the old one."
                    ));
                    return;
                }
                if let Some(child) = self.child.take() {
                    child.reap(Duration::from_secs(3)).await;
                }
                // Same probe again, from the other side of the mailbox: a direct
                // `HostCmd` push must not be able to skip what the public verb checks.
                let checked = match cwoffee_engine::host::check_workspace(&path) {
                    Ok(p) => p,
                    Err(reason) => {
                        self.refuse(format!("{path}: {reason}"));
                        return;
                    }
                };
                let path = checked.display().to_string();
                self.cfg.launch.workdir = checked.clone();
                {
                    let mut st = self.state.lock().unwrap();
                    st.workspace = path.clone();
                    // Do NOT bump refusal_seq here. It is the caller's signal that a
                    // refusal landed, and bumping it on success made every successful
                    // move look like a refusal to the poller — the gateway smoke proved
                    // it: the folder really moved and the client was told "the move was
                    // refused". Success clears the last refusal and says nothing new.
                    st.workspace_refusal = None;
                }
                let _ = self.cfg.store.add_workspace(&path);
                self.note(format!("the tools now work in {path}"));
            }
            HostCmd::NewSession { title } => {
                let (workspace, model) = {
                    let st = self.state.lock().unwrap();
                    (st.workspace.clone(), st.model.clone())
                };
                match self.cfg.store.create_session(&title, &workspace, &model) {
                    Ok(id) => {
                        self.enter_row(id, &title).await;
                        if self.child.is_some() {
                            match self.request(json!({ "type": "new_session" }), Duration::from_secs(15)).await {
                                Ok(_) => {
                                    if let Ok(d) = self.request(json!({ "type": "get_state" }), Duration::from_secs(10)).await {
                                        self.absorb_state(&d);
                                    }
                                }
                                Err(e) => self.err(format!("the conversation list grew, but pi could not start a fresh thread: {e}")),
                            }
                        }
                    }
                    Err(e) => self.err(format!("could not create the conversation: {e}")),
                }
            }
            HostCmd::SwitchSession { id } => match self.cfg.store.session(id) {
                Ok(Some(s)) => {
                    self.enter_row(id, &s.title).await;
                }
                Ok(None) => self.err(format!(
                    "there is no conversation {id} to open; the list was not changed and this window stayed on {}",
                    self.state.lock().unwrap().session_id
                )),
                Err(e) => self.err(format!("could not read conversation {id}: {e}")),
            },
            HostCmd::ClearSession => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.session_id = 0;
                }
                let _ = self.ev.send(EngineEvent::SessionChanged { session_id: 0, title: String::new() });
            }
            HostCmd::Shutdown => {} // handled by the loop, which reaps the child
        }
    }

    /// Move this bridge's view to a stored conversation, and make pi talk about
    /// the same one. An unmapped row is not an error: its transcript is in the
    /// store, and pi starts a fresh thread with it said out loud — which is what
    /// a conversation carried over from the Rust core needs until Phase 4 feeds
    /// history in.
    /// A conversation and the folder it lives in travel together.
    ///
    /// The Rust core rebuilds its tool context from the stored session row, so opening
    /// a conversation that lives somewhere else moves the tools with it. A pi process
    /// cannot change the working directory of a live session (PI-CORE-PROTOCOL), so the
    /// move is process lifecycle: close the child, relaunch it in the other folder, and
    /// let `ensure_child` resume this row's session file there.
    ///
    /// Refusing while a turn runs is the other half of the rule (decision 11): moving
    /// the folder out from under an in-flight turn would send its writes into a tree the
    /// user never chose for that conversation. Returns false when the move — and so the
    /// turn that asked for it — was refused.
    async fn follow_workspace(&mut self, id: i64) -> bool {
        if id == self.row() {
            // The same conversation: a folder the user chose by hand wins over whatever
            // the row still says.
            return true;
        }
        let want = match self.cfg.store.session(id) {
            Ok(Some(s)) => s.workspace.trim().to_string(),
            _ => return true,
        };
        let have = self.cfg.launch.workdir.to_string_lossy().to_string();
        if want.is_empty() || want == have {
            return true;
        }
        if self.state.lock().unwrap().busy || self.tr.running() {
            self.err(format!(
                "conversation {id} lives in {want}, but the tools are working in {have} and a turn is running                  there; nothing was moved and nothing was sent. Stopping that turn first is what makes the                  folder change safe — its writes would otherwise land in the wrong tree."
            ));
            return false;
        }
        if let Some(child) = self.child.take() {
            child.reap(Duration::from_secs(3)).await;
        }
        self.cfg.launch.workdir = PathBuf::from(&want);
        {
            let mut st = self.state.lock().unwrap();
            st.workspace = want.clone();
            st.workspace_refusal = None;
        }
        let _ = self.cfg.store.add_workspace(&want);
        // Said out loud because the alternative is the bug this closes: the model is told
        // one folder by pi's own prompt and the user is looking at another in the sidebar.
        self.note(format!(
            "the tools moved to {want} — that is where conversation {id} lives; the model's own folder moved              with them"
        ));
        true
    }

    async fn enter_row(&mut self, id: i64, title: &str) -> bool {
        if !self.follow_workspace(id).await {
            return false;
        }
        {
            let mut st = self.state.lock().unwrap();
            st.session_id = id;
        }
        self.tr.session = id;
        let _ = self.ev.send(EngineEvent::SessionChanged { session_id: id, title: title.to_string() });
        if self.child.as_ref().map(|c| c.is_up()) == Some(true) {
            match self.map.get(id) {
                Some(file) => {
                    let req = json!({ "type": "switch_session", "sessionPath": file });
                    match self.request(req, Duration::from_secs(15)).await {
                        Ok(d) => {
                            if d.get("cancelled").and_then(|v| v.as_bool()) == Some(true) {
                                self.err("an extension refused the conversation switch; staying where this window was");
                            }
                        }
                        Err(e) => self.err(format!("pi could not open {file}: {e}")),
                    }
                }
                None => {
                    if let Err(e) = self.request(json!({ "type": "new_session" }), Duration::from_secs(15)).await {
                        self.err(format!("pi could not start a thread for conversation {id}: {e}"));
                    } else {
                        if let Ok(d) = self.request(json!({ "type": "get_state" }), Duration::from_secs(10)).await {
                            self.absorb_state(&d);
                        }
                        self.note(format!(
                            "conversation {id} has no pi thread of its own yet, so the model starts this one from \
                             scratch; the stored transcript is unchanged (PI-CORE-PLAN Phase 4)"
                        ));
                    }
                }
            }
        }
        true
    }

    async fn on_send(&mut self, text: String, attachments: Vec<Attachment>, session: Option<i64>) {
        if !attachments.is_empty() {
            // Silent attachment loss is the worst possible outcome: the user asked
            // about a spreadsheet and the model answers from its imagination.
            self.err(format!(
                "the pi core cannot carry {} attachment(s) yet — name the file and its path instead, and the \
                 file tools will open it (PI-CORE-PLAN Phase 2)",
                attachments.len()
            ));
            return;
        }
        if text.trim().is_empty() {
            self.err("an empty message was refused before it reached the model");
            return;
        }

        // The address, resolved exactly the way SessionHost does it: naming a
        // conversation moves the host there; naming nothing lands wherever the
        // last switch left us; naming a row that does not exist is refused.
        let current = self.row();
        let target = match session {
            None => current,
            Some(0) => 0,
            Some(id) if id == current => current,
            Some(id) => match self.cfg.store.session(id) {
                Ok(Some(_)) => id,
                Ok(None) => {
                    self.err(format!(
                        "that turn named conversation {id}, which does not exist; nothing was sent and this window \
                         stayed on {current}"
                    ));
                    return;
                }
                Err(e) => {
                    self.err(format!("that turn named conversation {id}, which could not be read: {e}"));
                    return;
                }
            },
        };
        if target != current {
            let title = self.title_for(target, &text);
            if !self.enter_row(target, &title).await {
                // The refusal said why. Sending anyway would have put this turn into the
                // conversation the user was *leaving*, in the folder they were not in.
                return;
            }
        }
        let row = self.row();
        let row = if row == 0 {
            let title = self.title_for(0, &text);
            let (workspace, model) = {
                let st = self.state.lock().unwrap();
                (st.workspace.clone(), st.model.clone())
            };
            match self.cfg.store.create_session(&title, &workspace, &model) {
                Ok(id) => {
                    self.enter_row(id, &title).await;
                    id
                }
                Err(e) => {
                    self.err(format!("could not create the conversation for this turn: {e}"));
                    return;
                }
            }
        } else {
            row
        };

        if let Err(e) = self.ensure_child().await {
            self.err(format!("cannot start the model process: {e}"));
            return;
        }
        self.tr.session = row;
        self.tr.model = self.state.lock().unwrap().model.clone();
        self.last_activity = Instant::now();
        {
            let mut st = self.state.lock().unwrap();
            st.busy = true;
        }
        match self.request(json!({ "type": "prompt", "message": text }), Duration::from_secs(30)).await {
            Ok(data) => {
                // "started" is an acceptance, not a finished turn; the stream
                // says when it is over. Saying otherwise would double-count.
                if let Some(d) = data.get("disposition").and_then(|v| v.as_str()) {
                    if d != "started" && std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                        eprintln!("[bridge] prompt disposition was {d}");
                    }
                }
            }
            Err(e) => {
                self.state.lock().unwrap().busy = false;
                let out = self.tr.abort_run("the turn never started");
                self.apply(out);
                self.err(format!("the model process refused the turn: {e}"));
            }
        }
    }

    fn title_for(&self, row: i64, text: &str) -> String {
        if row != 0 {
            if let Ok(Some(s)) = self.cfg.store.session(row) {
                return s.title;
            }
        }
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("new conversation");
        let chars: Vec<char> = first.trim().chars().take(60).collect();
        chars.into_iter().collect()
    }

    // ---- the stream -------------------------------------------------------

    fn apply(&mut self, out: Output) {
        let row = self.row();
        for m in &out.rows {
            if let Err(e) = self.cfg.store.append_message(row, m) {
                // The transcript is the user's record of what happened; failing to
                // write it is worth saying out loud, not logging quietly.
                self.err(format!("could not store that message: {e}"));
            }
        }
        for ev in out.events {
            let _ = self.ev.send(ev);
        }
        if let Some(n) = out.note {
            let session = self.state.lock().unwrap().session_id;
            let _ = self.ev.send(EngineEvent::Note { message: n, session: Some(session) });
        }
        for u in out.ui {
            // Dialogs are handled inline on purpose: they only touch bridge state
            // and write one line to a pipe, and pi's own timeout is what
            // eventually releases the turn.
            self.on_ui_request(u);
        }
        {
            let mut st = self.state.lock().unwrap();
            if let Some(ct) = out.context_tokens {
                st.context_tokens = ct;
            }
            if out.settled {
                st.busy = false;
                st.turns += 1;
                self.last_activity = Instant::now();
            }
        }
        if out.settled && !self.deferred.is_empty() {
            // The turn boundary: whatever the user asked this window to become while a
            // turn was running is applied now, in the order it arrived. Re-sent through
            // the mailbox rather than run inline, because this function is sync and a
            // command that still finds a turn running re-queues itself the same way.
            for c in std::mem::take(&mut self.deferred) {
                let _ = self.cmd_tx.send(c);
            }
        }
        let compactions = out.compaction_start;
        if compactions {
            self.state.lock().unwrap().compactions += 1;
        }
    }

    fn on_ui_request(&mut self, u: UiRequest) {
        let now = Instant::now();
        match u {
            UiRequest::Confirm { pi_id, title, message, timeout_ms } => {
                let tool = tool_from_title(&title);
                let workdir = self.cfg.launch.workdir.clone();
                let escapes = outside_paths(&message, &workdir);
                let risk = if escapes.is_empty() { Risk::Workspace } else { Risk::Sensitive };
                let mode = self.state.lock().unwrap().mode.clone();
                if matches!(mode, ApprovalMode::Auto) || self.auto_allow.contains(&tool) {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "confirmed": true }));
                    self.note(format!("{tool} was allowed without asking (approval mode {:?})", mode));
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                // The summary is what a person reads before deciding, so it says what
                // the Rust core says: the tool, the thing it wants, and which paths
                // leave the workspace. pi's dialog carries only title and message, so
                // the escape list is derived here from the message text.
                let summary = if escapes.is_empty() {
                    if message.is_empty() { format!("{tool}: {title}") } else { format!("{tool}: {message}") }
                } else {
                    format!(
                        "{tool}: {} — OUTSIDE the workspace: {}",
                        if message.is_empty() { title.clone() } else { message.clone() },
                        escapes.join(", ")
                    )
                };
                let req = ApprovalRequest {
                    id,
                    tool: tool.clone(),
                    risk,
                    summary,
                    args: json!({ "title": title, "message": message }),
                    escapes,
                };
                let (tx, rx) = oneshot::channel();
                // One card, one owner. The shell that took the approvals receiver turns
                // the request into the event every front end renders — `approval_event`
                // is shared precisely so the desktop, the gateway and this bridge cannot
                // disagree about the card. Emitting it here too put **two identical cards
                // on screen** under the gateway, which the pi-core smoke caught. Only if
                // nobody took the receiver do we render it ourselves, and then we answer
                // cancelled rather than leave pi waiting on a client that does not exist.
                if self.ap.send((req.clone(), tx)).is_err() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    let _ = self.ev.send(approval_event(&req));
                    self.err("an approval arrived with nobody to answer it; it was cancelled instead of left open");
                    return;
                }
                let secs = timeout_ms.max(1000) / 1000;
                self.waits.insert(
                    id,
                    Wait::Approval { pi_id: pi_id.clone(), tool, deadline: Instant::now() + Duration::from_secs(secs) },
                );
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(a) => Some(a),
                        Err(_) => None,
                    };
                    let _ = ui_tx.send(UiAnswer::Approval { id, answer });
                });
            }
            UiRequest::Select { pi_id, title, options, timeout_ms } => {
                if options.is_empty() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    self.err("the model offered a choice with no options; it was cancelled rather than answered blindly");
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                let q = Question {
                    id,
                    question: title,
                    options: options.iter().map(|o| QuestionOption { label: o.clone(), detail: None }).collect(),
                    recommended: None,
                    allow_custom: false,
                    multi: false,
                    set: None,
                };
                let (tx, rx) = oneshot::channel();
                // Same rule as the approval above: the shell renders it. A second send
                // would show the question twice and answer it once.
                if self.q.send((q.clone(), tx)).is_err() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    let _ = self.ev.send(question_event(&q));
                    return;
                }
                let secs = timeout_ms.max(1000) / 1000;
                self.waits.insert(
                    id,
                    Wait::Question {
                        pi_id: pi_id.clone(),
                        kind: QKind::Select { options },
                        deadline: Instant::now() + Duration::from_secs(secs),
                        answered: false,
                    },
                );
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(a) => Some(a),
                        Err(_) => None,
                    };
                    let _ = ui_tx.send(UiAnswer::Question { id, answer });
                });
            }
            UiRequest::Input { pi_id, title, placeholder, timeout_ms } => {
                let id = self.next_id;
                self.next_id += 1;
                let q = Question {
                    id,
                    question: if placeholder.is_empty() { title.clone() } else { format!("{title} — {placeholder}") },
                    options: Vec::new(),
                    recommended: None,
                    allow_custom: true,
                    multi: false,
                    set: None,
                };
                let (tx, rx) = oneshot::channel();
                // Same rule as the approval above: the shell renders it. A second send
                // would show the question twice and answer it once.
                if self.q.send((q.clone(), tx)).is_err() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    let _ = self.ev.send(question_event(&q));
                    return;
                }
                let secs = timeout_ms.max(1000) / 1000;
                self.waits.insert(
                    id,
                    Wait::Question { pi_id: pi_id.clone(), kind: QKind::Input, deadline: Instant::now() + Duration::from_secs(secs), answered: false },
                );
                let ui_tx = self.ui_tx.clone();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(a) => Some(a),
                        Err(_) => None,
                    };
                    let _ = ui_tx.send(UiAnswer::Question { id, answer });
                });
            }
            UiRequest::Unsupported { pi_id, method } => {
                // Answering "cancelled" is how we tell the extension nobody is
                // going to answer, instead of letting it hang to its own timeout.
                self.tr.dialog_resolved(now);
                self.reply(&pi_id, json!({ "cancelled": true }));
                self.note(format!("this interface cannot show a {method} dialog, so it was cancelled"));
            }
        }
    }

    fn reply(&self, pi_id: &str, body: Value) {
        if let Some(child) = &self.child {
            let mut msg = body;
            msg["type"] = json!("extension_ui_response");
            msg["id"] = json!(pi_id);
            child.notify(msg);
        }
    }

    async fn on_ui_answer(&mut self, ans: UiAnswer) {
        let now = Instant::now();
        match ans {
            UiAnswer::Approval { id, answer } => {
                let wait = match self.waits.remove(&id) {
                    Some(Wait::Approval { pi_id, tool, .. }) => Some((pi_id, tool)),
                    other => {
                        if other.is_some() {
                            self.err("an approval answer arrived for something that was not an approval card");
                        }
                        None
                    }
                };
                let Some((pi_id, tool)) = wait else {
                    // The card already closed: pi denied on its own timeout, or
                    // the turn was cancelled. Silently writing a response would
                    // imply the human's answer reached the tool.
                    self.note("an approval was answered after its card had closed; that answer was ignored");
                    return;
                };
                match answer {
                    Some(ApprovalAnswer::Allow) => {
                        self.tr.dialog_resolved(now);
                        self.reply(&pi_id, json!({ "confirmed": true }));
                    }
                    Some(ApprovalAnswer::AllowSession) => {
                        self.tr.dialog_resolved(now);
                        self.auto_allow.insert(tool.clone());
                        self.reply(&pi_id, json!({ "confirmed": true }));
                        self.note(format!("{tool} is now allowed for the rest of this session"));
                    }
                    Some(ApprovalAnswer::Deny) => {
                        self.tr.dialog_resolved(now);
                        self.reply(&pi_id, json!({ "confirmed": false }));
                    }
                    // The sender was dropped: the client went away, or the turn
                    // was cancelled while it was parked. pi's own timeout then
                    // denies, and the transcript says so.
                    None => self.note(format!("{tool} was not answered; the tool did not run")),
                }
            }
            UiAnswer::Question { id, answer } => {
                let wait = match self.waits.get_mut(&id) {
                    Some(Wait::Question { pi_id, kind, answered, .. }) => {
                        *answered = true;
                        Some((pi_id.clone(), match kind {
                            QKind::Select { options } => Some(options.clone()),
                            QKind::Input => None,
                        }))
                    }
                    _ => None,
                };
                let Some((pi_id, options)) = wait else {
                    self.note("a choice was answered after its card had closed; that answer was ignored");
                    return;
                };
                let body = match (&answer, options) {
                    (Some(a), Some(labels)) => {
                        // The UI answers with either a picked index or typed text.
                        let value = match a.options.first() {
                            Some(i) => labels.get(*i).cloned().unwrap_or_else(|| a.text.clone()),
                            None => a.text.clone(),
                        };
                        json!({ "value": value })
                    }
                    (Some(a), None) => json!({ "value": a.text }),
                    (None, _) => json!({ "cancelled": true }),
                };
                self.tr.dialog_resolved(now);
                self.reply(&pi_id, body);
                let _ = self.ev.send(EngineEvent::QuestionAnswered { id });
            }
        }
    }

    async fn on_tick(&mut self) {
        // Close cards that timed out. pi denies/forgets the dialog on its own;
        // what the bridge owes the UI is that a card cannot sit there offering a
        // choice nothing will ever take.
        let now = Instant::now();
        let mut expired = Vec::new();
        for (id, w) in self.waits.iter() {
            match w {
                Wait::Approval { deadline, tool, .. } if now >= *deadline => expired.push((*id, Some(tool.clone()))),
                Wait::Question { deadline, answered, .. } if now >= *deadline && !*answered => expired.push((*id, None)),
                _ => {}
            }
        }
        for (id, tool) in expired {
            self.waits.remove(&id);
            // The engine's own timeout fired: the run is generating again, or it
            // is over. Either way the wait is not the model's remaining time.
            self.tr.dialog_resolved(now);
            match tool {
                Some(tool) => self.note(format!("{tool} was not answered in time; the tool did not run")),
                None => {
                    let _ = self.ev.send(EngineEvent::QuestionExpired { id });
                }
            }
        }

        let dead = self.child.as_ref().map(|c| !c.is_up()).unwrap_or(false);
        if dead {
            if self.tr.running() {
                let out = self.tr.abort_run("the model process ended during the turn");
                self.apply(out);
                {
                    let mut st = self.state.lock().unwrap();
                    st.busy = false;
                }
            }
            self.child = None;
            return;
        }

        // Locked decision 14: close an idle pi, and reopen it against the same
        // pi session file on the next turn. Never while busy, and never with a
        // card open in some window.
        let idle = self.last_activity.elapsed() >= self.cfg.idle_reap;
        if idle && !self.tr.running() && self.waits.is_empty() {
            if let Some(child) = self.child.take() {
                let pid = child.pid;
                child.reap(Duration::from_secs(3)).await;
                self.note(format!(
                    "the model process for this workspace went idle and was closed (pid {pid}); the next turn \
                     reopens the same conversation"
                ));
            }
        }
    }
}

/// pi's `confirm` dialog carries no tool name, and cwoffee's approval card needs
/// one (the card header, and `AllowSession`, which is per tool). Our own
/// extensions shape the title as `Allow <tool>?`, which is the convention
/// recorded in `docs/PI-CORE-PROTOCOL.md`; anything else says "the tool" rather
/// than inventing a name.
fn tool_from_title(title: &str) -> String {
    let t = title.trim();
    let core = t
        .strip_prefix("Allow ")
        .unwrap_or(t)
        .trim_end_matches('?')
        .trim();
    let word: String = core.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
    if word.is_empty() {
        "the tool".into()
    } else {
        word
    }
}

/// Absolute paths in the message that sit outside the workspace. These are what
/// make an approval answerable: "read a file" is not a question, "read
/// /home/me/.ssh/id_ed25519" is, so the card shows them verbatim.
fn outside_paths(text: &str, workdir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let t = token.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-' && c != '~'
        });
        if !t.starts_with('/') || t.len() < 2 {
            continue;
        }
        let p = std::path::PathBuf::from(t);
        if !p.starts_with(workdir) && !out.contains(&t.to_string()) {
            out.push(t.to_string());
        }
    }
    out
}

/// cwoffee's reasoning levels are a subset of pi's vocabulary
/// (`off|minimal|low|medium|high|xhigh|max`).
fn pi_level(r: &Reasoning) -> &'static str {
    match r {
        Reasoning::Off => "off",
        Reasoning::Low => "low",
        Reasoning::Medium => "medium",
        Reasoning::High => "high",
        // The two settings above High only exist on models that expose them;
        // pi clamps a level the selected model does not have.
        _ => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approval_card_names_the_tool_our_extensions_ask_for() {
        // Our extensions title the dialog `Allow <tool>?`; the card header and
        // `AllowSession` both need that name, and inventing one would let a user
        // approve the wrong tool.
        assert_eq!(tool_from_title("Allow read_file?"), "read_file");
        assert_eq!(tool_from_title("Allow sheet_sql?"), "sheet_sql");
        // Anything else says so rather than guessing.
        assert_eq!(tool_from_title("Clear session?"), "Clear");
        assert_eq!(tool_from_title("??"), "the tool");
    }

    #[test]
    fn paths_outside_the_workspace_are_shown_verbatim() {
        // "read a file" is not a question anyone can answer; "read
        // /home/me/.ssh/id_ed25519" is.
        let ws = std::path::Path::new("/home/me/work");
        let got = outside_paths("read_file /home/me/.ssh/id_ed25519", ws);
        assert_eq!(got, vec!["/home/me/.ssh/id_ed25519".to_string()]);
        // Inside the workspace is not an escape, and repeats collapse to one.
        let got = outside_paths("/home/me/work/a.txt /home/me/work/a.txt ./rel.txt", ws);
        assert!(got.is_empty(), "workspace paths were reported as escapes: {got:?}");
    }

    #[test]
    fn reasoning_levels_use_pis_vocabulary() {
        assert_eq!(pi_level(&Reasoning::Off), "off");
        assert_eq!(pi_level(&Reasoning::Medium), "medium");
        assert_eq!(pi_level(&Reasoning::High), "high");
    }
}
