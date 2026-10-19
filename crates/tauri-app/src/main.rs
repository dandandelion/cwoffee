//! Cwoffee desktop host.
//!
//! Thin by design. Everything interesting lives in `crates/engine`; this file
//! wires one up and gets out of the way.

mod clipboard;
mod commands;
mod gateway_ctl;
mod gpu;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cwoffee_engine::approval::ApprovalAnswer;
use cwoffee_engine::host::{approval_event, question_event, HostConfig, HostOps, SessionHost};
use cwoffee_engine::provider::Provider;
use cwoffee_engine::store::{Store, StoreOps};
use cwoffee_engine::tools::Registry;
use cwoffee_engine::Config;
use tokio::sync::{mpsc, oneshot};

/// The provider entry this app writes into pi's provider file (see `pi_host`). Named
/// because the same word appears in the boot log, and a log that cannot be grepped for
/// is a log that will not be grepped.
const PI_PROVIDER: &str = "cwoffee";
use tauri::Manager;

use commands::AppState;

/// Locate a built frontend to serve over the network.
///
/// A packaged cwoffee embeds its assets in the binary, which the webview can
/// read and an HTTP listener cannot. So the gateway needs a real directory; the
/// candidates cover a dev checkout and an installed copy, and CWOFFEE_UI_DIST
/// overrides both.
fn ui_dist_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CWOFFEE_UI_DIST") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    // The path recorded when this binary was compiled, if it still exists. Preferred
    // over anything derived from the executable's location, because the executable's
    // location says where the build directory was, not which UI was embedded.
    if let Some(baked) = option_env!("CWOFFEE_UI_DIST_BAKED") {
        let p = PathBuf::from(baked);
        if p.is_dir() {
            return p;
        }
        eprintln!(
            "[cwoffee] the UI this binary was built with is gone ({}); falling back to looking near the executable",
            baked
        );
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    for cand in [
        exe.join("../../ui/dist"),
        exe.join("../ui/dist"),
        PathBuf::from("/usr/share/cwoffee/ui"),
    ] {
        if cand.is_dir() {
            return cand;
        }
    }
    exe.join("ui/dist")
}

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
        .join("cwoffee")
}

fn main() {
    // MUST be the first thing that happens: the GPU policy has to be in place
    // before GTK/EGL is initialised, or libglvnd hands us an NVIDIA context on a
    // GPU that is already ~97% full from inference. See PLAN.md §12.
    gpu::pin_intel_gpu();

    // WebKitGTK's own rendering path, also env-driven and also needed before the
    // webview exists (the web process inherits this environment at fork time).
    let webkit_mode = gpu::apply_webkit_mode();
    eprintln!("[cwoffee] webkit render path: {}", webkit_mode.name);
    if let Some(w) = webkit_mode.warning {
        eprintln!("[cwoffee] WARNING: {w}");
    }

    let report = gpu::report();
    eprintln!(
        "[cwoffee] EGL vendor pinned to: {}\n[cwoffee] GLX vendor: {}",
        report.egl_vendor, report.glx_vendor
    );
    if report.leaked_nvidia {
        eprintln!("[cwoffee] WARNING: this process holds an NVIDIA render node despite the pin");
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
            eprintln!("[cwoffee] {e}");
            std::process::exit(1);
        }
    };
    // The file the search panel writes back to. None from `load` means no file
    // existed yet, which is not "nowhere to write": the panel creates one at the
    // default path, so a provider chosen on a phone survives a restart.
    let config_path = loaded.clone().unwrap_or_else(Config::default_path);
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
        eprintln!("[cwoffee] config: {}", p.display());
    }

    // ---- engine wiring ----------------------------------------------------
    let url = appcfg.provider.base_url.clone();
    // Empty is allowed: adopt_first_model() below asks the endpoint. Hardcoding
    // a placeholder like "local" only works by luck with llama.cpp's alias and
    // silently breaks every other server.
    let configured_model = appcfg.provider.model.trim().to_string();
    let model = configured_model.clone();
    // Default the workspace to ~/cwoffee-workspace, not the process CWD. A GUI app
    // launched from a .desktop file or by a WM inherits an arbitrary CWD, and
    // under `tauri dev` that is crates/tauri-app - which would quietly make the
    // agent edit the wrong tree.
    let workspace = appcfg
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env_or("HOME", ".")).join("cwoffee-workspace"));
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        eprintln!("[cwoffee] cannot create workspace {}: {e}", workspace.display());
    }
    // Rename safety: move ~/.config/cowry and ~/.local/share/cowry to their
    // cwoffee equivalents before anything reads them, or the rebrand strands
    // every existing session.
    let migration_notes = cwoffee_engine::legacy::migrate_legacy_dirs();
    for note in &migration_notes {
        println!("[cwoffee] {note}");
    }

    let db_path = data_dir().join("sessions.db");
    let mode = appcfg.mode();

    if let Err(e) = std::fs::create_dir_all(db_path.parent().unwrap_or_else(|| std::path::Path::new("."))) {
        eprintln!("[cwoffee] cannot create data dir: {e}");
    }

    let store = match Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[cwoffee] cannot open store at {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    let provider: std::sync::Arc<dyn Provider> = match appcfg.build_provider() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[cwoffee] {e}");
            std::process::exit(1);
        }
    };

    let mut cfg = HostConfig::new(model, workspace.clone());
    // A mode or thinking level chosen in the window is written back to this file, so the
    // next start - and the gateway on a phone - agrees with the one you just made.
    cfg.cfg_path = Some(config_path.clone());
    cfg.mode = mode;
    // The thinking chip is persisted the same way the mode is, so it has to be read
    // back at boot or the file and the running app disagree from the first second.
    // A word that is not a level stops the app: an ignored typo here means the model
    // runs at a level nobody chose.
    match appcfg.reasoning_checked() {
        Ok(Some(r)) => cfg.reasoning = r,
        Ok(None) => {}
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    cfg.context_budget = appcfg.context.target_tokens;
    // The prompt states what the sandbox may reach. Read from the same ExecCfg the
    // registry got, so the sentence and the sandbox cannot disagree: the model's
    // willingness to run `npm install` or `git pull` hinges on it.
    cfg.egress = appcfg.exec_cfg().net.mode;
    // None means the server's own output cap applies; see Config.
    cfg.max_tokens = appcfg.max_output_tokens;
    // 0 = no cap. The hardcoded 14 was cutting real tasks short; see Config.
    cfg.max_tool_iters = appcfg.max_tool_iters();
    // Empty unless `[sampling]` says otherwise, in which case the keys go on every
    // request. Validated here rather than at request time: a bad range should stop
    // the app at boot, not bend the model's behaviour invisibly.
    match appcfg.sampling_checked() {
        Ok(s) => cfg.sampling = s,
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    // How long a question card stays open. Validated at boot for the same reason
    // sampling is: a value that is nonsense should stop the app, not be ignored.
    match appcfg.question_timeout_checked() {
        Ok(d) => cfg.question_timeout = d,
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    // Same rule for the sandbox's egress block: a mode word that does not parse is
    // a startup error naming itself, not a silent fall back to the default.
    if let Err(e) = appcfg.net_checked() {
        eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
        std::process::exit(1);
    }
    // Say what the sandbox will actually do. Without this line a user who set
    // `network = "open"` has no way to know the app honoured it.
    println!("[cwoffee] sandbox egress: {}", appcfg.exec_cfg().net.describe());
    // No env block from here on purpose: the host derives it from the folder a
    // turn actually runs in. A snapshot of the startup folder used to be shown to
    // sessions living somewhere else.
    cfg.env_override = None;
    // Fail-closed default: an unanswered prompt refuses rather than hanging the
    // turn forever.
    cfg.approval_timeout =
        std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    // Two cores behind one seam, chosen at boot (PI-CORE-PLAN decision 12: the desktop
    // joins in-process, it does not open a second localhost socket to talk to itself).
    // `CWOFFEE_CORE=pi` puts a `pi --mode rpc` child where the Rust engine used to be,
    // and everything after this point — the window's pump, the gateway handle, every
    // command — is identical, because it only ever spoke `HostOps`.
    let core = std::env::var("CWOFFEE_CORE").unwrap_or_else(|_| "rust".into());
    if core != "rust" && core != "pi" {
        eprintln!("[cwoffee] CWOFFEE_CORE={core:?} is neither \"rust\" nor \"pi\"; refusing to guess which core you meant");
        std::process::exit(1);
    }
    // Cowork or chat, resolved before the core starts: in chat the model process gets
    // no tools and a one-line prompt, which is not something you can change afterwards.
    // `appcfg.persona` is what the window writes when the user clicks the tab, so the
    // app comes back in the mode the user left it in.
    let persona_raw = std::env::var("CWOFFEE_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| appcfg.persona.clone())
        .unwrap_or_else(|| "cowork".into());
    let persona = match cwoffee_engine::host::Persona::parse(&persona_raw) {
        Some(p) => p,
        None => {
            eprintln!("[cwoffee] persona: {persona_raw:?} is neither \"cowork\" nor \"chat\"; refusing to guess which mode you meant");
            std::process::exit(1);
        }
    };
    if persona == cwoffee_engine::host::Persona::Chat && core != "pi" {
        eprintln!(
            "[cwoffee] persona: chat is not available on the rust core — that core is the coworker prompt, \
             the tool registry and a workspace. Set CWOFFEE_CORE=pi for a tool-less chat, or persona = cowork."
        );
        std::process::exit(1);
    }

    // Read the level the file asked for before `cfg` moves into the host: the boot
    // line states the dials the app actually started with, which is the only way a
    // drift between `config.toml` and the running app is visible in a log.
    let reasoning_at_boot = cfg.reasoning;
    let (host, receivers, questions, model_label): (
        Arc<dyn HostOps>,
        Option<(
            cwoffee_engine::host::EventRx,
            tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::host::ApprovalRx>,
        )>,
        Option<tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::tools::ask::QuestionRx>>,
        String,
    ) = tauri::async_runtime::block_on(async {
        if core == "pi" {
            match pi_host(&appcfg, &cfg, &store, &workspace, &configured_model, persona).await {
                Err(e) => {
                    eprintln!("[cwoffee] pi core did not start: {e}");
                    std::process::exit(1);
                }
                Ok(mut h) => {
                    // Receivers can only be taken once, and they need &mut: take them
                    // before the host becomes the Arc the window and gateway share.
                    let r = h.take_receivers();
                    let q = h.take_questions();
                    (Arc::new(h) as Arc<dyn HostOps>, r, q, configured_model.clone())
                }
            }
        } else {
            // The engine spawns tasks, so it must be constructed inside a tokio
            // context. Tauri's async runtime is tokio, so entering it here is enough;
            // calling SessionHost::spawn from bare `main` panics with "no reactor".
            let mut h = SessionHost::spawn(
                provider,
                Registry::with_v1_tools_exec(appcfg.search_backend(), appcfg.exec_cfg()),
                Arc::clone(&store),
                cfg,
            );
            // No config file means no model id. Ask the endpoint rather than sending
            // an empty one and taking a 400.
            let adopted = h.adopt_first_model().await;
            if let Some(m) = &adopted {
                eprintln!("[cwoffee] no model configured; using the endpoint's first: {m}");
            }
            let r = h.take_receivers();
            let q = h.take_questions();
            // Read the adopted id from the return value, not host.state(): SetModel is
            // applied by the host task, so state() can still show the old value here.
            let label = adopted.unwrap_or_else(|| configured_model.clone());
            (Arc::new(h) as Arc<dyn HostOps>, r, q, label)
        }
    });
    eprintln!(
        "[cwoffee] engine up: core={core} persona={} url={url} model={model_label} workspace={} mode={:?} reasoning={:?}",
        persona.as_str(), workspace.display(), mode, reasoning_at_boot
    );
    // The exec deadline used to be invisible: a [sandbox] block that parsed,
    // validated, and then went nowhere looked identical to one that worked.
    let ec = appcfg.exec_cfg();
    eprintln!(
        "[cwoffee] exec: ceiling {}s, sandbox={}",
        ec.timeout_secs, ec.sandbox_enabled
    );
    // Same reason: a search backend you cannot see is a backend you cannot tell
    // apart from one that is silently falling back. `describe()` is the same wording
    // the settings card shows, so the log and the UI cannot disagree.
    {
        let b = appcfg.search_backend();
        let hint = match &b {
            cwoffee_engine::tools::web::SearchBackend::DuckDuckGo =>
                " — starts answering with a bot challenge after a handful of queries; set [search] backend = \"auto\" to have Cwoffee run its own SearXNG",
            _ => "",
        };
        eprintln!("[cwoffee] search: {}{}", b.describe(), hint);
    }

    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    // A second fan-out for non-webview listeners (the gateway). The webview gets
    // its own ordered channel; a browser tab gets a broadcast subscription, so
    // two tabs and the window all see the same turn rather than competing for
    // one receiver.
    let (bcast_tx, _bcast_rx) = tokio::sync::broadcast::channel::<cwoffee_engine::EngineEvent>(512);
    // One map, shared by the pump (which parks reply senders) and the
    // respond_approval command (which resolves them). Two separate maps meant the
    // command always looked up an empty one.
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_pump = Arc::clone(&pending);
    // ask_user works the same way: the pump parks the reply sender, and
    // respond_question (from the webview or the gateway) resolves it.
    let pending_q: Arc<Mutex<HashMap<u64, oneshot::Sender<cwoffee_engine::tools::ask::Answer>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_q_pump = Arc::clone(&pending_q);

    // The pump starts at boot, not when the webview attaches: nothing is lost
    // during startup, and if the webview goes away the gate's timeout is what
    // stops the agent — not the absence of a forwarder.
    let mut question_rx = questions.expect("question receiver");
    if let Some((mut ev_rx, mut ap_rx)) = receivers {
        let qtx = queue_tx.clone();
        let btx = bcast_tx.clone();
        let pending_q_pump = Arc::clone(&pending_q_pump);
        tauri::async_runtime::spawn(async move {
            let (mut ev_open, mut ap_open, mut q_open) = (true, true, true);
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if ev_open => {
                        match ev {
                            Some(e) => {
                                // The webview queue is the load-bearing one; the
                                // broadcast has no listeners in the common case
                                // and its failure must not end the pump.
                                let _ = btx.send(e.clone());
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
                                let _ = btx.send(approval_event(&req));
                                if qtx.send(approval_event(&req)).is_err() {
                                    // Nobody to ask: refuse rather than run unattended.
                                    let _ = reply.send(ApprovalAnswer::Deny);
                                    continue;
                                }
                                pending_pump.lock().unwrap().insert(id, reply);
                            }
                            None => ap_open = false,
                        }
                    }
                    qu = question_rx.recv(), if q_open => {
                        match qu {
                            Some((question, reply)) => {
                                let id = question.id;
                                let _ = btx.send(question_event(&question));
                                if qtx.send(question_event(&question)).is_err() {
                                    // Nobody to ask: tell the model that rather
                                    // than parking the turn until the timeout.
                                    drop(reply);
                                    continue;
                                }
                                pending_q_pump.lock().unwrap().insert(id, reply);
                            }
                            None => q_open = false,
                        }
                    }
                    else => break,
                }
                if !ev_open && !ap_open && !q_open {
                    break;
                }
            }
            eprintln!("[cwoffee] event pump finished");
        });
    }

    // The gateway controller exists whether or not the gateway is on. The footer
    // has to answer "is it running?" and "how many devices are on it?", and that
    // question has to be answerable about a gateway started from the environment
    // as well as one started by clicking the footer.
    let gw_handle = Arc::new(cwoffee_gateway::EngineHandle {
        host: Arc::clone(&host),
        store: Arc::clone(&store) as Arc<dyn StoreOps>,
        events: bcast_tx.clone(),
        pending: Arc::clone(&pending),
        pending_questions: Arc::clone(&pending_q),
        clients: gateway_ctl::client_gauge(),
        migration_notes: migration_notes.clone(),
        config_path: config_path.clone(),
    });
    let gateway = Arc::new(gateway_ctl::GatewayCtl::new(gw_handle));

    let state = AppState {
        host,
        store,
        note_tx: queue_tx.clone(),
        queue_rx: Mutex::new(Some(queue_rx)),
        pending_approvals: Arc::clone(&pending),
        pending_questions: Arc::clone(&pending_q),
        rendered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        migration_notes,
        gateway: Arc::clone(&gateway),
        config_path: config_path.clone(),
    };

    // Boot-time gateway: off unless CWOFFEE_GATEWAY_PORT is set. Started through
    // the same controller the footer toggle uses, so the status line reports the
    // truth about an environment-started gateway too - port, bind address, token
    // source and live client count.
    if let Some(port) = std::env::var("CWOFFEE_GATEWAY_PORT")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let bind: std::net::IpAddr = env_or("CWOFFEE_GATEWAY_HOST", "127.0.0.1")
            .parse()
            .unwrap_or_else(|_| std::net::Ipv4Addr::LOCALHOST.into());
        let port = port.parse().unwrap_or(8099);
        let (drive, source) = gateway_ctl::drive_token();
        let watch = std::env::var("CWOFFEE_GATEWAY_WATCH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        // Started before the window exists rather than alongside it. The first
        // `gateway_status` read happens as the UI boots, and a status taken while
        // the accept loop is still being spawned says "off" about a gateway that is
        // about to be reachable - the one reading of this pill that could mislead.
        let ctl = Arc::clone(&gateway);
        if let Err(e) =
            tauri::async_runtime::block_on(ctl.start(bind, port, drive, watch, source))
        {
            eprintln!("[cwoffee] gateway not running: {e}");
        }
    }

    let quit_armed: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    let rendered_flag = state.rendered.clone();
    let app = tauri::Builder::default()
        // Rendered markdown contains links to pages the model fetched. Those must
        // open in the user's browser, never in this webview (which would replace
        // the app), and never through a shell string we built ourselves.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        // A webview that never paints leaves no trace: the engine logs look
        // healthy, WebKit processes are up, memory is fine. The only outward
        // symptom is a missing "render OK" line. So log what the webview was
        // actually given, at startup, before anyone has to guess.
        .setup(move |app| {
            // The window manager and the dock ask the *window* for an icon. A
            // frameless window built from a bare binary has none, so GNOME shows
            // its generic gear. The bundle icons only apply once the app is
            // installed with a .desktop file, which is not how this runs in dev.
            if let Some(w) = app.get_webview_window("main") {
                if let Err(e) = w.set_icon(tauri::include_image!("icons/64x64.png")) {
                    eprintln!("[cwoffee] window icon not set: {e}");
                }
                // The self-test measures layout and animation in a page the
                // compositor is painting. An occluded or minimized window stops
                // requestAnimationFrame dead and freezes every CSS animation at its
                // start value, at which point those checks measure the window
                // manager rather than the app - and the run used to discover that as
                // a fifteen-minute timeout with no verdict at all. So the armed
                // harness asks for the window it needs, and the log says so: a run
                // that moved your windows should say it moved your windows.
                if commands::selftest_armed() {
                    let _ = w.unminimize();
                    let _ = w.set_always_on_top(true);
                    let _ = w.set_focus();
                    eprintln!(
                        "[selftest] window un-minimized, pinned above others and focused: the layout and \
                         reveal checks need a page the compositor is actually painting"
                    );
                }
            }
            let cfg = app.config();
            let cfg_build = cfg.build.clone();
            eprintln!(
                "[cwoffee] frontend_dist={:?} csp_hashes={}",
                cfg.build.frontend_dist,
                cfg.app.security.csp.is_some(),
            );
            // A webview that never paints leaves no trace. Process liveness,
            // memory and sockets all report healthy; the only outward symptom is
            // a missing "render OK" line, which is easy to miss in a log. So make
            // the absence itself an explicit failure.
            let painted = rendered_flag.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                if !painted.load(std::sync::atomic::Ordering::SeqCst) {
                    eprintln!(
                        "[cwoffee] SUSPECT: no render report after 8s - the window is probably blank. frontend_dist={:?}; a debug build loads devUrl and needs vite running.",
                        cfg_build.frontend_dist
                    );
                }
            });
            Ok(())
        })
        // Decision 15: quitting with a turn running is a question, not a silent kill.
        // The refusal rides on a `Note`, which the window already knows how to render —
        // so the policy needs no UI edit. A second close within 8 s means "stop it and
        // quit": abort, give the turn 2 s to settle so the transcript flushes, then let
        // the window close. A window that vanishes and leaves an agent working
        // unattended is the same failure the approval gate exists to prevent.
        .on_window_event(move |window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else { return };
            let state = window.app_handle().state::<AppState>();
            let busy = state.host.state().busy;
            let mut armed = quit_armed.lock().unwrap();
            let already = armed.map(|t| t.elapsed() < std::time::Duration::from_secs(8)).unwrap_or(false);
            if !busy {
                return;
            }
            if !already {
                *armed = Some(std::time::Instant::now());
                api.prevent_close();
                let st = state.host.state();
                let _ = state.note_tx.send(cwoffee_engine::EngineEvent::Note {
                    message: "a turn is still running — closing was refused. Close again within 8 seconds to stop it and quit.".into(),
                    session: Some(st.session_id),
                });
                return;
            }
            state.host.send(cwoffee_engine::host::HostCmd::Cancel);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while state.host.state().busy && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let still_busy = state.host.state().busy;
            eprintln!(
                "[cwoffee] quit asked while busy: aborted, {}the turn settled{}",
                if still_busy { "and " } else { "" },
                if still_busy { " — its children are being killed" } else { "" }
            );
        })
        .invoke_handler(tauri::generate_handler![
            commands::attach,
            commands::start_turn,
            commands::pick_workspace,
            commands::set_workspace,
            commands::cancel_turn,
            commands::respond_approval,
            commands::respond_question,
            commands::set_mode,
            commands::set_reasoning,
            commands::set_persona,
            commands::tools_view,
            commands::titlegen_status,
            commands::list_models,
            commands::migration_notes,
            commands::gateway_status,
            commands::gateway_start,
            commands::gateway_stop,
            commands::set_model,
            commands::session_state,
            commands::list_sessions,
            commands::search_messages,
            commands::session_messages,
            commands::share_markdown,
            commands::save_markdown_file,
            commands::search_settings,
            commands::set_search_backend,
            commands::search_probe,
            commands::set_context_budget,
            commands::open_file,
            commands::new_session,
            commands::list_workspaces,
            commands::add_workspace,
            commands::remove_workspace,
            commands::set_workspace_label,
            commands::rename_session,
            commands::set_session_persona,
            commands::compact,
            commands::delete_session,
            commands::clear_session,
            commands::switch_session,
            commands::selftest_armed,
            commands::gpu_report,
            commands::clipboard_image,
            commands::selftest_stage_clipboard,
            commands::render_report,
            commands::boot_log
        ]);

    if let Err(e) = app.run(tauri::generate_context!()) {
        eprintln!("[cwoffee] error while running: {e}");
        std::process::exit(1);
    }
}

/// Start a `pi --mode rpc` core for the desktop.
///
/// The `CWOFFEE_PI_*` variables are development switches for this experiment: the
/// packaging decisions (S2.5 for extensions, S4.1 for the pinned runtime) are what
/// replace them. Until then the boot line states exactly what was mounted, because a
/// guardrail that is silently off is the failure mode worth making visible.
async fn pi_host(
    appcfg: &Config,
    cfg: &HostConfig,
    store: &Arc<Store>,
    workspace: &std::path::Path,
    model: &str,
    persona: cwoffee_engine::host::Persona,
) -> Result<cwoffee_bridge::PiHost, String> {
    if model.trim().is_empty() {
        return Err(
            "a pi core needs a model id: pi selects by provider+model and does not adopt an endpoint's first model"
                .into(),
        );
    }
    let env_path = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty()).map(PathBuf::from);
    let bin = env_path("CWOFFEE_PI").unwrap_or_else(|| PathBuf::from("pi"));
    let agent_dir = env_path("CWOFFEE_PI_AGENT_DIR").unwrap_or_else(|| {
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
            });
        base.join("cwoffee").join("pi")
    });
    let session_dir = env_path("CWOFFEE_PI_SESSION_DIR").unwrap_or_else(|| agent_dir.join("sessions"));
    std::fs::create_dir_all(&session_dir).map_err(|e| format!("cannot create {}: {e}", session_dir.display()))?;

    // Never overwrite a provider the user or their own pi setup already put there. The
    // dev file exists so the experiment runs against cwoffee's own config; keeping an
    // existing one is what lets a real pi login be used instead.
    let models = agent_dir.join("models.json");
    // One probe against the endpoint, through the same call the Rust core uses, and
    // the window it states goes into pi's model entry and into the host state. Without
    // it there is no denominator: the metrics panel can only say "no window reported",
    // and a context meter with no ceiling is a gauge with no numbers on it.
    let window = match appcfg.build_provider() {
        Ok(p) => cwoffee_bridge::mount::probe_window(p.as_ref(), model).await,
        Err(e) => {
            println!("[cwoffee/pi] no window probe: {e}");
            None
        }
    };
    println!(
        "[cwoffee/pi] context window for {model}: {}",
        window.map(|w| w.to_string()).unwrap_or_else(|| "the endpoint did not say".into())
    );
    // Skipping the whole file because it exists used to be the rule, and it cost an
    // attachment: the first entry written had no `input`, and pi drops image blocks for a
    // model it thinks is text-only, without saying so. Now the entry this app owns is
    // refreshed every boot (merge, not clobber — see `mount::write_dev_provider`), and only
    // an explicit CWOFFEE_PI_KEEP_MODELS keeps cwoffee out of the file.
    if std::env::var("CWOFFEE_PI_KEEP_MODELS").is_ok() {
        println!(
            "[cwoffee/pi] CWOFFEE_PI_KEEP_MODELS is set: leaving {} alone. pi's entry, not cwoffee's, decides the window and which modalities a picture may travel through.",
            agent_dir.display()
        );
    } else {
        let key = appcfg.api_key().unwrap_or_else(|| "unused".into());
        let path = cwoffee_bridge::mount::write_dev_provider(
            &agent_dir,
            PI_PROVIDER,
            &appcfg.provider.base_url,
            model,
            &key,
            cwoffee_bridge::mount::pi_api_for_kind(&appcfg.provider.kind),
            window,
        )
        .map_err(|e| format!("cannot write {}: {e}", models.display()))?;
        println!("[cwoffee/pi] provider cwoffee -> {} (written to {})", appcfg.provider.base_url, path.display());
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
                    model,
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
    }

    let srcs: Vec<PathBuf> = std::env::var("CWOFFEE_PI_EXT")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let exts = cwoffee_bridge::mount::mount_extensions(&agent_dir, &bin, &srcs)
        .map_err(|e| format!("mounting extensions failed: {e}"))?;
    let ext_count = exts.len();

    // pi's own coding tools stay off: the tools that matter here are cwoffee's, and the
    // sandbox and approvals live with them. CWOFFEE_PI_TOOLS exists to add ours, not to
    // drop the minus signs by omission.
    let tools: Vec<String> = match std::env::var("CWOFFEE_PI_TOOLS") {
        Ok(v) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Err(_) => vec!["-read".into(), "-bash".into(), "-edit".into(), "-write".into()],
    };
    let mut extra_env = Vec::new();
    if let Ok(v) = std::env::var("CWOFFEE_SHIM") {
        extra_env.push(("CWOFFEE_SHIM".to_string(), v));
    }
    let pinned = if std::env::var("CWOFFEE_PI_ANY").is_ok() {
        None
    } else {
        Some(cwoffee_bridge::process::PINNED_PI.to_string())
    };

    let host = cwoffee_bridge::PiHost::spawn(cwoffee_bridge::PiHostConfig {
        launch: cwoffee_bridge::PiLaunch {
            bin,
            workdir: workspace.to_path_buf(),
            provider: "cwoffee".into(),
            model: model.to_string(),
            agent_dir: agent_dir.clone(),
            session_dir,
            session_file: None,
            extensions: exts,
            tools: tools.clone(),
            // Chat shaping is the bridge's, per process: a chat starts with no
            // extension mounted and --no-tools.
            no_tools: false,
            system_prompt: None,
            extra_env,
            pinned_version: pinned,
        },
        store: Arc::clone(store) as Arc<dyn StoreOps>,
        mode: cfg.mode,
        reasoning: cfg.reasoning,
        approval_timeout: cfg.approval_timeout,
        question_timeout: cfg.question_timeout,
        idle_reap: std::time::Duration::from_secs(
            std::env::var("CWOFFEE_PI_IDLE_REAP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        ),
        context_budget: cwoffee_bridge::mount::context_budget(cfg.context_budget, window.map(|w| w as usize)),
        budget_pin: cfg.context_budget,
        model_window: window.map(|w| w as usize),
        session_map: agent_dir.join("cwoffee-pi-sessions.json"),
        persona,
        // The tab the user clicks writes `persona` back to the same config file the
        // app booted from, so the mode is a property of the install, not of one window.
        cfg_path: cfg.cfg_path.clone(),
        max_tool_iters: appcfg.max_tool_iters(),
        compact_timeout: appcfg.compact_timeout(),
    })
    .await
    .map_err(|e| format!("{e}"))?;
    println!(
        "[cwoffee/pi] core pi {} up in {}; {} extension(s); tools {}",
        host.pi_version,
        workspace.display(),
        ext_count,
        if tools.is_empty() { "pi defaults".to_string() } else { tools.join(",") }
    );
    Ok(host)
}
