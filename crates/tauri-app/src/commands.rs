//! Tauri commands. Kept thin on purpose: they start engine work and hand back a
//! channel. No business logic here — if it matters, it belongs in `crates/engine`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode};
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{HostCmd, HostOps, HostState};
use cwoffee_engine::provider::Reasoning;
use cwoffee_engine::store::{SearchHit, Session, Store};
use tauri::ipc::Channel;
use tauri::State;
use tokio::sync::{mpsc, oneshot};

use crate::gpu;

/// Shared shell state.
///
/// Event flow, one direction only:
///
///   engine ──(pump, started at boot)──▶ `queue_tx` ──▶ `queue_rx` ──▶ Channel ──▶ webview
///
/// The pump runs whether or not a UI is attached, so nothing is lost during
/// startup and nothing is lost if the webview reloads. `attach` wires the last
/// hop exactly once and reuses that Channel for every turn — PLAN.md §1.4
/// forbids an `invoke()` per token, and a long-lived channel is how we honour it.
pub struct AppState {
    /// Shared so the gateway can drive the *same* host the window drives.
    /// `dyn HostOps`, not `SessionHost`, because the core behind it is now a choice:
    /// the Rust engine or a `pi --mode rpc` child (PI-CORE-PLAN decision 12). Every
    /// verb the window uses is on the trait, which is the whole point of Move 0.
    pub host: Arc<dyn HostOps>,
    pub store: Arc<Store>,
    /// A note straight into the webview queue. The quit-while-busy refusal uses it,
    /// and it is the only way to say something to the window without editing the UI:
    /// notes are already a rendered concept.
    pub note_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Taken by `attach` exactly once and forwarded to the webview.
    pub queue_rx: Mutex<Option<mpsc::UnboundedReceiver<EngineEvent>>>,
    /// Open approval requests, waiting on an answer from the UI.
    /// Shared with the startup pump. It used to be a plain `Mutex<HashMap>` here
    /// while the pump inserted into a separate `Arc<Mutex<HashMap>>` of its own,
    /// so every answer looked up an empty map and every gated tool call failed
    /// closed - approvals had never worked in the desktop app.
    pub pending_approvals: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>>,
    /// ask_user round trips. Shared with the gateway handle, so a question asked
    /// while the phone is open can be answered from either side - first answer
    /// wins, and the other view closes its card on the QuestionAnswered event.
    pub pending_questions:
        Arc<Mutex<HashMap<u64, oneshot::Sender<cwoffee_engine::tools::ask::Answer>>>>,
    /// Set the first time the page reports its own DOM state. A webview that
    /// never paints is otherwise indistinguishable from one that is fine.
    pub rendered: Arc<std::sync::atomic::AtomicBool>,
    /// What the cowry -> cwoffee directory migration did, or why it declined.
    /// Shown in the UI: "left both alone" in a log file the user never opens is
    /// indistinguishable from "your history is gone".
    pub migration_notes: Vec<String>,
    /// Owns the LAN gateway task, so the UI can show and change whether remote
    /// access is on. See gateway_ctl.
    pub gateway: std::sync::Arc<crate::gateway_ctl::GatewayCtl>,
    /// The config file the app actually read (or where it would read one). The
    /// search panel writes back to this, because a provider that resets on restart
    /// is a provider that was never enabled.
    pub config_path: std::path::PathBuf,
}

/// Is remote access on, on what address, and how many devices are attached.
///
/// Deliberately a Tauri-only command: the payload contains the token inside the
/// ready-to-open URLs, and the gateway's own RPC allowlist does not expose this.
/// A remote client learns the gateway is up from the fact that it is talking to
/// it; it never learns the token from here.
#[tauri::command]
pub fn gateway_status(state: tauri::State<'_, AppState>) -> crate::gateway_ctl::GatewayStatus {
    state.gateway.status()
}

/// Turn remote access on. `lan` = bind every interface, which requires a token;
/// with no token anywhere this refuses rather than exposing a tool-using agent.
#[tauri::command]
pub async fn gateway_start(
    state: tauri::State<'_, AppState>,
    port: Option<u16>,
    lan: Option<bool>,
) -> Result<crate::gateway_ctl::GatewayStatus, String> {
    let port = port.unwrap_or(8099);
    let bind: std::net::IpAddr = if lan.unwrap_or(false) {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv4Addr::LOCALHOST.into()
    };
    let (drive, source) = crate::gateway_ctl::drive_token();
    let watch = std::env::var("CWOFFEE_GATEWAY_WATCH_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    state.gateway.start(bind, port, drive, watch, source).await
}

/// Turn it off. Aborting the task drops the listener and every connection on
/// it; clients reconnect when it is turned back on.
#[tauri::command]
pub fn gateway_stop(state: tauri::State<'_, AppState>) -> Result<crate::gateway_ctl::GatewayStatus, String> {
    state.gateway.stop()
}

/// Register the webview's event channel. Called once at startup.
#[tauri::command]
pub fn attach(state: tauri::State<'_, AppState>, on_event: Channel<EngineEvent>) -> Result<(), String> {
    let rx = state
        .queue_rx
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| "event channel already attached".to_string())?;
    // Reaching here proves the page's JS ran: it is the first call the app makes.
    // Without it a blank window is indistinguishable from a healthy one.
    eprintln!("[cwoffee] webview attached: page JS is running");

    tauri::async_runtime::spawn(async move {
        let mut rx = rx;
        while let Some(ev) = rx.recv().await {
            // A send error means the webview went away. Stop forwarding; the
            // gate's timeout then refuses any further tool call, so a dead UI
            // cannot become an unattended agent.
            if on_event.send(ev).is_err() {
                eprintln!("[cwoffee] event channel closed; detaching webview");
                break;
            }
        }
    });
    Ok(())
}

/// An attachment as the webview submits it. The engine re-validates mime, size
/// and base64 at its own boundary; this only refuses the obviously absurd before
/// it crosses IPC.
#[derive(Debug, serde::Deserialize)]
pub struct AttachmentIn {
    pub mime: String,
    pub data_b64: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Send a user message. Events stream over the channel registered by `attach`.
#[tauri::command]
pub fn start_turn(
    state: tauri::State<'_, AppState>,
    text: String,
    attachments: Option<Vec<AttachmentIn>>,
    // Which conversation the sender is looking at. The engine refuses a turn whose
    // session does not match the one it has active, so a second client - a phone, a
    // test harness - cannot write into the desktop's open conversation.
    session: Option<i64>,
) -> Result<(), String> {
    let atts: Vec<cwoffee_engine::provider::Attachment> = attachments
        .unwrap_or_default()
        .into_iter()
        .map(|a| cwoffee_engine::provider::Attachment {
            mime: a.mime,
            data_b64: a.data_b64,
            name: a.name,
        })
        .collect();
    // Audit line, same idea as the gateway's: a turn that nobody remembers
    // starting should be traceable to whoever started it. Lengths, not content.
    eprintln!(
        "[cwoffee] start_turn (desktop): session={:?} text={} attachments={}",
        session,
        text.chars().count(),
        atts.len()
    );
    if text.trim().is_empty() && atts.is_empty() {
        return Err("empty message".into());
    }
    state.host.send_turn_to(text, atts, session);
    Ok(())
}

/// Open a native folder picker. Non-blocking: the plugin hands the result to a
/// callback, so the command awaits it rather than parking the main thread.
/// Turn the picker's result into a filesystem path.
///
/// `FilePath` is either a `PathBuf` or a URL, and which one it is depends on the
/// platform file chooser. Under Wayland/GNOME the portal hands back
/// `file:///home/...`, and `FilePath`'s `Display` prints that URL verbatim - so
/// `to_string()` produced a string that is not a path, `PathBuf::from` kept the
/// `file://` prefix, `is_dir()` failed, and the engine refused a folder the user
/// had just picked. That is the whole "I chose a folder and it went back to
/// cwoffee-workspace" report.
fn picked_to_path(picked: tauri_plugin_dialog::FilePath) -> Option<String> {
    use tauri_plugin_dialog::FilePath;
    match picked {
        FilePath::Path(p) => Some(p.to_string_lossy().into_owned()),
        FilePath::Url(u) => u
            .to_file_path()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    }
}

#[tauri::command]
pub async fn pick_workspace(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .pick_folder(move |p| {
            // Which variant the portal hands back is platform-dependent and was
            // the entire bug, so record it: "I picked a folder and nothing
            // happened" is otherwise unfixable from a log.
            eprintln!(
                "[cwoffee] folder pick: {}",
                match &p {
                    Some(tauri_plugin_dialog::FilePath::Path(_)) => "path",
                    Some(tauri_plugin_dialog::FilePath::Url(_)) => "url",
                    None => "nothing",
                }
            );
            let _ = tx.send(p.and_then(picked_to_path));
        });
    // A dialog the user never closes must not pin a task forever.
    tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "folder picker timed out".to_string())?
        .map_err(|_| "folder picker closed without a result".to_string())
}

/// Move the tools to a different workspace. The engine refuses a non-directory,
/// a non-writable directory, and any change while a turn is running.
#[tauri::command]
pub async fn set_workspace(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    // Awaited rather than fire-and-forget: the caller needs to know whether the
    // engine accepted the directory, otherwise the UI shows a workspace that is
    // not the one the tools are using.
    state.host.set_workspace_checked(&path).await
}

#[tauri::command]
pub fn cancel_turn(state: tauri::State<'_, AppState>) {
    // Drop the parked question senders first. The tool holding the turn wakes on
    // the channel closing and tells the model the question was cancelled; if we
    // only cancelled the turn, the tool would sit until its timeout holding a
    // turn the user already stopped.
    let queued: Vec<u64> = state
        .pending_questions
        .lock()
        .unwrap()
        .keys()
        .copied()
        .collect();
    if !queued.is_empty() {
        let mut map = state.pending_questions.lock().unwrap();
        for id in &queued {
            map.remove(id);
        }
        eprintln!("[cwoffee] cancelled with {} question(s) pending", queued.len());
    }
    // Same for approvals. The engine now also releases a parked approval when the
    // cancel flag lands, so a front end that forgets this is not broken - but the
    // sender in hand releases the tool immediately instead of on the next poll, and
    // the count is worth logging: "I pressed stop and nothing happened" is the most
    // annoying possible bug to report without a trace.
    let parked: Vec<u64> = state
        .pending_approvals
        .lock()
        .unwrap()
        .keys()
        .copied()
        .collect();
    if !parked.is_empty() {
        let mut map = state.pending_approvals.lock().unwrap();
        for id in &parked {
            map.remove(id);
        }
        eprintln!("[cwoffee] cancelled with {} approval(s) pending", parked.len());
    }
    state.host.cancel();
}

/// Answer a question the model asked.
///
/// `option` is the index of the choice clicked, or None for a typed answer; the
/// distinction is reported to the model so it knows whether it got a choice from
/// its own list or something the user wrote.
#[tauri::command]
pub fn respond_question(
    state: tauri::State<'_, AppState>,
    id: u64,
    text: String,
    options: Vec<usize>,
) -> Result<(), String> {
    use cwoffee_engine::tools::ask::Answer;
    let reply = state.pending_questions.lock().unwrap().remove(&id);
    match reply {
        Some(r) => {
            let picked = if options.is_empty() {
                "free text".to_string()
            } else {
                options
                    .iter()
                    .map(|i| format!("{}", i + 1))
                    .collect::<Vec<_>>()
                    .join("+")
            };
            eprintln!("[cwoffee] question {id} answered by {picked}");
            r.send(Answer { text, options })
                .map_err(|_| "turn already ended".to_string())
        }
        None => {
            eprintln!(
                "[cwoffee] question {id} answer dropped: no pending question with that id (already answered, timed out, or the turn ended)"
            );
            Err(format!("no pending question with id {id}"))
        }
    }
}

/// Answer an approval prompt.
///
/// An unknown id is not surfaced as an error: the usual cause is the UI answering
/// after the turn already ended, and the gate has already failed closed by then.
#[tauri::command]
pub fn respond_approval(
    state: tauri::State<'_, AppState>,
    id: u64,
    answer: ApprovalAnswer,
) -> Result<(), String> {
    let reply = state.pending_approvals.lock().unwrap().remove(&id);
    match reply {
        Some(r) => {
            eprintln!("[cwoffee] approval {id} answered {answer:?}");
            r.send(answer).map_err(|_| "turn already ended".to_string())
        }
        // Previously silent. A click that reaches this path with no parked sender
        // means the answer was lost, and the gate then fails closed after the full
        // timeout - which reads to the user as "I allowed it and it ran anyway".
        None => {
            eprintln!(
                "[cwoffee] approval {id} answer dropped: no pending request with that id (already answered, timed out, or the turn ended)"
            );
            Err(format!("no pending approval with id {id}"))
        }
    }
}

#[tauri::command]
pub fn set_mode(state: tauri::State<'_, AppState>, mode: String) -> Result<(), String> {
    let m = ApprovalMode::parse(&mode).ok_or_else(|| format!("unknown mode {mode:?}"))?;
    state.host.send(HostCmd::SetMode { mode: m });
    Ok(())
}

/// The tool list for the settings screen, straight from the Rust registry the tools run
/// from - on either core. Read-only: there is no per-tool permission to set yet, and the
/// screen says so instead of showing a switch that goes nowhere.
#[tauri::command]
pub fn tools_view(state: tauri::State<'_, AppState>) -> Vec<cwoffee_engine::host::ToolView> {
    let _ = &state;
    cwoffee_engine::host::tool_views()
}

/// Cowork or chat. Both cores answer through the same verb: the pi bridge restarts the
/// model process between turns, the rust core refuses in words. `session_state` is what
/// the window reads back, so the tab cannot end up claiming a mode the host did not take.
#[tauri::command]
pub fn set_persona(state: tauri::State<'_, AppState>, persona: String) -> Result<String, String> {
    let p = cwoffee_engine::host::Persona::parse(&persona)
        .ok_or_else(|| "persona: write cowork or chat".to_string())?;
    state.host.set_persona(p)
}

#[tauri::command]
pub fn set_reasoning(state: tauri::State<'_, AppState>, level: String) -> Result<(), String> {
    let r = match level.as_str() {
        "off" => Reasoning::Off,
        "low" => Reasoning::Low,
        "medium" => Reasoning::Medium,
        "high" => Reasoning::High,
        "xhigh" => Reasoning::XHigh,
        other => return Err(format!("unknown reasoning level {other:?}")),
    };
    state.host.send(HostCmd::SetReasoning { reasoning: r });
    Ok(())
}

#[tauri::command]
pub fn session_state(state: tauri::State<'_, AppState>) -> HostState {
    state.host.state()
}

#[tauri::command]
pub fn list_sessions(state: tauri::State<'_, AppState>) -> Result<Vec<Session>, String> {
    state.store.list_sessions(50).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn search_messages(state: tauri::State<'_, AppState>, query: String) -> Result<Vec<SearchHit>, String> {
    state.store.search(&query, 25).map_err(|e| e.to_string())
}

/// Open a produced file with the desktop's default handler.
///
/// The containment check lives here, not in the plugin: the Rust-side
/// `open_path` bypasses the opener plugin's JS scope entirely, so the only
/// thing standing between a model-authored path and `~/.ssh/id_rsa` is this
/// confine() call. Paths are workspace-relative on the wire for the same
/// reason - an absolute path in a tool result would be an invitation.
#[tauri::command]
pub fn open_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;
    let ws = state.host.state().workspace;
    let full = cwoffee_engine::tools::path::confine(std::path::Path::new(&ws), &path)
        .map_err(|e| e.to_string())?;
    let md = std::fs::metadata(&full).map_err(|_| format!("{path} no longer exists"))?;
    if !md.is_file() {
        return Err(format!("{path} is not a file"));
    }
    app.opener()
        .open_path(full.display().to_string(), None::<&str>)
        .map_err(|e| format!("could not open {path}: {e}"))?;
    Ok(full.display().to_string())
}

#[tauri::command]
/// The document behind "Share as Markdown": a suggested name and the text, with
/// nothing written anywhere. It used to write into `<workspace>/exports/` and
/// return an `OutputFile`, which made a share look like model output - a chip in
/// the conversation, a file in the folder the tools work in, and a later turn
/// tidying up a document nobody asked for.
///
/// It is still a command rather than UI-side string building: the UI is not the
/// only client, and the document has to be the same object over the gateway, where
/// there is no engine-side view of the conversation at all.
pub fn share_markdown(
    state: tauri::State<'_, AppState>,
    session: i64,
) -> Result<cwoffee_engine::share::ShareDoc, String> {
    let row = state
        .store
        .session(session)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("session {session} does not exist"))?;
    let rows = state.store.messages(session).map_err(|e| e.to_string())?;
    Ok(cwoffee_engine::share::share_document(&row.title, &row.workspace, &rows))
}

/// Ask the person where the shared document should go, then write it there.
///
/// Desktop-only by construction: a browser page cannot put a file wherever it
/// likes, so over the gateway the same Share click downloads instead (see the UI).
/// The dialog result is `None` when the person cancels, which is a real answer and
/// not an error.
#[tauri::command]
pub async fn save_markdown_file(
    app: tauri::AppHandle,
    name: String,
    contents: String,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&name)
        .save_file(move |p| {
            eprintln!(
                "[cwoffee] save dialog: {}",
                match &p {
                    Some(tauri_plugin_dialog::FilePath::Path(_)) => "path",
                    Some(tauri_plugin_dialog::FilePath::Url(_)) => "url",
                    None => "cancelled",
                }
            );
            let _ = tx.send(p.and_then(picked_to_path));
        });
    let chosen = tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "save dialog timed out".to_string())?
        .map_err(|_| "save dialog closed without a result".to_string())?;
    let Some(path) = chosen else {
        return Ok(None);
    };
    std::fs::write(&path, contents.as_bytes()).map_err(|e| format!("cannot write {path}: {e}"))?;
    eprintln!("[cwoffee] share: wrote {path}");
    Ok(Some(path))
}

/// Web search settings. The logic lives in the engine (`settings`), because the
/// phone's browser page has the same panel and must not get a second, drifting
/// implementation of "which search is on". These are the desktop's thin calls:
/// the config path comes from the shell, the answer comes from the engine.
#[tauri::command]
pub async fn search_settings(
    state: tauri::State<'_, AppState>,
) -> Result<cwoffee_engine::settings::SearchView, String> {
    Ok(state.host.search_view(&state.config_path).await)
}

#[tauri::command]
pub async fn set_search_backend(
    state: tauri::State<'_, AppState>,
    backend: String,
    searxng_url: Option<String>,
) -> Result<cwoffee_engine::settings::SearchView, String> {
    state
        .host
        .set_search_backend(&state.config_path, &backend, &searxng_url.unwrap_or_default())
        .await
}

#[tauri::command]
pub async fn search_probe(
    state: tauri::State<'_, AppState>,
    query: Option<String>,
) -> Result<cwoffee_engine::settings::SearchProbe, String> {
    state.host.search_probe(query).await
}


#[tauri::command]
pub fn session_messages(
    state: tauri::State<'_, AppState>,
    id: i64,
    before: Option<i64>,
    limit: Option<usize>,
) -> Result<cwoffee_engine::store::MessagesPage, String> {
    // Bounded on purpose. A session can hold thousands of messages; loading all
    // of them costs memory and scroll performance for content nobody is reading.
    state
        .store
        .messages_page(id, before, limit.unwrap_or(80))
        .map_err(|e| e.to_string())
}

#[tauri::command]
/// Create a session row immediately and return its id.
///
/// It used to be a message queued to the host, which meant that during a turn
/// the UI had no id to show: the click appeared to do nothing, and the session
/// the user was in seemed to vanish. The host is still told to move there, so
/// the next message lands in the new session once the current turn finishes.
pub fn new_session(
    state: tauri::State<'_, AppState>,
    title: String,
    workspace: Option<String>,
) -> Result<i64, String> {
    let st = state.host.state();
    // The sidebar can open a session inside a particular folder. The engine, not
    // the UI, decides whether that folder is usable, and switching to the row
    // moves the tools there - a session filed under a folder that then runs its
    // tools somewhere else is a lie in the list and a surprise on disk.
    let ws = match workspace.as_deref() {
        Some(raw) => cwoffee_engine::host::check_workspace(raw)
            .map_err(|e| format!("{raw:?}: {e}"))?,
        None => std::path::PathBuf::from(&st.workspace),
    };
    let ws = ws.display().to_string();
    let id = state
        .store
        .create_session(&title, &ws, &st.model)
        .map_err(|e| e.to_string())?;
    state.store.add_workspace(&ws).ok();
    state.host.send(HostCmd::SwitchSession { id });
    Ok(id)
}

/// Folders the sidebar offers: everything registered, plus everything that holds
/// sessions. The engine owns the fact; this is the list.
#[tauri::command]
pub fn list_workspaces(state: tauri::State<'_, AppState>) -> Result<Vec<WorkspaceRow>, String> {
    let rows = state.store.list_workspaces().map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|w| WorkspaceRow {
            path: w.path,
            label: w.label,
            sessions: w.sessions,
        })
        .collect())
}

#[derive(serde::Serialize)]
pub struct WorkspaceRow {
    pub path: String,
    pub label: String,
    pub sessions: i64,
}

/// Add a folder to the sidebar. Validated by the engine, so the UI cannot offer a
/// path it would later refuse to run tools in.
#[tauri::command]
pub fn add_workspace(state: tauri::State<'_, AppState>, path: String) -> Result<String, String> {
    let p = cwoffee_engine::host::check_workspace(&path).map_err(|e| format!("{path:?}: {e}"))?;
    let p = p.display().to_string();
    state.store.add_workspace(&p).map_err(|e| e.to_string())?;
    eprintln!("[cwoffee] workspace added: {p}");
    Ok(p)
}

/// Name a workspace row. The path never changes: it is what the sandbox is
/// confined to, and renaming a folder in the sidebar must not move anyone's work.
#[tauri::command]
pub fn set_workspace_label(
    state: tauri::State<'_, AppState>,
    path: String,
    label: String,
) -> Result<bool, String> {
    let n = state
        .store
        .set_workspace_label(&path, &label)
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Drop a folder from the sidebar. Sessions that live in it are untouched, so it
/// comes back if they do.
#[tauri::command]
pub fn remove_workspace(state: tauri::State<'_, AppState>, path: String) -> Result<usize, String> {
    Ok(state.store.remove_workspace(&path).map_err(|e| e.to_string())?)
}

/// Is the in-app harness allowed to run?
///
/// The self-test bundle is baked into `ui/dist` by `scripts/selftest.sh`, and the
/// release binary embeds whatever `ui/dist` holds at build time. So a production
/// start right after a harness run ships a bundle that drives *whatever endpoint
/// and config the user has* - which is exactly what happened once: it opened
/// sessions in the real store, wrote `mode` and `reasoning` into the real
/// `config.toml`, and ran turns against the real model. The build-time flag alone
/// is not a gate; the harness also has to be armed by the process that launched
/// it, and a bundle that is not armed says so in the log instead of going quiet.
#[tauri::command]
pub fn selftest_armed() -> bool {
    std::env::var("CWOFFEE_SELFTEST").map(|v| v == "1").unwrap_or(false)
}

#[tauri::command]
pub fn switch_session(state: tauri::State<'_, AppState>, id: i64) {
    state.host.send(HostCmd::SwitchSession { id });
}

/// Model ids offered by the endpoint. A provider that exposes no list returns
/// an empty vec rather than an error: "no picker" is a normal state.
#[tauri::command]
pub async fn list_models(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<cwoffee_engine::provider::ModelInfo>, String> {
    // An endpoint with no /models is a normal configuration, not a failure the
    // user should see as an error banner.
    // On the trait, not `host.provider()`: a pi core has no cwoffee `Provider` to
    // hand out, and reaching past the seam here would have quietly pinned this
    // command to one core. `models_info` answers for both — the Rust core asks the
    // endpoint, the bridge answers from pi or from its own configuration.
    state
        .host
        .models_info()
        .await
        .inspect_err(|e| eprintln!("[cwoffee] list_models: {e}"))
        .map(Ok)
        .unwrap_or_else(|_| Ok(Vec::new()))
}

#[tauri::command]
pub fn set_model(state: tauri::State<'_, AppState>, model: String) {
    state.host.send(HostCmd::SetModel { model, persist: true });
}

#[tauri::command]
pub fn rename_session(state: tauri::State<'_, AppState>, id: i64, title: String) -> Result<(), String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("session title cannot be empty".into());
    }
    state.store.rename_session(id, &title).map_err(|e| e.to_string())
}

/// Say which mode a conversation belongs to, so the chat list can show chats and the cowork
/// sidebar can show folders. The store defaults a row to "cowork"; the client that created it
/// in chat mode says so here.
#[tauri::command]
pub fn set_session_persona(
    state: tauri::State<'_, AppState>,
    id: i64,
    persona: String,
) -> Result<(), String> {
    if persona != "chat" && persona != "cowork" {
        return Err("persona: write cowork or chat".into());
    }
    state
        .store
        .set_session_persona(id, &persona)
        .map_err(|e| e.to_string())
}

/// Fold the conversation down to a summary, on request. On the pi core this is pi's own
/// compaction, and the note quotes pi's own token counts; on the rust core the verb
/// refuses in words, because that core folds the prompt when it reaches its budget.
#[tauri::command]
pub async fn compact(
    state: tauri::State<'_, AppState>,
    instructions: Option<String>,
) -> Result<String, String> {
    state.host.compact(instructions).await
}

#[tauri::command]
pub fn delete_session(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    let st = state.host.state();
    if id == st.session_id && st.busy {
        // The agent appends to the active session; removing the row mid-turn
        // would either orphan the writes or trip the foreign key.
        return Err("cannot delete the active session while a turn is running".into());
    }
    if id == st.session_id {
        // Move the host off the row before deleting it, otherwise the next turn
        // writes into a session that no longer exists.
        //
        // Reuse an existing empty session when there is one. Always creating a
        // fresh session left an orphan row per delete, which accumulated into a
        // sidebar full of blank entries.
        let reuse = state
            .store
            .list_sessions(50)
            .ok()
            .and_then(|list| list.into_iter().find(|s| s.id != id && s.message_count == 0))
            .map(|s| s.id);
        match reuse {
            Some(sid) => state.host.send(HostCmd::SwitchSession { id: sid }),
            // Draft, not a fresh row: the replacement used to be an empty
            // session, which is exactly the blank entry that piled up.
            None => state.host.send(HostCmd::ClearSession),
        }
    }
    state.store.delete_session(id).map_err(|e| e.to_string())
}

/// Drop the host to draft state: no session row, empty prompt.
#[tauri::command]
pub fn clear_session(state: tauri::State<'_, AppState>) {
    state.host.send(HostCmd::ClearSession);
}

/// Page-side log line, forwarded into the same stream as the Rust output.
/// This is the only way to see a page-side failure in a release build, which is
/// exactly where the "blank window, no error" class of bug hides.
#[tauri::command]
pub fn boot_log(msg: String) {
    eprintln!("[cwoffee/page] {msg}");
}

/// Which GPU this process actually ended up on (PLAN.md §12 check).
#[tauri::command]
pub fn migration_notes(state: State<'_, AppState>) -> Vec<String> {
    state.migration_notes.clone()
}

#[tauri::command]
pub fn gpu_report() -> gpu::GpuReport {
    gpu::report()
}

/// A paste of an image that lives only in the system clipboard.
///
/// The page cannot read it: WebKitGTK gives the paste event no file and no image
/// item for a screenshot, and there is no `navigator.clipboard.read()` in an
/// embedded webview. So the shell reads the selection and hands back the same
/// shape a pasted file would have had. Always answers; `note` says why there is
/// no image when the user plainly copied one.
#[tauri::command]
pub async fn clipboard_image() -> crate::clipboard::ClipProbe {
    crate::clipboard::read_image().await
}

/// What the front end actually rendered.
///
/// This exists because every machine-checkable gate in M0 passed while the
/// webview was showing a stale vite error overlay. "Is it painted?" is not
/// answerable from process liveness, memory, or sockets — so the UI reports it.
#[derive(serde::Deserialize)]
pub struct RenderReport {
    pub titlebar: bool,
    pub sidebar: bool,
    pub footer: bool,
    pub stream_el: bool,
    /// True when the view is showing the draft/empty state. Reported because a
    /// hidden transcript is expected there, and a diagnostic that lies about it
    /// trains you to ignore the line.
    pub empty_mode: bool,
    pub body_bg: String,
    pub overlay_present: bool,
    pub text_len: usize,
    pub device_pixel_ratio: f64,
}

#[tauri::command]
pub fn render_report(state: tauri::State<'_, AppState>, report: RenderReport) {
    // The watchdog in main.rs keys off this; without it a blank window looks
    // healthy from every outside signal.
    state.rendered.store(true, std::sync::atomic::Ordering::SeqCst);
    let sane = report.titlebar
        && report.footer
        && !report.overlay_present
        && report.text_len > 0
        && report.body_bg != "rgba(0, 0, 0, 0)"
        && report.body_bg != "rgb(0, 0, 0)";
    eprintln!(
        "[cwoffee] render {}: titlebar={} sidebar={} footer={} stream={} empty={} overlay={} text={} bg={} dpr={:.2}",
        if sane { "OK" } else { "SUSPECT" },
        report.titlebar,
        report.sidebar,
        report.footer,
        report.stream_el,
        report.empty_mode,
        report.overlay_present,
        report.text_len,
        report.body_bg,
        report.device_pixel_ratio,
    );
}

#[cfg(test)]
mod tests {
    use super::picked_to_path;
    use std::path::PathBuf;
    use tauri_plugin_dialog::FilePath;

    /// Under Wayland/GNOME the desktop portal returns a URL, not a path, and
    /// `FilePath`'s `Display` prints that URL verbatim. `to_string()` was the
    /// whole bug: a folder the user had just picked became `file:///home/...`,
    /// which is not a directory, so the engine refused it and the workspace
    /// control snapped back to whatever the config said.
    #[test]
    fn a_portal_url_becomes_a_real_path() {
        let u = url::Url::parse("file:///home/dandandelion/Downloads").unwrap();
        assert_eq!(
            picked_to_path(FilePath::Url(u)).as_deref(),
            Some("/home/dandandelion/Downloads")
        );
    }

    #[test]
    fn a_plain_path_survives_unchanged() {
        let p = FilePath::Path(PathBuf::from("/home/dandandelion/cwoffee-workspace"));
        assert_eq!(
            picked_to_path(p).as_deref(),
            Some("/home/dandandelion/cwoffee-workspace")
        );
    }

    /// A non-file URL (Android `content://`) has no filesystem meaning here. The
    /// caller must see "nothing usable was picked" rather than a garbage path that
    /// the engine then rejects with a confusing message.
    #[test]
    fn a_non_file_url_is_not_a_path() {
        let u = url::Url::parse("content://media/external/images/1").unwrap();
        assert_eq!(picked_to_path(FilePath::Url(u)), None);
    }
}
