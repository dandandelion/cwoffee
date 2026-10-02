//! Cowry desktop host.
//!
//! Thin by design. Everything interesting lives in `crates/engine`; this file
//! wires one up and gets out of the way.

mod commands;
mod gpu;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cowry_engine::approval::ApprovalAnswer;
use cowry_engine::host::{approval_event, HostConfig, SessionHost};
use cowry_engine::provider::Provider;
use cowry_engine::store::Store;
use cowry_engine::tools::Registry;
use cowry_engine::Config;
use tokio::sync::{mpsc, oneshot};

use commands::AppState;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Minimal flag parser. Tauri hands us its own arguments too, so unknown flags
/// are ignored rather than fatal.
fn flag(args: &[String], long: &str, short: Option<&str>) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if (a == long || short.map(|s| a == s).unwrap_or(false)) && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
    }
    None
}

fn data_dir() -> PathBuf {
    PathBuf::from(env_or("XDG_DATA_HOME", &format!("{}/.local/share", std::env::var("HOME").unwrap_or_default())))
        .join("cowry")
}

fn main() {
    // MUST be the first thing that happens: the GPU policy has to be in place
    // before GTK/EGL is initialised, or libglvnd hands us an NVIDIA context on a
    // GPU that is already ~97% full from inference. See PLAN.md §12.
    gpu::pin_intel_gpu();

    let report = gpu::report();
    eprintln!(
        "[cowry] EGL vendor pinned to: {}\n[cowry] GLX vendor: {}",
        report.egl_vendor, report.glx_vendor
    );
    if report.leaked_nvidia {
        eprintln!("[cowry] WARNING: this process holds an NVIDIA render node despite the pin");
    }

    // ---- configuration ----------------------------------------------------
    // precedence: CLI flags > env > config file > defaults
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = flag(&args, "--config", None);
    let (mut appcfg, loaded) = match Config::load(cfg_path.as_deref().map(std::path::Path::new)) {
        Ok(v) => v,
        Err(e) => {
            // A malformed config must not silently fall back to defaults: that
            // would send the agent at the wrong endpoint with no explanation.
            eprintln!("[cowry] {e}");
            std::process::exit(1);
        }
    };
    appcfg.apply_env();
    if let Some(p) = flag(&args, "--url", None) {
        appcfg.provider.base_url = p;
    }
    if let Some(p) = flag(&args, "--model", None) {
        appcfg.provider.model = p;
    }
    if let Some(p) = flag(&args, "-C", Some("--workspace")) {
        appcfg.workspace = Some(p);
    }
    if let Some(p) = flag(&args, "--mode", None) {
        appcfg.mode = Some(p);
    }
    if let Some(p) = &loaded {
        eprintln!("[cowry] config: {}", p.display());
    }

    // ---- engine wiring ----------------------------------------------------
    let url = appcfg.provider.base_url.clone();
    let model = if appcfg.provider.model.is_empty() {
        "local".to_string()
    } else {
        appcfg.provider.model.clone()
    };
    // Default the workspace to ~/cowry-workspace, not the process CWD. A GUI app
    // launched from a .desktop file or by a WM inherits an arbitrary CWD, and
    // under `tauri dev` that is crates/tauri-app - which would quietly make the
    // agent edit the wrong tree.
    let workspace = appcfg
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env_or("HOME", ".")).join("cowry-workspace"));
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        eprintln!("[cowry] cannot create workspace {}: {e}", workspace.display());
    }
    let db_path = data_dir().join("sessions.db");
    let mode = appcfg.mode();

    if let Err(e) = std::fs::create_dir_all(db_path.parent().unwrap_or_else(|| std::path::Path::new("."))) {
        eprintln!("[cowry] cannot create data dir: {e}");
    }

    let store = match Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[cowry] cannot open store at {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    let provider: Box<dyn Provider> = match appcfg.build_provider() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[cowry] {e}");
            std::process::exit(1);
        }
    };

    let mut cfg = HostConfig::new(model, workspace.clone());
    cfg.mode = mode;
    cfg.context_budget = appcfg.context.target_tokens;
    cfg.env_block = cowry_engine::host::default_env_block(&workspace, "linux");
    // Fail-closed default: an unanswered prompt refuses rather than hanging the
    // turn forever.
    cfg.approval_timeout =
        std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    // The engine spawns tasks, so it must be constructed inside a tokio
    // context. Tauri's async runtime is tokio, so entering it here is enough;
    // calling SessionHost::spawn from bare `main` panics with "no reactor".
    let mut host = tauri::async_runtime::block_on(async {
        SessionHost::spawn(
            provider,
            Registry::with_v1_tools_search(appcfg.search_backend()),
            Arc::clone(&store),
            cfg,
        )
    });
    eprintln!(
        "[cowry] engine up: url={url} model={} workspace={} mode={:?}",
        appcfg.provider.model, workspace.display(), mode
    );

    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>> = Arc::new(Mutex::new(HashMap::new()));

    // The pump starts at boot, not when the webview attaches: nothing is lost
    // during startup, and if the webview goes away the gate's timeout is what
    // stops the agent — not the absence of a forwarder.
    if let Some((mut ev_rx, mut ap_rx)) = host.take_receivers() {
        let qtx = queue_tx.clone();
        tauri::async_runtime::spawn(async move {
            let (mut ev_open, mut ap_open) = (true, true);
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if ev_open => {
                        match ev {
                            Some(e) => {
                                if qtx.send(e).is_err() {
                                    break;
                                }
                            }
                            None => ev_open = false,
                        }
                    }
                    ap = ap_rx.recv(), if ap_open => {
                        match ap {
                            Some((req, reply)) => {
                                let id = req.id;
                                if qtx.send(approval_event(&req)).is_err() {
                                    // Nobody to ask: refuse rather than run unattended.
                                    let _ = reply.send(ApprovalAnswer::Deny);
                                    continue;
                                }
                                pending.lock().unwrap().insert(id, reply);
                            }
                            None => ap_open = false,
                        }
                    }
                    else => break,
                }
                if !ev_open && !ap_open {
                    break;
                }
            }
            eprintln!("[cowry] event pump finished");
        });
    }

    let state = AppState {
        host,
        store,
        queue_rx: Mutex::new(Some(queue_rx)),
        pending_approvals: Mutex::new(HashMap::new()),
    };

    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::attach,
            commands::start_turn,
            commands::cancel_turn,
            commands::respond_approval,
            commands::set_mode,
            commands::set_reasoning,
            commands::session_state,
            commands::list_sessions,
            commands::search_messages,
            commands::session_messages,
            commands::new_session,
            commands::switch_session,
            commands::gpu_report,
            commands::render_report
        ]);

    if let Err(e) = app.run(tauri::generate_context!()) {
        eprintln!("[cowry] error while running: {e}");
        std::process::exit(1);
    }
}
