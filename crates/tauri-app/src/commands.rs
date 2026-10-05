//! Tauri commands. Kept thin on purpose: they start engine work and hand back a
//! channel. No business logic here — if it matters, it belongs in `crates/engine`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode};
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{HostCmd, HostState, SessionHost};
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
    pub host: Arc<SessionHost>,
    pub store: Arc<Store>,
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
    if text.trim().is_empty() && atts.is_empty() {
        return Err("empty message".into());
    }
    state.host.send_with(text, atts);
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
    option: Option<usize>,
) -> Result<(), String> {
    use cwoffee_engine::tools::ask::Answer;
    let reply = state.pending_questions.lock().unwrap().remove(&id);
    match reply {
        Some(r) => {
            eprintln!(
                "[cwoffee] question {id} answered by {}",
                option.map(|i| format!("option {}", i + 1)).unwrap_or_else(|| "free text".into())
            );
            r.send(Answer { text, option })
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
pub fn new_session(state: tauri::State<'_, AppState>, title: String) -> Result<i64, String> {
    let st = state.host.state();
    let id = state
        .store
        .create_session(&title, &st.workspace, &st.model)
        .map_err(|e| e.to_string())?;
    state.host.send(HostCmd::SwitchSession { id });
    Ok(id)
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
    state
        .host
        .provider()
        .list_models_info()
        .await
        .inspect_err(|e| eprintln!("[cwoffee] list_models: {e}"))
        .map(Ok)
        .unwrap_or_else(|_| Ok(Vec::new()))
}

#[tauri::command]
pub fn set_model(state: tauri::State<'_, AppState>, model: String) {
    state.host.send(HostCmd::SetModel { model });
}

#[tauri::command]
pub fn rename_session(state: tauri::State<'_, AppState>, id: i64, title: String) -> Result<(), String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("session title cannot be empty".into());
    }
    state.store.rename_session(id, &title).map_err(|e| e.to_string())
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
