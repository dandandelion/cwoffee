//! Standalone cwoffee gateway: an engine plus a network listener, no window.
//!
//! Useful headless. Note what it is *not*: it owns its own `SessionHost`, so it
//! is a different conversation from a desktop app that happens to be running.
//! To have a phone mirror the desktop's live turn, run the gateway inside the
//! desktop app with CWOFFEE_GATEWAY_PORT set - that path shares one host.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cwoffee_engine::approval::ApprovalAnswer;
use cwoffee_engine::host::{HostConfig, HostOps, SessionHost};

/// The three streams the fan-out task consumes. Named once because both cores
/// produce exactly these, and that equality is the claim worth reading.
type EventIn = cwoffee_engine::host::EventRx;
type ApprovalIn = tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::host::ApprovalRx>;
type QuestionIn = tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::tools::ask::QuestionRx>;
/// Name of the provider entry written into pi's `models.json` for this gateway.
const PI_PROVIDER: &str = "cwoffee";
use cwoffee_engine::store::{Store, StoreOps};
use cwoffee_engine::tools::Registry;
use cwoffee_engine::Config;
use tokio::sync::{broadcast, oneshot};

fn help() -> &'static str {
    "\
cwoffee-gateway - serve the cwoffee UI and engine over HTTP

  --config PATH      config file (default ~/.config/cwoffee/config.toml, like the app)
  --host ADDR        bind address (default 127.0.0.1; anything else needs --token)
  --port N           port (default 8099)
  --token T          grants watch AND drive; required unless binding to loopback.
                     Unset falls back to CWOFFEE_GATEWAY_TOKEN, then the token file the
                     desktop app writes next to its config — the hand-off at window close
                     relies on that, because a token in argv is readable in `ps`.
  --watch-token T    optional; grants read only (same fallbacks, CWOFFEE_GATEWAY_WATCH_TOKEN)
  --dist PATH        built frontend (default <repo>/ui/dist)
  --url URL          endpoint base, e.g. http://127.0.0.1:8090/v1
  --model ID         model id (empty asks the endpoint)
  --workspace PATH   where tools operate
  --db PATH          session database
  --mode ask|session|auto
  --core rust|pi     engine behind the gateway (default rust, or CWOFFEE_CORE; pi = `pi --mode rpc`)
  --persona cowork|chat
                     cowork = tools + workspace (default). chat = the model with nothing
                     mounted: no tools, no folder, one line of system prompt
  --pi PATH          pi binary for --core pi (default: CWOFFEE_PI, then whatever PATH resolves)
  --pi-version V     refuse a pi that is not exactly this version (default pinned)
  --any-pi           skip the version handshake; development only
  --pi-ext PATH      extension to mount for --core pi (repeatable; this is where
                     the Rust tool shims hang off pi; falls back to CWOFFEE_PI_EXT)
  --pi-tools LIST    pi --tools entries (default -read,-bash,-edit,-write: pi's own
                     coding tools stay off, ours come in as extensions; falls back to
                     CWOFFEE_PI_TOOLS)
  --pi-agent-dir PATH  pi config home (default CWOFFEE_PI_AGENT_DIR, else <state>/cwoffee/pi)
  --pi-session-dir PATH  where pi files its conversations (default CWOFFEE_PI_SESSION_DIR,
                     else <agent dir>/sessions)
  --pi-idle-reap SECS  close an idle pi process after this (default 600)
  -h, --help
"
}

/// A token, in the order the desktop app uses: flag, then environment, then the file written
/// next to the config when someone turned remote access on.
///
/// The fallback is not a convenience. A token in argv is visible to every process on the
/// machine through `ps`, so the hand-off at window close passes none — it starts a gateway with
/// the same environment and lets it read the same file.
fn token_from(flag: Option<String>, env: &str, file: &std::path::Path) -> Option<String> {
    flag.filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var(env).ok().filter(|s| !s.trim().is_empty()))
        .or_else(|| std::fs::read_to_string(file).ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Where the desktop app keeps the gateway token (`gateway_ctl::token_path`). One name, one
/// place: two definitions of "where the secret lives" is how a phone ends up with a gateway the
/// app says nothing about.
fn token_file(name: &str) -> std::path::PathBuf {
    cwoffee_engine::config::Config::default_path()
        .parent()
        .map(|p| p.join(name))
        .unwrap_or_else(|| std::path::PathBuf::from(name))
}

/// A value the desktop app reads from the environment, so a gateway started from the same
/// launcher resolves the same paths it does. Flags still win: an explicit flag is a decision,
/// an inherited variable is context.
fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.trim().is_empty())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", help());
        return Ok(());
    }
    let get = |flag: &str, short: Option<&str>| -> Option<String> {
        for (i, a) in args.iter().enumerate() {
            if a == flag || short.map(|s| a == s).unwrap_or(false) {
                return args.get(i + 1).cloned();
            }
        }
        None
    };
    let many = |flag: &str| -> Vec<String> {
        args.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == flag)
            .filter_map(|(i, _)| args.get(i + 1).cloned())
            .collect()
    };
    let flag = |f: &str| args.iter().any(|a| a == f);

    let bind_ip: IpAddr = get("--host", None)
        .unwrap_or_else(|| "127.0.0.1".into())
        .parse()
        .map_err(|e| anyhow::anyhow!("bad --host: {e}"))?;
    let port: u16 = get("--port", None)
        .unwrap_or_else(|| "8099".into())
        .parse()
        .map_err(|e| anyhow::anyhow!("bad --port: {e}"))?;
    let drive_token = token_from(
        get("--token", None),
        "CWOFFEE_GATEWAY_TOKEN",
        &token_file("gateway.token"),
    );
    let watch_token = token_from(
        get("--watch-token", None),
        "CWOFFEE_GATEWAY_WATCH_TOKEN",
        &token_file("gateway.watch.token"),
    );

    // Reuse the same config file and precedence as the desktop app and the TUI.
    let (mut appcfg, loaded) =
        Config::load(get("--config", None).as_deref().map(std::path::Path::new))
            .map_err(anyhow::Error::msg)?;
    // Same file the desktop panel writes: one config, whichever shell changed it.
    let config_path = loaded.unwrap_or_else(Config::default_path);
    appcfg.apply_env();
    if let Some(u) = get("--url", None) {
        appcfg.provider.base_url = u;
    }
    if let Some(m) = get("--model", None) {
        appcfg.provider.model = m;
    }
    if let Some(w) = get("--workspace", None) {
        appcfg.workspace = Some(w);
    }
    if let Some(pm) = get("--persona", None) {
        appcfg.persona = Some(pm);
    }
    if let Some(m) = get("--mode", None) {
        appcfg.mode = Some(m);
    }

    // Persona is resolved before any core starts, because it decides what the model
    // process IS: which tools exist at all, and what the system prompt says. An
    // unparseable value stops startup rather than quietly handing the user the
    // coworker they did not ask for.
    let persona = match appcfg.persona.as_deref() {
        None => cwoffee_engine::host::Persona::Cowork,
        Some(raw) => cwoffee_engine::host::Persona::parse(raw).unwrap_or_else(|| {
            eprintln!("[cwoffee] persona: {raw} is not a mode. Write cowork or chat.");
            std::process::exit(1);
        }),
    };

    let workspace = appcfg
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("cwoffee-workspace"));
    std::fs::create_dir_all(&workspace)?;

    let db = get("--db", None)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let base = std::env::var("XDG_DATA_HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
                });
            base.join("cwoffee").join("sessions.db")
        });
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let provider = appcfg.build_provider().map_err(anyhow::Error::msg)?;
    // Same relocation the desktop and the TUI do, before the database is opened:
    // a headless gateway left running for a week should not be the one client that
    // never re-homed its own sessions.
    let migration_notes = cwoffee_engine::legacy::migrate_legacy_dirs();
    for note in &migration_notes {
        eprintln!("[cwoffee/gateway] migration: {note}");
    }
    let store = Arc::new(Store::open(&db)?);

    let configured_model = appcfg.provider.model.clone();
    let mut cfg = HostConfig::new(configured_model.clone(), workspace.clone());
    cfg.cfg_path = Some(config_path.clone());
    if let Some(m) = &appcfg.mode {
        cfg.mode = match m.as_str() {
            "ask" => cwoffee_engine::ApprovalMode::Ask,
            "session" => cwoffee_engine::ApprovalMode::Session,
            "auto" => cwoffee_engine::ApprovalMode::Auto,
            other => anyhow::bail!("unknown mode {other:?}"),
        };
    }
    cfg.context_budget = appcfg.context.target_tokens;
    // Same ExecCfg the registry got: the system prompt states the egress mode, so a
    // phone driving this host gets the same network story as the desktop window.
    cfg.egress = appcfg.exec_cfg().net.mode;
    // None means the server's own output cap applies; see Config.
    cfg.max_tokens = appcfg.max_output_tokens;
    // 0 = no cap. The hardcoded 14 was cutting real tasks short; see Config.
    cfg.max_tool_iters = appcfg.max_tool_iters();
    // The thinking level the window persists is the level the phone sees on connect.
    // Two clients of one engine showing different levels is the same drift the mode
    // persistence closed, one dial further along the console.
    if let Some(r) = appcfg.reasoning_checked().map_err(anyhow::Error::msg)? {
        cfg.reasoning = r;
    }
    // Validated at boot, not per request: see Config::sampling_checked.
    cfg.sampling = appcfg.sampling_checked().map_err(anyhow::Error::msg)?;
    // The exec sandbox's egress block, validated here so a mode word that does not
    // parse stops the app rather than silently becoming the default.
    appcfg.net_checked().map_err(anyhow::Error::msg)?;
    cfg.question_timeout = appcfg.question_timeout();
    appcfg.question_timeout_checked().map_err(anyhow::Error::msg)?;
    // Per-turn, from the live workspace: see SessionHost.
    cfg.env_override = None;
    cfg.approval_timeout = std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    // Two cores behind one trait. `--core rust` is what ships today; `--core pi` puts
    // a `pi --mode rpc` child behind the same `HostOps` and the same three receivers
    // the gateway already consumes. Move 0 is what makes this a construction choice
    // instead of a rewrite: nothing below this point knows, or can tell, which core it
    // is serving — which is exactly the claim the smoke test has to prove.
    // The core is the one decision a launcher carries, so read it the way the desktop app
    // does when no flag names it: a gateway started by that launcher must not silently serve a
    // different engine than the window it replaced.
    let core = get("--core", None)
        .or_else(|| env_nonempty("CWOFFEE_CORE"))
        .unwrap_or_else(|| "rust".into());
    if persona == cwoffee_engine::host::Persona::Chat && core != "pi" {
        eprintln!(
            "[cwoffee] persona: chat is not available on the rust core — that core is the coworker prompt, \
             the tool registry and a workspace. Run with --core pi for a tool-less chat, or set persona = cowork."
        );
        std::process::exit(1);
    }
    if core != "rust" && core != "pi" {
        anyhow::bail!("unknown --core {core:?} (rust or pi)");
    }
    let (host, receivers, questions): (
        Arc<dyn HostOps>,
        Option<(EventIn, ApprovalIn)>,
        Option<QuestionIn>,
    ) = if core == "pi" {
        // pi selects a model by provider+model out of its own provider file, so an
        // empty model is not "ask the endpoint" the way it is for the Rust core. It is
        // a configuration this bridge cannot honour, so it is an error and not a guess.
        if configured_model.is_empty() {
            anyhow::bail!(
                "--core pi needs a model id (--model, or the config file): pi picks a model \
                 by provider+model and does not adopt the endpoint's first"
            );
        }
        let pi_bin = PathBuf::from(
            get("--pi", None)
                .or_else(|| env_nonempty("CWOFFEE_PI"))
                .unwrap_or_else(|| "pi".into()),
        );
        let agent_dir = get("--pi-agent-dir", None)
            .or_else(|| env_nonempty("CWOFFEE_PI_AGENT_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let base = std::env::var("XDG_STATE_HOME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                            .join(".local/share")
                    });
                base.join("cwoffee").join("pi")
            });
        let session_dir = get("--pi-session-dir", None)
            .or_else(|| env_nonempty("CWOFFEE_PI_SESSION_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| agent_dir.join("sessions"));
        std::fs::create_dir_all(&session_dir)?;
        // A pi core needs a provider entry the way cwoffee needs a config file. This
        // one is the development shape — see `mount::write_dev_provider` for why real
        // credential handling is S4.1's decision and not this line's.
        let key = appcfg.api_key().unwrap_or_else(|| "unused".into());
        // The endpoint's own number, probed the way the Rust core probes it: the
        // metrics percentage and the context meter both need a ceiling that somebody
        // stated, not one cwoffee invented.
        let window = match appcfg.build_provider() {
            Ok(p) => cwoffee_bridge::mount::probe_window(p.as_ref(), &configured_model).await,
            Err(e) => {
                eprintln!("[cwoffee/pi] no window probe: {e}");
                None
            }
        };
        let (models, mounted) =
            cwoffee_bridge::mount::write_dev_provider(
                &agent_dir,
                PI_PROVIDER,
                &appcfg.provider.base_url,
                &configured_model,
                &key,
                cwoffee_bridge::mount::pi_api_for_kind(&appcfg.provider.kind),
                &appcfg.provider.kind,
                window,
                // The levels this endpoint said it takes. Without them pi clamps every
                // reasoning request to off - see `mount::reasoning_decl`.
                &appcfg
                    .models
                    .get(&configured_model)
                    .map(|r| r.efforts.clone())
                    .unwrap_or_default(),
            )?;
        // The line says what happened rather than what was intended: an entry left pointing
        // at another endpoint is a conversation that ends with nothing on screen, and a boot
        // log that says "written" in that state sends people looking at the wrong thing.
        match &mounted {
            cwoffee_bridge::mount::MountOutcome::Written => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} -> {} (written to {})",
                appcfg.provider.base_url,
                models.display()
            ),
            cwoffee_bridge::mount::MountOutcome::Replaced { previous } => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} -> {} (replaced the entry pointing at {previous}, \
                 which did not list {configured_model})",
                appcfg.provider.base_url
            ),
            cwoffee_bridge::mount::MountOutcome::Foreign { base_url } => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} left as found: {base_url} lists {configured_model}, \
                 so that is where its requests go — not {} from cwoffee's config",
                appcfg.provider.base_url
            ),
        }
        // Same on the browser/phone shell: an endpoint the model screen registered has to be
        // in pi's file, or picking one of its models fails on the far side of the wire.
        let cfg_path = cwoffee_engine::config::Config::default_path();
        match cwoffee_bridge::mount::mount_provider_rows(&agent_dir, &cfg_path) {
            Ok(list) if !list.is_empty() => println!(
                "[cwoffee/pi] also mounted {} endpoint(s) from {}: {}",
                list.len(),
                cfg_path.display(),
                list.join(", ")
            ),
            Ok(_) => {}
            Err(e) => eprintln!(
                "[cwoffee/pi] could not mount the registered endpoints from {}: {e} — models on them will not be pickable",
                cfg_path.display()
            ),
        }
        // pi folds on its own schedule (`compaction.reserveTokens`, 16 384 by default,
        // chosen for coding sessions) while cwoffee's meter fills against a budget.
        // Writing the reserve makes them one number: the fold happens where the app says
        // the room is gone. No window probed means nothing to compute, and the boot line
        // says so rather than claiming a threshold that was never written.
        match window {
            Some(w) => {
                let budget =
                    cwoffee_bridge::mount::context_budget(cfg.context_budget, Some(w as usize));
                match cwoffee_bridge::mount::mount_settings(
                    &agent_dir,
                    PI_PROVIDER,
                    &configured_model,
                    budget,
                    w as usize,
                ) {
                    Ok(Some(sentence)) => println!("[cwoffee/pi] {sentence}"),
                    Ok(None) => println!("[cwoffee/pi] compaction: pi keeps its own defaults"),
                    Err(e) => println!("[cwoffee/pi] compaction: pi was NOT told where to fold: {e}"),
                }
            }
            None => println!(
                "[cwoffee/pi] compaction: the endpoint stated no window, so pi keeps its own \
                 folding default and the meter falls back to {}",
                cwoffee_bridge::mount::context_budget(cfg.context_budget, None)
            ),
        }
        let ext_srcs: Vec<PathBuf> = {
            let flagged = many("--pi-ext");
            let list = if !flagged.is_empty() {
                flagged
            } else {
                env_nonempty("CWOFFEE_PI_EXT")
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
                    .unwrap_or_default()
            };
            list.into_iter().map(PathBuf::from).collect()
        };
        if ext_srcs.is_empty() {
            eprintln!(
                "[cwoffee/pi] no tool extension mounted (--pi-ext / CWOFFEE_PI_EXT): the model will \
                 be told \"tool not found\" for every tool it tries to use"
            );
        }
        let exts = cwoffee_bridge::mount::mount_extensions(&agent_dir, &pi_bin, &ext_srcs)?;
        let ext_count = exts.len();
        // The product rule, stated as a default rather than left to drift: pi's own
        // `read`/`bash`/`edit`/`write` stay off, because the tools that matter here are
        // cwoffee's and the guardrails live with them. The flag exists to add ours, not
        // to remove the minus signs by omission.
        let tools = match get("--pi-tools", None)
            .or_else(|| env_nonempty("CWOFFEE_PI_TOOLS"))
        {
            Some(v) => v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => vec![
                "-read".into(),
                "-bash".into(),
                "-edit".into(),
                "-write".into(),
            ],
        };
        // How the Rust tool shims find their binary today. Packaging that properly is
        // S2.5; forwarding the variable is the stand-in that keeps this gate honest.
        let mut extra_env = Vec::new();
        if let Ok(v) = std::env::var("CWOFFEE_SHIM") {
            extra_env.push(("CWOFFEE_SHIM".to_string(), v));
        }
        let pinned = if flag("--any-pi") {
            None
        } else {
            Some(
                get("--pi-version", None)
                    .unwrap_or_else(|| cwoffee_bridge::process::PINNED_PI.to_string()),
            )
        };
        if persona == cwoffee_engine::host::Persona::Chat {
            eprintln!(
                "[cwoffee/pi] persona chat: no extension mounted, --no-tools, no folder claimed, system prompt \"{}\"",
                cwoffee_engine::host::Persona::CHAT_PROMPT
            );
        }
        let mut host = cwoffee_bridge::PiHost::spawn(cwoffee_bridge::PiHostConfig {
            launch: cwoffee_bridge::PiLaunch {
                bin: pi_bin,
                workdir: workspace.clone(),
                provider: PI_PROVIDER.into(),
                model: configured_model.clone(),
                agent_dir: agent_dir.clone(),
                session_dir,
                session_file: None,
                extensions: exts,
                tools,
                // Persona shaping is the bridge's, per process: a chat starts with no
                // extension mounted and --no-tools.
                no_tools: false,
                system_prompt: None,
                extra_env,
                pinned_version: pinned,
            },
            store: Arc::clone(&store) as Arc<dyn StoreOps>,
            mode: cfg.mode,
            reasoning: cfg.reasoning,
            approval_timeout: cfg.approval_timeout,
            question_timeout: cfg.question_timeout,
            // Locked decision 14: no cap on live pi processes, an idle one is closed
            // and the next turn reopens it against the same pi session file.
            idle_reap: std::time::Duration::from_secs(
                get("--pi-idle-reap", None)
                    .unwrap_or_else(|| "600".into())
                    .parse::<u64>()
                    .unwrap_or(600),
            ),
            // Same fallback the Rust core uses: a context meter that reads
            // differently per core is drift, not a difference in the model.
            context_budget: cwoffee_bridge::mount::context_budget(cfg.context_budget, window.map(|w| w as usize)),
            budget_pin: cfg.context_budget,
            model_window: window.map(|w| w as usize),
            session_map: agent_dir.join("cwoffee-pi-sessions.json"),
        db_path: Some(db.clone()),
        lean_tools: {
            let (c, _) = cwoffee_engine::config::Config::load(None).unwrap_or_default();
            c.lean_tools()
                || matches!(
                    std::env::var("CWOFFEE_PI_TOOL_MODE").ok().as_deref(),
                    Some("lean")
                )
        },
            persona,
            // A mode the user picks in the window is written back to the same config
            // file, so the next start continues the kind of conversation they chose
            // instead of resetting to the coworker.
            cfg_path: Some(config_path.clone()),
            max_tool_iters: appcfg.max_tool_iters(),
            compact_timeout: appcfg.compact_timeout(),
        })
        .await?;
        // Not a claim about tools or extensions: those are chosen per process by the
        // bridge, which logs every launch with the flags that process actually got. A
        // boot line asserting a guardrail the child never received is how a silently
        // off guardrail stays invisible.
        println!(
            "[cwoffee/pi] core {} pinned; persona {}; {} extension(s) staged — the bridge logs each \
             model process it starts, with the tools that process actually got",
            host.pi_version,
            persona.as_str(),
            ext_count
        );
        let receivers = host.take_receivers();
        let questions = host.take_questions();
        (Arc::new(host) as Arc<dyn HostOps>, receivers, questions)
    } else {
        let mut host = SessionHost::spawn(
            provider,
            Registry::with_v1_tools_exec(appcfg.search_backend(), appcfg.exec_cfg()),
            Arc::clone(&store),
            cfg,
        );
        // Receivers can only be taken once and need &mut, so take them before the host
        // goes behind the Arc the gateway shares. A second consumer would silently
        // split one event stream.
        let receivers = host.take_receivers();
        let questions = host.take_questions();
        let host = Arc::new(host);
        if configured_model.is_empty() {
            if let Some(m) = host.adopt_first_model().await {
                println!("[cwoffee] using the endpoint's first model: {m}");
            }
        }
        (host as Arc<dyn HostOps>, receivers, questions)
    };

    // The host's receivers are taken once and fanned out to every connected
    // browser through a broadcast, so two tabs see the same turn.
    let (ev_tx, _ev_rx_unused) = broadcast::channel::<cwoffee_engine::EngineEvent>(512);
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_q: Arc<Mutex<HashMap<u64, oneshot::Sender<cwoffee_engine::tools::ask::Answer>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    if let Some((mut ev_rx, mut ap_rx)) = receivers {
        let tx = ev_tx.clone();
        let pending = Arc::clone(&pending);
        let pending_q = Arc::clone(&pending_q);
        let mut q_rx = questions.expect("question receiver");
        tokio::spawn(async move {
            let (mut ev_open, mut ap_open, mut q_open) = (true, true, true);
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if ev_open => {
                        match ev {
                            Some(e) => { if tx.send(e).is_err() { break; } }
                            None => ev_open = false,
                        }
                    }
                    ap = ap_rx.recv(), if ap_open => {
                        match ap {
                            Some((req, reply)) => {
                                // Park the reply so respond_approval can resolve
                                // it; the engine's timeout still fails closed if
                                // nobody ever answers.
                                let id = req.id;
                                let _ = tx.send(cwoffee_engine::host::approval_event(&req));
                                pending.lock().unwrap().insert(id, reply);
                            }
                            None => ap_open = false,
                        }
                    }
                    q = q_rx.recv(), if q_open => {
                        match q {
                            Some((question, reply)) => {
                                let id = question.id;
                                let _ = tx.send(cwoffee_engine::host::question_event(&question));
                                pending_q.lock().unwrap().insert(id, reply);
                            }
                            None => q_open = false,
                        }
                    }
                }
            }
        });
    }

    let dist = get("--dist", None)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist"));

    let gw = cwoffee_gateway::GatewayConfig {
        host: bind_ip,
        port,
        dist,
        drive_token,
        watch_token,
    };
    let handle = Arc::new(cwoffee_gateway::EngineHandle {
        host: Arc::clone(&host) as Arc<dyn HostOps>,
        store: Arc::clone(&store) as Arc<dyn StoreOps>,
        events: ev_tx.clone(),
        pending,
        pending_questions: Arc::clone(&pending_q),
        clients: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        migration_notes,
        config_path,
    });

    cwoffee_gateway::serve(gw, handle).await.map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-off at window close passes no token on the command line at all. That is only
    /// safe because these fallbacks exist — and an explicit flag still wins, because a flag is
    /// a decision and an inherited variable is context.
    #[test]
    fn a_flag_token_beats_the_environment_and_the_file() {
        std::env::set_var("CWOFFEE_TEST_GATEWAY_TOKEN", "from-env");
        let missing = PathBuf::from("/nonexistent/cwoffee-token");
        let t = token_from(Some("from-flag".into()), "CWOFFEE_TEST_GATEWAY_TOKEN", &missing);
        assert_eq!(t.as_deref(), Some("from-flag"));
        let t2 = token_from(None, "CWOFFEE_TEST_GATEWAY_TOKEN", &missing);
        assert_eq!(t2.as_deref(), Some("from-env"), "the environment is the launcher's voice");
        std::env::remove_var("CWOFFEE_TEST_GATEWAY_TOKEN");
    }

    #[test]
    fn a_token_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!("cw-gw-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("gateway.token");
        std::fs::write(&p, "  written-by-the-app  \n").unwrap();
        let t = token_from(None, "CWOFFEE_TEST_GATEWAY_TOKEN_ABSENT", &p);
        assert_eq!(t.as_deref(), Some("written-by-the-app"), "the trailing newline came with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty variable is not a token. It used to count as one, which let a LAN bind through
    /// while authenticating nothing.
    #[test]
    fn an_empty_value_is_absent_not_a_token() {
        std::env::set_var("CWOFFEE_TEST_EMPTY_TOKEN", "   ");
        assert_eq!(
            token_from(None, "CWOFFEE_TEST_EMPTY_TOKEN", &PathBuf::from("/nonexistent/x")),
            None
        );
        assert!(env_nonempty("CWOFFEE_TEST_EMPTY_TOKEN").is_none());
        std::env::remove_var("CWOFFEE_TEST_EMPTY_TOKEN");
    }

    #[test]
    fn the_token_file_sits_next_to_the_config() {
        // Same place gateway_ctl writes it. Two answers to "where is the secret" is how a
        // phone gets a gateway the desktop says nothing about.
        let p = token_file("gateway.token");
        assert!(p.ends_with("cwoffee/gateway.token"), "{}", p.display());
    }
}
