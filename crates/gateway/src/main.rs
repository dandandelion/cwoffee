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
use cwoffee_engine::host::{HostConfig, SessionHost};
use cwoffee_engine::store::Store;
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
    if let Some(m) = &appcfg.mode {
        cfg.mode = match m.as_str() {
            "ask" => cwoffee_engine::ApprovalMode::Ask,
            "session" => cwoffee_engine::ApprovalMode::Session,
            "auto" => cwoffee_engine::ApprovalMode::Auto,
            other => anyhow::bail!("unknown mode {other:?}"),
        };
    }
    cfg.context_budget = appcfg.context.target_tokens;
    // None means the server's own output cap applies; see Config.
    cfg.max_tokens = appcfg.max_output_tokens;
    // 0 = no cap. The hardcoded 14 was cutting real tasks short; see Config.
    cfg.max_tool_iters = appcfg.max_tool_iters();
    // Validated at boot, not per request: see Config::sampling_checked.
    cfg.sampling = appcfg.sampling_checked().map_err(anyhow::Error::msg)?;
    // Per-turn, from the live workspace: see SessionHost.
    cfg.env_override = None;
    cfg.approval_timeout = std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    let mut host = SessionHost::spawn(
        provider,
        Registry::with_v1_tools_exec(appcfg.search_backend(), appcfg.exec_cfg()),
        Arc::clone(&store),
        cfg,
    );
    // Receivers can only be taken once and need &mut, so take them before the
    // host goes behind the Arc that the gateway shares.
    let receivers = host.take_receivers();
    // Both receivers must be taken before the host becomes an Arc: they are
    // `Option`s moved out of it, and a second consumer would silently split one
    // event stream.
    let questions = host.take_questions();
    let host = Arc::new(host);
    if configured_model.is_empty() {
        if let Some(m) = host.adopt_first_model().await {
            println!("[cwoffee] using the endpoint's first model: {m}");
        }
    }

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
        host: Arc::clone(&host),
        store: Arc::clone(&store),
        events: ev_tx.clone(),
        pending,
        pending_questions: Arc::clone(&pending_q),
        clients: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        migration_notes,
        config_path,
    });

    cwoffee_gateway::serve(gw, handle).await.map_err(|e| anyhow::anyhow!("{e}"))
}
