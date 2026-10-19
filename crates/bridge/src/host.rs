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
    pub question_timeout: Duration,
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
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
            context_budget: cfg.context_budget,
            budget_pin: cfg.budget_pin,
            model_window: cfg.model_window,
            approval_timeout_secs: cfg.approval_timeout.as_secs(),
            question_timeout_secs: cfg.question_timeout.as_secs(),
            workspace_refusal: None,
            refusal_seq: 0,
        }));
        let map = SessionMap::open(cfg.session_map.clone());
        let web_last = Arc::new(Mutex::new(None::<Instant>));
        let boot_workdir = cfg.launch.workdir.clone();
        let iter_cap = cfg.max_tool_iters;
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
            q_sets: HashMap::new(),
            titlegen: cwoffee_engine::titlegen::TitleGen::new(
                cwoffee_engine::config::Config::load(None)
                    .map(|(c, _)| c.titlegen_cfg())
                    .unwrap_or_default(),
            ),
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
        deadline: Instant,
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
    /// The same local title helper the rust core offers, from the same config, so the
    /// sidebar names conversations the same way whichever core is behind it.
    titlegen: cwoffee_engine::titlegen::TitleGen,
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
            Some(w) => match crate::mount::mount_settings(
                &self.cfg.launch.agent_dir,
                &self.cfg.launch.provider,
                &model,
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
                     but pi's own threshold could not be set: {e}"
                )),
            },
            None => self.note(format!(
                "context ceiling is now {budget} tokens ({pin_word}); pi keeps its own folding default, \
                 because no window is known for this model to compute a threshold from"
            )),
        }
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

    /// Read what pi says about itself: the model it picked, its window, and the
    /// session file that identifies this conversation on disk.
    fn absorb_state(&mut self, data: &Value) {
        let mut st = self.state.lock().unwrap();
        if let Some(m) = data.get("model") {
            if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                st.model = id.to_string();
            }
            if let Some(w) = m.get("contextWindow").and_then(|v| v.as_u64()) {
                if st.model_window != Some(w as usize) {
                    st.model_window = Some(w as usize);
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
                            // Different model, different window: a budget carried over
                            // from the previous model is how a fold warning shows up in a
                            // conversation that is nowhere near full.
                            self.recompute_budget(&mut st);
                        }
                        self.cfg.launch.model = model.clone();
                        if persist {
                            self.note(
                                "the model change applies to this run; writing the default model to the config \
                                 file is not wired on this core yet",
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
                             scratch; the stored transcript is unchanged"
                        ));
                    }
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
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("new conversation");
        let chars: Vec<char> = first.trim().chars().take(60).collect();
        chars.into_iter().collect()
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
            self.turn_files.extend(files);
        }
        // Stamp at the end of the run, on the answer, once. `take()` so a second settle
        // in the same process cannot re-attach the same files to a later answer.
        if out.settled {
            // Named after the turn, not during it: the row already carries an honest
            // automatic title, and a phrase from a 230 M-parameter model is worth waiting
            // a settled turn for but never worth delaying an answer.
            let session = self.state.lock().unwrap().session_id;
            if session != 0 {
                cwoffee_engine::titlegen::maybe_name(
                    &self.titlegen,
                    &self.cfg.store,
                    &self.ev,
                    session,
                );
            }
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
                let secs = timeout_ms.max(1000) / 1000;
                self.waits.insert(
                    id,
                    Wait::Question {
                        pi_id: pi_id.clone(),
                        kind: QKind::Select { options: labels },
                        deadline: Instant::now() + Duration::from_secs(secs),
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
                let secs = timeout_ms.max(1000) / 1000;
                self.waits.insert(
                    id,
                    Wait::Question {
                        pi_id: pi_id.clone(),
                        kind: QKind::Input,
                        deadline: Instant::now() + Duration::from_secs(secs),
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
                        self.note("a choice was answered after its card had closed; that answer was ignored");
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
                Wait::Question { deadline, answered, set_id, .. } if now >= *deadline && !*answered => {
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
    fn a_plain_dialog_title_is_still_just_a_question() {
        let mut next = 3u64;
        let mut sets: HashMap<String, QSet> = HashMap::new();
        assert!(canon_question("Which format?", &mut next, &mut sets).is_none(), "an ordinary extension dialog keeps its ordinary shape");
        assert!(canon_question("CWOFFEEQ not json", &mut next, &mut sets).is_none());
        assert_eq!(next, 3, "a payload we could not read changes nothing: the arm builds the plain card");
    }
}