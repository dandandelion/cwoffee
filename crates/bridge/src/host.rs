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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode, ApprovalRequest};
use cwoffee_engine::config::Config;
use cwoffee_engine::tools::searxng::{runtime as searxng_runtime, AutoSearXNG, AutoStatus};
use cwoffee_engine::tools::web::WebSearchTool;
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{approval_event, question_event, ApprovalRx, HostCmd, HostOps, HostState};
use cwoffee_engine::provider::{Attachment, ModelInfo, Reasoning};
use cwoffee_engine::settings::{ProbeHit, SearchProbe, SearchView};
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
    /// `None` — the default — puts no clock on a question card: the turn parks until a
    /// person answers or the turn is stopped. `question_timeout_secs` in config.toml is the
    /// way to ask for a clock anyway.
    pub question_timeout: Option<Duration>,
    /// Locked decision 14: no cap on live pi processes; an idle one is closed
    /// after this long and reopened on the next turn against the same pi session
    /// file, so nothing the model knew is lost.
    pub idle_reap: Duration,
    pub context_budget: usize,
    /// The ceiling as the person left it in `config.toml`: `Some(n)` pins it, `None`
    /// means "derive it from whatever window this model has". Kept apart from the
    /// resolved number because the intent has to outlive the model it was resolved for.
    pub budget_pin: Option<usize>,
    pub model_window: Option<usize>,
    /// cwoffee row id <-> pi session file. See `sessions.rs`.
    pub session_map: PathBuf,
    /// The SQLite file this host opened, handed to the model process so the one tool that
    /// reads conversation history (`recall`) can read it too. `None` when the bridge does not
    /// know — and then `recall` refuses in words instead of answering "you never mentioned
    /// that", which is the difference between a wrong answer and an honest one.
    pub db_path: Option<PathBuf>,
    /// How much of the tool set the model is *shown*. `false` (the default, and today's
    /// behaviour) declares every mounted tool. `true` declares only the two the registry says
    /// must always be there (`Registry::lean_tool_names`) and leaves the rest one `explore`
    /// call away. It is a switch, not a conclusion: on a 262k-token endpoint the full menu is
    /// affordable, and a small local model with a small window pays a fifth of its room for a
    /// menu it will not use. The registry owns which tools those two are.
    pub lean_tools: bool,
    /// Cowork or chat at boot. In Chat the process itself is different: no extension
    /// mounted, `--no-tools`, pi's prompt replaced by one line, and the conversation's
    /// folder left where it is.
    pub persona: cwoffee_engine::host::Persona,
    /// The config file the shells started from, so a mode switch is written down and
    /// survives a restart instead of being a fact about one window.
    pub cfg_path: Option<std::path::PathBuf>,
    /// `max_tool_iters` from the config: 0 means no cap, which is the default and the
    /// reason a long turn has to say so out loud. pi owns the loop on this core, so the
    /// bridge counts the calls and says the engine's sentence.
    pub max_tool_iters: usize,
    /// How long a manual compaction may take before cwoffee stops waiting and says so,
    /// in words, including that nothing was thrown away. A fold of a ~200k-token
    /// conversation is a model call over the whole of it: many minutes on a local
    /// endpoint. A tight ceiling does not make it faster; it makes a compaction that
    /// works look broken, and the user pays for the summary twice.
    pub compact_timeout: Duration,
}

/// The handle the gateway and the desktop window hold, deliberately shaped like
/// `SessionHost`: the receivers are taken once, before this goes behind the `Arc`
/// both shells share, because a second consumer would silently split one stream.
pub struct PiHost {
    cmd_tx: mpsc::UnboundedSender<HostCmd>,
    /// A request that needs an answer, run by the model task against the live
    /// child: the model list, and anything else a front end asks and waits on.
    rpc_tx: mpsc::UnboundedSender<(Value, oneshot::Sender<Result<Value, String>>)>,
    /// Shared with the model task: when a network tool last ran. The panel's own probe
    /// counts, so asking "try one search" keeps alive for the idle window what the probe
    /// just started.
    web_last: Arc<Mutex<Option<Instant>>>,
    state: Arc<Mutex<HostState>>,
    ev_rx: Option<EventRx>,
    ap_rx: Option<ApprovalIn>,
    q_rx: Option<QuestionIn>,
    pub pi_version: String,
    /// How long this side waits on a compaction. The model task runs the same wait from
    /// the config (`compact_timeout`) and is the one that knows the number, writes the
    /// reassuring note, and keeps the token counts out of the message; so this side waits
    /// a little *longer* on purpose. If it gave up first, the caller would get a bare
    /// timeout with no counts and no statement that the history is intact.
    compact_wait: Duration,
}

/// The managed instance, when the config asks Cwoffee to run one. Built from the same
/// config the tool host reads, so the bridge's idea of the container and the tool's are
/// about one container rather than two.
fn managed_searxng() -> Option<AutoSearXNG> {
    match Config::load(None) {
        Ok((cfg, _)) => match cfg.search_backend() {
            cwoffee_engine::tools::web::SearchBackend::Auto(a) => Some(a),
            _ => None,
        },
        Err(_) => None,
    }
}

impl PiHost {
    /// Nothing is started here except a version read. The pi process itself is
    /// started by the first turn that needs it, which is what keeps a desktop
    /// launch from paying ~75 MB and a few hundred milliseconds for a model the
    /// user may not use in this session.
    pub async fn spawn(cfg: PiHostConfig) -> Result<PiHost, PiError> {
        let pi_version = pi_version(&cfg.launch.bin).await?;
        // Read before `cfg` moves into the model task: this side of the channel needs the
        // compaction ceiling too, and needs to wait longer than the task does.
        let compact_wait = cfg.compact_timeout + Duration::from_secs(15);
        // A bridge that died badly leaves its pi behind, and a pi has no reason to notice:
        // it goes on serving a client that is gone and keeps its context loaded while it
        // does. Two such orphans from killed runs measured 205 MB between them, with no
        // idle clock anywhere. Adopt the ledger's processes before starting our own.
        crate::process::adopt_orphans(
            &cfg.launch.agent_dir,
            &cfg.launch.bin,
            |m| eprintln!("{m}"),
        );
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
            persona: cfg.persona,
            reasoning: cfg.reasoning.clone(),
            reasoning_default: cfg.reasoning.clone(),
            reasoning_overridden: false,
            reasoning_levels: Vec::new(),
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
            context_budget: cfg.context_budget,
            budget_pin: cfg.budget_pin,
            model_window: cfg.model_window,
            approval_timeout_secs: cfg.approval_timeout.as_secs(),
            question_timeout_secs: cfg.question_timeout.map(|d| d.as_secs()),
            // Filled in as the run streams (`apply_output` publishes it) — a turn that has
            // not started yet has said nothing.
            open_text: String::new(),
            open_reasoning: String::new(),
            workspace_refusal: None,
            refusal_seq: 0,
        }));
        let map = SessionMap::open(cfg.session_map.clone());
        let web_last = Arc::new(Mutex::new(None::<Instant>));
        let boot_workdir = cfg.launch.workdir.clone();
        let iter_cap = cfg.max_tool_iters;
        // Read before `cfg` moves into the engine: the settings default is needed on both sides of
        // the move (the reported state and the host's own bookkeeping).
        let default_thinking = cfg.reasoning.clone();
        let engine = Engine {
            boot_workdir,
            cfg,
            state: Arc::clone(&state),
            ev: ev_tx,
            ap: ap_tx,
            q: q_tx,
            ui_tx,
            rec_tx,
            child: None,
            tr: Translator::with_iter_cap(iter_cap),
            turn_files: Vec::new(),
            last_assistant_row: None,
            resume_announced: std::collections::HashSet::new(),
            title_slot: cwoffee_engine::titling::TitleSlot::default(),
            row_thinking: HashMap::new(),
            draft_thinking: None,
            applied_thinking: HashMap::new(),
            default_thinking,
            q_sets: HashMap::new(),
            map,
            cmd_tx: cmd_tx.clone(),
            deferred: Vec::new(),
            auto: managed_searxng(),
            auto_idle: Config::load(None).map(|(c, _)| c.search.auto_idle_secs).unwrap_or(120),
            auto_checked: None,
            auto_seen: None,
            web_last: Arc::clone(&web_last),
            auto_allow: HashMap::new(),
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
            web_last: Arc::clone(&web_last),
            rpc_tx,
            state,
            ev_rx: Some(ev_rx),
            ap_rx: Some(ap_rx),
            q_rx: Some(q_rx),
            pi_version,
            compact_wait,
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
    async fn compact(&self, instructions: Option<String>) -> Result<String, String> {
        let mut cmd = serde_json::json!({ "type": "compact" });
        if let Some(i) = instructions.filter(|t| !t.trim().is_empty()) {
            cmd["customInstructions"] = serde_json::Value::String(i);
        }
        let v = self.rpc(cmd, self.compact_wait).await?;
        let before = v.get("tokensBefore").and_then(|x| x.as_u64());
        let after = v.get("estimatedTokensAfter").and_then(|x| x.as_u64());
        Ok(match (before, after) {
            (Some(b), Some(a)) => format!("compacted: {b} tokens before, about {a} after"),
            _ => "compacted; pi reported no token counts".to_string(),
        })
    }

    fn set_persona(&self, persona: cwoffee_engine::host::Persona) -> Result<String, String> {
        // The switch is applied by the model task, which owns the process. What this
        // returns is what will happen; `session_state().persona` is what did.
        self.push(HostCmd::SetPersona { persona });
        Ok(format!(
            "{} requested — applied between turns, since the tool set is chosen when the model process starts",
            persona.as_str()
        ))
    }

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
                // No address here. pi's catalogue names a provider and never a server build, and
                // this field means the serving implementation ("cwoffee-mock 1.0", a llama.cpp
                // build) on the core that can answer it — putting the endpoint URL in this slot
                // would make one field mean two things depending on which core filled it. The
                // address lives on the endpoint picture instead: `provider_status.chosen_base_url`,
                // which both cores answer from the config the app started with.
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
            // The same answers the Rust core's panel gets, from the same code: what the
            // live tool says it is doing, whether a container runtime exists on this
            // machine, and whether the managed container is answering on its port right
            // now. A panel that says "no container runtime" on a box with podman, while a
            // search through the tool host starts one and answers, has been asked nothing.
            describing: cfg.search_backend().describe(),
            runtime: searxng_runtime().map(str::to_string),
            auto: if cfg.search.backend == "auto" {
                let a = AutoSearXNG::new(cfg.search.auto_port, cfg.search.auto_idle_secs, cfg.search_auto_dir());
                Some(a.status().await)
            } else {
                None
            },
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
        let backend = backend.trim();
        let url = searxng_url.trim();
        // The engine's own parser, so a typo is an error here exactly as it is in
        // the Rust core, and both cores read the same file afterwards.
        cwoffee_engine::settings::parse_backend(backend, url, &cfg)?;
        // The values handed to the config writer are TOML *literals*, not bare words. The
        // writer re-parses the edited file before it replaces it — the guard that keeps a
        // settings click from writing a config the next boot cannot open — and `backend = auto`
        // fails that parse, so on this core every provider switch answered
        // `invalid string expected "` and saved nothing while the Rust core quoted it and
        // worked. One helper, one quoting rule, checked by a test that reads the file back.
        let keys = search_keys(backend, url);
        let refs: Vec<(&str, &str)> = keys.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let written = Config::set_search_keys(cfg_path, &refs)?;
        let mut view = self.search_view(cfg_path).await;
        view.saved_to = Some(written.join(", "));
        Ok(view)
    }

    /// "Try one search", asking the provider rather than the model — the same probe the
    /// Rust core runs, through the same tool a session will use. Under a pi core this is
    /// also how the managed container can be started from the panel; the bridge's tick
    /// then owns its idle clock like any other search.
    /// Pin or free the context ceiling.
    ///
    /// Three things have to agree, in that order: the config file (so a restart keeps
    /// it), this host's meter (so the number moves now), and pi's own fold threshold
    /// (so the fold happens where the app says the conversation is full, not at a
    /// default chosen for coding sessions). The file is written here because the answer
    /// has to be built from what the file now says; the meter and pi follow inside the
    /// task that owns the model process, which is also the only place that knows whether
    /// one is up — and its note reports that, instead of this side guessing.
    async fn set_context_budget(
        &self,
        cfg_path: &std::path::Path,
        target: Option<usize>,
    ) -> Result<String, String> {
        // Validate first: a refusal must leave the file, the meter and the running
        // process exactly as they were.
        let window = self.state().model_window;
        if let Some(n) = target {
            cwoffee_engine::host::check_budget(n, window)?;
        }
        let changed = match target {
            Some(n) => cwoffee_engine::config::Config::set_context_keys(
                cfg_path,
                &[("target_tokens", &n.to_string())],
            ),
            None => cwoffee_engine::config::Config::unset_context_keys(cfg_path, &["target_tokens"]),
        }
        .map_err(|e| format!("could not save the ceiling: {e}"))?;
        for c in &changed {
            println!("[cwoffee/pi] context config: {c} ({})", cfg_path.display());
        }
        // Read the file back: what this run applies has to be what the next start reads,
        // or "saved" describes a different app than the one on screen.
        let back = cwoffee_engine::config::Config::load(Some(cfg_path))
            .map(|(c, _)| c.context.target_tokens)
            .unwrap_or(None);
        if back != target {
            return Err(format!(
                "{} now says {:?}, which is not what was asked for ({:?}): nothing was applied",
                cfg_path.display(),
                back,
                target
            ));
        }
        self.push(HostCmd::SetContextBudget { target: back });
        Ok(match back {
            Some(n) => format!(
                "context ceiling pinned at {n} tokens ({}); saved to {}",
                cwoffee_engine::host::budget_share(n, window),
                cfg_path.display()
            ),
            None => format!(
                "context ceiling freed: it will be derived as half of whatever window the endpoint \
                 states for this model; saved to {}",
                cfg_path.display()
            ),
        })
    }

    async fn search_probe(&self, query: Option<String>) -> Result<SearchProbe, String> {
        let (cfg, _) = Config::load(None).map_err(|e| format!("config could not be read: {e}"))?;
        let backend = cfg.search_backend();
        let tool = WebSearchTool::from_backend(backend.clone());
        let q = query.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "coffee roasting".into());
        let t0 = Instant::now();
        *self.web_last.lock().unwrap() = Some(Instant::now());
        let (note, res) = tool.search_with(&q).await;
        Ok(match res {
            Ok(results) => SearchProbe {
                ms: t0.elapsed().as_millis(),
                note,
                backend: backend.describe(),
                results: results.into_iter().take(5).map(|r| ProbeHit { title: r.title, url: r.url }).collect(),
                error: None,
            },
            Err(e) => SearchProbe {
                ms: t0.elapsed().as_millis(),
                note,
                backend: backend.describe(),
                results: Vec::new(),
                error: Some(e),
            },
        })
    }
}

/// What the bridge knows about an outstanding dialog, in both id spaces.
/// Whether a standing grant covers this call. Two conditions, both taken from the Rust
/// One line for the approval card: the tool, the engine's own sentence about the call, and the
/// paths that leave the workspace when that sentence does not already name them.
///
/// The extension hands over `cwtool check`'s summary, and that sentence already carries the
/// escape clause — so appending the list a second time used to produce
/// `read_file: read: /etc/hostname — OUTSIDE the workspace: /etc/hostname — OUTSIDE the workspace: /etc/hostname`.
/// Repeating a path is not emphasis: a card whose sentence stutters is a card a person cannot
/// weigh, and the structured `escapes` field still carries the fact for the security rules that
/// actually read it.
fn approval_summary(tool: &str, message: &str, title: &str, escapes: &[String]) -> String {
    let body = if message.trim().is_empty() { title } else { message };
    if escapes.is_empty() || body.contains("OUTSIDE the workspace") {
        format!("{tool}: {body}")
    } else {
        format!("{tool}: {body} — OUTSIDE the workspace: {}", escapes.join(", "))
    }
}

/// core's gate (engine `ApprovalGate::check`), because a permit means the same thing on
/// either core or the app has two security models:
///
/// * the grant belongs to **this conversation** — "for the rest of this session" means the
///   conversation you clicked in, not every conversation this model process serves; and
/// * the call **leaves nothing behind** — a grant is about the tool in your own folder, and
///   remembering an out-of-workspace yes for the rest of the session would turn one yes
///   into unlimited access outside the workspace.
fn permit_covers(auto: &HashMap<i64, HashSet<String>>, row: i64, tool: &str, escapes: &[String]) -> bool {
    escapes.is_empty() && auto.get(&row).map_or(false, |s| s.contains(tool))
}

enum Wait {
    /// `row` is the conversation whose turn raised the card. A standing permit
    /// belongs to that conversation and to no other.
    Approval {
        pi_id: String,
        tool: String,
        deadline: Instant,
        row: i64,
    },
    Question {
        pi_id: String,
        kind: QKind,
        /// `None`: this card has no clock. The turn is parked, not failing — see
        /// `PiHostConfig::question_timeout`.
        deadline: Option<Instant>,
        answered: bool,
        /// The set this question belongs to, in this process's numbering. An answered or
        /// expired question releases its set, which is what lets a later call reuse the
        /// caller's key without reusing this card.
        set_id: Option<u64>,
    },
}

/// What an unanswered card was, so the expiry can be said in the right words and the right
/// set slot can be released.
enum Expired {
    Approval(String),
    Question,
}

enum QKind {
    /// pi's `select` takes plain strings and returns the one chosen.
    Select { options: Vec<String> },
    Input,
}

/// A cwoffee tool that asks a person (today: `ask_user`) cannot hold the question open
/// inside its own process — the tool host is a short-lived `cwtool` run — so it asks
/// through pi's dialog primitives and the answers travel back as an argument. pi's
/// primitives carry a title and a list of labels, and nothing else: no trade-off lines,
/// no recommended option, no "pick more than one", no "this is question 2 of 3". So the
/// engine's own `Question` rides in the title after this marker, and the card the user
/// answers is the card the engine designed rather than a flattened summary of it. If the
/// payload ever fails to parse, the arms below fall back to treating the title as the
/// question, which is what any other extension's dialog would do.
pub const CANON_Q: &str = "CWOFFEEQ ";

/// The coworker's prompt, plus the one sentence that depends on where this conversation is.
///
/// pi appends a `<cwd>` block naming the working directory, and for a conversation with no
/// workspace that directory is a scratch folder under the system's temp root. Left alone the
/// model reads that block and tells the person it is working in `/tmp` — which is the report:
/// "the UI should say No workspace and the model should say none". The Rust core says the
/// same thing in its `<env>` block (`engine::host::default_env_block`); two cores owe the
/// person one answer, not two.
///
/// It is appended rather than folded into the shared prompt text because it is the one part
/// of the stable prefix that varies by conversation, and it varies at most once per session:
/// the bridge already restarts the child when the workspace changes, so nothing that was paid
/// for is thrown away.
fn cowork_prompt_with_workspace_note(lean: bool, workdir: &Path) -> String {
    let mut prompt = cwoffee_engine::prompt::cowork_prompt_text(lean);
    if cwoffee_engine::host::is_temp_root(&workdir.to_string_lossy()) {
        prompt.push_str(
            "\n\nThis conversation has no workspace. The folder named in <cwd> is a scratch folder this app made for itself: nobody chose it, it holds none of the person's files, and it is not theirs to be told about. Never call it their workspace, and if asked where they are working, answer \"none\". Files written with relative paths are still kept for this conversation.",
        );
    }
    prompt
}

/// The `[search]` keys a backend change writes, already quoted as TOML literals.
///
/// Split out because the quoting is the whole failure this prevents. The config writer
/// parses the edited text before it replaces the file — a settings click must not be able to
/// write a config the next boot cannot open — and a bare word is not a TOML string. Handing
/// it `backend = auto` made the write refuse itself, so the panel kept the old provider while
/// the click looked like it had done nothing but show an error, in the app's own words:
/// `invalid string expected "`. The Rust core quoted through `Config::toml_str`; this core
/// passed the word through. A test re-reads the written file, and
/// `scripts/gateway-search-write.mjs` asks the same question over the wire on either core.
///
/// The URL is written when the choice needs one. An address already in the file is left
/// there when the choice moves away, on purpose: somebody who tried SearXNG and went back to
/// DuckDuckGo should not have to type their address again to try it once more, and the panel
/// keeps the box filled for the same reason. Which is why the wire gate asserts that the
/// *choice* moved and that the file still parses, rather than asserting the line vanishes.
fn search_keys(backend: &str, url: &str) -> Vec<(String, String)> {
    let mut keys = vec![("backend".to_string(), Config::toml_str(backend))];
    if backend == "searxng" && !url.is_empty() {
        keys.push(("searxng_url".to_string(), Config::toml_str(url)));
    }
    keys
}

/// The card's own clock, if it has one. `None` means the card never closes by itself.
///
/// The tool extension asks pi's dialog primitives without a `timeout` for a cwoffee
/// question, so `timeout_ms` arrives 0 and the card parks until a person answers or the
/// turn is stopped — which is what the user asked for after three-minute cards answered
/// questions they needed to take to somebody else. Any dialog that does state a budget is
/// still honoured: the clock has to come from the caller, not from a default in here.
fn question_deadline(timeout_ms: u64) -> Option<Instant> {
    if timeout_ms == 0 {
        return None;
    }
    Some(Instant::now() + Duration::from_millis(timeout_ms.max(1000)))
}

/// Recover the engine's question from a dialog title, and re-number it for this process.
///
/// The ids a client answers back are the ids this process announced on the card, and the
/// set ids the UI groups by have to be unique *here*: the ids in the payload came from a
/// `cwtool` process that has already exited, and the next ask_user in the next
/// conversation hands out the same numbers. So the payload's call key (`k`) maps to a set
/// id of this process's own, and one key keeps one id while the card is open — which is
/// what makes "question 2 of 3" true rather than approximate.
/// One question set as this process numbers it: the id the UI groups by, how many
/// questions the set promised, and how many have arrived / been closed.
#[derive(Debug, Clone)]
struct QSet {
    id: u64,
    total: usize,
    seen: usize,
    closed: usize,
}

/// Answer to give back for a set whose questions have all arrived but whose card should no
/// longer accept members. A set that is complete and closed is dropped from the map, which
/// is what lets a later call reuse the caller's key and still get its own card.
/// Close one slot of a set. An answered question and an expired one are the same thing to
/// the card: that question is finished. Once every question the set promised is closed, the
/// set leaves the map — which is what lets a later call arrive with the same caller key and
/// build its own card instead of appending questions to one that has already settled.
fn mark_set_closed(sets: &mut HashMap<String, QSet>, set_id: u64) {
    for e in sets.values_mut() {
        if e.id == set_id {
            e.closed += 1;
        }
    }
    sets.retain(|_, e| e.closed < e.total);
}

fn canon_question(title: &str, next_id: &mut u64, sets: &mut HashMap<String, QSet>) -> Option<Question> {
    let rest = title.strip_prefix(CANON_Q)?;
    let envelope: Value = serde_json::from_str(rest).ok()?;
    let mut q: Question = serde_json::from_value(envelope.get("q")?.clone()).ok()?;
    q.id = *next_id;
    *next_id += 1;
    if let Some(set) = q.set.as_mut() {
        let key = envelope.get("k").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let want_total = set.total;
        let reusable = sets
            .get(&key)
            .filter(|e| e.seen < e.total && e.closed < e.total)
            .map(|e| e.id);
        let id = reusable.unwrap_or_else(|| {
            let fresh = *next_id;
            *next_id += 1;
            sets.insert(key.clone(), QSet { id: fresh, total: want_total.max(1), seen: 0, closed: 0 });
            fresh
        });
        if let Some(e) = sets.get_mut(&key) {
            e.seen += 1;
        }
        set.id = id;
    }
    Some(q)
}

enum UiAnswer {
    /// `None` means the card closed without an answer (closed webview, cancel).
    Approval { id: u64, answer: Option<ApprovalAnswer> },
    Question { id: u64, answer: Option<Answer> },
}

struct Engine {
    cfg: PiHostConfig,
    /// Where the app was started. Chat mode runs its process here instead of in a
    /// conversation's folder; nothing in a chat reads or writes that folder.
    boot_workdir: std::path::PathBuf,
    state: Arc<Mutex<HostState>>,
    ev: mpsc::UnboundedSender<EngineEvent>,
    ap: mpsc::UnboundedSender<ApprovalRx>,
    q: mpsc::UnboundedSender<QuestionRx>,
    ui_tx: mpsc::UnboundedSender<UiAnswer>,
    rec_tx: mpsc::UnboundedSender<Value>,
    child: Option<PiChild>,
    tr: Translator,
    /// Tool-call id -> the store row that holds that call's result, and files whose row
    /// has not landed yet. pi splits the two across records; the pairing is what makes the
    /// file chips appear on the right row on this core.
    /// What the tools made during this run, and the row to attach it to. Collected across
    /// the run and stamped on the **answer that closed the turn**, because that is where
    /// the Rust core puts it ("a reload shows the same attachments the live turn showed")
    /// and where the transcript's file strip knows to render. pi reports files per tool
    /// call, and the row carrying them is a tool row — a shape the UI has no rendering
    /// for, so a faithful per-call stamp produced a turn with six files and no strip.
    turn_files: Vec<cwoffee_engine::tools::OutputFile>,
    last_assistant_row: Option<i64>,
    /// Conversations this process has already explained on reopening. The rust core says
    /// it once per session per process; saying it on every switch would train the user to
    /// ignore a note that is only worth reading when the history is reopened.
    resume_announced: std::collections::HashSet<i64>,
    /// One title request at a time, for the same reason the Rust core keeps one: the
    /// ask delays a first answer, and two conversations should not both be stalled
    /// while one of them waits for a name.
    title_slot: cwoffee_engine::titling::TitleSlot,
    /// The thinking effort chosen per conversation, for this run of the app.
    ///
    /// pi keeps a level inside each session file and restores it on `switch_session`; what this map
    /// carries is the level chosen *during* this run, which has to survive the conversation's own
    /// creation. That creation is lazy - a draft's first send makes the row and asks pi for a new
    /// thread in the same breath - and a level applied to the pi session before that was thrown away
    /// by it: pi starts a thread at its own default. Which is why the composer dial used to snap
    /// back to the settings value at the moment the user pressed send.
    row_thinking: HashMap<i64, Reasoning>,
    /// A level chosen while the conversation is still a draft (row 0). It follows the draft into
    /// the row it becomes.
    draft_thinking: Option<Reasoning>,
    /// The level this process last *set* for a conversation, whether it came from the person's
    /// override or from the settings default at the conversation's creation. Not the person's
    /// choice - that is `row_thinking` - and it exists because pi's own `get_state` answers with the
    /// level the model can actually run (for a model that cannot think, "off"), which is a fact
    /// about the model and not an answer to "what did this conversation ask for".
    applied_thinking: HashMap<i64, Reasoning>,
    /// What the settings screen says a new conversation starts at.
    default_thinking: Reasoning,
    map: SessionMap,
    cmd_tx: mpsc::UnboundedSender<HostCmd>,
    /// Conversation-retargeting verbs that arrived during a turn. The Rust core queues
    /// command verbs until the turn finishes; a `select!` loop owes the same, or the rest
    /// of a running turn lands in whichever conversation the user clicked.
    deferred: Vec<HostCmd>,
    /// The managed SearXNG the config asks for, when that is the managed choice. The
    /// tool host starts it lazily, exactly as the Rust core's tool does. What the bridge
    /// has to own is the **idle clock**: the reaper that stops an idle container lives in
    /// the process that started it, and under a pi core that process is a `cwtool` call
    /// that exits seconds later — so the container would otherwise run forever.
    auto: Option<AutoSearXNG>,
    auto_idle: u64,
    auto_checked: Option<Instant>,
    /// When this app first saw an adopted container answering. Only used as the idle
    /// clock when the app itself has not run a search through it.
    auto_seen: Option<Instant>,
    /// When a network tool last actually ran (in a turn, or the panel's own probe). The
    /// bridge cannot see inside the tool host's clock, so it keeps its own.
    web_last: Arc<Mutex<Option<Instant>>>,
    /// Approvals granted with `AllowSession`: the same tool is not asked again.
    /// "allow for the rest of this session", by conversation. Keyed by the session row —
    /// `sessions.id` is AUTOINCREMENT, so an id never comes back to haunt a later
    /// conversation. A host-wide set would mean that clicking "allow for this session"
    /// while talking about one folder also silenced the card in every other conversation
    /// this process serves, which is not what the button says.
    auto_allow: HashMap<i64, HashSet<String>>,
    waits: HashMap<u64, Wait>,
    /// Call key from a cwoffee question payload -> the set this process built for it.
    ///
    /// The engine numbers a fresh set in every `cwtool` process, so its set id says
    /// "first set of this call" and nothing else; even the call id is not reliable, because
    /// a scripted endpoint hands the same tool-call id to the same scenario every time it
    /// runs. So a key maps to a card **while that card is still open**: once every question
    /// the set promised has arrived, and then answered or expired, the key is released and
    /// the next call that reuses it gets a card of its own.
    q_sets: HashMap<String, QSet>,
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
                        } else if cmd.get("type").and_then(|t| t.as_str()) == Some("compact") {
                            // Nothing running means no thread to fold: pi's session file
                            // belongs to a process that is not up, and asking a model that
                            // is not running to summarize a conversation it never held is a
                            // question that cannot be answered.
                            if self.state.lock().unwrap().busy {
                                // Folding the conversation while the model is answering would
                                // rewrite the history under a run that is reading it.
                                let _ = reply.send(Err(
                                    "a turn is running; compaction happens between turns".into(),
                                ));
                            } else if self.child.is_none() {
                                let _ = reply.send(Err(
                                    "nothing to compact yet: this conversation has no running model process. Send a message first.".into(),
                                ));
                            } else {
                            // Compaction is a model call over the whole conversation, and a
                            // ~200k-token fold runs for many minutes on a local endpoint. The
                            // ceiling is a config knob for that reason: a tight one does not
                            // make compaction quicker, it only reports a timeout over a fold
                            // that was still going.
                            let secs = self.cfg.compact_timeout.as_secs();
                            let out = self.request(cmd, self.cfg.compact_timeout).await;
                            if let Ok(v) = &out {
                                // pi reports the numbers, so the note quotes them. The counter
                                // is not touched here: pi also emits compaction_start, and
                                // counting in both places would double it.
                                let before = v.get("tokensBefore").and_then(|x| x.as_u64()).unwrap_or(0);
                                let after = v.get("estimatedTokensAfter").and_then(|x| x.as_u64()).unwrap_or(0);
                                let session = self.state.lock().unwrap().session_id;
                                let _ = self.ev.send(EngineEvent::Note {
                                    session: Some(session),
                                    message: format!(
                                        "compacted: {before} tokens of conversation before, about {after} after. The older turns are replaced by a summary; what the model still reads verbatim starts at the kept entry."
                                    ),
                                });
                            }
                            if let Err(e) = &out {
                                // Whatever the reason, the person needs to know their history
                                // is still there: compaction is the operation people fear, and
                                // a bare error line reads as "and now it is gone".
                                self.note(format!(
                                    "compaction did not finish in {secs}s ({e}). Nothing was thrown away: the conversation is still whole as far as cwoffee can tell, and pi may still be summarizing it. The context meter after the next turn is the fact to look at — raise `compact_timeout_secs` if this conversation is simply big."
                                ));
                            }
                            let _ = reply.send(out);
                            }
                        } else {
                            let out = self.request(cmd, Duration::from_secs(30)).await;
                            let _ = reply.send(out);
                        }
                    }
                    None => break,
                },
                rec = rec_rx.recv() => match rec {
                    Some(r) => {
                        if r.get("type").and_then(|v| v.as_str()) == Some("tool_execution_start") {
                            if matches!(
                                r.get("toolName").and_then(|v| v.as_str()),
                                Some("web_search") | Some("web_fetch") | Some("download_file")
                            ) {
                                *self.web_last.lock().unwrap() = Some(Instant::now());
                            }
                        }
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

    /// The ceiling for whatever window this host now believes the model has.
    ///
    /// The Rust core re-derives the budget when a model with a different window is
    /// loaded; this side has to do the same or one screen ends up with a meter sized for
    /// a model that is not running. The pin is the person's number and survives; only the
    /// derived half follows the window.
    /// The live half of a ceiling change: the meter, and pi's own folding threshold.
    ///
    /// pi reads `settings.json` when a process starts, so a process that is already
    /// running keeps the threshold it started with. Saying that is the honest form of
    /// "applied"; the alternative is a number on screen that the running model has no
    /// idea about.
    fn set_budget(&mut self, target: Option<usize>) {
        self.cfg.budget_pin = target;
        let (window, model) = {
            let mut st = self.state.lock().unwrap();
            st.budget_pin = target;
            st.context_budget = crate::mount::context_budget(target, st.model_window);
            (st.model_window, st.model.clone())
        };
        let budget = self.state.lock().unwrap().context_budget;
        let pin_word = match target {
            Some(n) => format!(
                "pinned by you at {n}, {}",
                cwoffee_engine::host::budget_share(n, window)
            ),
            None => "derived: half of whatever window the endpoint states".to_string(),
        };
        match window {
            Some(w) => {
                // The key pi reads this under is its own pair, not cwoffee's spelling of the
                // model: see `mount_fold_for`.
                let (pi_provider, pi_model) = match self.cfg.cfg_path.as_ref().map(|p| {
                    crate::mount::pi_target(p, &self.cfg.launch.provider, &model)
                }) {
                    Some(Ok(pair)) => pair,
                    Some(Err(e)) => {
                        self.err(format!("{e} — the ceiling was set, but pi was not told where to fold"));
                        return;
                    }
                    None => (self.cfg.launch.provider.clone(), model.clone()),
                };
                match crate::mount::mount_settings(
                    &self.cfg.launch.agent_dir,
                    &pi_provider,
                    &pi_model,
                    budget,
                    w,
                ) {
                Ok(Some(sentence)) => {
                    self.note(format!(
                        "context ceiling is now {budget} tokens ({pin_word}). {sentence}"
                    ));
                }
                Ok(None) => self.note(format!(
                    "context ceiling is now {budget} tokens ({pin_word}); pi keeps its own folding \
                     default, because no window is known for this model to compute a threshold from"
                )),
                Err(e) => self.err(format!(
                    "context ceiling is now {budget} tokens ({pin_word}) and the config was written, \
                     but pi's own threshold could not be set: {e}",
                )),
                }
            },
            None => self.note(format!(
                "context ceiling is now {budget} tokens ({pin_word}); pi keeps its own folding default, \
                 because no window is known for this model to compute a threshold from"
            )),
        }
    }

    /// Tell pi where to fold, under the `provider/model` pair pi actually keys it by.
    ///
    /// `compaction.modelOverrides` is keyed `provider/model`. A reserve written under the
    /// launch provider for a model pi reaches through a different provider is a number pi
    /// never reads: the context meter and pi's own fold then disagree, one on each side of
    /// the seam. `None` means no window is known, which is said out loud rather than left
    /// as a threshold nobody computed.
    fn mount_fold_for(&self, provider: &str, model: &str) -> Option<String> {
        let (window, budget) = {
            let st = self.state.lock().unwrap();
            (st.model_window?, st.context_budget)
        };
        crate::mount::mount_settings(&self.cfg.launch.agent_dir, provider, model, budget, window)
            .ok()
            .flatten()
    }

    fn recompute_budget(&self, st: &mut HostState) {
        st.budget_pin = self.cfg.budget_pin;
        st.context_budget = crate::mount::context_budget(self.cfg.budget_pin, st.model_window);
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

    /// What pi can answer for, as pi says it.
    ///
    /// pi clamps a thinking level onto the levels the model entry declares, and an entry that
    /// declares nothing declares `off` alone. The only way to know whether the dial in front of the
    /// reader is wired to anything is to ask the model process which levels exist - a question to
    /// the process, not a call to the model, so it costs nothing and spends no prefill.
    async fn learn_levels(&mut self) {
        match self
            .request(
                json!({ "type": "get_available_thinking_levels" }),
                Duration::from_secs(10),
            )
            .await
        {
            Ok(data) => {
                let levels = data
                    .get("levels")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                    eprintln!("[bridge] pi offers these thinking levels: {levels:?}");
                }
                self.state.lock().unwrap().reasoning_levels = levels;
            }
            Err(e) => eprintln!("[bridge] pi would not say which thinking levels it offers: {e}"),
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
        let lean = self.cfg.lean_tools;
        if self.state.lock().unwrap().persona == cwoffee_engine::host::Persona::Chat {
            // A chat has no tools and no folder. The extension is not mounted at all,
            // so its tool declarations never reach the prompt; `--no-tools` covers
            // pi's own four; and the conversation's workspace is ignored, because
            // claiming a folder while offering no file tools is a claim with nothing
            // behind it. The boot workspace is used purely so pi has somewhere to put
            // its scratch files.
            launch.extensions.clear();
            launch.no_tools = true;
            launch.system_prompt =
                Some(cwoffee_engine::host::Persona::CHAT_PROMPT.to_string());
            launch.workdir = self.boot_workdir.clone();
        } else {
            // Cowork: the coworker's own words. Left to itself the pi core sends pi's
            // coding-harness preamble — measured at 3,183 B of instructions about reading
            // files, running commands and reading pi's own docs, none of which describe
            // cwoffee's job and all of which describe tools cwoffee has switched off. The
            // same `cwoffee-v1` text the Rust core sends is used here, plus the discovery
            // note when the declared tool set is deliberately incomplete: a model that is
            // shown a subset and never told it is a subset concludes the app cannot do the
            // thing, and then it stops trying.
            launch.system_prompt = Some(cowork_prompt_with_workspace_note(lean, &launch.workdir));
        }
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
        // The conversation store, named rather than assumed. `recall` reads the user's own
        // history out of this file; a tool host that was never told where it lives says so
        // instead of reporting that nothing was ever said.
        launch.extra_env.push((
            "CWOFFEE_TOOL_MODE".to_string(),
            if lean { "lean".to_string() } else { "full".to_string() },
        ));
        if let Some(db) = &self.cfg.db_path {
            launch.extra_env.push(("CWOFFEE_DB".to_string(), db.display().to_string()));
        }
        launch.extra_env.push((
            "CWOFFEE_PI_SESSION_MAP".to_string(),
            self.cfg.session_map.display().to_string(),
        ));
        let (child, mut rx) = PiChild::spawn(&launch).await.map_err(|e| self.explain_now(&e))?;
        let pid = child.pid;
        // Name the process we started, in a file the next bridge to start reads.
        crate::process::note_children(&launch.agent_dir, &[pid]);
        // Log the launch the child actually got, every time. The shells used to print
        // their own guess at boot; once the tool set and prompt could change with the
        // mode and per conversation, that guess became a line claiming a guardrail
        // that was not in the process at all.
        if !launch.extensions.is_empty() && std::env::var("CWOFFEE_CWTOOL").is_err() {
            // The extension prints its own complaint when the tool host is missing, but
            // in RPC mode that print does not reach us — pi keeps its child output to
            // itself. So the bridge says it here, where the log actually goes, in terms
            // of what the user will see: a model that answers "tool not found".
            eprintln!(
                "[cwoffee/pi] {} extension(s) mounted but CWOFFEE_CWTOOL is unset — the Rust tools will not \
                 register and the model will be told \"tool not found\". Point CWOFFEE_CWTOOL at cwtool, \
                 or run scripts/run-pi-app.sh.",
                launch.extensions.len()
            );
        }
        eprintln!(
            "[cwoffee/pi] process up in {} — persona {}, {} extension(s), tools {}",
            launch.workdir.display(),
            self.state.lock().unwrap().persona.as_str(),
            launch.extensions.len(),
            if launch.no_tools {
                "none (--no-tools)".to_string()
            } else if launch.tools.is_empty() {
                "pi's own defaults".to_string()
            } else {
                launch.tools.join(",")
            }
        );
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

    /// The window to believe for one model: a `[models."<id>"] context_window` the person set
    /// in the model screen outranks the endpoint's claim. Without this, the number typed in the
    /// screen would be decoration while the meter kept believing the server.
    fn believed_window(&self, model: &str, from_endpoint: usize) -> usize {
        let stored = self
            .cfg
            .cfg_path
            .as_ref()
            .and_then(|p| cwoffee_engine::config::window_override_in(p, model));
        cwoffee_engine::host::chosen_window(stored, Some(from_endpoint)).unwrap_or(from_endpoint)
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
                // What the person stored for this model beats what pi claims, and the meter,
                // the derived ceiling and pi's own fold threshold are all rebuilt from that.
                let believed = self.believed_window(&st.model.clone(), w as usize);
                if st.model_window != Some(believed) {
                    st.model_window = Some(believed);
                    self.recompute_budget(&mut st);
                }
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
        if matches!(
            cmd,
            HostCmd::SwitchSession { .. } | HostCmd::ClearSession | HostCmd::NewSession { .. } | HostCmd::SetPersona { .. }
        )
            && (self.tr.running() || self.state.lock().unwrap().busy)
        {
            self.deferred.push(cmd);
            return;
        }
        match cmd {
            HostCmd::SetPersona { persona } => self.set_persona(persona).await,
            HostCmd::SetContextBudget { target } => self.set_budget(target),
            HostCmd::Send { text, attachments, session } => self.on_send(text, attachments, session).await,
            HostCmd::Cancel => {
                // Release the cards the turn is parked on *before* asking the model to stop.
                // Two reasons, and they are not the same one.
                //
                // The card is the user's: a question parks a turn with no clock, so the only
                // event that closes it is the turn ending, and Stop is that event.
                //
                // And the child is waiting on that answer too: its tool call does not unwind
                // until its dialog resolves, and `abort` is only answered once the abort has
                // finished - which is the tool call unwinding. Send `abort` first and wait for
                // the reply and each side holds what the other needs. Measured on a pi core:
                // Stop landed 10002ms later - the abort timeout to the millisecond - and the
                // app then reported "could not stop the turn: the model process did not answer
                // abort" about a turn that had in fact just stopped. The rust core has always
                // released what a turn is parked on for the same reason.
                self.release_open_cards();
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
            HostCmd::SetReasoning { reasoning, persist } => {
                if persist {
                    // The settings screen: the effort a new conversation starts at. It goes in the
                    // config file, exactly as the rust core writes it, so a phone and the desktop
                    // read one answer - and it moves the current conversation too, if that
                    // conversation has never moved its own dial.
                    self.default_thinking = reasoning;
                    let word = serde_json::to_value(reasoning)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| "low".into());
                    cwoffee_engine::host::persist_setting(
                        &self.cfg.cfg_path,
                        "reasoning",
                        &cwoffee_engine::Config::toml_str(&word),
                    );
                } else {
                    // The composer: this conversation, for the rest of it, and nothing else. The
                    // config file is not written - a level picked for one chat must not become the
                    // level every future chat starts at.
                    let row = self.row();
                    if row == 0 {
                        self.draft_thinking = Some(reasoning);
                    } else {
                        self.row_thinking.insert(row, reasoning);
                    }
                }
                let level = if persist {
                    let row = self.row();
                    self.row_thinking.get(&row).copied().or(self.draft_thinking).unwrap_or(self.default_thinking)
                } else {
                    reasoning
                };
                let pi = pi_level(&level).to_string();
                if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                    // Which slot the choice went into, and what is about to be sent to pi: the two
                    // ways this can be wrong (the draft never follows into the conversation, or the
                    // row map is bypassed) are invisible without it.
                    eprintln!(
                        "[bridge] thinking: persist={persist} row={} slot={} sending {pi}",
                        self.row(),
                        if self.row() == 0 { "draft" } else { "row" }
                    );
                }
                match self.request(json!({ "type": "set_thinking_level", "level": pi }), Duration::from_secs(10)).await {
                    Ok(_) => {
                        let row = self.row();
                        let mut st = self.state.lock().unwrap();
                        st.reasoning = level;
                        st.reasoning_default = self.default_thinking;
                        st.reasoning_overridden = self.row_thinking.get(&row).is_some()
                            || (row == 0 && self.draft_thinking.is_some());
                    }
                    Err(e) => self.err(format!("the model refused that reasoning level: {e}")),
                }
                // pi clamps a level to what the model entry declares; a dial that moves while the
                // wire says nothing is the complaint this answers. Ask pi what it can run, now that
                // the request has just round-tripped and the process is certainly up.
                self.learn_levels().await;
                if !persist {
                    // Remember what pi was told, for this conversation, so the next time this row is
                    // entered the answer does not come from pi's model capability instead.
                    let row = self.row();
                    self.applied_thinking.insert(row, level);
                }
            }
            HostCmd::SetModel { model, persist } => {
                // Which pi provider a cwoffee model id belongs to is not the launch
                // provider. It used to be, and that is the whole reason a model added on the
                // settings screen could not be used for inference: the app registered the
                // endpoint, mounted it in pi's `models.json`, and then asked pi for
                // `cwoffee/<that model>` — a model pi had never heard of. The answer came
                // back as an error naming a model nobody had typed, and the model picker
                // looked broken.
                let cfg_path = match self.cfg.cfg_path.clone() {
                    Some(p) => p,
                    None => cwoffee_engine::config::Config::default_path(),
                };
                let (pi_provider, pi_model) =
                    match crate::mount::pi_target(&cfg_path, &self.cfg.launch.provider, &model) {
                        Ok(pair) => pair,
                        Err(e) => {
                            self.err(format!("cannot answer with {model}: {e}"));
                            return;
                        }
                    };
                // A model registered while this window is open is in cwoffee's config and not
                // yet in pi's file: the mount of endpoints happens at boot. Re-mounting here
                // is what makes "add endpoint, pick model" work without a restart.
                if !crate::mount::provider_has_model(&self.cfg.launch.agent_dir, &pi_provider, &pi_model) {
                    let mounted = crate::mount::mount_provider_rows(&self.cfg.launch.agent_dir, &cfg_path);
                    match &mounted {
                        Ok(list) => {
                            if list.is_empty() {
                                self.err(format!(
                                    "{model} is not on an endpoint cwoffee has registered, so pi cannot \
                                     answer with it. Add the endpoint under Settings > Models first."
                                ));
                                return;
                            }
                            eprintln!(
                                "[cwoffee/pi] mounted {} endpoint(s) for the switch to {model}: {}",
                                list.len(),
                                list.join(", ")
                            );
                        }
                        Err(e) => {
                            self.err(format!(
                                "cannot tell pi about the endpoints registered in {}: {e} — {model} stays \
                                 unreachable until that is fixed",
                                cfg_path.display()
                            ));
                            return;
                        }
                    }
                    if !crate::mount::provider_has_model(&self.cfg.launch.agent_dir, &pi_provider, &pi_model) {
                        self.err(format!(
                            "{model} is registered under {pi_provider} in {}, but pi's provider file has no \
                             such model on it — press Probe models on that endpoint, or restart the app to \
                             rewrite it.",
                            cfg_path.display()
                        ));
                        return;
                    }
                }
                match self.request(
                    json!({ "type": "set_model", "provider": pi_provider, "modelId": pi_model }),
                    Duration::from_secs(20),
                )
                .await
                {
                    Ok(data) => {
                        {
                            let mut st = self.state.lock().unwrap();
                            st.model = data
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&model)
                                .to_string();
                            if let Some(w) = data.get("contextWindow").and_then(|v| v.as_u64()) {
                                st.model_window =
                                    Some(self.believed_window(&st.model.clone(), w as usize));
                            }
                            // Different model, different window: a budget carried over
                            // from the previous model is how a fold warning shows up in a
                            // conversation that is nowhere near full.
                            self.recompute_budget(&mut st);
                        }
                        self.cfg.launch.model = model.clone();
                        // The ceiling has to travel with the model, not stay on the one that
                        // was mounted at boot: pi folds on the reserve it read from
                        // settings.json, and a reserve computed for the previous model makes
                        // the fold and the context meter disagree.
                        match self.mount_fold_for(&pi_provider, &pi_model) {
                            Some(sentence) => eprintln!("[cwoffee/pi] {sentence}"),
                            None => eprintln!(
                                "[cwoffee/pi] compaction: no window known for {model}, so pi keeps the \
                                 threshold it started with"
                            ),
                        }
                        if persist {
                            self.note(
                                "the model change applies to this run; writing the default model to the config \
                                 file is not wired on this core yet",
                            );
                        }
                    }
                    Err(e) => self.err(format!(
                        "could not switch to {model}: pi was asked for {pi_provider}/{pi_model} and refused \
                         ({e}). That endpoint is registered in {} — Probe models on it re-reads what it \
                         actually serves.",
                        cfg_path.display()
                    )),
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
                // Same probe first, from the other side of the mailbox: a direct
                // `HostCmd` push must not be able to skip what the public verb checks.
                // Before the process is reaped, not after: a refusal that costs the model
                // its process and its warm prefix cache is not a free refusal, and a move
                // that was turned away should have changed nothing at all.
                let checked = match cwoffee_engine::host::check_workspace(&path) {
                    Ok(p) => p,
                    Err(reason) => {
                        self.refuse(format!("{path}: {reason}"));
                        return;
                    }
                };
                // A chat owns no folder, so there is nothing here to move; accepting one
                // would group a conversation that has no tools to use it.
                if self.cfg.persona == cwoffee_engine::host::Persona::Chat {
                    self.refuse(format!(
                        "not moving the tools to {path}: chat mode has no folder and no tools to \
                         use one with. Switch to cowork to work in a folder."
                    ));
                    return;
                }
                // The folder lock the rust core states, on this core too: a conversation
                // that already has a folder keeps it. Left unsaid here, the same client
                // check passes on one core and fails on the other, which is what "the
                // guardrail is in the engine" was supposed to prevent.
                let session_id = self.state.lock().unwrap().session_id;
                if session_id != 0 {
                    let current = match self.cfg.store.session(session_id) {
                        Ok(Some(s)) => s.workspace.trim().to_string(),
                        _ => String::new(),
                    };
                    if !current.is_empty() && !cwoffee_engine::host::is_temp_root(&current) {
                        let same = std::fs::canonicalize(&current)
                            .map(|c| c == checked)
                            .unwrap_or_else(|_| std::path::PathBuf::from(&current) == checked);
                        if !same {
                            self.refuse(cwoffee_engine::host::folder_lock(&current));
                            return;
                        }
                    }
                }
                if let Some(child) = self.child.take() {
                    child.reap(Duration::from_secs(3)).await;
                }
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
    /// Cowork <-> chat. The running process is killed rather than reconfigured: what
    /// tools exist and what the system prompt says are decided when pi is exec'd, and
    /// there is no verb to change either afterwards.
    async fn set_persona(&mut self, want: cwoffee_engine::host::Persona) {
        use cwoffee_engine::host::Persona;
        let had = self.state.lock().unwrap().persona;
        if had == want {
            return;
        }
        self.state.lock().unwrap().persona = want;
        if let Some(p) = &self.cfg.cfg_path.clone() {
            let written = format!("\"{}\"", want.as_str());
            match cwoffee_engine::config::Config::set_top_keys(p, &[("persona", written.as_str())]) {
                Ok(_) => {}
                Err(e) => self.err(format!(
                    "the mode is live but was NOT written to {}: {e}. this start keeps it; the next one will not",
                    p.display()
                )),
            }
        }
        if let Some(c) = self.child.take() {
            c.reap(std::time::Duration::from_secs(2)).await;
        }
        self.note(format!(
            "{} mode: {} — the model process restarts on the next turn, which starts that prefix cache cold",
            want.as_str(),
            match want {
                Persona::Chat => "no tools, no folder, one line of system prompt".to_string(),
                Persona::Cowork => format!(
                    "tools mounted again, folder {}",
                    self.state.lock().unwrap().workspace
                ),
            }
        ));
    }

    async fn follow_workspace(&mut self, id: i64) -> bool {
        if id == self.row() {
            // The same conversation: a folder the user chose by hand wins over whatever
            // the row still says.
            return true;
        }
        if self.state.lock().unwrap().persona == cwoffee_engine::host::Persona::Chat {
            // Chat mode owns no folder. Following a conversation into one would put a
            // path back into the process that has no file tools to use it.
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
                "conversation {id} lives in {want}, but the tools are working in {have} and a turn is running there; nothing was moved and nothing was sent. Stopping that turn first is what makes the folder change safe — its writes would otherwise land in the wrong tree."
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
            "the tools moved to {want} — that is where conversation {id} lives; the model's own folder moved with them"
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
            let mut fresh_thread = false;
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
                    fresh_thread = true;
                    if let Err(e) = self.request(json!({ "type": "new_session" }), Duration::from_secs(15)).await {
                        self.err(format!("pi could not start a thread for conversation {id}: {e}"));
                    } else {
                        if let Ok(d) = self.request(json!({ "type": "get_state" }), Duration::from_secs(10)).await {
                            self.absorb_state(&d);
                        }
                        self.note(format!(
                            "conversation {id} has no pi thread of its own yet, so the model starts this one from \
                             scratch; the stored transcript is unchanged"
                        ));
                    }
                }
            }
            // The thinking effort belongs to the conversation, not to whatever pi's process was
            // last asked for.
            //
            // This is where the reported bug lived, and this is the line that fixes it. A
            // conversation's pi thread is created lazily: a draft's first send creates the row and
            // `new_session`s pi in the same breath. A level chosen in the composer before anything
            // was typed had been applied to a pi session that this very block then replaced, and pi
            // brings a new thread up at its own default - which is the value from the settings
            // screen. So the dial snapped back at the moment the user pressed send, which is exactly
            // what it looked like from the chair: the send undoing the choice.
            if let Some(t) = self.draft_thinking.take() {
                // A level chosen while the conversation was a draft follows it into the conversation
                // it becomes.
                self.row_thinking.entry(id).or_insert(t);
            }
            let chosen = match self.row_thinking.get(&id).copied() {
                Some(t) => Some(t),
                // A brand-new thread starts at the settings default, whatever the previous
                // conversation happened to be using.
                None if fresh_thread => Some(self.default_thinking),
                // Reopened inside this run: the level this process last set for it, which is what
                // the person asked for. Asking pi instead answers with the level its model can
                // actually run - for a model that cannot think, "off" - and that answer overwrites
                // a choice nobody changed. A conversation this process never configured has no
                // entry here, so this is None and pi's own restored level is the honest answer.
                None => self.applied_thinking.get(&id).copied(),
            };
            if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                eprintln!(
                    "[bridge] thinking: conversation {id} has a {} thread; chosen={}, settings default={}, this row's stored={:?}",
                    if fresh_thread { "fresh" } else { "resumed" },
                    chosen
                        .as_ref()
                        .map(|c| pi_level(c).to_string())
                        .unwrap_or_else(|| "leave pi alone".into()),
                    pi_level(&self.default_thinking),
                    self.row_thinking.get(&id).map(|t| pi_level(t).to_string()),
                );
            }
            if let Some(level) = chosen {
                let pl = pi_level(&level).to_string();
                if let Err(e) = self
                    .request(json!({ "type": "set_thinking_level", "level": pl }), Duration::from_secs(10))
                    .await
                {
                    self.err(format!("this conversation asked for {pl} thinking and the model refused it: {e}"));
                }
                self.applied_thinking.insert(id, level);
                let mut st = self.state.lock().unwrap();
                st.reasoning = level;
                st.reasoning_default = self.default_thinking;
                st.reasoning_overridden = self.row_thinking.contains_key(&id);
            } else if let Ok(d) = self.request(json!({ "type": "get_state" }), Duration::from_secs(10)).await {
                // Nothing was chosen for this conversation in this run, so publish what pi is
                // actually running at: the level that conversation last used, and possibly a clamped
                // version of what was once asked for. A dial that shows a level the model is not at
                // is a dial that lies.
                if let Some(lvl) = d.get("thinkingLevel").and_then(|v| v.as_str()).and_then(from_pi_level) {
                    if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                        eprintln!(
                            "[bridge] thinking: pi reports {} for conversation {id}; publishing that as-is",
                            pi_level(&lvl)
                        );
                    }
                    let mut st = self.state.lock().unwrap();
                    st.reasoning = lvl;
                    st.reasoning_default = self.default_thinking;
                    st.reasoning_overridden = false;
                }
            }
        }
        // What reopening this conversation cost — the same sentence the rust core says, so
        // the same click is described the same way behind either core. The size always comes
        // from the stored transcript; a token figure only exists once a turn in this process
        // has reported one, and a cache claim never appears at all, because that stays the
        // endpoint's business.
        if !self.resume_announced.contains(&id) {
            let n = self.cfg.store.messages(id).map(|v| v.len()).unwrap_or(0);
            if n > 0 && self.resume_announced.insert(id) {
                let (est, budget) = {
                    let st = self.state.lock().unwrap();
                    (st.context_tokens, st.context_budget)
                };
                self.note(cwoffee_engine::host::resumed_note(
                    id,
                    n,
                    (est > 0).then_some(est),
                    budget,
                ));
            }
        }
        true
    }

    async fn on_send(&mut self, text: String, attachments: Vec<Attachment>, session: Option<i64>) {
        // pi carries images on the prompt itself, so an attachment is not something
        // this core refuses: it is the same bytes handed to the same model. The gate
        // is the engine's own `check_attachments` with the engine's own default limits,
        // because a bridge that invents its own rules would reject a picture the Rust
        // core accepts, and that is a difference the user feels as flakiness.
        if !attachments.is_empty() {
            let limits = cwoffee_engine::host::AttachmentLimits::defaults();
            if let Err(reason) = cwoffee_engine::host::check_attachments(&attachments, limits) {
                self.err(format!("attachment rejected: {reason}"));
                return;
            }
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
        // The conversation this window was standing in can stop existing between the
        // click and the command: the desktop deletes by moving the host off the row
        // first, and that move is queued. Answering into a row nothing lists any more
        // produces an answer the reader can never open again, so the row is checked
        // where it is checked on the Rust core - just before the turn, not at delete
        // time.
        let row = if row != 0 && self.cfg.store.session(row).ok().flatten().is_none() {
            self.note(
                "that conversation was deleted while your message was on its way. Nothing was \
                 lost from your side: the reply starts a new conversation.",
            );
            0
        } else {
            row
        };
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

        // A send is a fact before it is asked of a model. The rust core stores the user's
        // message before the provider call; here pi used to echo it back, and the row was
        // created from that echo — which left a window, while the bridge opened a pi thread
        // for this conversation, in which the sentence the user had just typed was on no
        // screen and in no store. It read as the message vanishing.
        let mut user_row = cwoffee_engine::provider::Message::user(text.clone());
        user_row.attachments = attachments.clone();
        if let Err(e) = self.cfg.store.append_message(row, &user_row) {
            // Storing first is the whole point: a message the store refused to keep is a
            // message that was never sent, and saying so now beats letting pi answer a
            // sentence that will not survive the next reload.
            self.err(format!("could not store your message: {e}"));
            return;
        }
        self.tr.store_user_early(&attachments);

        // Name the conversation before pi answers it. Same ask, same parser, same
        // fallback, same events as the Rust core's title pass: the pi core hands the
        // answer to pi, and must not hand the *name* to pi, because pi would answer it
        // inside the conversation's own context. The reader asked for those two to be
        // separate, and a name that leaks into the history also costs prompt tokens on
        // every later turn.
        self.name_conversation(row, &text).await;

        if let Err(e) = self.ensure_child().await {
            self.err(format!("cannot start the model process: {e}"));
            return;
        }
        if self.state.lock().unwrap().reasoning_levels.is_empty() {
            // A conversation is about to run at some level. Whether that level exists for this
            // model is a fact the process knows and the settings screen does not, and "the dial did
            // nothing" is only diagnosable once the offered list is on the state.
            self.learn_levels().await;
        }
        self.tr.session = row;
        self.tr.model = self.state.lock().unwrap().model.clone();
        self.last_activity = Instant::now();
        {
            let mut st = self.state.lock().unwrap();
            st.busy = true;
            // A new turn starts with nothing said. Left over from the previous one, a client
            // that reloaded would be handed the last turn's tail as if it were this turn's.
            st.open_text.clear();
            st.open_reasoning.clear();
        }
        // `images` is pi's own ImageContent shape: unprefixed base64 plus the mime the
        // boundary check just allowed. A non-image attachment is refused above rather
        // than smuggled through as something the model was never meant to interpret.
        let images: Vec<Value> = attachments
            .iter()
            .map(|a| json!({ "type": "image", "data": a.data_b64, "mimeType": a.mime }))
            .collect();
        self.tr.expect_attachments(attachments.clone());
        let mut prompt = json!({ "type": "prompt", "message": text });
        if !images.is_empty() {
            prompt["images"] = Value::Array(images);
        }
        match self.request(prompt, Duration::from_secs(30)).await {
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
        // The placeholder, on this core exactly as on the Rust core. It used to be the
        // opening line cut at 60 characters, which put a name on the row before the
        // conversation had been understood - and in front of an animation that was
        // about to type a different one.
        let _ = text;
        cwoffee_engine::store::UNTITLED.to_string()
    }

    /// The endpoint cwoffee answers through, as an engine provider, for the one ask
    /// that is not a conversation: naming one.
    ///
    /// Built from cwoffee's own config through `Config::build_provider`, so the auth
    /// headers and the chat-template knobs are the same ones the Rust core would use.
    /// A title must not come from a different model than the one answering the question
    /// it names.
    fn title_provider(&self) -> Option<std::sync::Arc<dyn cwoffee_engine::provider::Provider>> {
        let path = self.cfg.cfg_path.clone()?;
        let (p, base) = title_provider_at(&path)?;
        // Once per process, because a name that never arrives is otherwise a silent 8 s
        // stall: the line says which endpoint was asked, and that is the difference between
        // "the model gave us nothing usable" and "we asked somewhere else".
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
            eprintln!("[cwoffee/titling] the endpoint asked for names is {base}");
        }
        Some(p)
    }

    /// Ask for this conversation's name, stream it out as it arrives, and settle it.
    ///
    /// Deliberately a mirror of the Rust core's title pass rather than a re-derivation:
    /// `titling::TITLE_ASK`, the parser and the fallback are shared, so a conversation
    /// gets the same name whichever core is running. A miss is not fatal and is not
    /// silent: the row falls back to the opening line and the reason goes to the log,
    /// because "every sidebar row says New Session" is the failure mode this path has to
    /// avoid.
    async fn name_conversation(&mut self, row: i64, text: &str) {
        if !self.title_slot.claim() {
            return;
        }
        let stored_title = self
            .cfg
            .store
            .session(row)
            .ok()
            .flatten()
            .map(|s| s.title)
            .unwrap_or_default();
        let first_user = match self.cfg.store.history(row) {
            Ok(h) => h
                .into_iter()
                .find(|m| matches!(m.role, cwoffee_engine::provider::Role::User))
                .map(|m| m.content)
                .unwrap_or_else(|| text.to_string()),
            Err(_) => text.to_string(),
        };
        if !cwoffee_engine::titling::still_untitled(&stored_title, &first_user) {
            self.title_slot.release();
            return;
        }
        let model = self.state.lock().unwrap().model.clone();
        let outcome = match self.title_provider() {
            Some(p) => {
                let ev = self.ev.clone();
                cwoffee_engine::titling::ask_title(
                    p.as_ref(),
                    &model,
                    &first_user,
                    |t, done| {
                        let _ = ev.send(EngineEvent::TitleDelta {
                            session_id: row,
                            text: t.to_string(),
                            done,
                        });
                    },
                )
                .await
            }
            None => {
                // No reachable endpoint in the config: keep the placeholder rather than
                // stalling a turn that pi is going to answer anyway.
                self.title_slot.release();
                return;
            }
        };
        let name = match outcome {
            cwoffee_engine::titling::Outcome::Named(n) => Some(n),
            cwoffee_engine::titling::Outcome::FellBack(why) => {
                eprintln!("[cwoffee/titling] conversation {row} keeps a fallback name: {why}");
                let fb = cwoffee_engine::store::derive_title(&first_user);
                if fb.is_empty() { None } else { Some(fb) }
            }
            cwoffee_engine::titling::Outcome::NotEligible => None,
        };
        self.title_slot.release();
        if let Some(n) = name {
            match self.cfg.store.rename_session(row, &n) {
                Ok(()) => {
                    let _ = self
                        .ev
                        .send(EngineEvent::SessionChanged { session_id: row, title: n });
                }
                Err(e) => eprintln!("[cwoffee/titling] named it but the rename failed: {e}"),
            }
        }
    }

    // ---- the stream -------------------------------------------------------

    fn apply(&mut self, mut out: Output) {
        let row = self.row();
        if out
            .events
            .iter()
            .any(|e| matches!(e, EngineEvent::TurnStart { .. }))
        {
            self.turn_files.clear();
            self.last_assistant_row = None;
        }
        for m in &out.rows {
            match self.cfg.store.append_message(row, m) {
                Ok(mid) => {
                    if matches!(m.role, cwoffee_engine::provider::Role::Assistant) {
                        self.last_assistant_row = Some(mid);
                    }
                }
                Err(e) => {
                    // The transcript is the user's record of what happened; failing to
                    // write it is worth saying out loud, not logging quietly.
                    self.err(format!("could not store that message: {e}"));
                }
            }
        }
        for (_, files) in out.files_by_call.drain(..) {
            for f in files {
                merge_turn_file(&mut self.turn_files, f);
            }
        }
        // Stamp at the end of the run, on the answer, once. `take()` so a second settle
        // in the same process cannot re-attach the same files to a later answer.
        if out.settled {
            // Naming happened before the answer, in `name_conversation`, not here: the
            // reader asked for the title request to stall the first answer rather than
            // trail it, and the ask shares its prompt, parser and fallback with the Rust
            // core so a conversation gets the same name behind either core.
        }
        if out.settled && !self.turn_files.is_empty() {
            let files = std::mem::take(&mut self.turn_files);
            match self.last_assistant_row.take() {
                Some(mid) => {
                    if let Err(e) = self.cfg.store.set_message_files(mid, &files) {
                        self.err(format!("could not record what that run made: {e}"));
                    }
                }
                None => self.err(format!(
                    "that run made {} file(s) but stored no answer to attach them to — they are on disk, not in the transcript",
                    files.len()
                )),
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
            // Publish what the turn has said so far, before anything else in this block.
            // Computed outside the lock: `tr` is another field, and a guard held across a call
            // that reads it is how this gets tangled.
            let open = self.tr.open_of_run();
            let mut st = self.state.lock().unwrap();
            if let Some(ct) = out.context_tokens {
                st.context_tokens = ct;
            }
            // The half of the answer that has no row yet. A browser page reloaded mid-answer
            // asks for the stored rows and finds nothing for the message still streaming, so
            // it painted the reply starting at whatever arrived after the reload — the top of
            // the message missing, repairing itself only when the turn ended and its row was
            // written. This is the part that had no row: the client paints it and the live
            // deltas carry on underneath it. Cleared when the run has nothing open, so a
            // settled turn cannot hand a later reload text that is already stored.
            match open {
                Some((t, r)) => {
                    st.open_text = t;
                    st.open_reasoning = r;
                }
                None => {
                    st.open_text.clear();
                    st.open_reasoning.clear();
                }
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
                let row = self.row();
                let permit = permit_covers(&self.auto_allow, row, &tool, &escapes);
                if matches!(mode, ApprovalMode::Auto) || permit {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "confirmed": true }));
                    // Two different reasons, and the person is entitled to know which one
                    // let this through: a mode they set, or a grant they gave in this
                    // conversation earlier.
                    self.note(if permit {
                        format!("{tool} was allowed without asking: this conversation granted it earlier")
                    } else {
                        format!("{tool} was allowed without asking (approval mode {:?})", mode)
                    });
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                // The summary is what a person reads before deciding, so it says what
                // the Rust core says: the tool, the thing it wants, and which paths
                // leave the workspace. pi's dialog carries only title and message, so
                // the escape list is derived here from the message text.
                let summary = approval_summary(&tool, &message, &title, &escapes);
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
                    Wait::Approval {
                        pi_id: pi_id.clone(),
                        tool,
                        deadline: Instant::now() + Duration::from_secs(secs),
                        row,
                    },
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
                let q = match self.canon_question(&title) {
                    Some(q) => q,
                    None => {
                        if title.starts_with(CANON_Q) {
                            // A card we could not read is a card the user answers in the
                            // dark: no trade-offs, no recommendation, no "2 of 3". Say so —
                            // this is the difference between a feature that rotted and one
                            // that was never there.
                            self.err(format!(
                                "a cwoffee question arrived with the engine's payload in its title and the payload did                                  not parse, so the card shows a flattened question instead: {}",
                                &title[CANON_Q.len()..(CANON_Q.len() + 120).min(title.len())]
                            ));
                        }
                        let id = self.next_id;
                        self.next_id += 1;
                        Question {
                            id,
                            question: title.clone(),
                            options: options.iter().map(|o| QuestionOption { label: o.clone(), detail: None }).collect(),
                            recommended: None,
                            allow_custom: false,
                            multi: false,
                            set: None,
                        }
                    }
                };
                let id = q.id;
                // The labels the reply maps picks back onto. The card may have been built
                // from the engine's payload (labels plus one-line trade-offs) rather than
                // from pi's bare list, so take them from the question we are asking.
                let labels: Vec<String> = q.options.iter().map(|o| o.label.clone()).collect();
                let (tx, rx) = oneshot::channel();
                // Same rule as the approval above: the shell renders it. A second send
                // would show the question twice and answer it once.
                if self.q.send((q.clone(), tx)).is_err() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    let _ = self.ev.send(question_event(&q));
                    return;
                }
                let deadline = question_deadline(timeout_ms);
                self.waits.insert(
                    id,
                    Wait::Question {
                        pi_id: pi_id.clone(),
                        kind: QKind::Select { options: labels },
                        deadline,
                        answered: false,
                        set_id: q.set.as_ref().map(|st| st.id),
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
                let q = match self.canon_question(&title) {
                    Some(q) => q,
                    None => {
                        if title.starts_with(CANON_Q) {
                            self.err(format!(
                                "a cwoffee question arrived with the engine's payload in its title and the payload did                                  not parse, so the card shows a flattened question instead: {}",
                                &title[CANON_Q.len()..(CANON_Q.len() + 120).min(title.len())]
                            ));
                        }
                        let id = self.next_id;
                        self.next_id += 1;
                        Question {
                            id,
                            question: if placeholder.is_empty() { title.clone() } else { format!("{title} — {placeholder}") },
                            options: Vec::new(),
                            recommended: None,
                            allow_custom: true,
                            multi: false,
                            set: None,
                        }
                    }
                };
                let id = q.id;
                let (tx, rx) = oneshot::channel();
                // Same rule as the approval above: the shell renders it. A second send
                // would show the question twice and answer it once.
                if self.q.send((q.clone(), tx)).is_err() {
                    self.tr.dialog_resolved(now);
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    let _ = self.ev.send(question_event(&q));
                    return;
                }
                let deadline = question_deadline(timeout_ms);
                self.waits.insert(
                    id,
                    Wait::Question {
                        pi_id: pi_id.clone(),
                        kind: QKind::Input,
                        deadline,
                        answered: false,
                        set_id: q.set.as_ref().map(|st| st.id),
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
            UiRequest::Unsupported { pi_id, method } => {
                // Answering "cancelled" is how we tell the extension nobody is
                // going to answer, instead of letting it hang to its own timeout.
                self.tr.dialog_resolved(now);
                self.reply(&pi_id, json!({ "cancelled": true }));
                self.note(format!("this interface cannot show a {method} dialog, so it was cancelled"));
            }
        }
    }

    /// Recover the engine's question from a dialog title. See the free `canon_question`.
    fn canon_question(&mut self, title: &str) -> Option<Question> {
        canon_question(title, &mut self.next_id, &mut self.q_sets)
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
                    Some(Wait::Approval { pi_id, tool, row, .. }) => Some((pi_id, tool, row)),
                    other => {
                        if other.is_some() {
                            self.err("an approval answer arrived for something that was not an approval card");
                        }
                        None
                    }
                };
                let Some((pi_id, tool, row)) = wait else {
                    // The card already closed. A *dropped* sender is this host letting go of
                    // the turn (Stop, or a window that went away): nobody answered, and a line
                    // about an ignored answer would describe an answer that never existed. An
                    // answer that turns up after the card closed is worth the one line, because
                    // it means somebody pressed something the app had already let go of.
                    if answer.is_some() {
                        self.note("an approval was answered after its card had closed; that answer was ignored");
                    }
                    return;
                };
                match answer {
                    Some(ApprovalAnswer::Allow) => {
                        self.tr.dialog_resolved(now);
                        self.reply(&pi_id, json!({ "confirmed": true }));
                    }
                    Some(ApprovalAnswer::AllowSession) => {
                        self.tr.dialog_resolved(now);
                        self.auto_allow.entry(row).or_default().insert(tool.clone());
                        self.reply(&pi_id, json!({ "confirmed": true }));
                        self.note(format!("{tool} is now allowed for the rest of this session (conversation {row})"));
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
                    Some(Wait::Question { pi_id, kind, answered, set_id, .. }) => {
                        *answered = true;
                        Some((pi_id.clone(), match kind {
                            QKind::Select { options } => Some(options.clone()),
                            QKind::Input => None,
                        }, *set_id))
                    }
                    _ => None,
                };
                let (pi_id, options, set_id) = match wait {
                    Some(t) => t,
                    None => {
                        // Same rule as the approval above: the dropped-sender case is the
                        // stop path, and the stop path already said what happened by closing
                        // the card. Only a late answer is worth a line.
                        if answer.is_some() {
                            self.note("a choice was answered after its card had closed; that answer was ignored");
                        }
                        return;
                    }
                };
                if let Some(st) = set_id {
                    mark_set_closed(&mut self.q_sets, st);
                }

                let body = match (&answer, options) {
                    (Some(a), Some(labels)) => {
                        // The UI answers with either picked indexes or typed text. pi's
                        // `select` hands the extension one label, which is a narrower answer
                        // than a cwoffee multi-select card can produce — so several picks
                        // travel as one string with this separator, and the extension splits
                        // it back into option indexes. Engine option labels are "few words"
                        // and never contain the separator; if one somehow does, the extension
                        // falls back to reporting the whole answer as free text rather than
                        // inventing a pick.
                        let picks: Vec<String> =
                            a.options.iter().filter_map(|i| labels.get(*i).cloned()).collect();
                        let value = if picks.is_empty() {
                            a.text.clone()
                        } else {
                            picks.join(" | ")
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

    /// Answer every dialog this process is holding open, because the turn that raised it is
    /// being stopped. Returns how many cards were open.
    ///
    /// The window and the child both need this. The window needs the frame: a question card
    /// has no clock any more, so nothing but the turn ending closes it, and a card left in
    /// "waiting for you" offers a choice nothing will ever take. The child needs the reply:
    /// pi's tool call is parked on the dialog, and an `abort` it never gets an answer to is
    /// an `abort` that times out.
    ///
    /// An unanswered approval is answered here as a denial - the tool does not run, which is
    /// the same thing the rust core's release-on-cancel does, and the safe direction for a
    /// turn somebody just told the app to abandon.
    fn release_open_cards(&mut self) -> usize {
        // (wait id, pi's request id, the tool if it was an approval, the set if it was a question)
        let open: Vec<(u64, String, Option<String>, Option<u64>)> = self
            .waits
            .iter()
            .filter_map(|(id, w)| match w {
                Wait::Approval { pi_id, tool, .. } => Some((*id, pi_id.clone(), Some(tool.clone()), None)),
                Wait::Question { pi_id, answered, set_id, .. } if !*answered =>
                    Some((*id, pi_id.clone(), None, *set_id)),
                _ => None,
            })
            .collect();
        let now = Instant::now();
        let released = open.len();
        for (id, pi_id, tool, set_id) in open {
            self.waits.remove(&id);
            self.tr.dialog_resolved(now);
            match tool {
                Some(tool) => {
                    self.reply(&pi_id, json!({ "confirmed": false }));
                    self.note(format!("{tool} was not answered; the tool did not run"));
                }
                None => {
                    self.reply(&pi_id, json!({ "cancelled": true }));
                    // The card is finished, so the next call that reuses this caller's key gets
                    // a card of its own rather than another question on a dead one.
                    if let Some(st) = set_id {
                        mark_set_closed(&mut self.q_sets, st);
                    }
                    let _ = self.ev.send(EngineEvent::QuestionExpired {
                        id,
                        why: cwoffee_engine::event::QuestionWhy::TurnStopped,
                    });
                }
            }
        }
        released
    }

    async fn on_tick(&mut self) {
        // Someone has to be alive long enough to stop an idle container. The reaper
        // inside `AutoSearXNG` is armed in the process that started it, and a tool call
        // is gone in a second, so under a pi core its timer would die with it and the
        // container would sit there indefinitely.
        if let Some(a) = self.auto.clone() {
            let due = self.auto_checked.map(|t| t.elapsed() >= Duration::from_secs(15)).unwrap_or(true);
            if due {
                self.auto_checked = Some(Instant::now());
                let st: AutoStatus = a.status().await;
                if !st.up {
                    self.auto_seen = None;
                }
                if st.up {
                    // "Never used here" is not the same as "used a moment ago": a
                    // container the tool host started before this app existed, or before
                    // this app saw a search, still has to be reaped. So the adopted
                    // container's own sighting is the fallback clock.
                    if self.auto_seen.is_none() {
                        self.auto_seen = Some(Instant::now());
                    }
                    let used = self.web_last.lock().unwrap().map(|t| t.elapsed());
                    let idle_for = match used {
                        Some(t) => t,
                        None => self.auto_seen.map(|t| t.elapsed()).unwrap_or_default(),
                    };
                    if idle_for >= Duration::from_secs(self.auto_idle) {
                        a.stop().await;
                        self.note(format!(
                            "the search container was stopped: {} seconds without a search; it starts again with the next one",
                            idle_for.as_secs()
                        ));
                    }
                }
            }
        }
        // Close cards that timed out. pi denies/forgets the dialog on its own;
        // what the bridge owes the UI is that a card cannot sit there offering a
        // choice nothing will ever take.
        let now = Instant::now();
        // (id, what expired, and the set slot to release if it was a question)
        let mut expired: Vec<(u64, Expired, Option<u64>)> = Vec::new();
        for (id, w) in self.waits.iter() {
            match w {
                Wait::Approval { deadline, tool, .. } if now >= *deadline => {
                    expired.push((*id, Expired::Approval(tool.clone()), None))
                }
                Wait::Question { deadline, answered, set_id, .. }
                    if !*answered && deadline.map(|d| now >= d).unwrap_or(false) =>
                {
                    expired.push((*id, Expired::Question, *set_id))
                }
                _ => {}
            }
        }
        for (id, what, set_id) in expired {
            self.waits.remove(&id);
            // The engine's own timeout fired: the run is generating again, or it
            // is over. Either way the wait is not the model's remaining time.
            self.tr.dialog_resolved(now);
            match what {
                Expired::Approval(tool) => self.note(format!("{tool} was not answered in time; the tool did not run")),
                Expired::Question => {
                    // A question nobody answered closes its set slot for the same reason an
                    // answered one does: the card is finished, and the next call that shares
                    // the caller's key must get a card of its own.
                    if let Some(st) = set_id {
                        mark_set_closed(&mut self.q_sets, st);
                    }
                    let _ = self.ev.send(EngineEvent::QuestionExpired {
                        id,
                        why: cwoffee_engine::event::QuestionWhy::TimedOut,
                    });
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

/// One record per path for a whole turn, newest write winning.
///
/// pi reports the files a tool call produced, call by call, and a model that rebuilds the
/// same workbook three times in one turn really did write it three times. Storing all three
/// records is what put three near-identical attachment units in the transcript — same name,
/// slightly different byte counts, and only one file on disk. The path is the identity the
/// open and Save controls act on, so it is also the identity of the record; the byte count of
/// an overwritten write describes bytes that are no longer there.
/// Read cwoffee's config and build the provider it points at, **with the environment applied**.
///
/// `Config::load` reads the file; the environment is applied by the caller - the desktop does it
/// at boot with `apply_env`. Re-loading a config for one ask and skipping that step produced a
/// quiet and expensive bug on the pi core: with no config file yet (a first run, or a self-test
/// scratch home) the load answered `Config::default()`, whose base url is the documented local
/// endpoint `http://127.0.0.1:8090/v1` - the reader's own model server. Every conversation then
/// stalled eight seconds waiting for a name from an endpoint the app was not answering through,
/// and on a normal machine that ask is a real request to a real model, costing a prefill nobody
/// agreed to. The harness sets `CWOFFEE_URL` for exactly this reason; honouring it here is what
/// keeps a test off the reader's cache.
pub fn title_provider_at(
    cfg_path: &Path,
) -> Option<(Arc<dyn cwoffee_engine::provider::Provider>, String)> {
    let (mut cfg, _) = Config::load(Some(cfg_path)).ok()?;
    cfg.apply_env();
    let base = cfg.provider.base_url.clone();
    Some((cfg.build_provider().ok()?, base))
}

fn merge_turn_file(
    turn: &mut Vec<cwoffee_engine::tools::OutputFile>,
    f: cwoffee_engine::tools::OutputFile,
) {
    match turn.iter().position(|e| e.path == f.path) {
        Some(at) => turn[at] = f,
        None => turn.push(f),
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

/// pi's vocabulary back to cwoffee's. pi names two levels cwoffee does not (`minimal`, `max`); both
/// fold to the nearest cwoffee level rather than being dropped, because this mapping's job is to
/// report the depth a model is actually running at - for instance after pi clamped a request the
/// selected model could not honour.
fn from_pi_level(s: &str) -> Option<Reasoning> {
    Some(match s {
        "off" => Reasoning::Off,
        "minimal" | "low" => Reasoning::Low,
        "medium" => Reasoning::Medium,
        "high" => Reasoning::High,
        "xhigh" | "max" => Reasoning::XHigh,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    /// The endpoint the titler asks has to be the endpoint the turns go to.
    ///
    /// A self-test run, and the dev launcher, name their endpoint through `CWOFFEE_URL` and
    /// write no config file at all. Loading the file and stopping there answers
    /// `Config::default()`, whose base url is the documented local endpoint - the reader's own
    /// model server on 127.0.0.1:8090. Measured in a pi-core pass: 42 conversations each
    /// stalled 8 s on a name that never came back, and on a normal machine those are 42 requests
    /// to a model the reader pays for. So the env has to be applied here exactly as the desktop
    /// applies it at boot.
    #[test]
    fn the_title_endpoint_follows_the_environment_the_turns_follow() {
        // One env var, one process: serialise against anything else that touches it.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("cw-title-url-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml"); // deliberately never written

        std::env::set_var("CWOFFEE_URL", "http://127.0.0.1:8190/v1");
        let got = title_provider_at(&path);
        let (_, base) = match got {
            Some(v) => v,
            None => {
                std::env::remove_var("CWOFFEE_URL");
                panic!("an endpoint named in the environment must be usable for the title ask")
            }
        };
        assert_eq!(base, "http://127.0.0.1:8190/v1", "the name ask goes where the turns go");

        // And with nothing in the environment, the default is still the documented local
        // server - which is the right answer for a normal start, and the reason the env has to
        // be honoured when it is set.
        std::env::remove_var("CWOFFEE_URL");
        let (_, base) = title_provider_at(&path).expect("the default endpoint is still an endpoint");
        assert_eq!(base, "http://127.0.0.1:8090/v1", "no env, no surprise: the documented local endpoint");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Three writes of one path are one attachment.
    ///
    /// The session this comes from asked for one spreadsheet. The model rebuilt it three times
    /// in one turn, the bridge stored all three records, and the transcript printed three units
    /// for one file — same name, three byte counts, and a disk holding exactly one copy.
    #[test]
    fn three_writes_of_one_path_are_one_record() {
        use cwoffee_engine::tools::{FileKind, OutputFile};
        let sheet = |bytes: u64| OutputFile {
            path: ".cws/assistant_attachments/ph_used_laptops.xlsx".into(),
            name: "ph_used_laptops.xlsx".into(),
            mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".into(),
            bytes,
            kind: FileKind::Attachment,
        };
        let mut turn: Vec<OutputFile> = Vec::new();
        for write in [sheet(10_094), sheet(10_678), sheet(11_736)] {
            merge_turn_file(&mut turn, write);
        }
        assert_eq!(turn.len(), 1, "one path, one record: {turn:?}");
        assert_eq!(turn[0].bytes, 11_736, "the newest write is the file on disk");

        // A different path in the same turn is a different deliverable, and arrival order —
        // which is the reader's order — survives the merge.
        merge_turn_file(
            &mut turn,
            OutputFile {
                path: ".cws/assistant_attachments/notes.md".into(),
                name: "notes.md".into(),
                mime: "text/markdown".into(),
                bytes: 7,
                kind: FileKind::Attachment,
            },
        );
        merge_turn_file(&mut turn, sheet(12_000));
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0].path, ".cws/assistant_attachments/ph_used_laptops.xlsx");
        assert_eq!(turn[0].bytes, 12_000);
    }

    #[test]
    fn an_approval_card_says_each_path_once() {
        // The extension hands over `cwtool check`'s sentence, which already names the path that
        // leaves the workspace. The bridge used to append its own copy of the list, which made the
        // card stutter; the structured `escapes` field is what the security rules read.
        let escapes = vec!["/etc/hostname".to_string()];
        let s = approval_summary(
            "read_file",
            "read: /etc/hostname — OUTSIDE the workspace: /etc/hostname",
            "",
            &escapes,
        );
        assert_eq!(s.matches("OUTSIDE the workspace").count(), 1, "{s}");
        assert_eq!(s.matches("/etc/hostname").count(), 2, "the engine's sentence names it twice: {s}");

        // A message that does not name the escape still gets the clause: the card has to be
        // answerable from the card alone.
        let s2 = approval_summary("exec", "run: rm -rf ./build", "", &escapes);
        assert!(s2.contains("OUTSIDE the workspace: /etc/hostname"), "{s2}");

        // Nothing escaping, nothing claimed.
        let s3 = approval_summary("write_file", "write notes.md", "", &[]);
        assert!(!s3.contains("OUTSIDE"), "{s3}");

        // pi's dialog sometimes carries only a title; the card says that rather than nothing.
        let s4 = approval_summary("ask_user", "", "Which of these?", &[]);
        assert_eq!(s4, "ask_user: Which of these?");
    }

    use super::*;

    #[test]
    fn a_permit_given_in_one_conversation_does_not_silence_the_card_in_another() {
        // The user clicked "allow for this session" while working in conversation 7.
        let mut auto: HashMap<i64, HashSet<String>> = HashMap::new();
        auto.entry(7).or_default().insert("read_file".to_string());
        let inside: Vec<String> = vec![];
        assert!(
            permit_covers(&auto, 7, "read_file", &inside),
            "the grant applies where it was given"
        );
        assert!(
            !permit_covers(&auto, 8, "read_file", &inside),
            "conversation 8 granted nothing, so its card must still appear"
        );
        assert!(!permit_covers(&auto, 7, "exec", &inside), "a grant for one tool is not a grant for the next");
        // And a grant never covers a path that leaves the folder — the rule the Rust core
        // states outright, and the reason an out-of-workspace read is asked about every
        // time rather than once.
        assert!(
            !permit_covers(&auto, 7, "read_file", &["/etc/hostname".to_string()]),
            "one yes inside the workspace is not a yes for reading outside it"
        );
    }

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

    /// A cwoffee question set reaches the host through pi's dialog primitives, which carry
    /// a title and bare labels. Everything the engine designed — the one-line trade-offs,
    /// the recommended option, multi, "question 2 of 3" — has to come back out of the
    /// payload, and the set has to hold one id inside this process even though `cwtool`
    /// numbers every call from one.
    #[test]
    fn a_questions_own_card_survives_pis_narrower_dialog() {
        let payload = json!({ "k": "set-1", "q": {
            "id": 2,
            "question": "Which file?",
            "options": [{ "label": "January", "detail": "the closed books" }, { "label": "February" }],
            "recommended": 0,
            "allow_custom": false,
            "multi": true,
            "set": { "id": 1, "index": 0, "total": 2 }
        } });
        let mut next = 7u64;
        let mut sets: HashMap<String, QSet> = HashMap::new();
        let q = canon_question(&format!("{CANON_Q}{payload}"), &mut next, &mut sets).expect("the payload is the card");
        assert_eq!(q.question, "Which file?");
        assert_eq!(q.options[0].detail.as_deref(), Some("the closed books"));
        assert_eq!(q.recommended, Some(0));
        assert!(q.multi, "multi is the model's call, and the card has to honour it");
        assert!(!q.allow_custom);
        assert_eq!(q.id, 7, "a client answers the id this process announced on the card");
        let first_set = q.set.as_ref().expect("a set for more than one question").id;
        assert_ne!(first_set, 1, "the payload's set id came from a process that has already exited");

        let second_payload = json!({ "k": "set-1", "q": {
            "id": 3, "question": "Totals or rows?", "options": [{ "label": "Totals" }],
            "allow_custom": true, "multi": false, "set": { "id": 1, "index": 1, "total": 2 }
        } });
        let second = canon_question(&format!("{CANON_Q}{second_payload}"), &mut next, &mut sets).expect("the second question of the set");
        assert_eq!(second.set.as_ref().unwrap().id, first_set, "one card holding two questions");
        assert_eq!(second.set.as_ref().unwrap().index, 1);

        // The caller's key is not unique — a scripted endpoint hands the same tool-call id to
        // the same scenario every time it runs, and `cwtool` numbers the first set of every
        // call at 1. A complete set is therefore closed to new members, and a later call that
        // reuses the key builds its own card.
        let again = canon_question(&format!("{CANON_Q}{}", json!({ "k": "set-1", "q": {
            "id": 2, "question": "Same key, later call", "options": [{ "label": "x" }], "multi": false, "allow_custom": true,
            "set": { "id": 1, "index": 0, "total": 2 } } })), &mut next, &mut sets).expect("the same key, later call");
        assert_ne!(again.set.as_ref().unwrap().id, first_set, "a settled card must not collect questions from a later call");
        mark_set_closed(&mut sets, first_set);
        mark_set_closed(&mut sets, first_set);
        assert!(!sets.values().any(|e| e.id == first_set), "a fully closed set leaves the map");
        let third = canon_question(&format!("{CANON_Q}{}", json!({ "k": "set-1", "q": {
            "id": 2, "question": "After the card closed", "options": [{ "label": "x" }], "multi": false, "allow_custom": true,
            "set": { "id": 1, "index": 0, "total": 1 } } })), &mut next, &mut sets).expect("a fresh card after closing");
        assert_ne!(third.set.as_ref().unwrap().id, first_set);

        let other = canon_question(&format!("{CANON_Q}{}", json!({ "k": "set-1-next-conversation", "q": {
            "id": 2, "question": "Unrelated", "options": [{ "label": "x" }], "multi": false, "allow_custom": true,
            "set": { "id": 1, "index": 0, "total": 2 } } })), &mut next, &mut sets).expect("another conversation's call");
        assert_ne!(other.set.as_ref().unwrap().id, first_set, "two conversations must not merge into one card");
    }

    #[test]
    fn a_question_card_has_no_clock_unless_somebody_asks_for_one() {
        // The extension sends no `timeout` for a cwoffee question, so the bridge must not
        // invent a deadline of its own: an invented one re-times-out the exact card the
        // user said should stay open until they answer.
        assert!(question_deadline(0).is_none(), "no stated budget must mean no clock");
        assert!(question_deadline(180_000).is_some(), "a stated budget is honoured");
    }

    #[test]
    fn a_plain_dialog_title_is_still_just_a_question() {
        let mut next = 3u64;
        let mut sets: HashMap<String, QSet> = HashMap::new();
        assert!(canon_question("Which format?", &mut next, &mut sets).is_none(), "an ordinary extension dialog keeps its ordinary shape");
        assert!(canon_question("CWOFFEEQ not json", &mut next, &mut sets).is_none());
        assert_eq!(next, 3, "a payload we could not read changes nothing: the arm builds the plain card");
    }

    /// The model's answer to "where are you working?" is whatever it was told, and pi names
    /// the working directory in a `<cwd>` block of its own. For a conversation with no
    /// workspace that directory is a scratch folder under the temp root, so the model recited
    /// `/tmp` to a person who had never chosen a folder. The note is appended only for those
    /// conversations — and for the others the prompt must stay byte for byte what it was,
    /// because the stable prefix is what the endpoint's prefix cache is keyed on and a spare
    /// sentence in there is paid for by every session that has a real folder.
    #[test]
    fn a_conversation_with_a_folder_is_told_exactly_what_it_was_before() {
        let before = cwoffee_engine::prompt::cowork_prompt_text(false);
        let after = cowork_prompt_with_workspace_note(false, Path::new("/home/u/cwoffee-workspace"));
        assert_eq!(before, after, "the stable prompt grew for conversations that do have a workspace");
    }

    #[test]
    fn a_conversation_in_the_temp_folder_is_told_it_has_none() {
        let p = cowork_prompt_with_workspace_note(false, Path::new("/tmp"));
        assert!(p.contains("no workspace"), "{p}");
        assert!(p.contains("answer \"none\""), "{p}");
        // Naming the scratch path in the note would put the words back in its mouth.
        assert!(!p.contains("/tmp"), "the note recited the scratch path: {p}");
        // A folder inside the temp folder was chosen by somebody, so it is a workspace.
        let r = cowork_prompt_with_workspace_note(false, Path::new("/tmp/cwoffee-project"));
        assert_eq!(
            cwoffee_engine::prompt::cowork_prompt_text(false),
            r,
            "a chosen folder inside the temp folder was called no-workspace"
        );
    }

    /// The user's report: "can't switch search engine", with the app quoting a TOML parse
    /// error — `backend = auto`, `invalid string expected "` — and saving nothing. The pi
    /// core built the key list by hand and left the value unquoted; the writer's own
    /// re-parse guard then refused the edit. So the assertion is not "the value contains a
    /// quote", it is "the file the writer produces still parses, and says what was clicked".
    #[test]
    fn a_search_backend_choice_writes_a_line_the_config_file_can_still_read() {
        let text = "# my notes — do not lose me\nmode = \"ask\"\n\n[search]\nbackend = \"duckduckgo\"\nmax_results = 8\n";
        for want in ["auto", "duckduckgo", "searxng"] {
            let keys = if want == "searxng" {
                search_keys(want, "http://127.0.0.1:8080/search")
            } else {
                search_keys(want, "")
            };
            let refs: Vec<(&str, &str)> = keys.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let (out, changed) = Config::set_search_keys_in(text, &refs);
            assert!(!changed.is_empty(), "nothing was written for backend={want}");
            let back: cwoffee_engine::config::Config =
                toml::from_str(&out).unwrap_or_else(|e| panic!("backend={want} wrote a file that will not parse: {e}\n{out}"));
            assert_eq!(back.search.backend, want, "the file does not say what was clicked: {out}");
            assert!(out.contains("# my notes"), "the notes went away: {out}");
            assert!(out.contains("max_results = 8"), "the rest of the section went away: {out}");
            if want == "searxng" {
                assert!(
                    out.contains("searxng_url = \"http://127.0.0.1:8080/search\""),
                    "the manual instance needs its URL in the file: {out}"
                );
            }
        }
        // A provider that does not use a URL must not leave one behind as a claim.
        let keys = search_keys("duckduckgo", "http://127.0.0.1:8080/search");
        assert_eq!(keys.len(), 1, "a duckduckgo choice wrote a searxng URL: {keys:?}");
        assert_eq!(keys[0].1, "\"duckduckgo\"", "the value is not a TOML literal: {keys:?}");
    }

}
