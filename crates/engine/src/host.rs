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
    /// Cowork or chat, as the shells report it. The UI reads this rather than
    /// remembering what it last asked for, so a second client cannot be shown a
    /// switch that the host did not make.
    pub persona: Persona,
    /// Whether a chat conversation may reach the web — `web_search` and `web_fetch` mounted, and the
    /// prompt telling the model they exist. See [`HostState::web`] for why this is the host's fact
    /// to report rather than the window's to remember.
    ///
    /// Off by default in both directions, on purpose: a coworker already has these tools as part of
    /// its job, and a chat that suddenly grows network reach is a change in what the person agreed
    /// to. The chip in the composer is what turns it on, per conversation.
    pub web: bool,
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
    /// The MCP catalog: third-party servers and the tools they offered at startup, in a few
    /// hundred bytes. Appended inside `<env>` per turn rather than put in the system prompt,
    /// because the env block is rewritten every turn anyway (it names the folder and the
    /// sandbox) while the system prompt is a cache prefix — so turning a server on or off costs
    /// history bytes, not a permanently edited prompt for every session.
    ///
    /// Empty means no servers, and then the block is byte-identical to what it was before MCP
    /// existed, which is what keeps a machine with no `[mcp]` from paying for a feature it has.
    pub mcp_catalog: String,
    /// How long an unanswered approval prompt waits before refusing.
    pub approval_timeout: std::time::Duration,
    /// How long an `ask_user` card stays open. `question_timeout_secs` in config.toml.
    /// `None` — the default — puts no clock on it: the card is a **paused turn**, and it
    /// stays open until a person answers or the turn is stopped. A three-minute default
    /// answered questions on the user's behalf when the answer was not theirs to give.
    pub question_timeout: Option<std::time::Duration>,
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
    /// The browser this host can offer, and what it knows about the model's eyes. The
    /// driver half is the caller's (it is the caller that built the registry, and the two
    /// have to agree or a card names a tool the prompt never declared); the vision half is
    /// re-read per turn below, because the model changes and the config file is where the
    /// answer lives.
    pub browser: crate::tools::browser::BrowserCfg,
    /// The **kept** browser this app is holding, if any: the browser behind the window's
    /// sidebar. `None` means nothing is kept here, and `browse` with `session: true` says so
    /// rather than opening a second browser the person cannot see.
    ///
    /// It is a handle rather than a path because the browser lives in *this* process on the
    /// Rust core, and on the pi core it lives in the app while the tool host does not — so
    /// that side passes a socket instead. Both sides carry the same synchronous state mirror
    /// for the same reason: `risk_for` cannot await.
    pub browser_route: Option<crate::tools::browser::BrowserRoute>,
    /// What a message typed during a turn promises: into it, or after it (`crate::steer`).
    ///
    /// Stored here rather than remembered per window: the composer says which kind the next press
    /// is, and a phone tab and the desktop pressing Enter in the same conversation have to be given
    /// the same answer. The value comes from `mid_turn` in config.toml, and the settings screen
    /// writes it back through the same field the mode and the thinking default use.
    pub mid_turn: crate::steer::MidTurn,
    /// The palette every surface starts from (`crate::theme`). One value, in `config.toml`, read at
    /// start by the window, a device tab and the terminal.
    ///
    /// The host publishes it rather than leaving it in the file because the alternative is what this
    /// replaced: the key was stored, nobody read it, and the palette lived in one browser's
    /// `localStorage` — a setting that only one surface remembered.
    pub theme: crate::theme::Theme,
}

impl HostConfig {
    pub fn new(model: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            workspace: workspace.into(),
            mode: ApprovalMode::Ask,
            persona: Persona::Cowork,
            // A chat starts with no network reach; the composer chip is what grants it, per
            // conversation. Cowork keeps its web tools because they are part of the job.
            web: false,
            reasoning: Reasoning::XHigh,
            // Uncapped by default; set max_tool_iters in config.toml for a hard stop.
            max_tool_iters: 0,
            max_tokens: None,
            context_budget: None,
            env_override: None,
            mcp_catalog: String::new(),
            approval_timeout: std::time::Duration::from_secs(300),
            question_timeout: None,
            max_attachments: DEFAULT_MAX_ATTACHMENTS,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            sampling: crate::provider::Sampling::default(),
            cfg_path: None,
            egress: crate::tools::net::NetCfg::default().mode,
            // No driver by default: a test host, a probe, and an example do not open
            // browsers, and the tool says "this start has no browser driver" rather than
            // pretending. The desktop and the gateway fill this from the config file.
            browser: crate::tools::browser::BrowserCfg::default(),
            browser_route: None,
            mid_turn: crate::steer::MidTurn::default(),
            theme: crate::theme::Theme::default(),
        }
    }
}

/// Whether the model named here takes image input, as the config file records it.
///
/// Read per turn rather than remembered, from the same file the settings screen writes: the
/// model can change mid-session, and a fact that is a turn stale decides whether a window is
/// painted on a person's desktop. `None` (no config file, or a model nobody probed) is
/// Unknown, which the browser tool treats as "no pixels, no window" and says out loud.
/// Shared with the pi core, which needs the same answer for the same reason: `cwtool` decides
/// headless versus headed from that row, and the host's published state and the settings line have
/// to agree with what the tool will do. A second copy of this lookup would drift the first time a
/// model changed mid-session, and the drift would be a window on someone's desktop.
pub fn model_vision(cfg_path: Option<&std::path::Path>, model: &str) -> crate::tools::browser::Vision {
    use crate::tools::browser::Vision;
    let Some(p) = cfg_path else {
        return Vision::Unknown;
    };
    match crate::config::vision_in(p, model) {
        Some(true) => Vision::Yes,
        Some(false) => Vision::No,
        None => Vision::Unknown,
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

/// One tool, as a settings screen shows it.
///
/// Built from the registry the tools actually run from. A list copied into the UI is a
/// list that goes stale the first time a description changes - and the description is
/// also what the model is handed, so a UI-side copy drifts from the prompt without
/// anyone deciding anything.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolView {
    pub name: String,
    pub description: String,
}

/// The tool list, read under the same config the tools run under. Names and
/// descriptions only: nothing here is a permission, and the settings screen says so
/// rather than implying a switch that does nothing.
pub fn tool_views() -> Vec<ToolView> {
    let cfg = crate::config::Config::load(None)
        .ok()
        .map(|(c, _)| c)
        .unwrap_or_default();
    let bc = cfg.browser_cfg();
    let reg = crate::tools::Registry::with_v1_tools_browser(
        cfg.search_backend(),
        cfg.exec_cfg(),
        bc.usable().then(|| bc.clone()),
    );
    reg.defs()
        .into_iter()
        .map(|d| ToolView { name: d.name, description: d.description })
        .collect()
}

/// The ceiling the engine gives itself for a window: a share of what the endpoint
/// states, never more, so a prompt cannot overflow the model and have the server
/// truncate it from the front with no note attached.
pub fn auto_budget(window: usize) -> usize {
    // Half the window, and it used to be 80%. The 20% that was left over was meant to
    // cover "the reply plus the estimate's own error", and a reply is not a fixed cost:
    // a chat turn asked for an SVG of a pelican on a bicycle and the answer is thousands
    // of lines of markup, which ran into the ceiling and was cut off mid-drawing. A
    // ceiling that truncates an answer is not a ceiling, it is a truncation. So the
    // split is honest: half the window for the conversation, half for what comes back.
    // The failure this guards is still a prompt that overflows the window, which the
    // server truncates from the front with no note attached to it at all.
    ((window as f64 * 0.50) as usize).max(4_096)
}

/// Refuse a pinned context ceiling that cannot mean anything.
///
/// The ceiling is the number the context meter fills against and the number the prompt
/// is folded at, so a nonsense pin is not a harmless preference: too small and the app
/// folds a conversation that had plenty of room, too large and the meter can never
/// reach full, which makes the meter a decoration while the endpoint silently truncates
/// the prompt from the front. Both cores refuse in these words.
pub fn check_budget(target: usize, window: Option<usize>) -> Result<(), String> {
    if target < 4_096 {
        return Err(format!(
            "{target} tokens is below the floor of 4096: a prompt that small cannot hold the system \
             prompt, the tool definitions and a tool result, and still leave room for an answer. Leave \
             the ceiling unset to derive it from the endpoint instead."
        ));
    }
    if let Some(w) = window {
        if target >= w {
            return Err(format!(
                "{target} tokens is not below this endpoint's stated window ({w}): the server would cut \
                 the prompt from the front before cwoffee ever said the context was full. A ceiling has \
                 to be smaller than the window it is measured against."
            ));
        }
    }
    Ok(())
}

/// How the pin sits against the window, in the words the app shows back: a ceiling far
/// below the window is legitimate (someone who wants short, cheap turns) but should be
/// said out loud rather than left as an unexplained number on the meter.
pub fn budget_share(target: usize, window: Option<usize>) -> String {
    match window {
        Some(w) if w > 0 => {
            let pct = (target as f64 / w as f64) * 100.0;
            if pct < 25.0 {
                format!("{pct:.0}% of the endpoint's {w} tokens - that folds early on purpose")
            } else {
                format!("{pct:.0}% of the endpoint's {w} tokens")
            }
        }
        _ => "the endpoint has not stated a window, so nothing but this number is known".to_string(),
    }
}

/// Which window this app believes for one model: what the person stored wins over what the
/// endpoint claims.
///
/// The meter, the derived ceiling and pi's fold threshold all come from this one answer, so a
/// stored override cannot drift from the number on screen. Storing a window is the fix for the
/// common case where the server's real context is set by a launch flag the client never sees.
pub fn chosen_window(stored: Option<usize>, from_endpoint: Option<usize>) -> Option<usize> {
    stored.or(from_endpoint)
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
    cfg_path: Option<&Path>,
) {
    let model = agent.model();
    let info = provider
        .list_models_info()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|m| m.id == model);
    let advertised = info.as_ref().and_then(|m| m.context_tokens).map(|t| t as usize);
    // A stored window is a fact about a config file, so it is read from the file this shell was
    // started with. Asking the default path instead makes the meter disagree with the settings
    // screen: the screen edits one file, the meter reads another, and the number on the bar is
    // neither.
    let cfg_file = cfg_path.map(PathBuf::from).unwrap_or_else(crate::config::Config::default_path);
    let stored = crate::config::window_override_in(&cfg_file, &model);
    let window = chosen_window(stored, advertised);

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
    // The ceiling, where it came from, and the window it was derived against are one
    // claim, so they are written in one lock. A reader that caught this between the
    // writes saw `budget_pin: None` next to the *old pinned* number — which is neither
    // the setting nor the derivation, and the page harness caught it reporting exactly
    // that. A state a client can read half-updated is a state that lies.
    let mut s = state.lock().unwrap();
    s.context_budget = budget;
    s.budget_pin = pinned;
    s.model_window = window;
    drop(s);
    if announce {
        eprintln!(
            "[cwoffee] context budget {budget} tokens (model window {}, budget {})",
            match (window, stored) {
                (Some(w), Some(_)) => format!("{w}, set by you rather than the endpoint's {a}", a = advertised.unwrap_or(w)),
                (Some(w), None) => w.to_string(),
                (None, _) => "unknown".into(),
            },
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
/// Public because a second core has the same obligation: the pi bridge changes the same settings
/// from the same screens, and a file that only one core writes is two answers to one question.
///
/// Written *before* the live value is announced, for the same reason the search
/// panel does it that way: if the running app and `config.toml` disagree, then
/// one of them is lying, and the one the user reads is the file.
/// The thinking levels this model can answer for, spelled in the words the dial uses.
///
/// Three different claims, and the function is written so they cannot be confused:
/// * a probed model reports exactly the efforts its endpoint named - the strongest claim, and the
///   only one that came from this machine's server;
/// * an unprobed model gets its **dialect's documented** efforts, which is what the Rust provider
///   would send anyway. Reporting nothing here would imply "this model cannot think", which is a
///   different claim and a false one;
/// * a level the endpoint never named is left out, because a dial step that the server folds or
///   rejects is the thing that made a reader believe the knob was dead.
///
/// `high` and `xhigh` fold onto one template value for a llama.cpp-shaped server - the same folding
/// `llama_effort` applies when building the request - so an endpoint that accepts `xhigh` can be set
/// from either dial step. An OpenAI-shaped endpoint has both as its own values, so no folding there.
fn dial_levels(cfg_path: Option<&std::path::Path>, model: &str) -> Vec<String> {
    const TEMPLATE: [&str; 3] = ["low", "medium", "xhigh"];
    const OPENAI: [&str; 3] = ["low", "medium", "high"];
    let mut cfg_kind = String::new();
    let mut efforts: Vec<String> = Vec::new();
    if let Some(p) = cfg_path {
        if let Ok((cfg, _)) = Config::load(Some(p)) {
            cfg_kind = cfg.provider.kind.clone();
            efforts = cfg
                .models
                .get(model)
                .map(|r| r.efforts.clone())
                .unwrap_or_default();
        }
    }
    let openai = cfg_kind.eq_ignore_ascii_case("openai");
    if efforts.is_empty() {
        efforts = if openai {
            OPENAI.iter().map(|s| s.to_string()).collect()
        } else {
            TEMPLATE.iter().map(|s| s.to_string()).collect()
        };
    }
    let mut out = vec!["off".to_string()];
    for e in efforts.iter().map(|e| e.as_str()) {
        let add: &[&str] = match e {
            "low" => &["low"],
            "medium" => &["medium"],
            "high" => &["high"],
            // A template that takes xhigh is reached by both top dial steps; an OpenAI-shaped
            // endpoint spells them apart.
            "xhigh" => {
                if openai {
                    &["xhigh"]
                } else {
                    &["high", "xhigh"]
                }
            }
            _ => &[],
        };
        for w in add {
            if !out.iter().any(|have| have == w) {
                out.push((*w).to_string());
            }
        }
    }
    out
}

pub fn persist_setting(path: &Option<std::path::PathBuf>, key: &str, toml_value: &str) {
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

/// Take up whatever endpoint the config names now, in the running host.
///
/// Queued by [`HostOps::reload_provider`], so it only ever runs between turns — which is the whole
/// point of putting it in the queue: a stream that changed endpoint mid-answer would be one answer
/// written by two models, and the transcript would not say so.
async fn reload_provider_now(
    agent: &mut crate::agent::Agent,
    budget_provider: &mut Arc<dyn Provider>,
    provider_cell: &Arc<Mutex<Arc<dyn Provider>>>,
    cfg: &HostConfig,
    state: &Arc<Mutex<HostState>>,
    gauge: &Arc<Mutex<crate::prompt::ContextGauge>>,
    budget_pin: Option<usize>,
    ev_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let cfg_now = crate::config::load_effective(cfg.cfg_path.as_deref());
    let url = cfg_now.provider.base_url.trim().to_string();
    if url.is_empty() {
        // The config says nothing, so the running host keeps whatever it was already answering
        // with. Saying so is the difference between "the setting did not take" and "the setting said
        // there is nothing to take".
        let sess = state.lock().unwrap().session_id;
        let _ = ev_tx.send(EngineEvent::Note {
            session: Some(sess),
            message: format!(
                "nothing was taken up: {} still names no endpoint, so this app keeps answering from whatever it started with. Add an address under Settings › Models.",
                cfg.cfg_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the config".into())
            ),
        });
        return;
    }
    let built = match cfg_now.build_provider() {
        Ok(p) => p,
        Err(e) => {
            // An `Error` event, not a `Note`: the person asked for an endpoint and did not get one.
            // The variant carries no session by design — an endpoint failure is not a conversation's.
            let _ = ev_tx.send(EngineEvent::Error {
                message: format!("the endpoint in the config was not taken up: {e}"),
                fatal: false,
            });
            return;
        }
    };
    let kind = cfg_now.provider.kind.clone();
    let model = agent.model();
    agent.set_provider(Arc::clone(&built));
    *budget_provider = built;
    // Swap the shell's handle in its own statement, and do not hold the guard across anything that
    // can await or ask for the same lock — that exact shape deadlocked this task once before.
    {
        let mut cell = provider_cell.lock().unwrap();
        *cell = Arc::clone(budget_provider);
    }
    // Different endpoint, different model's window. The compaction ceiling comes from the window, so
    // leaving it at the previous endpoint's number is how a fold shows up in a conversation that is
    // nowhere near full — or never shows up in one that is.
    apply_context_budget(
        agent,
        budget_provider,
        budget_pin,
        state,
        &cfg.sampling,
        gauge,
        true,
        cfg.cfg_path.as_deref(),
    )
    .await;
    let sess = state.lock().unwrap().session_id;
    let _ = ev_tx.send(EngineEvent::Note {
        session: Some(sess),
        message: format!(
            "this app answers from {url} ({kind}) now, with {model}: {t}",
            t = context_budget_note(state, &model)
        ),
    });
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

/// Grant or take away web reach for one conversation, on both transports, in one place.
///
/// Two doors lead here — the desktop window and the gateway on a phone — and both have to refuse the
/// same asks for the same reasons, otherwise "the Web chip" means one thing on a laptop and another
/// on a phone. The record is written here (never by the host), and the host is only told when the
/// conversation being changed is the one it is holding.
///
/// The refusals are the substance:
///
///  * **not a chat** — a cowork conversation already has `web_search` and `web_fetch`; writing a
///    grant for it would put a number on the row that nothing reads, and a stored setting that does
///    nothing is the start of a UI that lies;
///  * **mid-turn, for the conversation in hand** — the model was given its tools when the turn
///    started. Flipping the chip then would show a switch the running turn never had, which is the
///    same half-applied failure the folder move refuses to make.
pub fn set_conversation_web(
    store: &dyn crate::store::StoreOps,
    host: &dyn HostOps,
    session: i64,
    on: bool,
) -> Result<String, String> {
    let st = host.state();
    let held = st.session_id == session;
    if held && st.busy {
        return Err(format!(
            "a turn is running in conversation {session}: it was given the tools it started with. Ask again once it finishes, or stop it first — otherwise the switch says one thing and the model is busy answering with another."
        ));
    }
    let row = store
        .session(session)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("conversation {session} is not in the record"))?;
    if row.persona != "chat" {
        return Err(format!(
            "conversation {session} is a cowork conversation: its coworker already has web_search and web_fetch. The Web switch is a chat thing — a chat starts with no tools at all."
        ));
    }
    store.set_session_web(session, on).map_err(|e| e.to_string())?;
    if held {
        host.set_web(on);
    }
    Ok(format!(
        "conversation {session} can {}reach the web{}.",
        if on { "" } else { "no longer " },
        if held {
            " — the model process remounts on the next turn"
        } else {
            ", and it will be mounted that way when you open it"
        }
    ))
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
/// The per-session folder lock, in words, in one place. A conversation's folder is the
/// one its user chose (or the folder the sidebar row was made from); moving the tools
/// somewhere else while a session already has one turns the sidebar's grouping into
/// decoration and makes `write_file` land somewhere nobody named. Both cores have to be
/// able to say it, because the client's check is for the *rule*, not for one core's
/// phrasing — a rule one core states and the other silently violates is two rules.
pub fn folder_lock(current: &str) -> String {
    format!(
        "this session works in {current}; a session keeps its folder, so start a new \
         session to work somewhere else"
    )
}

/// What reopening a conversation cost, in one sentence both cores say.
///
/// It lives here rather than in each core because a note only one core prints is a parity
/// hole the user feels as flakiness: the same click on the same row explained differently
/// depending on what is behind it. The token figure is optional on purpose — a bridge that
/// has not run a turn yet has no figure to quote, and inventing one to complete a sentence
/// is worse than a sentence that says where the number comes from.
pub fn resumed_note(id: i64, n: usize, est: Option<usize>, budget: usize) -> String {
    let measure = match est {
        Some(t) => format!(", ~{t} tokens of a {budget} token budget"),
        None => ", counted from the stored transcript".to_string(),
    };
    format!(
        "resumed session {id}: {n} message(s){measure}. Whether the endpoint still holds this \
                 session's prefix cache is its own business; the first reply settles it in numbers."
    )
}

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
/// Whether this session has a workspace and a tool set at all.
///
/// Deliberately not an `ApprovalMode`: ask/auto decides whether a tool may run, which
/// presumes there are tools. In Chat there is nothing to ask about - no tools are
/// mounted, no folder is claimed, and the system prompt is one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    /// The coworker: eleven Rust tools, a workspace, the full prompt.
    #[default]
    Cowork,
    /// Nothing but the model. One line of system prompt, no tools, no folder.
    Chat,
}

impl Persona {
    /// The whole prompt Chat mode gets. Short on purpose: everything longer is a
    /// claim about what the assistant can do, and in this mode it can do nothing
    /// but answer.
    /// The whole instruction for a chat, and it has to stand on its own: on the pi core this
    /// *replaces* the runtime's default prompt, and nothing else cwoffee says reaches the
    /// model. The second sentence is not decoration — pi appends its own `<cwd>` block
    /// naming the workspace folder even to a chat with no file tools, so the prompt is where
    /// the capability claim gets corrected.
    pub const CHAT_PROMPT: &'static str = "You are cwoffee, a helpful assistant. This mode is \
        conversation only: you have no tools and no folder to work in, so never offer to open, \
        run or change anything — say what you think instead.";

    /// The same assistant with the two network tools granted. See [`Persona::chat_prompt`] for when
    /// each is sent, and the test `the_two_chat_prompts_are_one_assistant` for why the shared opening
    /// is asserted rather than trusted.
    pub const CHAT_WEB_PROMPT: &'static str = "You are cwoffee, a helpful assistant. This mode is \
        conversation only: you have no file tools and no folder to work in, so never offer to open, \
        run or change anything on this machine — say what you think instead. Two tools are yours: \
        web_search, which finds pages about a topic, and web_fetch, which reads one. Use them \
        whenever an answer depends on something only looking can tell you — prices, versions, dates, \
        anything current — and name the page an answer came from. A search that returns nothing \
        useful is answered by saying so, not by guessing.";

    /// Written as its own text rather than an appended sentence on the plain one, because the plain
    /// one spends its first clause denying tools. "…except these two" bolted onto a denial is how a
    /// model ends up not searching: the two halves answer each other instead of the question. So the
    /// web text restates the file-and-shell prohibition where the model will read it, because in this
    /// mode that is still the sentence keeping a chat from promising to fix someone's folder.

    /// The whole instruction a chat gets, in whichever shape the Web switch makes it.
    ///
    /// Two texts, chosen here rather than assembled at each call site, so the chip in the composer,
    /// the tools mounted by the model process and the words the model reads are decided in one place.
    /// A chat with Web off must never be told about `web_search`; a chat with it on must be told,
    /// because a tool nobody describes is a tool the model does not reach for.
    pub fn chat_prompt(web: bool) -> &'static str {
        if web {
            Self::CHAT_WEB_PROMPT
        } else {
            Self::CHAT_PROMPT
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cowork" | "coworker" => Some(Self::Cowork),
            "chat" => Some(Self::Chat),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cowork => "cowork",
            Self::Chat => "chat",
        }
    }
}

pub enum HostCmd {
    /// Switch between the coworker and a bare chat. Relaunching the core is part of
    /// this on the pi side: the tool set and the system prompt are decided when the
    /// process starts, so switching is process lifecycle, not a request parameter.
    SetPersona { persona: Persona },
    /// Grant or take away the two web tools for the chat conversation this host is holding.
    ///
    /// Deliberately not a general "set tools" verb: the pair is the whole of what a chat is allowed
    /// to be given, and a knob that could mount any tool from the wire would be a much larger door
    /// for a phone to walk through.
    SetWeb { on: bool },
    /// Take up the endpoint this app's config now names, without a restart.
    ///
    /// The address a turn goes to is written by the model screen; if the running host could not
    /// accept it, the only honest answer would be "restart the app", and that is a poor price for
    /// changing one line. On the Rust core this swaps the provider the agent streams through and
    /// re-reads the window the budget comes from. On the pi core it re-mounts cwoffee's endpoints in
    /// pi's provider file and closes the model process, which reads that file when it starts.
    ReloadProvider,
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
    /// "What is declared to the model right now, and what is registered but withheld."
    ///
    /// Answered by the host loop rather than by the shells, because the registry lives in the
    /// agent: a settings page or a check that rebuilt a tool list from config would be grading a
    /// second, invented registry. Asked and answered on a reply channel like `Branch` for the same
    /// reason — the caller needs the answer, not an event that may arrive.
    ToolInventory {
        reply: oneshot::Sender<Result<(Vec<String>, Vec<String>), String>>,
    },
    /// Set the thinking effort.
    ///
    /// `persist` says which dial was moved, and the two are different asks:
    ///
    /// * `true` - the settings screen. This is the effort a *new* conversation starts at, and it
    ///   goes in the config file, where a phone and the desktop read the same answer.
    /// * `false` - the composer. This is an override for **this conversation**, kept for the rest of
    ///   it, and it must not move the default. Writing config from here is what made the dial feel
    ///   broken: the next conversation silently inherited a choice made for one chat.
    SetReasoning { reasoning: Reasoning, persist: bool },
    /// Pin the context ceiling, or `None` to derive it from what the endpoint states
    /// about the model's window. The value here is already read back from the config
    /// file: the file is the authority, so a phone and the desktop cannot end up with
    /// two different answers to "where does this conversation fold".
    SetContextBudget { target: Option<usize> },
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
    /// Forget what the host was told after the stored transcript for this conversation now ends.
    ///
    /// Ordering is the whole contract: the caller has already cut the store (`crate::edit`), and
    /// this verb re-syncs the *model's* memory to what the record says. Sent before the re-send, so
    /// the queue does the ordering for it: rewind, then the new turn.
    ///
    /// It is a verb rather than something the caller does, because only the host loop owns the
    /// memory: on the Rust core that is the agent's in-process history, on the pi core it is a
    /// JSONL file pi owns. A host that is not holding this conversation has nothing to forget and
    /// says so in the transcript.
    Rewind { session: i64 },
    /// Make a second conversation out of part of this one, and move this host onto it.
    ///
    /// Queued with a reply channel because the answer is a new conversation id the caller has to
    /// open, and only the host loop can make it: on the Rust core that means replaying the copied
    /// rows into the prompt, on the pi core it means forking pi's session tree and mapping the fork
    /// file onto the new row. Both cores read [`crate::branch`] for the rules and the wording, so a
    /// phone and a desktop are refused the same way and told the same thing when it works.
    Branch {
        req: crate::branch::BranchReq,
        reply: oneshot::Sender<Result<crate::branch::BranchOutcome, String>>,
    },
    /// Move this conversation's leaf — the node its next answer reads through.
    ///
    /// One write, one owner: this loop sets the record's leaf **and** re-seeds the agent's in-process
    /// history from it. Doing the two separately is what lets a record point at a branch the model is
    /// not reading. Re-seeded the way [`Self::Rewind`] re-seeds — replayed from the store, with the
    /// same `persisted_next` bookkeeping so the rows the branch reads are not written a second time.
    ///
    /// The reply is the host's own sentence about its memory, which goes into the outcome note. A
    /// host holding a different conversation answers that it moved the record and had nothing else to
    /// move: that is a fact the user needs, not an error.
    MoveLeaf {
        session: i64,
        target: crate::tree::LeafTarget,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Cancel,
    /// Change what a message typed during a turn promises: `steer` (into this turn, at its next
    /// step) or `follow_up` (after it).
    ///
    /// Queued rather than applied from the shell, because this loop is the one that writes
    /// `config.toml` — the same reason `SetMode` and `SetReasoning` are queued. A message already
    /// waiting keeps the kind it was typed as: changing the setting does not re-promise a message
    /// that is already in the queue.
    SetMidTurn { kind: crate::steer::MidTurn },
    /// Change the stored palette. Queued for the same reason `SetMidTurn` is: this loop is the one
    /// that writes `config.toml`, and a choice that lived only in the running process would be gone
    /// at the next start — which is the whole complaint `theme` used to be.
    ///
    /// This is the **host's** value. A window's override never travels here: it belongs to that
    /// window's own store, and sending it would let one tab repaint every other surface.
    SetTheme { kind: crate::theme::Theme },
    /// The third-party servers just changed: a settings write put new keys in `config.toml` and the
    /// hub was reconciled, so this conversation's catalog and its withheld tools have to follow.
    ///
    /// Answered by the host loop because the registry lives with the agent — a second registry built
    /// on the command's way in would be prompt bytes the running turn never sees, which is the fault
    /// `tool_inventory` was moved off of. On the pi core this is a no-op for a reason rather than a
    /// stub: its tool host fetches the catalog from the hub on every call and the extension re-reads
    /// the catalog block on every request, so the next request already carries the change.
    McpRefresh {
        hub: Arc<tokio::sync::Mutex<crate::mcp::Hub>>,
    },
    /// A mid-turn press, already stamped with cwoffee's id and the moment Enter was pressed.
    ///
    /// The Rust core never sees this command: its queue is a shared lock, polled by the agent at the
    /// step boundary, because this loop runs the turn inline and would not read a command until the
    /// turn was over. The pi core does: its engine is a child process with its own queue, and
    /// `steer` / `follow_up` are how that queue is filled.
    SteerQueued { item: crate::steer::Item },
    /// Take a queued mid-turn message back before it lands, by cwoffee's id.
    WithdrawQueued { id: u64 },
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
    /// Cowork or chat, as the host has it. The shells read this instead of
    /// remembering what they last asked for, so a second client is never shown a
    /// switch the host did not make.
    pub persona: Persona,
    /// Whether the conversation the host is holding can reach the web: whether `web_search` and
    /// `web_fetch` are mounted for the model, and whether its system prompt says they are.
    ///
    /// It lives on the host's state rather than only in the window because it is a fact about what
    /// the model was told. The chip, the system prompt and the mounted tools are three places that
    /// have to agree, and a phone watching the same conversation must read the same answer the
    /// desktop flipped. Defaulted so a client built before the field exists reads "off" rather than
    /// losing the whole state payload.
    #[serde(default)]
    pub web: bool,
    /// What the running model was actually given, in words: "no tools at all", "web_search,
    /// web_fetch", "the coworker's set".
    ///
    /// This exists because a switch is a promise, and a promise should be checkable. The chip says a
    /// chat may search; this field is what the process was launched with, and a check can compare the
    /// two instead of trusting that the code which sets one also sets the other. Empty means "not
    /// stated" — the Rust core does not fill it today, and an empty string must never be rendered as
    /// though it said "none".
    #[serde(default)]
    pub web_tools: String,
    pub reasoning: Reasoning,
    /// The effort new conversations start at - what the settings screen sets. `reasoning` is what
    /// *this* conversation is using. The two part company the moment the composer dial moves, and a
    /// settings screen that displayed the second would appear to change the default every time
    /// somebody changed their mind for one chat.
    #[serde(default)]
    pub reasoning_default: Reasoning,
    /// Whether `reasoning` is a per-conversation override rather than the default. The composer and
    /// the settings screen are the same control, so the only honest label for it is one that says
    /// which of the two is currently in force.
    #[serde(default)]
    pub reasoning_overridden: bool,
    /// The thinking levels this model can answer for, as the host learned them: the endpoint's own
    /// probe on the Rust core, pi's model entry on the pi core.
    ///
    /// This exists because the dial used to move and the wire to say nothing: pi clamps a request to
    /// what the model entry declares, and an entry that declares no thinking answers only `off`. A
    /// host that offers levels the model cannot run is showing a control with no effect, so the
    /// offered list belongs on the state rather than being inferred from the settings screen. Empty
    /// means nobody looked - which is not the same claim as "the model can do nothing".
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
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
    /// The ceiling the meter fills against.
    pub context_budget: usize,
    /// How that ceiling got its value: `Some(n)` is a number a person pinned in the
    /// config or from the settings screen, `None` means it is derived from the window
    /// this model has. The panel has to show which, because a derived ceiling follows a
    /// model change and a pinned one does not — and a screen that cannot tell them apart
    /// cannot explain why the number moved on its own.
    pub budget_pin: Option<usize>,
    #[serde(default)]
    pub model_window: Option<usize>,
    /// What the host currently believes about this model's inputs, which is what decides whether
    /// `browse` paints a window or runs headless.
    ///
    /// Published rather than kept in the tool, because the question "will this open something on my
    /// screen?" is asked from the settings screen and from a phone, and the answer has to come from
    /// the running host — the same thing that will actually run the browser. `unknown` is a real
    /// state here, not a synonym for `no`: it means the endpoint never stated its modalities, and
    /// the tool's own sentence says as much.
    #[serde(default)]
    pub browser_vision: crate::tools::browser::Vision,
    /// Seconds before an unanswered approval fails closed. The UI shows this as a
    /// countdown: a turn blocked on a human looks identical to a turn still
    /// working, and that difference is the entire message.
    pub approval_timeout_secs: u64,
    /// Seconds an unanswered `ask_user` card stays open, same reason as the approval
    /// countdown: a turn parked on a person and a turn still working look identical.
    /// `None` means there is no clock on it — the turn is paused until somebody answers.
    #[serde(default)]
    pub question_timeout_secs: Option<u64>,
    /// What the in-flight turn has said so far but not yet closed — answer text and
    /// reasoning, each empty until there is any.
    ///
    /// This exists for one situation: a browser page pointed at the gateway that is
    /// reloaded while an answer is arriving. The stored rows it gets back stop at the last
    /// closed message, so the answer it paints begins at whatever delta arrives after the
    /// reload — the top of the message is silently missing, and it only repairs itself when
    /// the turn ends and the whole row lands. A client that has this paints the part that
    /// already happened instead.
    #[serde(default)]
    pub open_text: String,
    #[serde(default)]
    pub open_reasoning: String,
    // (The pi core publishes these as the run streams. The Rust core leaves them empty —
    //  its agent loop holds the streaming text in a place that does not reach `state()`, and
    //  the reloaded page falls back to starting where it connected, exactly as before.)
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
    /// What a message typed during a turn promises — into it, or after it.
    ///
    /// On the state rather than only in the config file because the composer announces it, and the
    /// announcement has to be the host's answer rather than each window's guess: two tabs with two
    /// different Enter keys is one conversation getting two different promises.
    #[serde(default)]
    pub mid_turn: crate::steer::MidTurn,
    /// The composer's mid-turn line, in the engine's own words (`crate::steer::composer_hint`).
    ///
    /// Published rather than written by the page: the placeholder is a claim about what Enter will
    /// do, and a UI that phrased it on its own would be a second answer to that question — one that
    /// drifts the moment the host's wording changes.
    #[serde(default)]
    pub mid_turn_hint: String,
    /// What the stored choice means, for the settings screen (`crate::steer::setting_note`).
    /// Quoted rather than rewritten by the page: one promise, one sentence, on either core.
    #[serde(default)]
    pub mid_turn_note: String,
    /// What a mid-turn press cannot do: answer a card, move the approval mode, change the tools
    /// this turn started with. The doors obey this list and the settings page prints it, so a
    /// reader is not told one thing by a refusal and another by a description.
    #[serde(default)]
    pub mid_turn_refusal: String,
    /// What is waiting in the mid-turn queue right now, for the conversation this host holds.
    ///
    /// Published so a client that reloaded mid-turn paints the pending lines it missed, rather than
    /// showing a transcript that lost a message the person typed. Whole rather than incremental,
    /// for the same reason as `QueueChanged`: a stale line about a message that already landed is
    /// worse than no line.
    #[serde(default)]
    pub queued: Vec<crate::steer::View>,
    /// The stored palette — the host's answer, not this window's copy of it.
    #[serde(default)]
    pub theme: crate::theme::Theme,
    /// The settings row's sentence about where the choice lives (`crate::theme::setting_note`),
    /// quoted so a phone and a desktop cannot paraphrase the two homes differently.
    #[serde(default)]
    pub theme_note: String,
    /// About's line: which value is the host's and which is this client's. The client passes its own
    /// override in when it asks, and this is the sentence the screen prints with it — see
    /// `crate::theme::about_says`.
    #[serde(default)]
    pub theme_about: String,
    /// How the palette this surface is *showing* got chosen. For a window that is the media query;
    /// for a terminal it is `COLORFGBG` or the honest absence of one.
    #[serde(default)]
    pub theme_resolved: String,
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
    ///
    /// Behind a lock because the endpoint is not fixed for the life of the process: the model screen
    /// can point this app at another address, and a picker that kept listing the old endpoint's
    /// models after that would be a UI answering from memory. The search tool below already says the
    /// same thing about itself; this is the rest of the app catching up.
    provider: Arc<Mutex<Arc<dyn Provider>>>,
    /// And the search tool, typed: the provider can change while the app runs, and
    /// the only safe place to change it is inside the live tool. Rebuilding the
    /// registry would move prompt bytes and drop every prefix cache, and the
    /// settings card would then be a button that costs the next turn a cold prefill
    /// in every session.
    search: Option<Arc<crate::tools::web::WebSearchTool>>,
    /// The other half of the queue the agent polls. See [`SessionHost::queue_mid_turn`].
    queue: crate::steer::Queue,
    /// A sender into the same event stream the shell is reading. The mid-turn queue is pushed from
    /// the shell's thread — see the note on [`Self::queue`] — and a press has to be announced on the
    /// stream every client watches, not only in the state a poll would later read.
    ///
    /// Handed back on `Shutdown`, because "the events channel closes once the host task returns" is
    /// how a shell learns the host is gone: a sender held here would keep that channel open after
    /// the task ended, and a window would wait for an ending that never arrives.
    ev: Mutex<Option<crate::agent::EngineSink>>,
    /// The attachment gate, kept here so a mid-turn picture is checked against the same limits a
    /// turn's own picture is.
    limits: AttachmentLimits,
    /// Where a change made through the UI is written down. `None` means this host is ephemeral.
    cfg_path: Option<PathBuf>,
}

pub type EventRx = mpsc::UnboundedReceiver<EngineEvent>;
pub type ApprovalRx = (ApprovalRequest, oneshot::Sender<ApprovalAnswer>);

impl SessionHost {
    pub fn spawn(
        provider: Arc<dyn Provider>,
        registry: Registry,
        store: Arc<Store>,
        // `mut`: the loop re-stores the MCP catalog when a server is added while the app is open,
        // and the env block for the next turn is built from this same binding (see McpRefresh).
        mut cfg: HostConfig,
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
        // One title request at a time, process-wide. A conversation names itself
        // through the same endpoint it is about to answer (`crate::titling`): there
        // is no second model, no weights to cache, no helper script, and nothing for
        // a person to configure or apologize for. The slot is here because the ask
        // stalls a first answer, and two conversations should not both be stalled
        // while one of them waits.
        let title_slot = std::sync::Arc::new(crate::titling::TitleSlot::default());
        // The folder you start in is a workspace too. Registering it means the
        // sidebar shows where a brand-new session would write, instead of an
        // empty list until the first message happens to create a row.
        let _ = store.add_workspace(&cfg.workspace.display().to_string());
        // ...and the list is pruned of rows for folders that no longer exist. Runs that register a
        // scratch folder and delete it (the probes, a selftest, a moved temp dir) otherwise leave a
        // row behind forever, and the sidebar fills with folders nobody added.
        let _ = store.prune_dead_workspaces();
        // What the browser will do, decided before anything can run: the driver from the caller
        // (the same value it built the registry from), and the model's recorded inputs from the
        // config file. Both are re-read at the start of every turn below — a probe can record
        // modalities after boot, and a model change is a change to this answer.
        let boot_vision = model_vision(cfg.cfg_path.as_deref(), &cfg.model);
        let state = Arc::new(Mutex::new(HostState {
            session_id: 0,
            workspace: cfg.workspace.display().to_string(),
            model: cfg.model.clone(),
            mode: cfg.mode,
            persona: cfg.persona,
            web: cfg.web,
            // Not stated until a process exists to state it. The bridge fills this when it launches
            // the model; the Rust core's agent loop does not publish a tool list today.
            web_tools: String::new(),
            reasoning: cfg.reasoning,
            reasoning_default: cfg.reasoning,
            reasoning_overridden: false,
            reasoning_levels: dial_levels(cfg.cfg_path.as_deref(), &cfg.model),
            busy: false,
            turns: 0,
            context_tokens: 0,
            compactions: 0,
            approval_timeout_secs: cfg.approval_timeout.as_secs(),
            question_timeout_secs: cfg.question_timeout.map(|d| d.as_secs()),
            // The Rust core's agent loop keeps streaming text somewhere that does not reach
            // `state()`, so this core publishes nothing here: a gateway client that reloads
            // mid-answer falls back to starting where it connected, as it always did.
            open_text: String::new(),
            open_reasoning: String::new(),
            workspace_refusal: None,
            refusal_seq: 0,
            mid_turn: cfg.mid_turn,
            mid_turn_hint: crate::steer::composer_hint(cfg.mid_turn),
            mid_turn_note: crate::steer::setting_note(cfg.mid_turn),
            mid_turn_refusal: crate::steer::card_refusal(),
            theme: cfg.theme,
            theme_note: cfg.theme.setting_note(),
            theme_about: crate::theme::Theme::about_says(cfg.theme, None),
            theme_resolved: String::new(),
            queued: Vec::new(),
            context_budget: cfg.context_budget.unwrap_or(FALLBACK_BUDGET),
            budget_pin: cfg.context_budget,
            model_window: None,
            browser_vision: boot_vision,
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
        // The mid-turn queue, in a handle both this struct and the agent hold.
        //
        // Not a mailbox command: the loop below runs the turn *inline*, so a command sent during a
        // turn is read only after it — which is precisely the behaviour steering replaces. A lock the
        // agent polls at its step boundary is the one place a press can arrive while the model is
        // still working, and it is on the same footing as the cancel flag above for the same reason.
        let queue = crate::steer::Queue::new();
        let queue_bg = queue.clone();
        let limits = AttachmentLimits {
            max_count: cfg.max_attachments,
            max_bytes: cfg.max_attachment_bytes,        };

        let state_bg = Arc::clone(&state);
        let gauge_bg = Arc::clone(&gauge);
        // Cloned before the task takes them: the shell side needs a sender for queue announcements,
        // and it needs to know which file a settings change is written to. `limits` is `Copy`.
        let ev_bg = ev_tx.clone();
        let cfg_path_bg = cfg.cfg_path.clone();
        // The task takes the provider; the host keeps a second handle for
        // non-turn questions like the model list.
        // The handle the shell asks non-turn questions with, in a cell the loop can move: see
        // `SessionHost::provider`.
        let provider_cell = Arc::new(Mutex::new(Arc::clone(&provider)));
        // The loop task needs a handle too: the compaction budget comes from the
        // window the *loaded* model advertises, which is a provider question.
        let mut budget_provider = Arc::clone(&provider);
        // A second handle to the same cell for the loop task; the struct keeps the outer one.
        let loop_cell = Arc::clone(&provider_cell);

        // Built here, not inside the task, so the host can hold the agent's own
        // cancel flag. `SessionHost::cancel()` used to set a separate AtomicBool
        // that nothing ever read, so the UI's Stop button did nothing at all.
        // It has to be a shared flag rather than a HostCmd: the command loop is
        // sequential and is parked inside run_turn() for the whole turn, so a
        // Cancel command would only be processed after the turn it should stop.
        let gate = ApprovalGate::new(cfg.mode, ap_tx).with_timeout(cfg.approval_timeout);
        // The browser the tools will actually use. Vision is re-read here rather than
        // carried from boot: `browse` decides between a real window and a headless run on
        // this, and the model may have changed since the host started. A driver is only ever
        // present if the caller that built the registry saw the same driver — see
        // `HostConfig::browser`.

        let ctx = ToolCtx {
            browser: {
                let mut b = cfg.browser.clone();
                b.vision = boot_vision;
                b
            },
            // The kept browser, if this app has one. Without it `browse` still walks a
            // throwaway page exactly as before; with it, the tool can be pointed at the
            // browser a person is looking at instead of opening a second one behind their
            // back. See `BrowserRoute`.
            browser_route: cfg.browser_route.clone(),
            workspace: cfg.workspace.clone(),
            max_output_bytes: 24 * 1024,
            ask: Some(ask_tx),
            question_timeout: cfg.question_timeout,
            events: None,
            // The store this host already owns, handed to the one tool that reads memory
            // instead of files. Which conversation is asking is *not* set here: a host outlives
            // its conversations, so `session` is filled per turn by the agent loop.
            store: Some(Arc::clone(&store)),
            session: None,
            // Filled per round-trip by the agent from the registry it owns: the tools that exist
            // and are not declared yet. Empty here means "this host holds nothing back", which is
            // what a host with no MCP servers has.
            lazy_tools: Vec::new(),
            // Turn checkpoints, when there is a store to index them in. The bytes on disk
            // are useless without rows: a copy is only revertible if something can say
            // which turn took it and which message row that turn wrote.
            // A `SessionHost` always has a store — it is a constructor argument — so the
            // index can always be written. A host without one (a probe, an example) builds
            // its own `ToolCtx` and leaves this off.
            checkpoint_store: true,
            // The two settings `explore` needs in order to describe the tool set honestly.
            // They come from config because config is where the mounting is decided — the same
            // file the caller read when it built `registry` — and a card that names the wrong
            // search provider, or promises a sandbox that is switched off, is a claim about
            // this machine that is simply false.
            search_backend: crate::config::Config::load(None)
                .map(|(c, _)| c.search_backend())
                .unwrap_or(crate::tools::web::SearchBackend::DuckDuckGo),
            exec_cfg: crate::config::Config::load(None)
                .map(|(c, _)| c.exec_cfg())
                .unwrap_or_default(),
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
                queue: queue_bg.clone(),
                // The flag `SessionHost::cancel()` flips, taken back out of the agent below —
                // one handle, not two, which is the bug this field used to be.
                cancel: Arc::new(AtomicBool::new(false)),
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
            // The thinking effort each conversation is using, and the effort a new one starts at.
            //
            // Two dials in this app share one control, and they ask for different things: the
            // settings screen sets the effort a *new* conversation starts at (and that goes in the
            // config file, where every client reads the same answer), while the composer overrides
            // it for *this* conversation. The override has to be remembered per row, because a
            // conversation's identity outlives the prompt that made it: the row is created at the
            // first send, so an effort chosen before anything was typed would otherwise be thrown
            // away at exactly the moment it should have been applied. That was the bug - pick a
            // level in the composer of a new chat, press send, and the level went back to the
            // default, because the conversation did not exist yet.
            let mut row_thinking: std::collections::HashMap<i64, Reasoning> = Default::default();
            let mut draft_thinking: Option<Reasoning> = None;
            let mut default_reasoning = cfg.reasoning;

            /// Point the agent at the effort that belongs to `id`, and say on the reported state
            /// whether that is an override or the default. Called on every conversation change,
            /// including the one that happens implicitly when a draft's first message creates its
            /// row - which is where the override used to be lost.
            macro_rules! apply_thinking {
                ($target:expr) => {{
                    let id = $target;
                    if id != 0 {
                        // A draft's dial follows the draft into the conversation it becomes.
                        if let Some(t) = draft_thinking {
                            row_thinking.entry(id).or_insert(t);
                            draft_thinking = None;
                        }
                    }
                    let chosen = if id == 0 {
                        draft_thinking.unwrap_or(default_reasoning)
                    } else {
                        row_thinking.get(&id).copied().unwrap_or(default_reasoning)
                    };
                    let overridden = if id == 0 {
                        draft_thinking.is_some()
                    } else {
                        row_thinking.contains_key(&id)
                    };
                    agent.set_reasoning(chosen);
                    {
                        let mut st = state_bg.lock().unwrap();
                        st.reasoning = chosen;
                        st.reasoning_overridden = overridden;
                        st.reasoning_default = default_reasoning;
                    }
                    chosen
                }};
            }
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
                            // Whichever conversation this is, its thinking effort comes with it.
                            apply_thinking!(id);
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
                                    message: resumed_note(id, n as usize, Some(est), budget),
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
                cfg.cfg_path.as_deref(),
            ).await;

            // The ceiling in force for this process. It starts as whatever the config
            // file said, and a settings change replaces it here rather than waiting for
            // a restart: the file is written first, and this is the read-back value, so
            // the meter, the fold and the next start all agree on one number.
            let mut budget_pin = cfg.context_budget;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    HostCmd::Shutdown => break,

                    HostCmd::SetContextBudget { target } => {
                        budget_pin = target;
                        // `apply_context_budget` writes the pin, the budget and the window
                        // together; writing the pin here first is what let a client read a
                        // freed pin next to the number it had just stopped meaning.
                        // Announced, because a ceiling that moves without saying so is
                        // indistinguishable from the meter being wrong.
                        apply_context_budget(
                            &mut agent,
                            &budget_provider,
                            budget_pin,
                            &state_bg,
                            &cfg.sampling,
                            &gauge_bg,
                            true,
                            cfg.cfg_path.as_deref(),
                        )
                        .await;
                        let st = state_bg.lock().unwrap();
                        let sess = st.session_id;
                        let _ = ev_tx.send(EngineEvent::Note {
                            session: Some(sess),
                            message: format!(
                                "context ceiling is now {} tokens ({}); the meter and the fold both read against it",
                                st.context_budget,
                                match target {
                                    Some(t) => format!("pinned by you at {t}, {}", budget_share(t, st.model_window)),
                                    None => "derived: half of whatever the endpoint states".to_string(),
                                }
                            ),
                        });
                    }

                    HostCmd::SetPersona { persona } => {
                        // Half-applied would be worse than refused: this core's prompt,
                        // registry and workspace are one package, and a "chat" that
                        // still has eleven tools and a folder is a different product
                        // wearing the same label.
                        let sess = state_bg.lock().unwrap().session_id;
                        let _ = ev_tx.send(EngineEvent::Note {
                            message: format!(
                                "chat mode is not available on the rust core - it runs the coworker prompt and the tool set. {} stays as it is.",
                                persona.as_str()
                            ),
                            session: Some(sess),
                        });
                    }
                    HostCmd::ReloadProvider => {
                        reload_provider_now(
                            &mut agent,
                            &mut budget_provider,
                            &loop_cell,
                            &cfg,
                            &state_bg,
                            &gauge_bg,
                            budget_pin,
                            &ev_tx,
                        )
                        .await;
                    }
                    HostCmd::SetWeb { on } => {
                        // Nothing to change, and the reason is worth saying out loud: this core has no
                        // chat mode, and its coworker already has `web_search` and `web_fetch` as part
                        // of the job. A switch that answers "done" here would be reporting a change
                        // that could not be observed anywhere.
                        let sess = state_bg.lock().unwrap().session_id;
                        let _ = ev_tx.send(EngineEvent::Note {
                            message: format!(
                                "the web switch is a chat thing, and chat mode is not available on the rust core:                                  this core's coworker already has web_search and web_fetch. Nothing changed (asked: {}).",
                                if on { "on" } else { "off" }
                            ),
                            session: Some(sess),
                        });
                    }
                    HostCmd::Cancel => {
                        agent_cancel.store(true, Ordering::Relaxed);
                    }

                    HostCmd::SetTheme { kind } => {
                        // The file is the home of this choice, so the write happens here, in the
                        // loop that owns `cfg_path`. A palette that only the running process knew
                        // about is what `theme` used to be.
                        let mut st = state_bg.lock().unwrap();
                        st.theme = kind;
                        st.theme_note = kind.setting_note();
                        st.theme_about = crate::theme::Theme::about_says(kind, None);
                        persist_setting(&cfg.cfg_path, "theme", &crate::Config::toml_str(kind.word()));
                    }
                    HostCmd::McpRefresh { hub } => {
                        // The hub is the authority on what is running, so both halves of the answer
                        // come off it: the tool list the registry should hold and the catalog text
                        // the env block carries. Nothing here re-reads the config file — the caller
                        // reconciled the hub with the file before sending this, and a second reader
                        // in this loop could disagree with it.
                        let (catalog, block) = {
                            let h = hub.lock().await;
                            (h.catalog(), h.catalog_block())
                        };
                        let mut added = 0usize;
                        let dropped;
                        {
                            let reg = agent.registry_mut();
                            // First take away what the hub no longer offers, then add what it newly
                            // does: a server that was removed and renamed in the same edit should not
                            // leave the old name callable until the next start.
                            let offered: std::collections::HashSet<String> =
                                catalog.iter().map(|t| t.declared.clone()).collect();
                            dropped = reg.forget_stale_mcp(&offered);
                            for info in catalog.iter() {
                                // Only names this registry has never seen. Re-registering a live
                                // server's tool would replace the handle mid-turn and reset the
                                // hidden/declared state of a tool the model may already be using.
                                if reg.get_shared(&info.declared).is_some() {
                                    continue;
                                }
                                reg.register_lazy(Arc::new(crate::mcp::McpTool::new(
                                    info.clone(),
                                    Arc::clone(&hub),
                                )));
                                added += 1;
                            }
                        }
                        cfg.mcp_catalog = block;
                        eprintln!(
                            "[cwoffee] mcp refresh: {added} new tool(s) registered hidden, {} dropped (the hub no longer offers them), catalog re-stored for the next turn",
                            dropped.len()
                        );
                    }

                    HostCmd::SetMidTurn { kind } => {
                        // Handled here as well as by `SessionHost::set_mid_turn`, which answers the
                        // composer immediately: this is where the file gets written, and a choice
                        // that lived only in this process would be gone at the next start.
                        let mut st = state_bg.lock().unwrap();
                        st.mid_turn = kind;
                        st.mid_turn_hint = crate::steer::composer_hint(kind);
                        st.mid_turn_note = crate::steer::setting_note(kind);
                        st.mid_turn_refusal = crate::steer::card_refusal();
                        persist_setting(
                            &cfg.cfg_path,
                            "mid_turn",
                            &crate::Config::toml_str(kind.word()),
                        );
                    }

                    HostCmd::SteerQueued { item } => {
                        // On this core a mid-turn press goes straight into the shared queue, because
                        // this loop is parked inside `run_turn` and would not read a command until
                        // the turn was over. Arriving here means it was sent between turns, and the
                        // queue is the same queue either way — so the press lands rather than being
                        // dropped by a host that happens to have been addressed the other core's way.
                        let session = item.session;
                        queue_bg.push(session, item.kind, item.text, item.atts, item.typed_at);
                        let _ = ev_tx.send(EngineEvent::QueueChanged {
                            session,
                            items: queue_bg.snapshot(),
                        });
                    }

                    HostCmd::WithdrawQueued { id } => {
                        let session = state_bg.lock().unwrap().session_id;
                        if queue_bg.withdraw(id).is_none() {
                            // It landed, or somebody else took it back. Say which of the two the
                            // reader is looking at, in the same words the trait uses.
                            let _ = ev_tx.send(EngineEvent::Error {
                                message: crate::steer::withdraw_gone_refusal(),
                                fatal: false,
                            });
                            continue;
                        }
                        let _ = ev_tx.send(EngineEvent::QueueChanged {
                            session,
                            items: queue_bg.snapshot(),
                        });
                    }

                    HostCmd::Rewind { session } => {
                        // Half of "edit a message and send it again" happened in the store, before
                        // this was queued. What is left is the part only this loop can do: the
                        // agent's in-process history still holds the messages the person has just
                        // cut out of the record, and a turn built from that answers a question that
                        // is no longer on screen.
                        //
                        // Re-seeded from the store rather than spliced by hand, because the store is
                        // the transcript: it already holds exactly what the conversation now says,
                        // in the order it says it, and replaying it is the same path a reopened
                        // conversation takes (see `adopt_view!`), down to the sequence bookkeeping
                        // that keeps those rows from being persisted a second time.
                        if session != session_id {
                            // The host is holding a different conversation, so it was never told
                            // the one that was edited: there is nothing here to take back. The edit
                            // itself stands — the record was cut, and that is what the user asked
                            // for — so this is a note, not an error.
                            let _ = ev_tx.send(EngineEvent::Note {
                                session: Some(session),
                                message: format!(
                                    "conversation {session} was taken back to that message. This engine is \
                                     holding conversation {session_id}, so it had not been told anything \
                                     about the one you edited; the next time you open it, it starts from \
                                     what the transcript now says."
                                ),
                            });
                            continue;
                        }
                        match store.history(session) {
                            Ok(hist) => {
                                let n = hist.len() as u64;
                                agent.prompt_mut().with_history(hist);
                                agent.prompt_mut().set_first_seq(0);
                                agent.reset_turn_id();
                                persisted_next = n;
                                {
                                    let mut s = state_bg.lock().unwrap();
                                    // Whatever folds the old in-memory prompt had been carrying go
                                    // with it; the replayed history is un-folded.
                                    s.compactions = 0;
                                    s.context_tokens = agent.prompt_mut().estimate_tokens("");
                                    agent.publish_gauge("");
                                }
                                let _ = ev_tx.send(EngineEvent::Note {
                                    session: Some(session),
                                    message: format!(
                                        "the model is back to {n} {} in conversation {session}: anything \
                                         after the message you edited is off its context.",
                                        if n == 1 { "message" } else { "messages" }
                                    ),
                                });
                            }
                            Err(e) => {
                                // The record is cut and the model still remembers: that is worth
                                // saying out loud, because the two will disagree until this works.
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!(
                                        "the transcript for conversation {session} was cut, but the model \
                                         could not be re-read from it ({e}) — it is still working from the \
                                         messages you removed. Reopen the conversation to fix that."
                                    ),
                                    fatal: false,
                                });
                            }
                        }
                    }

                    HostCmd::MoveLeaf { session, target, reply } => {
                        // One write here, not two: the record's leaf and this loop's memory move
                        // together, so a failure leaves the conversation where it was instead of
                        // pointing a record at a branch the model is not reading.
                        if let Err(e) = store.set_leaf(session, target.leaf) {
                            let _ = reply.send(Err(format!(
                                "the leaf of conversation {session} could not be written ({e}), so \
                                 nothing was moved"
                            )));
                            continue;
                        }
                        if session != session_id {
                            let _ = reply.send(Ok(format!(
                                "This engine is holding conversation {session_id}, so it had not been \
                                 told anything about conversation {session}: the branch is in the \
                                 record, and opening it starts the model there."
                            )));
                            continue;
                        }
                        match store.history(session) {
                            Ok(hist) => {
                                let n = hist.len() as u64;
                                agent.prompt_mut().with_history(hist);
                                agent.prompt_mut().set_first_seq(0);
                                agent.reset_turn_id();
                                persisted_next = n;
                                {
                                    let mut s = state_bg.lock().unwrap();
                                    s.compactions = 0;
                                    s.context_tokens = agent.prompt_mut().estimate_tokens("");
                                    agent.publish_gauge("");
                                }
                                let through = match target.leaf {
                                    Some(id) => format!("message {id}"),
                                    None => "its newest message".to_string(),
                                };
                                let _ = ev_tx.send(EngineEvent::Note {
                                    session: Some(session),
                                    message: format!(
                                        "the model is back to {n} {} in conversation {session}, read \
                                         through {through}: what came after that node is off its \
                                         context and still in the record.",
                                        if n == 1 { "message" } else { "messages" }
                                    ),
                                });
                                let _ = reply.send(Ok(
                                    "This engine re-read the branch from the conversation's messages, \
                                     so the model reads it from here."
                                        .to_string(),
                                ));
                            }
                            Err(e) => {
                                // The leaf is written and the model cannot be re-read: say so,
                                // because the two records will disagree until this works.
                                let _ = reply.send(Err(format!(
                                    "the leaf of conversation {session} was moved in the record, but \
                                     this engine could not re-read it from there ({e}) — the model is \
                                     still working from the whole conversation. Reopen it to put the \
                                     model on the branch."
                                )));
                            }
                        }
                    }

                    HostCmd::ToolInventory { reply } => {
                        // Read from the agent's own registry, in the loop that owns it. Nothing
                        // else in the app can say what the next request would carry.
                        let (declared, hidden) = agent.tool_inventory();
                        let _ = reply.send(Ok((declared, hidden)));
                    }

                    HostCmd::Branch { req, reply } => {
                        // No busy check here, and one would be dead code: `Send` runs the turn inline
                        // in this loop, so a branch cannot be read at all until the turn has ended.
                        // A mid-turn press is the one thing that does reach this loop while it is
                        // parked — through the shared queue, not through the mailbox, which is why
                        // `crate::steer::Queue` is a handle and `SteerQueued` is only the pi road.
                        // The refusal that matters is the one in `crate::branch::apply_and_open`,
                        // which runs before the command is queued — the same gate a phone goes
                        // through, so neither device can branch a conversation mid-answer.
                        //
                        // Row first, then the model. If the copy or the adoption fails there is no
                        // half-made conversation left to find in the sidebar: the row is deleted
                        // before the error goes back, because a row whose prompt the host could not
                        // load is a transcript that looks branched and answers like a fresh one.
                        let rows = match store.messages(req.session) {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = reply.send(Err(format!(
                                    "conversation {} could not be read ({e}), so nothing was branched",
                                    req.session
                                )));
                                continue;
                            }
                        };
                        let point = match crate::branch::resolve(&rows, &req) {
                            Ok(p) => p,
                            Err(reason) => {
                                let _ = reply.send(Err(reason));
                                continue;
                            }
                        };
                        let (new_id, title, copied) = match crate::branch::create_row(&*store, req.session, &point) {
                            Ok(v) => v,
                            Err(reason) => {
                                let _ = reply.send(Err(reason));
                                continue;
                            }
                        };
                        // The adoption is the branch: `adopt_view!` replays the copied rows into the
                        // prompt, rebinds where the next turn is persisted, moves the tools to the
                        // folder the new row names, and takes the conversation's thinking effort with
                        // it — which is exactly what "this conversation continues from there" needs
                        // the model to be told. Marking it announced first keeps the usual
                        // "reopened with N messages" note off a conversation that was never reopened:
                        // it was made, one turn ago, in this loop.
                        resume_announced.insert(new_id);
                        if !adopt_view!(new_id) {
                            let _ = store.delete_session(new_id);
                            let _ = reply.send(Err(format!(
                                "the branch's row ({new_id}) was created but this engine could not take \
                 it up, so it was removed: nothing was branched and conversation {} is unchanged",
                                req.session
                            )));
                            continue;
                        }
                        let note = crate::branch::outcome_note(req.session, &point, new_id, copied, &title);
                        // Say what happened to the model's memory as well as to the record: this
                        // engine has no thread file, and `adopt_view!` above is what makes the
                        // copied rows the prompt it answers from.
                        let note = format!("{note} {}", crate::branch::memory_note("rust", None));
                        let _ = ev_tx.send(EngineEvent::Note { session: Some(new_id), message: note.clone() });
                        let _ = reply.send(Ok(crate::branch::BranchOutcome {
                            session: new_id,
                            title,
                            copied,
                            prefill: point.ask,
                            note,
                        }));
                    }

                    HostCmd::ClearSession => {
                        session_id = 0;
                        persisted_next = 0;
                        agent.replace_prompt(PromptBuilder::new());
                        apply_thinking!(0);
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
                                        &folder_lock(&current),
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
                            {
                                let mut st = state_bg.lock().unwrap();
                                st.model = model;
                                // Levels belong to the model. Carrying the previous model's list
                                // would keep a dial offering levels this one clamps to off, which
                                // is the failure the list exists to catch.
                                st.reasoning_levels = dial_levels(cfg.cfg_path.as_deref(), &model_now);
                            }
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
                                budget_pin,
                                &state_bg,
                                &cfg.sampling,
                                &gauge_bg,
                                true,
                                cfg.cfg_path.as_deref(),
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

                    HostCmd::SetReasoning { reasoning, persist } => {
                        if persist {
                            // The settings screen: the effort new conversations start at. It goes
                            // in the config file, and it also moves a conversation that has never
                            // moved its own dial - a default that only applied to conversations you
                            // had not started yet would read as the setting not taking effect.
                            default_reasoning = reasoning;
                            let word = serde_json::to_value(reasoning)
                                .ok()
                                .and_then(|v| v.as_str().map(String::from))
                                .unwrap_or_else(|| "low".into());
                            persist_setting(&cfg.cfg_path, "reasoning", &Config::toml_str(&word));
                            apply_thinking!(session_id);
                        } else {
                            // The composer: this conversation, for the rest of it, and nothing else.
                            // The config file is deliberately not written - a level chosen for one
                            // chat must not become the level every future chat starts at.
                            if session_id == 0 {
                                draft_thinking = Some(reasoning);
                            } else {
                                row_thinking.insert(session_id, reasoning);
                            }
                            agent.set_reasoning(reasoning);
                            let mut st = state_bg.lock().unwrap();
                            st.reasoning = reasoning;
                            st.reasoning_overridden = true;
                            st.reasoning_default = default_reasoning;
                        }
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
                                // A new conversation starts at the default, or at the dial this draft
                                // had already moved to - not at whatever the last chat was using.
                                apply_thinking!(id);
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
                            apply_thinking!(0);
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
                        // The conversation can stop existing between the moment the
                        // reader looked at it and the moment their message lands: the
                        // desktop deletes a conversation by moving the host off the row
                        // first, but that move is a *queued command*, and a send that
                        // raced it would run a whole turn into an id nothing lists any
                        // more — an answer nobody can open again. Ask where we actually
                        // are, and if the answer is "nowhere", start a conversation
                        // rather than write into a grave.
                        if session_id != 0 && store.session(session_id).ok().flatten().is_none() {
                            let _ = ev_tx.send(EngineEvent::Note {
                                session: Some(session_id),
                                message: "that conversation was deleted while your message was on its way. \
                                          Nothing was lost from your side: the reply starts a new conversation."
                                    .to_string(),
                            });
                            session_id = 0;
                        }
                        if session_id == 0 {
                            let ws = agent.workspace().display().to_string();
                            let _ = store.add_workspace(&ws);
                            let model = agent.model();
                            match store.create_session("New Session", &ws, &model) {
                                Ok(id) => {
                                    session_id = id;
                                    persisted_next = 0;
                                    state_bg.lock().unwrap().session_id = id;
                                    // The row exists now. Whatever effort was chosen while this was
                                    // still a draft belongs to it, and it has to be re-applied here
                                    // rather than trusted to have stuck from before.
                                    apply_thinking!(id);
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
                        let env = cfg.env_override.clone().unwrap_or_else(|| {
                            // Read the mirror, not the hub: this runs inside the turn loop, and
                            // waiting on whoever is driving the browser would let a page click
                            // stall a model's prompt.
                            let open = cfg
                                .browser_route
                                .as_ref()
                                .map(|r| r.peek())
                                .filter(|s| s.alive && s.opened);
                            env_block_live(
                                agent.workspace(),
                                "linux",
                                cfg.egress,
                                &cfg.mcp_catalog,
                                open.as_ref(),
                            )
                        });
                        agent.prompt_mut().set_env(env);
                        // The browser's mode is a volatile fact on the same footing as the env
                        // block: it is decided from the config file, and it can change inside a
                        // session — the model changes, or a probe records modalities that were
                        // unknown at boot. Reading it here rather than once at spawn is what keeps
                        // "the settings page says this start runs headed" and "the card ran headed"
                        // from being two answers to one question. The selftest's `browse` group
                        // grades exactly that pair, and it caught the spawn-time version.
                        let vision = model_vision(cfg.cfg_path.as_deref(), &agent.model());
                        agent.set_browser_vision(vision);
                        {
                            // Written next to the agent's own copy so a reader of `state()` and the
                            // tool that runs cannot disagree about what the next browser call will do.
                            let mut s = state_bg.lock().unwrap();
                            s.browser_vision = vision;
                        }

                        set_busy(&state_bg, true);
                        // The window is the one fact the whole budget is derived
                        // from. If the endpoint was busy starting up when the host
                        // asked at boot, asking again here costs two GETs and is
                        // the difference between a 209k budget and a 32k one - and
                        // the smaller number is the one that deletes history.
                        if budget_pin.is_none()
                            && agent.prompt_mut().context_window.is_none()
                        {
                            apply_context_budget(
                                &mut agent,
                                &budget_provider,
                                budget_pin,
                                &state_bg,
                                &cfg.sampling,
                                &gauge_bg,
                                false,
                                cfg.cfg_path.as_deref(),
                            ).await;
                        }
                        // --- name the conversation before answering it -------------------
                        // It arrived titled "New Session" (or it is an older conversation
                        // that was never named). Ask this same endpoint, in its own context,
                        // for a name, and stream it out as it arrives so the sidebar row
                        // types it. The answer waits for that on purpose; `crate::titling`
                        // carries the reasoning and the eight-second bound. A miss is not a
                        // failure the reader pays for: the row falls back to the opening
                        // line, because a sidebar of identical rows says nothing.
                        if title_slot.claim() {
                            let row = store.session(session_id).ok().flatten();
                            let title_now = row.as_ref().map(|r| r.title.clone()).unwrap_or_default();
                            let first_user = {
                                let stored = store.history(session_id).unwrap_or_default();
                                stored
                                    .into_iter()
                                    .find(|m| matches!(m.role, Role::User))
                                    .map(|m| m.content)
                                    .unwrap_or_else(|| text.clone())
                            };
                            if crate::titling::still_untitled(&title_now, &first_user) {
                                let ev2 = ev_tx.clone();
                                let sid = session_id;
                                let outcome = crate::titling::ask_title(
                                    agent.provider(),
                                    &agent.model(),
                                    &first_user,
                                    |t, done| {
                                        let _ = ev2.send(EngineEvent::TitleDelta {
                                            session_id: sid,
                                            text: t.to_string(),
                                            done,
                                        });
                                    },
                                )
                                .await;
                                match outcome {
                                    crate::titling::Outcome::Named(name) => {
                                        if let Err(e) = store.rename_session(session_id, &name) {
                                            eprintln!("[cwoffee/titling] named it {name:?} but the rename failed: {e}");
                                        } else {
                                            let _ = ev_tx.send(EngineEvent::SessionChanged {
                                                session_id,
                                                title: name,
                                            });
                                        }
                                    }
                                    crate::titling::Outcome::FellBack(why) => {
                                        let fb = crate::store::derive_title(&first_user);
                                        if !fb.is_empty() {
                                            if let Err(e) = store.rename_session(session_id, &fb) {
                                                eprintln!("[cwoffee/titling] fallback rename failed: {e}");
                                            } else {
                                                let _ = ev_tx.send(EngineEvent::SessionChanged {
                                                    session_id,
                                                    title: fb,
                                                });
                                            }
                                        }
                                        eprintln!("[cwoffee/titling] session {session_id} keeps a fallback name: {why}");
                                    }
                                    crate::titling::Outcome::NotEligible => {}
                                }
                            }
                            title_slot.release();
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

                        // Name the answer row this turn's copies belong to, so the
                        // transcript can offer "revert files from this turn" against a row
                        // id instead of a guess. Done next to the file stamps for the same
                        // reason: the row only exists once the answer is finished, and a
                        // control addressed at a row that is not there yet is a control
                        // that has to say it is waiting.
                        if let Some(id) = last_assistant {
                            if let Err(e) = store.stamp_checkpoints_turn(session_id, agent.turn() as i64, id) {
                                let _ = ev_tx.send(EngineEvent::Error {
                                    message: format!("failed to index that turn's file checkpoints: {e}"),
                                    fatal: false,
                                });
                            }
                        }

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

                        // Naming happened before the answer, not after it: see the title
                        // pass above and `crate::titling`. Nothing runs here — a
                        // conversation that reaches its second turn with a name the reader
                        // did not choose has already been offered one, and the rule that a
                        // name a person typed is never overwritten still holds.

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

                        // The turn is over. Anything still queued for this conversation made a
                        // promise about *this* turn, and this turn ended without taking it — Stop,
                        // an error, or words typed after the last step boundary. They go back to the
                        // composer rather than migrating into whichever turn comes next, which would
                        // be a different promise than the one the person typed against.
                        let back = queue_bg.take_session(session_id);
                        if !back.is_empty() {
                            let views: Vec<crate::steer::View> = back.iter().map(|i| i.view()).collect();
                            let _ = ev_tx.send(EngineEvent::QueueReturned {
                                session: session_id,
                                items: views,
                            });
                            let _ = ev_tx.send(EngineEvent::Note {
                                session: Some(session_id),
                                message: crate::steer::returned_note(back.len()),
                            });
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
            provider: provider_cell,
            search: search_tool,
            queue,
            ev: Mutex::new(Some(ev_bg)),
            limits,
            cfg_path: cfg_path_bg,
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
        // Read the handle out and drop the guard before the await: a lock held across a network call
        // is how this app has deadlocked before (see the session-id note in the command loop).
        let provider = self.provider();
        let first = provider.list_models().await.ok()?.into_iter().next()?;
        self.send(HostCmd::SetModel { model: first.clone(), persist: false });
        Some(first)
    }

    /// Handle to the endpoint, for shell-level queries that are not turns. Cloned out rather than
    /// borrowed: the handle can be moved by the loop the moment the person changes endpoint, and a
    /// borrow would be a promise that it stays there.
    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider.lock().unwrap())
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
        if matches!(cmd, HostCmd::Shutdown) {
            // Give up our sender before the task ends, so the receivers see the stream close.
            let _ = self.ev.lock().unwrap().take();
        }
        let _ = self.cmd.send(cmd);
    }

    /// Announce something the shell's thread caused, on the stream every client reads. Silent when
    /// the sender is gone: after Shutdown there is no stream to announce on, and nothing to announce
    /// *to* either.
    fn announce(&self, ev: EngineEvent) {
        if let Some(tx) = self.ev.lock().unwrap().as_ref() {
            let _ = tx.send(ev);
        }
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

    /// Park the host in the busy state without running a turn.
    ///
    /// `checkpoint::tests` uses it to grade the revert's "a turn is running, so nothing
    /// was touched" refusal: that refusal is the thing being tested, and making it wait
    /// for a scripted turn to be mid-flight would grade the fixture's timing instead.
    /// Whether a real turn sets the flag at all is graded elsewhere — by the host's own
    /// tests and by the selftest's mid-turn claim.
    #[cfg(test)]
    pub(crate) fn set_busy_for_test(&self, busy: bool) {
        set_busy(&self.state, busy);
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
        // Read from the queue itself rather than kept in step by every push and drain: the agent
        // drains it at its own step boundary, on a thread that would otherwise have to remember to
        // publish, and a snapshot that lags shows a line for a message the model already read.
        s.queued = self.queue.snapshot().into_iter().filter(|i| i.session == s.session_id).collect();
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

    /// Take a conversation back to an earlier message.
    ///
    /// The stored transcript has already been cut by the caller — see [`crate::edit`]. This is the
    /// other half: the host has been told things the record no longer contains, and unless it is
    /// brought back too, the next turn is answered from a conversation the user cannot see any more.
    ///
    /// Queued like every other verb here, so it lands in the host's own loop where that memory
    /// lives, and it arrives ahead of the re-send that follows it.
    fn rewind(&self, session: i64) {
        self.send(HostCmd::Rewind { session })
    }

    /// Make a second conversation out of part of this one, and start holding it.
    ///
    /// Unlike [`Self::rewind`] this cannot be fire-and-forget: the caller has to be told which
    /// conversation now exists, because that is the row it opens and the conversation the branch
    /// note belongs in. So it waits for the host loop's own answer, and `Err` means nothing was
    /// created — no row in the sidebar, no thread in pi's directory, the original untouched.
    ///
    /// The default refuses in words, for a host with no way to fork its own memory. A client that
    /// hears that says it out loud rather than showing a conversation that only looks branched.
    async fn branch(&self, req: &crate::branch::BranchReq) -> Result<crate::branch::BranchOutcome, String> {
        let _ = req;
        Err(
            "this engine cannot branch a conversation yet: the model host has no way to fork its own \
             memory, and a copied transcript the model has not read is not a branch"
                .to_string(),
        )
    }

    /// Move this conversation's leaf — the node its next answer reads through — and say what this
    /// host did to **its own** memory.
    ///
    /// The record's leaf is already moved by the time a host is asked (see [`crate::tree::move_leaf`]),
    /// because every read path in this crate follows that column and an idle conversation should open
    /// on the branch it was left on. What only the host can do is move the memory a live model is
    /// reading: the agent's in-process history here, pi's session tree over there.
    ///
    /// It answers with a sentence rather than a bool on purpose. "pi moved its leaf" and "this engine
    /// re-read the messages" and "this host is holding another conversation" are three different
    /// facts, and the one the person has to know is whether the model moved with the record.
    async fn move_leaf(&self, session: i64, target: &crate::tree::LeafTarget) -> Result<String, String> {
        let _ = (session, target);
        Err(
            "this host has no memory of its own to move: the record's leaf moved, and the model was \
             not told".to_string(),
        )
    }

    /// This conversation's node list as **pi** holds it, or `None` when there is no pi tree to read.
    ///
    /// Read from the live child and not by parsing its session file: what decides the next answer is
    /// the tree the running process holds, and a file replaced underneath it should come back as a
    /// disagreement between the two records — which is what the caller reports rather than hiding.
    ///
    /// `None` is not "an empty tree". It means this host is not holding this conversation, or has no
    /// child at all, and the read says so instead of drawing a thread nobody read.
    async fn pi_tree(&self, session: i64) -> Option<crate::tree::PiTree> {
        let _ = session;
        None
    }

    /// "What is declared to the model right now, and what is registered but withheld."
    ///
    /// The default says plainly that this core cannot answer, rather than answering with two empty
    /// lists: `[]` hidden and `[]` declared look exactly like "nothing is withheld", which on the
    /// pi core is a falsehood — that list is decided inside the model process. A check grading an
    /// invented answer would pass while the real thing stayed unmeasured.
    async fn tool_inventory(&self) -> Result<(Vec<String>, Vec<String>), String> {
        Err("this core decides its tool list inside the model process, so it cannot report one".to_string())
    }

    /// Grant or take away web reach for the chat conversation this host holds. Queued like every
    /// other verb here: the tools live in the model process, and only its own loop can re-make it.
    ///
    /// The default refuses in words, on the Rust core, where a chat does not exist and the coworker's
    /// web tools are part of its job rather than a switch — see [`set_conversation_web`], which is
    /// where the check that this is a chat at all lives.
    fn set_web(&self, on: bool) {
        self.send(HostCmd::SetWeb { on })
    }

    /// Re-read the endpoint and start answering from it. Queued, so it waits for the turn like every
    /// other verb here; it answers `Err` when the config names no usable address, so the caller can
    /// say which half of the setup is missing instead of pointing at a transcript that stayed empty.
    fn reload_provider(&self) -> Result<String, String> {
        self.send(HostCmd::ReloadProvider);
        Ok("the endpoint was handed to the model host".into())
    }

    /// A message typed **while a turn is running**, queued with whatever promise this host's stored
    /// choice makes: into this turn at its next step, or after it.
    ///
    /// The returned [`crate::steer::View`] carries cwoffee's own id, which is the handle for taking
    /// it back — pi's queue is a list of plain strings and names nothing, so an id assigned here is
    /// the only withdrawal handle that exists on either core.
    ///
    /// The default refuses in words. A host with nowhere to put a mid-turn press says so rather than
    /// dropping the words, which is the failure this feature exists to remove.
    fn queue_mid_turn(
        &self,
        text: String,
        attachments: Vec<crate::provider::Attachment>,
        session: Option<i64>,
    ) -> Result<crate::steer::View, String> {
        let _ = (text, attachments, session);
        Err(
            "this host has no mid-turn queue, so a message typed during a turn has nowhere to go"
                .to_string(),
        )
    }

    /// Take a queued message back before it lands.
    ///
    /// `Err` means it is no longer waiting: it went into the conversation, and a message already on
    /// the wire cannot be unsent — the row stays and the card says so.
    fn withdraw_queued(&self, id: u64) -> Result<crate::steer::View, String> {
        let _ = id;
        Err("this host has no mid-turn queue, so there is nothing here to take back".to_string())
    }

    /// What is waiting right now, in the order it was typed. A client that reloaded mid-turn paints
    /// these lines rather than showing a transcript that lost what the person typed.
    fn queue_state(&self) -> Vec<crate::steer::View> {
        Vec::new()
    }

    /// Change what the next mid-turn press promises. Queued, because this loop is the one that
    /// writes the config file — the same reason [`Self::set_web`] is queued.
    fn set_mid_turn(&self, kind: crate::steer::MidTurn) {
        self.send(HostCmd::SetMidTurn { kind })
    }

    /// Change the **stored** palette, in the config file every surface reads at start.
    ///
    /// Deliberately not the way a window repaints itself: a per-window override is a client fact and
    /// never reaches the host, because a phone tab that could rewrite `theme` would change the
    /// desktop's colours. The screen that sets an override says the word *override*
    /// (`crate::theme::override_says`), and the About line names both homes.
    fn set_theme(&self, kind: crate::theme::Theme) {
        self.send(HostCmd::SetTheme { kind })
    }

    /// Tell this core that the third-party servers changed, so what the model is told about them and
    /// what it can be offered follow the file.
    ///
    /// Default is a no-op, and the reason is stated rather than implied: on the pi core the tool host
    /// reads the hub's catalog per call and the catalog block is re-read per request, so there is
    /// nothing stale to push. The Rust core builds the prompt's catalog from a value it stored at
    /// boot, so it has to be told.
    fn mcp_refresh(&self, _hub: Arc<tokio::sync::Mutex<crate::mcp::Hub>>) {}

    /// The About sentence for **this** client: the host's stored palette plus whatever the window is
    /// overriding it with. Asked of the host so the wording is the engine's on every surface, and
    /// read-only by design — the override is passed in for the sentence and never stored, because a
    /// phone tab that could write `theme` would repaint the desktop.
    fn theme_words(&self, window: Option<crate::theme::Theme>) -> String {
        crate::theme::Theme::about_says(self.state().theme, window)
    }

    /// Snapshot for status lines and the composer's current-state labels.
    fn state(&self) -> HostState;

    /// Which engine is answering, in one word: `rust` for the in-process agent, `pi` for a
    /// `pi --mode rpc` child.
    ///
    /// Asked of the host rather than read from `CWOFFEE_CORE` by whoever is rendering a page, because
    /// the environment is how the core is *chosen*, not evidence of what is running: a gateway started
    /// standalone, a desktop launched by a wrapper script, and a test harness all set that variable
    /// differently, and an About page that guessed wrong is a bug report filed against the wrong core.
    fn core_id(&self) -> &'static str {
        "rust"
    }

    /// Where this core keeps **the model's** copy of a conversation, for the About page.
    ///
    /// Empty means "there is no second copy: the record above is the whole of it", which is true of
    /// the Rust core — its history is rebuilt from the SQLite rows. The pi core overrides this,
    /// because on that core the model genuinely does keep its own thread files, in a directory this
    /// app owns and the person cannot find from the settings screen. A page that says "your
    /// conversations live in one file" to a pi-core user is a page that tells them where *not* to
    /// look.
    fn thread_store(&self) -> String {
        String::new()
    }

    /// Move the tools to another workspace, refusing anything the checks reject.
    async fn set_workspace_checked(&self, path: &str) -> Result<String, String>;

    /// Cowork or chat. The default refuses in words, because the shells all render a
    /// refusal, and "unavailable" without a reason reads like a broken button.
    /// The tool list, for the settings screen. Both cores answer from the same Rust
    /// registry: on the pi core the tools are mounted from it, so the list a user reads
    /// and the tools the model is offered cannot be two different things.
    fn tools_view(&self) -> Vec<ToolView> {
        tool_views()
    }

    /// Fold this conversation down to a summary, on request. The default refuses in
    /// words: a core that folds the prompt when it reaches its own budget has no manual
    /// lever, and a button that does nothing is worse than no button.
    async fn compact(&self, _instructions: Option<String>) -> Result<String, String> {
        Err("this core folds the prompt when it reaches its own budget; there is no manual compaction to ask for".into())
    }

    fn set_persona(&self, persona: Persona) -> Result<String, String> {
        Err(format!(
            "this core keeps the coworker prompt and the tool set; chat mode is wired on the pi core only ({})",
            persona.as_str()
        ))
    }

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

    /// Pin or free the context ceiling. Both cores write the same config file and then
    /// apply what they just read back, so the meter, the fold and the next start all
    /// agree; a ceiling that only lives in RAM is a setting that resets on restart.
    ///
    /// The answer is the sentence the app shows: what the ceiling is now, what it is a
    /// share of, and which part takes effect when.
    async fn set_context_budget(
        &self,
        _cfg_path: &Path,
        _target: Option<usize>,
    ) -> Result<String, String> {
        Err("this core cannot change its context ceiling".to_string())
    }

    // ---- providers and models, as the settings screen sees them ----------------------
    //
    // Every one of these takes the config file the *shell* was started with, for the same reason
    // `set_context_budget` and `search_view` do. A verb that reads `~/.config/cwoffee/config.toml`
    // regardless is a verb that writes one file and reads another: the gateway's own test suite
    // wrote a model row into a developer's real settings file, because the handle believed its
    // `/tmp` path and the writer never looked.

    /// Every endpoint the app knows, the models filed under each, and which one is answering.
    ///
    /// File-based on purpose: this must answer without touching the network, because a settings
    /// screen that goes blank when the box is offline tells you nothing about why.
    fn provider_status(&self, cfg_path: &Path) -> crate::config::ProviderStatus {
        let (cfg, _) = crate::config::Config::load(Some(cfg_path)).unwrap_or_default();
        crate::config::provider_status_with(cfg_path, &cfg)
    }

    /// Ask one endpoint whether it is there, registering nothing. The answer is a sentence,
    /// because that is what a settings row has room for.
    async fn provider_test(&self, _cfg_path: &Path, req: &crate::config::ProviderReq) -> String {
        crate::config::probe_endpoint(req).await
    }

    /// Ask one endpoint what its models can do: window, tool calling, images, thinking levels.
    /// An unreachable endpoint is refused in words; returning an empty list would read as "this
    /// endpoint has nothing on it", which is a different claim entirely.
    async fn provider_probe(
        &self,
        _cfg_path: &Path,
        req: &crate::config::ProviderReq,
    ) -> Result<Vec<crate::config::ModelSpec>, String> {
        crate::config::probe_models(req).await
    }

    /// Register an endpoint and the models on it in one action. The per-model rows written
    /// alongside are what make the window, the context ceiling and pi's fold agree from then on.
    fn provider_add(
        &self,
        cfg_path: &Path,
        req: &crate::config::ProviderReq,
        caps: &[crate::config::ModelSpec],
    ) -> String {
        crate::config::add_provider_with(cfg_path, req, caps)
    }

    fn provider_remove(&self, cfg_path: &Path, name: &str) -> String {
        crate::config::remove_provider_with(cfg_path, name)
    }

    /// One model's window, sampling and thinking levels, written to the file that is the source
    /// of truth. A model process already running keeps what it started with; the next one picks
    /// this up. The sentence says that, because the meter moving is not the model re-reading.
    fn model_set(&self, cfg_path: &Path, id: &str, row: &crate::config::ModelRow) -> String {
        crate::config::set_model_with(cfg_path, id, row)
    }

    fn model_forget(&self, cfg_path: &Path, id: &str) -> String {
        crate::config::forget_model_with(cfg_path, id)
    }
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
    /// Ask this host's own loop to make a branch and wait for the conversation it made.
    ///
    /// Waiting is the point: the caller has to open the new row, and a fire-and-forget verb would
    /// leave a client watching for a conversation whose id it was never given. The loop is a task of
    /// its own, so awaiting here does not stall it — and on a single-threaded runtime the await is
    /// what lets the loop run at all.
    async fn branch(&self, req: &crate::branch::BranchReq) -> Result<crate::branch::BranchOutcome, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(HostCmd::Branch { req: req.clone(), reply: tx })
            .map_err(|_| "the model host is gone; nothing was branched".to_string())?;
        rx.await.map_err(|_| "the model host ended without answering the branch".to_string())?
    }
    async fn move_leaf(&self, session: i64, target: &crate::tree::LeafTarget) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(HostCmd::MoveLeaf { session, target: target.clone(), reply: tx })
            .map_err(|_| "the model host is gone; the leaf was not moved".to_string())?;
        rx.await.map_err(|_| "the model host ended without answering the leaf move".to_string())?
    }
    async fn tool_inventory(&self) -> Result<(Vec<String>, Vec<String>), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(HostCmd::ToolInventory { reply: tx })
            .map_err(|_| "the model host is gone; nothing was read".to_string())?;
        rx.await.map_err(|_| "the model host ended without answering".to_string())?
    }
    fn state(&self) -> HostState {
        SessionHost::state(self)
    }
    /// Queue a mid-turn press. Straight into the shared queue, not into the mailbox: this host's
    /// loop is parked inside `run_turn` for the whole turn, so a command sent now would be read
    /// after the turn — which is the behaviour steering replaces. See [`crate::steer`].
    fn queue_mid_turn(
        &self,
        text: String,
        attachments: Vec<crate::provider::Attachment>,
        session: Option<i64>,
    ) -> Result<crate::steer::View, String> {
        let st = self.state();
        if text.trim().is_empty() && attachments.is_empty() {
            return Err("nothing was typed, so nothing was queued".to_string());
        }
        crate::steer::checked_attachments(&attachments, self.limits)?;
        if !st.busy {
            return Err(crate::steer::idle_refusal());
        }
        let here = st.session_id;
        // The same address rule `Send` runs, and the same refusal: a mismatch is refused rather than
        // rerouted. `Some(0)` is "I am on a draft", which mid-turn cannot be true of the
        // conversation this host is answering — moving the host is not a steering act.
        if let Some(want) = session {
            if want != here {
                return Err(crate::steer::wrong_session_refusal(want, here));
            }
        }
        let item = self
            .queue
            .push(here, st.mid_turn, text, attachments, crate::steer::now_secs());
        let view = item.view();
        self.announce(EngineEvent::QueueChanged { session: here, items: self.queue.snapshot() });
        Ok(view)
    }

    fn withdraw_queued(&self, id: u64) -> Result<crate::steer::View, String> {
        let st = self.state();
        match self.queue.withdraw(id) {
            Some(item) => {
                let view = item.view();
                self.announce(EngineEvent::QueueChanged { session: st.session_id, items: self.queue.snapshot() });
                Ok(view)
            }
            None => Err(crate::steer::withdraw_gone_refusal()),
        }
    }

    fn queue_state(&self) -> Vec<crate::steer::View> {
        self.state().queued
    }

    /// Write the choice to the file and to the live state, on the spot.
    ///
    /// Not queued behind the turn: the composer announces this value, and a dial that keeps saying
    /// the old promise until the answer finishes reads as a control that did nothing. The already-
    /// queued messages keep the kind they were typed as, which is what the settings note says.
    fn set_theme(&self, kind: crate::theme::Theme) {
        // Answered immediately as well as queued: the settings row shows what is stored now, and a
        // row that kept its old value until the loop got round would read as a control that did
        // nothing — the complaint `theme` arrived with.
        let mut st = self.state.lock().unwrap();
        st.theme = kind;
        st.theme_note = kind.setting_note();
        st.theme_about = crate::theme::Theme::about_says(kind, None);
        persist_setting(&self.cfg_path, "theme", &crate::Config::toml_str(kind.word()));
        self.send(HostCmd::SetTheme { kind });
    }

    fn mcp_refresh(&self, hub: Arc<tokio::sync::Mutex<crate::mcp::Hub>>) {
        // Queued rather than answered here: the registry belongs to the agent this loop owns, and
        // answering from outside it is how a second, invented registry gets built — the fault
        // `tool_inventory` was moved off of for the same reason.
        self.send(HostCmd::McpRefresh { hub });
    }
    fn set_mid_turn(&self, kind: crate::steer::MidTurn) {
        let mut st = self.state.lock().unwrap();
        st.mid_turn = kind;
        st.mid_turn_hint = crate::steer::composer_hint(kind);
        st.mid_turn_note = crate::steer::setting_note(kind);
        st.mid_turn_refusal = crate::steer::card_refusal();
        persist_setting(&self.cfg_path, "mid_turn", &crate::Config::toml_str(kind.word()));
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

    async fn set_context_budget(
        &self,
        cfg_path: &Path,
        target: Option<usize>,
    ) -> Result<String, String> {
        crate::settings::set_context_budget(self, cfg_path, target).await
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
/// What the model is told about where it is working.
///
/// The `workspace:` line is the answer the model gives when somebody asks it "where are you
/// working?", so it has to be phrased the way the app phrases it. A conversation parked in
/// the system's temp folder has no workspace in any sense the user would recognise: nobody
/// chose it, nothing of theirs is in it, and it is gone on the next reboot. Reciting
/// `/tmp/...` made it sound as if a folder had been picked, which is how the report reads:
/// "the backend workspace is /tmp; the UI should say No workspace and the model should say
/// none." So the path is not printed for those conversations - the tools resolve relative
/// paths against it themselves, so naming it only invited the model to quote it back - and
/// what is true instead is said in words: no workspace, a scratch folder that is not the
/// user's, writable.
pub fn default_env_block(workspace: &std::path::Path, os: &str, egress: crate::tools::net::NetMode) -> String {
    // One line, three truths. It said "no network" whatever the config said, which
    // is a second place (after the system prompt) where the model was told it was
    // offline by an app whose default mode routes package installs through a proxy.
    let egress_line = match egress {
        crate::tools::net::NetMode::Off => "no network",
        crate::tools::net::NetMode::Proxy => "network via an allowlist proxy",
        crate::tools::net::NetMode::Open => "network open",
    };
    let scratch = is_temp_root(&workspace.to_string_lossy());
    let (ws_line, writable) = if scratch {
        (
            "workspace: none (this conversation has no workspace: the tools work in a scratch folder that the user did not choose and does not keep)".to_string(),
            "only the scratch folder",
        )
    } else {
        (format!("workspace: {}", workspace.display()), "only the workspace")
    };
    format!(
        "<env>\nos: {os}\n{ws_line}\nshell: sh\n\
         commands run sandboxed: {egress_line}, {writable} is writable\n\
         </env>",
    )
}

/// `default_env_block` with the MCP catalog folded in, before the closing tag.
///
/// A separate function rather than a parameter on `default_env_block`, because the bridge and
/// every caller that has no servers keeps the three-argument form — and a signature that makes
/// everyone pass an empty string to mean "no third-party tools" is how a feature that is not
/// running still shows up in the prompt.
pub fn env_block_with_mcp(
    workspace: &std::path::Path,
    os: &str,
    egress: crate::tools::net::NetMode,
    catalog: &str,
) -> String {
    let base = default_env_block(workspace, os, egress);
    if catalog.trim().is_empty() {
        return base;
    }
    match base.rfind("</env>") {
        Some(i) => format!("{}\n{}\n</env>", &base[..i], catalog.trim_end()),
        None => format!("{base}\n{catalog}"),
    }
}

/// The env block with the things about *this* start that are not fixed: the third-party tool
/// catalog, and — while one is open — the browser sitting in the person's sidebar.
///
/// The browser line belongs in the env block rather than the system prompt for the reason
/// everything volatile does: the system prompt is written once and must stay true for the whole
/// session, while "is a browser open?" changes between turns and even between calls. A model
/// that is never told cannot use the page the person is looking at, and a prompt line claiming
/// a browser when none is open is the other half of the same mistake.
///
/// The line states four things the model would otherwise guess: where the page is, who opened
/// it, that acting in it is an explicit choice (`session: true`), and that it does not last.
pub fn env_block_live(
    workspace: &std::path::Path,
    os: &str,
    egress: crate::tools::net::NetMode,
    catalog: &str,
    session: Option<&crate::browser_hub::Snapshot>,
) -> String {
    let base = env_block_with_mcp(workspace, os, egress, catalog);
    let Some(s) = session.filter(|s| s.alive && s.opened) else {
        return base;
    };
    let line = format!(
        "a browser is open at {} — {} opened it, last touched by {}; `browse` with \"session\": \
         true acts on that page instead of opening another browser, and the person watching the \
         sidebar sees it. It is not permanent: it closes itself after {} of nobody touching it.",
        s.url,
        s.opened_by.words(),
        s.last_actor.words(),
        crate::browser_hub::idle_words(s.idle_secs),
    );
    match base.rfind("</env>") {
        Some(i) => format!("{}\n{line}\n</env>", &base[..i]),
        None => format!("{base}\n{line}"),
    }
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
pub(crate) mod tests {
    /// The dial's steps have to be the levels this model answers for.
    ///
    /// Three claims, deliberately kept apart here: what this endpoint was **probed** to accept, what
    /// the **dialect** documents, and what a dial may therefore honestly offer. The bug this pins is
    /// a UI offering steps a model cannot run - a control that moves and a wire that says nothing.
    #[test]
    fn the_levels_offered_are_the_levels_this_model_answers_for() {
        let dir = std::env::temp_dir().join(format!("cw-dial-levels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Probed: this endpoint named two efforts, so only those two are on the dial. "high" is
        // absent because nothing here said the server takes it.
        crate::config::set_model_row_in(
            &path,
            "probed/two",
            &crate::config::ModelRow {
                efforts: vec!["low".into(), "medium".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dial_levels(Some(&path), "probed/two"), ["off", "low", "medium"]);

        // Not probed: the dialect's documented values. An unprobed model is not a model that cannot
        // think, and reporting nothing would say exactly that.
        let all = dial_levels(Some(&path), "never/probed");
        assert_eq!(all, ["off", "low", "medium", "high", "xhigh"], "a llama.cpp-shaped dialect by default");

        // The fold is the template's, not a universal: an OpenAI-shaped endpoint spells high and
        // xhigh apart, so an endpoint that named only xhigh is reachable by the top step alone.
        crate::config::set_model_row_in(
            &path,
            "oa/xhigh",
            &crate::config::ModelRow {
                efforts: vec!["low".into(), "xhigh".into()],
                ..Default::default()
            },
        )
        .unwrap();
        crate::config::Config::set_provider_keys(&path, &[("kind", "\"openai\"")]).unwrap();
        assert_eq!(dial_levels(Some(&path), "oa/xhigh"), ["off", "low", "xhigh"]);
        // And the same file read for a model nobody probed gives the OpenAI dialect's documented
        // values, which do not include xhigh.
        assert_eq!(dial_levels(Some(&path), "oa/never-probed"), ["off", "low", "medium", "high"]);

        // A model nobody has any information about still gets the honest answer: off is always
        // available, and the list is never empty.
        assert!(!dial_levels(None, "nope").is_empty(), "no config file is not the same as no thinking");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window the person typed beats the window the endpoint claims, and only when there
    /// is one. A default that quietly preferred the endpoint would make the setting in the
    /// model screen decorative.
    #[test]
    fn a_stored_window_beats_the_endpoint_and_says_so() {
        assert_eq!(chosen_window(Some(32_768), Some(131_072)), Some(32_768));
        assert_eq!(chosen_window(None, Some(131_072)), Some(131_072));
        assert_eq!(chosen_window(Some(32_768), None), Some(32_768));
        assert_eq!(chosen_window(None, None), None, "nothing known is still nothing known");
        // The budget formula then sees the smaller window, which is the point: the fold
    // happens where the server will actually run out.
        assert!(auto_budget(32_768) < auto_budget(131_072));
    }


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
        /// How many messages each request carried. What a conversation's own prompt was
        /// seeded with is otherwise invisible from the outside, and "a branch is a
        /// conversation the model has read" is exactly the claim that needs seeing.
        seen_prompt: std::sync::Mutex<Vec<usize>>,
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
            // A conversation names itself before its first answer (`crate::titling`).
            // That ask belongs to no conversation, so here it is not drawn from
            // `replies`, not counted in `calls`, and not recorded in `seen_wire`: a
            // scripted conversation must not have to budget a round for its own name,
            // and a test that audits what the conversation sent must not find the title
            // prompt in the audit. It gets a name, and the fixture moves on.
            if req.messages.len() == 1
                && req.messages[0].role == crate::provider::Role::User
                && req.messages[0].content.starts_with(crate::titling::TITLE_ASK)
            {
                // A fixture that wants to see the fallback path says so in its own
                // first message; the endpoint then answers the ask with nothing.
                if req.messages[0].content.contains("no title please") {
                    sink.send(Delta::Done(FinishReason::Stop))
                        .map_err(|_| ProviderError::Cancelled)?;
                    return Ok(());
                }
                sink.send(Delta::Text("a scripted name".into()))
                    .map_err(|_| ProviderError::Cancelled)?;
                sink.send(Delta::Done(FinishReason::Stop))
                    .map_err(|_| ProviderError::Cancelled)?;
                return Ok(());
            }
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
            self.seen_prompt.lock().unwrap().push(req.messages.len());
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
        let (h, store, _p) = host_with(replies);
        (h, store)
    }

    /// The same host, with the endpoint kept in hand. A branch is not only a copied
    /// transcript: the model has to be answering out of the copy, and that is only
    /// checkable by asking the endpoint what it was given.
    /// A host for another module's tests, without the endpoint handle: `Scripted` is a
    /// private type, and a caller that only needs to ask a host whether it is busy should
    /// not have to see the fixture's provider.
    #[cfg(test)]
    pub(crate) fn host_for_test(replies: Vec<String>) -> SessionHost {
        let (h, _store, _p) = host_with(replies);
        h
    }

    /// The same host, with the record it reads from.
    ///
    /// `crate::tree` tests need both halves: the tree is the conversation's rows read next to what
    /// the host's memory says, and a test that could only see one of them cannot check the sentence
    /// the two are supposed to produce together.
    #[cfg(test)]
    pub(crate) fn host_for_test_with_store(replies: Vec<String>) -> (SessionHost, Arc<Store>) {
        let (h, store, _p) = host_with(replies);
        (h, store)
    }

    fn host_with(replies: Vec<String>) -> (SessionHost, Arc<Store>, Arc<Scripted>) {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(replies),
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
            seen_prompt: Default::default(),
        });
        let h = SessionHost::spawn(
            p.clone(),
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        (h, store, p)
    }

    /// `theme` used to be a key the parser accepted and nobody read: the settings screen wrote it,
    /// the file held it, and the palette lived in one browser's `localStorage`. This is the claim
    /// that the value reaches the payload a surface actually reads — a config test alone cannot tell
    /// "parsed" from "published", which is exactly how the old key looked green while doing nothing.
    #[tokio::test]
    async fn the_stored_theme_reaches_the_payload_the_ui_reads() {
        let dir = std::env::temp_dir().join(format!("cwoffee_theme_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            // `theme` is top-level, so it goes before the first `[table]` header — the same trap
            // `table_hint` exists to explain, and a test that wrote it inside `[provider]` would be
            // testing the parser's error instead of the setting.
            "theme = \"dark\"\n[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let (appcfg, loaded) = Config::load(Some(&path)).unwrap();
        assert_eq!(loaded.as_deref(), Some(path.as_path()), "the file was not the one read");
        assert_eq!(appcfg.theme(), crate::theme::Theme::Dark);

        let mut cfg = HostConfig::new("test-model", std::env::temp_dir());
        cfg.theme = appcfg.theme();
        cfg.cfg_path = Some(path.clone());
        let p = Arc::new(Scripted {
            replies: std::sync::Mutex::new(vec![]),
            window: None,
            window_from_call: 0,
            asks: std::sync::atomic::AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            seen_attachments: Arc::new(AtomicUsize::new(0)),
            seen_wire: Default::default(),
            seen_prompt: Default::default(),
        });
        let h = SessionHost::spawn(p, Registry::new(), Arc::new(Store::in_memory().unwrap()), cfg);
        let st = h.state();
        assert_eq!(st.theme, crate::theme::Theme::Dark);
        assert!(st.theme_note.contains("theme = \"dark\""), "{}", st.theme_note);

        // The payload, spelled the way the client reads it: a field the UI cannot deserialize is a
        // field that died in the config file.
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["theme"].as_str(), Some("dark"), "{v}");
        assert!(v["theme_note"].as_str().unwrap().contains("config file"));
        assert!(v["theme_about"].as_str().unwrap().contains("no override"));

        // And the way back: a change answers on the state and lands in the file.
        h.set_theme(crate::theme::Theme::Light);
        let after = h.state();
        assert_eq!(after.theme, crate::theme::Theme::Light);
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("theme = \"light\""), "{file}");
        let (reread, _) = Config::load(Some(&path)).unwrap();
        assert_eq!(reread.theme(), crate::theme::Theme::Light, "the next start would not see the change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A word that is not a palette is refused in words, not defaulted. `theme_loud` and
    /// `theme_aggressive` are retired keys and stay refused.
    #[test]
    fn a_theme_word_that_is_not_one_is_refused_in_words() {
        let mut c = Config::default();
        c.theme = Some("midnight".to_string());
        let e = c.theme_checked().unwrap_err();
        assert!(e.contains("theme = \"midnight\""), "{e}");
        assert!(e.contains("light, dark or system"), "{e}");
        for word in ["light", "dark", "system", "follow"] {
            c.theme = Some(word.to_string());
            assert!(c.theme_checked().is_ok(), "{word}");
        }
        c.theme = Some("theme_loud".to_string());
        assert!(c.theme_checked().is_err());
        c.theme = None;
        assert_eq!(c.theme(), crate::theme::Theme::System, "absent means follow");
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
            seen_prompt: Default::default(),
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
        assert_eq!(auto_budget(262_144), 131_072, "half the window: the other half is the reply");
        assert_eq!(auto_budget(8_192), 4_096, "half, floored - not a percentage minus a constant");
        assert_eq!(auto_budget(4_096), 4_096, "a tiny window cannot go below the floor");
        assert_eq!(auto_budget(0), 4_096);
    }

    #[test]
    fn the_chat_prompt_says_what_the_mode_cannot_do() {
        // On the pi core this string is the *whole* instruction: `--system-prompt` replaces
        // the runtime's default prompt, and with `--no-tools` nothing else reaches the model.
        // pi does append its own <cwd> block naming the workspace folder, so a chat prompt
        // that stays silent about its own limits leaves the model reading one claim from the
        // prompt and the opposite one from the block underneath it.
        let p = Persona::CHAT_PROMPT;
        assert!(p.contains("no tools"), "chat must say it has no tools: {p}");
        assert!(p.contains("no folder"), "chat must say it has no folder to work in: {p}");
        assert!(p.contains("never offer"), "chat must not leave the model offering to act: {p}");
        assert!(p.len() < 400, "the whole chat instruction stays small: {} B", p.len());
    }

    #[test]
    fn a_ceiling_that_cannot_mean_anything_is_refused_in_words() {
        // Too small to hold a conversation: the fold would fire before the tools did.
        let e = check_budget(900, Some(262_144)).unwrap_err();
        assert!(
            e.contains("below the floor of 4096") && e.contains("Leave the ceiling unset"),
            "{e}"
        );
        // Not below the window: the endpoint would truncate first and the meter could
        // never fill, which is a gauge drawn for decoration.
        let e = check_budget(262_144, Some(262_144)).unwrap_err();
        assert!(
            e.contains("not below this endpoint's stated window") && e.contains("262144"),
            "{e}"
        );
        // An endpoint that has not stated a window is not a licence to write any number.
        assert!(check_budget(900, None).is_err(), "the floor does not depend on the window");
        assert!(check_budget(8_000, Some(262_144)).is_ok());
        // An early fold is legitimate, and says so rather than leaving the number
        // unexplained next to a meter that fills up far too soon.
        assert!(budget_share(8_000, Some(262_144)).contains("folds early on purpose"));
        assert!(budget_share(131_072, Some(262_144)).contains("50%"));
        assert!(budget_share(131_072, None).contains("has not stated a window"));
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
            seen_prompt: Default::default(),
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
    async fn the_browsers_mode_follows_the_config_row_and_not_the_boot_snapshot() {
        use crate::tools::browser::Vision;
        // Whether `browse` paints a window on a person's desktop is decided from the config file's
        // model row. Freezing that at spawn was wrong twice over in ordinary use: a probe that
        // records modalities after boot, and a model change in the middle of a conversation. Both
        // are graded here — the first by writing the row after the host is up, the second by
        // switching to a model whose row says something different.
        let dir = std::env::temp_dir().join(format!("cwoffee_browsemode_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\n").unwrap();

        let mut cfg = HostConfig::new("blind-model", dir.join("ws"));
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
                seen_prompt: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        assert_eq!(
            h.state().browser_vision,
            Vision::Unknown,
            "no row at boot, so nobody has established what the model can see"
        );

        // The model screen probes, and the row lands. Nothing else tells the host.
        std::fs::write(
            &path,
            "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\n\n[models.\"blind-model\"]\nsupports_vision = true\n\n[models.\"text-only\"]\nsupports_vision = false\n",
        )
        .unwrap();
        h.send(HostCmd::Send {
            text: "one".into(),
            attachments: vec![],
            session: None,
        });
        wait_for("the turn ran".into(), || h.state().turns >= 1).await;
        assert_eq!(
            h.state().browser_vision,
            Vision::Yes,
            "the row written after boot has to reach the tool that runs"
        );

        // And a model change is a change to the answer, not to a remembered value.
        h.send(HostCmd::SetModel { model: "text-only".into(), persist: false });
        h.send(HostCmd::Send {
            text: "two".into(),
            attachments: vec![],
            session: None,
        });
        wait_for("the second turn ran".into(), || h.state().turns >= 2).await;
        assert_eq!(
            h.state().browser_vision,
            Vision::No,
            "a model recorded as text-only gets no window and no pixels"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            seen_prompt: Default::default(),
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
            seen_prompt: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        ready(&h).await;
        h.send(HostCmd::SetReasoning { reasoning: Reasoning::High, persist: true });
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

    /// The thinking level, scoped the way the report asks for it: the settings screen owns a
    /// default, a new conversation starts at that default, and a composer pick overrides it for
    /// that conversation alone - for the whole conversation, until it is moved again.
    ///
    /// The complaint: "when I try to pick another thinking level in a new session's composer, after
    /// I send the message, it uses the default again instead of my pick". The moment is not a
    /// coincidence. On both cores the conversation's row - and on the pi core its pi thread - is
    /// created by the *first send*, and a fresh pi thread seeds itself from pi's own default. The
    /// level had been applied to a session that the send then replaced. So the assertion that
    /// matters is the one taken after a send, and the one after that.
    #[tokio::test]
    async fn a_composers_thinking_pick_outlives_the_send_that_creates_the_conversation() {
        let dir = std::env::temp_dir().join(format!("cwoffee_think_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "reasoning = \"low\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let mut cfg = HostConfig::new("m", dir.join("ws"));
        cfg.cfg_path = Some(path.clone());
        // What a shell puts in this field is the value it read out of the file, so the file and the
        // field agreeing is part of the premise, not a convenience.
        cfg.reasoning = Reasoning::Low;
        let h = SessionHost::spawn(
            Arc::new(Scripted {
                replies: std::sync::Mutex::new(vec!["ok".into(), "ok".into()]),
                window: None,
                window_from_call: 0,
                asks: std::sync::atomic::AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                seen_attachments: Arc::new(AtomicUsize::new(0)),
                seen_wire: Default::default(),
            seen_prompt: Default::default(),
            }),
            Registry::new(),
            Arc::new(Store::in_memory().unwrap()),
            cfg,
        );
        ready(&h).await;

        // 1. a conversation nobody touched starts at the default and claims no override.
        assert_eq!(h.state().reasoning, Reasoning::Low, "the settings default did not reach the row");
        assert_eq!(h.state().reasoning_default, Reasoning::Low);
        assert!(!h.state().reasoning_overridden, "an untouched conversation claims its own override");

        // 2. the composer's dial, moved while the conversation is still a draft (session 0), which
        //    is exactly when a person reaches for it.
        h.send(HostCmd::SetReasoning { reasoning: Reasoning::XHigh, persist: false });
        wait_for("the composer's pick to land".into(), || h.state().reasoning == Reasoning::XHigh).await;
        assert!(h.state().reasoning_overridden, "a composer pick is an override, not a new default");
        assert_eq!(h.state().reasoning_default, Reasoning::Low, "the composer pick moved the default");

        // 3. and it must not write the settings file. This is the other half of the same bug: if a
        //    per-chat dial moved the default, one experiment in one chat would quietly change every
        //    conversation started afterwards.
        let file = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(file.contains("reasoning = \"low\""), "the file lost the default: {file}");
        assert!(!file.to_lowercase().contains("xhigh"), "a composer pick was persisted: {file}");

        // 4. send: the step that used to undo the dial, because it is where a draft becomes a row.
        h.submit("first");
        turns_done(&h, 1).await;
        assert_eq!(
            h.state().reasoning,
            Reasoning::XHigh,
            "the send reverted the pick to the default - this is the reported bug"
        );
        assert!(h.state().reasoning_overridden, "after the send the row no longer reports its own override");
        assert_eq!(h.state().reasoning_default, Reasoning::Low, "after the send the default reads as the pick");
        let file = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(file.contains("reasoning = \"low\""), "sending persisted a per-chat pick: {file}");

        // 5. a second turn in the same conversation keeps it: the override is for the conversation,
        //    not for the one message that happened to be typed while it was set.
        h.submit("second");
        turns_done(&h, 2).await;
        assert_eq!(h.state().reasoning, Reasoning::XHigh, "the pick did not last the whole conversation");

        // 6. a different conversation is untouched by all of it. `turns` counts every turn the host
        //    has ever run, so it is not the signal for "the new row is live"; `session_id` is.
        h.send(HostCmd::NewSession { title: "elsewhere".into() });
        wait_for(
            "a new conversation starting at the default".into(),
            || h.state().session_id != 0 && h.state().reasoning == Reasoning::Low,
        )
        .await;
        assert_eq!(h.state().reasoning, Reasoning::Low, "one chat's pick travelled to the next chat");
        assert!(!h.state().reasoning_overridden, "the next chat inherited the previous chat's override");
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
            seen_prompt: Default::default(),
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
            seen_prompt: Default::default(),
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
            seen_prompt: Default::default(),
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
            seen_prompt: Default::default(),
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
            seen_prompt: Default::default(),
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

    /// A conversation names itself before it answers, and the name never enters the
    /// conversation.
    ///
    /// Three claims, all of them user-visible: the row starts as the placeholder; the
    /// model's name arrives *before* the first answer starts, which is the stall the
    /// reader asked for and the reason the sidebar is not sitting on "New Session"
    /// while an answer scrolls past; and the title request lives in its own context,
    /// so the conversation's own history has no trace of it.
    #[tokio::test]
    async fn a_new_conversation_is_named_before_it_is_answered() {
        let (mut h, store) = host(vec!["the answer arrives here".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        h.submit("why is my cashflow sheet off by 300 dollars");
        turns_done(&h, 1).await;

        let sid = h.state().session_id;
        let row = store.session(sid).unwrap().expect("the conversation exists");
        assert_eq!(row.title, "a scripted name", "the titler did not name the conversation");

        let evs = drain_events(&mut ev).await;
        let named = evs
            .iter()
            .position(|e| matches!(e, EngineEvent::TitleDelta { text, .. } if text == "a scripted name"));
        let answered = evs.iter().position(|e| matches!(e, EngineEvent::TurnStart { .. }));
        assert!(named.is_some(), "no TitleDelta reached the front end: {} events", evs.len());
        assert!(answered.is_some(), "the turn never started");
        assert!(
            named.unwrap() < answered.unwrap(),
            "the name arrived after the answer had already started"
        );

        // Separate context: the ask is nowhere in the conversation.
        let hist = store.history(sid).unwrap();
        assert_eq!(hist.len(), 2, "the title request must not become a row: {hist:?}");
        assert_eq!(hist[0].content, "why is my cashflow sheet off by 300 dollars");
        assert!(
            !hist.iter().any(|m| m.content.contains(crate::titling::TITLE_ASK)),
            "a title prompt leaked into the conversation: {hist:?}"
        );
    }

    /// The name is the model's, or it is the opening line — but the row is never
    /// left holding the placeholder after a turn that could not name it.
    ///
    /// The fixture's hook is the phrase `no title please` in the first message: the
    /// scripted endpoint then answers the title ask with nothing at all, which is the
    /// real-world case of a server that accepts the request and produces no usable
    /// name. A sidebar of identical rows is a sidebar that says nothing, so the
    /// opening line takes over - the same rule the store used to enforce by renaming
    /// rows itself, with only the mechanism moved.
    #[tokio::test]
    async fn a_title_that_never_arrives_falls_back_to_the_opening_line() {
        let (mut h, store) = host(vec!["the answer arrives here".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        h.submit("no title please: explain gross versus net margin in one paragraph");
        turns_done(&h, 1).await;
        let sid = h.state().session_id;
        let title = store.session(sid).unwrap().unwrap().title;
        assert_ne!(title, crate::store::UNTITLED, "the placeholder survived a finished turn");
        assert!(
            title.starts_with("no title please"),
            "the fallback is the opening line, got {title:?}"
        );
        drain_events(&mut ev).await;
    }

    /// A conversation that stops existing while a message is on its way must not
    /// receive that message.
    ///
    /// The desktop's delete path moves the host off the row first and only then
    /// removes it — but that move is a queued command. This test is the race: the row
    /// is gone and the host still believes it is standing in it. Answering there
    /// writes rows nothing lists, an answer the reader can never open again.
    #[tokio::test]
    async fn a_deleted_conversation_does_not_take_the_turn_that_raced_it() {
        let (mut h, store) = host(vec!["first answer".into(), "second answer".into()]);
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        ready(&h).await;
        h.submit("first, in a conversation that is about to go");
        turns_done(&h, 1).await;
        let doomed = h.state().session_id;
        assert!(doomed > 0);

        store.delete_session(doomed).unwrap();
        assert!(store.session(doomed).unwrap().is_none());

        h.submit("this message raced the delete");
        turns_done(&h, 2).await;

        let now = h.state().session_id;
        assert_ne!(now, doomed, "the host answered into a conversation that no longer exists");
        let hist = store.history(now).unwrap();
        assert!(
            hist.iter().any(|m| m.content == "this message raced the delete"),
            "the message was dropped instead of starting a conversation: {hist:?}"
        );
        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::Note { message, .. }
                if message.contains("was deleted while your message was on its way"))),
            "the reader was never told where their message went"
        );
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

    /// A branch is a conversation the model answers from, not a transcript that only looks
    /// copied. So this checks the host moved, the record moved, and — through the endpoint's own
    /// record of what it was handed — that the branch's prompt was seeded from the branch's rows.
    #[tokio::test]
    async fn a_branch_moves_the_host_and_answers_from_its_own_history() {
        let (h, store, p) = host_with(vec![
            "first answer".into(),
            "second answer".into(),
            "answered in the branch".into(),
        ]);
        ready(&h).await;
        h.submit("first ask");
        turns_done(&h, 1).await;
        h.submit("second ask");
        turns_done(&h, 2).await;
        let original = h.state().session_id;
        let rows = store.messages(original).unwrap();

        // Branch from the person's second message: everything before it travels, and the message
        // itself comes back as something to ask again rather than being written twice.
        let out = h
            .branch(&crate::branch::BranchReq {
                session: original,
                message_id: Some(rows[2].id),
                from_end: None,
                role: String::new(),
                shown: "second ask".into(),
            })
            .await
            .expect("the branch was asked at a message this window can name");
        assert_eq!(out.copied, 2, "the first exchange travelled into the branch");
        assert_eq!(out.prefill.as_deref(), Some("second ask"), "the ask comes back for the composer");
        assert_eq!(h.state().session_id, out.session, "the host is answering in the branch now");
        assert_eq!(store.messages(original).unwrap().len(), 4, "a branch is not a rewind");

        // What the endpoint is handed proves the adoption. The host installs one message beyond the
        // conversation on every request — the env block naming the folder the tools run in — so the
        // envelope is measured from this test's own first turn rather than guessed at.
        let envelope = *p
            .seen_prompt
            .lock()
            .unwrap()
            .first()
            .expect("the first turn reached the endpoint")
            - 1;
        h.submit("asked again, worded differently");
        turns_done(&h, 3).await;
        let seen = p.seen_prompt.lock().unwrap().last().cloned().unwrap_or(0);
        assert_eq!(
            seen,
            out.copied + envelope + 1,
            "the branch's prompt is its own {} rows plus this message, got {seen}",
            out.copied
        );
        let wire = p.seen_wire.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(wire.contains("asked again, worded differently"), "{wire}");
        assert_eq!(store.messages(original).unwrap().len(), 4, "the original still has its own tail");
        let rows = store.messages(out.session).unwrap();
        assert_eq!(rows.len(), 4, "two copied, one asked again, one answered");
        assert_eq!(rows[2].content, "asked again, worded differently");

        // The event the transcript shows names both conversations, so a person who loses the row
        // in the sidebar can still tell which one is which.
        let note = store.messages(out.session).unwrap();
        assert_eq!(note.len(), 4);
    }

    /// Branching at an answer carries that answer and stops: asked at the newest answer, that is
    /// a copy of the whole conversation, and the original is still whole underneath it.
    #[tokio::test]
    async fn branching_at_an_answer_carries_it_and_leaves_the_original_whole() {
        let (h, store, p) = host_with(vec!["a".into(), "b".into(), "continued".into()]);
        ready(&h).await;
        h.submit("q1");
        turns_done(&h, 1).await;
        h.submit("q2");
        turns_done(&h, 2).await;
        let original = h.state().session_id;
        let rows = store.messages(original).unwrap();

        let out = h
            .branch(&crate::branch::BranchReq {
                session: original,
                message_id: Some(rows[3].id),
                from_end: None,
                role: "assistant".into(),
                shown: rows[3].content.clone(),
            })
            .await
            .expect("branching at the last answer is a copy of everything");
        assert_eq!(out.copied, 4);
        assert!(out.prefill.is_none(), "an answer is not something to ask again");
        assert_eq!(store.messages(original).unwrap().len(), 4);

        h.submit("from here");
        turns_done(&h, 3).await;
        let envelope = *p
            .seen_prompt
            .lock()
            .unwrap()
            .first()
            .expect("the first turn reached the endpoint")
            - 1;
        let seen = p.seen_prompt.lock().unwrap().last().cloned().unwrap_or(0);
        assert_eq!(
            seen,
            out.copied + envelope + 1,
            "the copy holds all {} rows and then the new ask, got {seen}",
            out.copied
        );
    }

    /// A refused branch creates nothing at all — no row, no host move, no note. The proof of which
    /// message was meant is the whole point of the verb, so a refusal must be as clean as a success.
    #[tokio::test]
    async fn a_branch_that_cannot_prove_which_message_it_means_creates_nothing() {
        let (h, store) = host(vec!["a".into(), "b".into()]);
        ready(&h).await;
        h.submit("q1");
        turns_done(&h, 1).await;
        let original = h.state().session_id;
        let before = store.list_sessions(50).unwrap().len();
        let rows = store.messages(original).unwrap();

        let e = h
            .branch(&crate::branch::BranchReq {
                session: original,
                message_id: Some(rows[0].id),
                from_end: None,
                role: String::new(),
                shown: "words this window never saw".into(),
            })
            .await
            .expect_err("the row says something else, so the branch is refused");
        assert!(e.contains("says something else now"), "{e}");
        assert_eq!(store.list_sessions(50).unwrap().len(), before, "no row was created");
        assert_eq!(h.state().session_id, original, "the host stayed where it was");
        assert_eq!(store.messages(original).unwrap().len(), 2, "and nothing was copied or cut");
    }

    /// A phone can branch a conversation the desktop is not working on. The host takes it up first
    /// and then branches it, which is the same adopt-then-act the pi core has to do — a core that
    /// could only branch its own conversation would answer the phone with a refusal the desktop
    /// never gets.
    #[tokio::test]
    async fn a_branch_of_a_conversation_this_host_is_not_on_is_adopted_first() {
        let (h, store) = host(vec!["a".into(), "b".into(), "c".into()]);
        ready(&h).await;
        h.submit("in the first one");
        turns_done(&h, 1).await;
        let first = h.state().session_id;
        h.send(HostCmd::NewSession { title: "second".into() });
        wait_for("a new conversation".into(), || h.state().session_id != first).await;
        h.submit("in the second one");
        turns_done(&h, 2).await;

        let rows = store.messages(first).unwrap();
        let out = h
            .branch(&crate::branch::BranchReq {
                session: first,
                message_id: Some(rows[0].id),
                from_end: None,
                role: String::new(),
                shown: "in the first one".into(),
            })
            .await
            .expect("branching a conversation this host is not holding");
        assert_eq!(h.state().session_id, out.session);
        assert_eq!(store.messages(first).unwrap().len(), 2, "the source is untouched");
        // The branch is filed where its source lives, not wherever the host happened to be.
        assert_eq!(
            store.session(out.session).unwrap().unwrap().workspace,
            store.session(first).unwrap().unwrap().workspace
        );
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
            seen_prompt: Default::default(),
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
    fn the_env_block_names_an_open_browser_and_how_to_act_in_it() {
        let snap = crate::browser_hub::Snapshot {
            alive: true,
            opened: true,
            url: "http://127.0.0.1:8090/selftest-page.html".to_string(),
            opened_by: crate::browser_hub::Who::Person,
            last_actor: crate::browser_hub::Who::Model,
            idle_secs: 300,
            ..Default::default()
        };
        let b = env_block_live(
            std::path::Path::new("/home/u/ws"),
            "linux",
            crate::tools::net::NetMode::Proxy,
            "",
            Some(&snap),
        );
        assert!(b.contains("a browser is open at http://127.0.0.1:8090/selftest-page.html"), "{b}");
        assert!(b.contains("you opened it"), "{b}");
        assert!(b.contains("last touched by the model"), "{b}");
        assert!(b.contains("\"session\": true"), "the line has to say how to use it: {b}");
        assert!(b.contains("closes itself after 5 minutes"), "and that it does not last: {b}");
        // Still one env block, still the last thing before the closing tag.
        assert_eq!(b.matches("<env>").count(), 1, "{b}");
        assert!(b.trim_end().ends_with("</env>"), "{b}");
    }

    #[test]
    fn the_env_block_says_nothing_about_a_browser_that_is_not_open() {
        // A prompt line claiming a browser is the mistake in the other direction: the model
        // passes `session: true`, and the call has to explain a mismatch the prompt invented.
        let closed = crate::browser_hub::Snapshot {
            alive: false,
            why: "nobody touched it for 5 minutes, so it closed itself".to_string(),
            ..Default::default()
        };
        let b = env_block_live(
            std::path::Path::new("/home/u/ws"),
            "linux",
            crate::tools::net::NetMode::Proxy,
            "",
            Some(&closed),
        );
        assert!(!b.contains("a browser is open"), "{b}");
        let none = env_block_live(std::path::Path::new("/home/u/ws"), "linux", crate::tools::net::NetMode::Proxy, "", None);
        assert_eq!(b, none, "a closed session and no session cost the same bytes");
        // And the catalog still arrives when both are present.
        let both = env_block_live(
            std::path::Path::new("/home/u/ws"),
            "linux",
            crate::tools::net::NetMode::Proxy,
            "third-party tools: mcp__fake__write_note",
            Some(&crate::browser_hub::Snapshot { alive: true, opened: true, url: "http://x".into(), idle_secs: 60, ..Default::default() }),
        );
        assert!(both.contains("mcp__fake__write_note") && both.contains("a browser is open"), "{both}");
        assert_eq!(both.matches("</env>").count(), 1, "{both}");
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

    #[test]
    fn a_conversation_parked_in_the_temp_folder_tells_the_model_it_has_none() {
        use crate::tools::net::NetMode as N;
        let b = default_env_block(std::path::Path::new("/tmp"), "linux", N::Proxy);
        assert!(b.contains("workspace: none"), "{b}");
        assert!(b.contains("scratch folder"), "{b}");
        // The path is the part that must not be printed. The tools resolve relative paths
        // against it themselves, so all naming it achieves is a model that quotes `/tmp`
        // back to the person when asked where it is working - which is the report.
        assert!(!b.contains("/tmp"), "the env block recited the scratch path: {b}");
        // A folder *inside* the temp folder was chosen by somebody, so it gets its name.
        let r = default_env_block(std::path::Path::new("/tmp/cwoffee-project"), "linux", N::Proxy);
        assert!(r.contains("workspace: /tmp/cwoffee-project"), "{r}");
        assert!(!r.contains("none"), "{r}");
    }

    // ---- mid-turn steering -------------------------------------------------------------

    /// A turn that is **still answering** when the test presses Enter. `Scripted` finishes inside
    /// one poll, so a mid-turn claim measured against it is measured against an idle host — the
    /// same mistake the page-side composer check made on its first run.
    struct Held {
        reply: String,
        hold: std::time::Duration,
        calls: AtomicUsize,
        seen: std::sync::Mutex<Vec<Vec<crate::provider::Message>>>,
    }

    #[async_trait]
    impl Provider for Held {
        fn id(&self) -> &str {
            "held"
        }
        fn label(&self) -> &str {
            "held"
        }
        fn wants_reasoning(&self) -> bool {
            false
        }
        async fn stream(
            &self,
            req: &crate::provider::ChatRequest,
            sink: &crate::provider::DeltaSink,
        ) -> Result<(), ProviderError> {
            // The naming ask is not the conversation's turn; see `Scripted` for why it is kept out
            // of the audit.
            if req.messages.len() == 1
                && req.messages[0].role == crate::provider::Role::User
                && req.messages[0].content.starts_with(crate::titling::TITLE_ASK)
            {
                sink.send(Delta::Text("a scripted name".into()))
                    .map_err(|_| ProviderError::Cancelled)?;
                sink.send(Delta::Done(FinishReason::Stop))
                    .map_err(|_| ProviderError::Cancelled)?;
                return Ok(());
            }
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            self.seen.lock().unwrap().push(req.messages.clone());
            if n == 0 {
                // The window a person types into.
                tokio::time::sleep(self.hold).await;
            }
            for chunk in self.reply.split(' ') {
                sink.send(Delta::Text(format!("{chunk} ")))
                    .map_err(|_| ProviderError::Cancelled)?;
            }
            sink.send(Delta::Done(FinishReason::Stop))
                .map_err(|_| ProviderError::Cancelled)?;
            Ok(())
        }
        async fn list_models_info(&self) -> Result<Vec<crate::provider::ModelInfo>, ProviderError> {
            Ok(vec![crate::provider::ModelInfo { id: "test-model".into(), ..Default::default() }])
        }
    }

    fn held_host(reply: &str, hold: std::time::Duration) -> (SessionHost, Arc<Store>, Arc<Held>) {
        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Held {
            reply: reply.into(),
            hold,
            calls: AtomicUsize::new(0),
            seen: Default::default(),
        });
        let h = SessionHost::spawn(
            p.clone() as Arc<dyn Provider>,
            Registry::new(),
            Arc::clone(&store),
            HostConfig::new("test-model", std::env::temp_dir()),
        );
        (h, store, p)
    }

    /// A press with nothing running is refused, in words that name the alternative. Queueing a
    /// message nothing will ever drain is how a typed sentence goes missing: pi's queues are only
    /// polled by a run, and the Rust core's boundary only comes round inside one.
    #[tokio::test]
    async fn a_mid_turn_press_with_nothing_running_is_refused_and_says_why() {
        let (h, _store, _p) = held_host("an answer", std::time::Duration::from_millis(50));
        let err = h
            .queue_mid_turn("typed too early".into(), vec![], None)
            .expect_err("idle host must not queue a steer");
        assert!(err.contains("nothing is running"), "{err}");
        assert!(err.contains("send it as a message"), "the refusal must name the alternative: {err}");
        assert!(h.state().queued.is_empty(), "the refusal still queued something");
    }

    /// The address rule `Send` carries, applied to a mid-turn press: a mismatch is refused, not
    /// rerouted, and nothing moves. A steer that could move the host would be a session switch
    /// wearing a message.
    #[tokio::test]
    async fn a_mid_turn_press_naming_another_conversation_is_refused_and_moves_nothing() {
        let (mut h, store, p) = held_host("an answer", std::time::Duration::from_millis(400));
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.send_turn_to("first question", vec![], None);
        wait_for("busy".into(), || h.state().busy).await;
        let here = h.state().session_id;
        let other = store.create_session("elsewhere", &std::env::temp_dir().display().to_string(), "test-model").unwrap();

        let err = h
            .queue_mid_turn("aimed somewhere else".into(), vec![], Some(other))
            .expect_err("a press aimed at another conversation must be refused");
        assert!(err.contains(&format!("conversation {other}")), "{err}");
        assert!(err.contains(&format!("on {here}")), "{err}");
        assert!(err.contains("cannot move the host"), "{err}");
        assert!(h.state().queued.is_empty());
        assert_eq!(h.state().session_id, here, "the refusal moved the host anyway");

        // And a `draft` claim is a mismatch too: mid-turn, the host is on a real conversation.
        let err = h
            .queue_mid_turn("claims a draft".into(), vec![], Some(0))
            .expect_err("a draft claim during a live turn is a mismatch");
        assert!(err.contains("conversation 0"), "{err}");
        turns_done(&h, 1).await;
        assert_eq!(p.seen.lock().unwrap().len(), 1, "a refused press reached the model");
        let _ = drain_events(&mut ev).await;
    }

    /// The press lands: the *next* request carries it, the record grows by exactly one user row,
    /// and the row is the person's own words. This is the claim the selftest's `steer` group makes
    /// against the mock endpoint, graded here against the request the provider was handed.
    #[tokio::test]
    async fn a_steered_turn_carries_the_message_and_grows_by_one_user_row() {
        let (mut h, store, p) = held_host("the answer", std::time::Duration::from_millis(400));
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.send_turn_to("write the note", vec![], None);
        wait_for("busy".into(), || h.state().busy).await;
        let sid = h.state().session_id;

        let view = h
            .queue_mid_turn("make it shorter".into(), vec![], Some(sid))
            .expect("a press during a live turn is queued");
        assert_eq!(view.kind, crate::steer::MidTurn::Steer, "the default promise is a steer");
        assert_eq!(h.state().queued.len(), 1, "the host does not list what is waiting");
        assert_eq!(h.state().queued[0].id, view.id);

        turns_done(&h, 1).await;
        let reqs = p.seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "the steered turn did not make a second request: {}", reqs.len());
        assert!(
            reqs[1].iter().any(|m| m.content.contains("make it shorter")),
            "the model never received the steer"
        );
        assert!(!reqs[0].iter().any(|m| m.content.contains("make it shorter")));

        let rows = store.messages(sid).unwrap();
        let users: Vec<&str> = rows.iter().filter(|r| r.role == "user").map(|r| r.content.as_str()).collect();
        assert_eq!(users.len(), 2, "the steer did not become its own row: {users:?}");
        assert!(users.contains(&"make it shorter"));
        // Exactly one user row more than a turn without a steer would leave: the question, the
        // steer, and two answers — one per request. Anything else means a row was duplicated or
        // the steering wrote the transcript twice.
        let assistants = rows.iter().filter(|r| r.role == "assistant").count();
        assert_eq!(assistants, 2, "the steered turn wrote {assistants} answer rows for two round-trips");
        assert_eq!(
            rows.len(),
            4,
            "the turn holds {} rows: question, answer, steer, answer to the steer — the record \
             grew by exactly the one user row the press added",
            rows.len()
        );
        assert!(h.state().queued.is_empty(), "a landed message is still listed as pending");

        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::SteerLanded { .. })),
            "no landing receipt on the event stream"
        );
        let note = evs.iter().find_map(|e| match e {
            EngineEvent::Note { message, .. } if message.contains("went into this turn") => Some(message.clone()),
            _ => None,
        });
        assert!(note.is_some(), "no receipt sentence: {:?}", evs.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>());
    }

    /// Withdrawal before it lands: the pending line goes, and the words reach neither the model nor
    /// the record. A second withdrawal says so rather than claiming to have taken something back.
    #[tokio::test]
    async fn withdrawing_a_steer_before_it_lands_keeps_it_off_the_wire() {
        let (mut h, store, p) = held_host("the answer", std::time::Duration::from_millis(400));
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.send_turn_to("write the note", vec![], None);
        wait_for("busy".into(), || h.state().busy).await;
        let sid = h.state().session_id;
        let view = h.queue_mid_turn("take me back".into(), vec![], Some(sid)).expect("queued mid-turn");
        assert_eq!(h.state().queued.len(), 1);

        let back = h.withdraw_queued(view.id).expect("the withdrawal took it back");
        assert_eq!(back.text, "take me back");
        assert!(h.state().queued.is_empty(), "the pending line survived the withdrawal");
        assert!(
            h.withdraw_queued(view.id).expect_err("a second withdrawal takes nothing").contains("no longer in the queue"),
            "the second withdrawal should say it removed nothing"
        );

        turns_done(&h, 1).await;
        for req in p.seen.lock().unwrap().iter() {
            assert!(!req.iter().any(|m| m.content.contains("take me back")), "a withdrawn steer reached the model");
        }
        assert!(
            !store.messages(sid).unwrap().iter().any(|r| r.content.contains("take me back")),
            "a withdrawn steer is in the record"
        );
        let evs = drain_events(&mut ev).await;
        assert!(
            !evs.iter().any(|e| matches!(e, EngineEvent::SteerLanded { .. })),
            "the stream reported a landing for a message that was taken back"
        );
    }

    /// Stop gives the words back. This is the half that loses a person's typing if it is designed
    /// badly: a queued message whose turn was stopped has nowhere else to go, and dropping it is a
        /// silent loss with a card on top of it.
    #[tokio::test]
    async fn stopping_a_turn_hands_the_queued_words_back_to_the_composer() {
        let (mut h, store, p) = held_host("the answer", std::time::Duration::from_millis(1200));
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.send_turn_to("write the note", vec![], None);
        wait_for("busy".into(), || h.state().busy).await;
        let sid = h.state().session_id;
        let view = h.queue_mid_turn("words that were never sent".into(), vec![], Some(sid)).expect("queued mid-turn");

        h.cancel();
        turns_done(&h, 1).await;

        let evs = drain_events(&mut ev).await;
        let returned = evs.iter().find_map(|e| match e {
            EngineEvent::QueueReturned { items, .. } if !items.is_empty() => Some(items.clone()),
            _ => None,
        });
        let items = returned.expect("no QueueReturned for a turn stopped with a message waiting");
        assert_eq!(items[0].id, view.id);
        assert_eq!(items[0].text, "words that were never sent");
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::Note { message, .. } if message.contains("back in the composer"))),
            "the transcript never said where the words went"
        );
        assert!(h.state().queued.is_empty(), "the queue still lists a message handed back");
        for req in p.seen.lock().unwrap().iter() {
            assert!(!req.iter().any(|m| m.content.contains("words that were never sent")), "a stopped turn sent it anyway");
        }
        assert!(
            !store.messages(sid).unwrap().iter().any(|r| r.content.contains("words that were never sent")),
            "the stopped turn wrote the words into the record"
        );
    }

    /// The choice is stored, not remembered per window: the composer announces it, and the file is
    /// what the next start reads. A phone tab and the desktop pressing the same Enter have to be
    /// given the same promise.
    #[tokio::test]
    async fn the_mid_turn_choice_reaches_the_file_the_next_start_reads() {
        let dir = std::env::temp_dir().join(format!("cwoffee_midturn_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "mode = \"ask\"\n").unwrap();

        let store = Arc::new(Store::in_memory().unwrap());
        let p = Arc::new(Held {
            reply: "ok".into(),
            hold: std::time::Duration::from_millis(10),
            calls: AtomicUsize::new(0),
            seen: Default::default(),
        });
        let mut cfg = HostConfig::new("test-model", std::env::temp_dir());
        cfg.cfg_path = Some(path.clone());
        let h = SessionHost::spawn(p as Arc<dyn Provider>, Registry::new(), store, cfg);
        assert_eq!(h.state().mid_turn, crate::steer::MidTurn::Steer);

        h.set_mid_turn(crate::steer::MidTurn::FollowUp);
        assert_eq!(h.state().mid_turn, crate::steer::MidTurn::FollowUp, "the live host did not take the choice up");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mid_turn = \"follow_up\""), "the file says: {text}");

        let (again, _) = Config::load(Some(&path)).unwrap();
        assert_eq!(again.mid_turn(), crate::steer::MidTurn::FollowUp, "the next start would not read it back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A follow-up is not a steer: the host's stored choice decides which promise the press makes,
    /// and the announcement the composer quotes has to match it.
    #[tokio::test]
    async fn the_hosts_choice_decides_which_promise_a_press_makes() {
        let (mut h, _store, p) = held_host("the answer", std::time::Duration::from_millis(300));
        let (mut ev, _ap) = h.take_receivers().expect("receivers");
        h.set_mid_turn(crate::steer::MidTurn::FollowUp);
        h.send_turn_to("write the note", vec![], None);
        wait_for("busy".into(), || h.state().busy).await;
        let sid = h.state().session_id;

        let view = h.queue_mid_turn("ask it afterwards".into(), vec![], Some(sid)).expect("queued mid-turn");
        assert_eq!(view.kind, crate::steer::MidTurn::FollowUp);
        assert!(crate::steer::MidTurn::FollowUp.announce().contains("after this turn"));

        turns_done(&h, 1).await;
        let reqs = p.seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "the follow-up was never asked");
        assert!(reqs[1].iter().any(|m| m.content.contains("ask it afterwards")));
        let evs = drain_events(&mut ev).await;
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::SteerLanded { kind: crate::steer::MidTurn::FollowUp, .. })),
            "the receipt did not say it was a follow-up"
        );
        assert!(
            evs.iter().any(|e| matches!(e, EngineEvent::Note { message, .. } if message.contains("asked after this turn"))),
            "a follow-up landed with a steer's sentence"
        );
    }
}
