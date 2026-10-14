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

  --host ADDR        bind address (default 127.0.0.1; anything else needs --token)
  --port N           port (default 8099)
  --token T          grants watch AND drive; required unless binding to loopback
  --watch-token T    optional; grants read only
  --dist PATH        built frontend (default <repo>/ui/dist)
  --url URL          endpoint base, e.g. http://127.0.0.1:8090/v1
  --model ID         model id (empty asks the endpoint)
  --workspace PATH   where tools operate
  --db PATH          session database
  --mode ask|session|auto
  --core rust|pi     engine behind the gateway (default rust; pi = `pi --mode rpc`)
  --pi PATH          pi binary for --core pi (default: whatever PATH resolves)
  --pi-version V     refuse a pi that is not exactly this version (default pinned)
  --any-pi           skip the version handshake; development only
  --pi-ext PATH      extension to mount for --core pi (repeatable; this is where
                     the Rust tool shims hang off pi)
  --pi-tools LIST    pi --tools entries (default -read,-bash,-edit,-write: pi's own
                     coding tools stay off, ours come in as extensions)
  --pi-agent-dir PATH  pi config home (default <state>/cwoffee/pi)
  --pi-idle-reap SECS  close an idle pi process after this (default 600)
  -h, --help
"
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
    let drive_token = get("--token", None);
    let watch_token = get("--watch-token", None);

    // Reuse the same config file and precedence as the desktop app and the TUI.
    let (mut appcfg, loaded) = Config::load(None).map_err(anyhow::Error::msg)?;
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
    if let Some(m) = get("--mode", None) {
        appcfg.mode = Some(m);
    }

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
    let core = get("--core", None).unwrap_or_else(|| "rust".into());
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
        let pi_bin = PathBuf::from(get("--pi", None).unwrap_or_else(|| "pi".into()));
        let agent_dir = get("--pi-agent-dir", None)
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
            .map(PathBuf::from)
            .unwrap_or_else(|| agent_dir.join("sessions"));
        std::fs::create_dir_all(&session_dir)?;
        // A pi core needs a provider entry the way cwoffee needs a config file. This
        // one is the development shape — see `mount::write_dev_provider` for why real
        // credential handling is S4.1's decision and not this line's.
        let key = appcfg.api_key().unwrap_or_else(|| "unused".into());
        let models =
            cwoffee_bridge::mount::write_dev_provider(
                &agent_dir,
                PI_PROVIDER,
                &appcfg.provider.base_url,
                &configured_model,
                &key,
                cwoffee_bridge::mount::pi_api_for_kind(&appcfg.provider.kind),
            )?;
        println!(
            "[cwoffee/pi] provider {PI_PROVIDER} -> {} (written to {})",
            appcfg.provider.base_url,
            models.display()
        );
        let exts = cwoffee_bridge::mount::mount_extensions(
            &agent_dir,
            &pi_bin,
            &many("--pi-ext").into_iter().map(PathBuf::from).collect::<Vec<_>>(),
        )?;
        let ext_count = exts.len();
        // The product rule, stated as a default rather than left to drift: pi's own
        // `read`/`bash`/`edit`/`write` stay off, because the tools that matter here are
        // cwoffee's and the guardrails live with them. The flag exists to add ours, not
        // to remove the minus signs by omission.
        let tools = match get("--pi-tools", None) {
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
        // Said before the launch owns the list, because what pi was told is worth
        // printing on every boot: this is the switch that keeps pi's own coding tools
        // off, and a silent change to it should be visible in a log line.
        let tools_note = if tools.is_empty() {
            "pi defaults".to_string()
        } else {
            tools.join(",")
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
            context_budget: cfg.context_budget.unwrap_or(cwoffee_engine::host::FALLBACK_BUDGET),
            model_window: None,
            session_map: agent_dir.join("cwoffee-pi-sessions.json"),
        })
        .await?;
        println!(
            "[cwoffee/pi] core pi {} up in {}; {} extension(s); tools {}",
            host.pi_version,
            workspace.display(),
            ext_count,
            tools_note
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
