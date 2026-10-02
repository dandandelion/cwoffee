//! Tauri commands. Kept thin on purpose: they start engine work and hand back a
//! channel. No business logic here — if it matters, it belongs in `crates/engine`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cowry_engine::approval::{ApprovalAnswer, ApprovalMode};
use cowry_engine::event::EngineEvent;
use cowry_engine::host::{HostCmd, HostState, SessionHost};
use cowry_engine::provider::Reasoning;
use cowry_engine::store::{SearchHit, Session, Store};
use tauri::ipc::Channel;
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
    pub host: SessionHost,
    pub store: Arc<Store>,
    /// Taken by `attach` exactly once and forwarded to the webview.
    pub queue_rx: Mutex<Option<mpsc::UnboundedReceiver<EngineEvent>>>,
    /// Open approval requests, waiting on an answer from the UI.
    /// Shared with the startup pump. It used to be a plain `Mutex<HashMap>` here
    /// while the pump inserted into a separate `Arc<Mutex<HashMap>>` of its own,
    /// so every answer looked up an empty map and every gated tool call failed
    /// closed - approvals had never worked in the desktop app.
    pub pending_approvals: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>>,
    /// Set the first time the page reports its own DOM state. A webview that
    /// never paints is otherwise indistinguishable from one that is fine.
    pub rendered: Arc<std::sync::atomic::AtomicBool>,
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
    eprintln!("[cowry] webview attached: page JS is running");

    tauri::async_runtime::spawn(async move {
        let mut rx = rx;
        while let Some(ev) = rx.recv().await {
            // A send error means the webview went away. Stop forwarding; the
            // gate's timeout then refuses any further tool call, so a dead UI
            // cannot become an unattended agent.
            if on_event.send(ev).is_err() {
                eprintln!("[cowry] event channel closed; detaching webview");
                break;
            }
        }
    });
    Ok(())
}

/// Send a user message. Events stream over the channel registered by `attach`.
#[tauri::command]
pub fn start_turn(state: tauri::State<'_, AppState>, text: String) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("empty message".into());
    }
    state.host.submit(text);
    Ok(())
}

#[tauri::command]
pub fn cancel_turn(state: tauri::State<'_, AppState>) {
    state.host.cancel();
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
            eprintln!("[cowry] approval {id} answered {answer:?}");
            r.send(answer).map_err(|_| "turn already ended".to_string())
        }
        // Previously silent. A click that reaches this path with no parked sender
        // means the answer was lost, and the gate then fails closed after the full
        // timeout - which reads to the user as "I allowed it and it ran anyway".
        None => {
            eprintln!(
                "[cowry] approval {id} answer dropped: no pending request with that id                  (already answered, timed out, or the turn ended)"
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

#[tauri::command]
pub fn session_messages(
    state: tauri::State<'_, AppState>,
    id: i64,
    before: Option<i64>,
    limit: Option<usize>,
) -> Result<MessagePage, String> {
    // Bounded on purpose. A session can hold thousands of messages; loading all
    // of them costs memory and scroll performance for content nobody is reading.
    let limit = limit.unwrap_or(80).clamp(1, 400);
    // Fetch one extra row as a probe for `has_older` rather than a second COUNT.
    let mut page = state
        .store
        .messages_before(id, before, limit + 1)
        .map_err(|e| e.to_string())?;
    let has_older = page.len() > limit;
    if has_older {
        page.remove(0); // drop the probe, keeping the newest `limit`
    }
    Ok(MessagePage { messages: page, has_older })
}

/// One page of transcript history, newest window returned in reading order.
#[derive(serde::Serialize)]
pub struct MessagePage {
    pub messages: Vec<cowry_engine::store::StoredMessage>,
    pub has_older: bool,
}

#[tauri::command]
pub fn new_session(state: tauri::State<'_, AppState>, title: String) {
    state.host.send(HostCmd::NewSession { title });
}

#[tauri::command]
pub fn switch_session(state: tauri::State<'_, AppState>, id: i64) {
    state.host.send(HostCmd::SwitchSession { id });
}

/// Model ids offered by the endpoint. A provider that exposes no list returns
/// an empty vec rather than an error: "no picker" is a normal state.
#[tauri::command]
pub async fn list_models(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    // An endpoint with no /models is a normal configuration, not a failure the
    // user should see as an error banner.
    state
        .host
        .provider()
        .list_models()
        .await
        .inspect_err(|e| eprintln!("[cowry] list_models: {e}"))
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
            None => state.host.send(HostCmd::NewSession { title: "new session".into() }),
        }
    }
    state.store.delete_session(id).map_err(|e| e.to_string())
}

/// Page-side log line, forwarded into the same stream as the Rust output.
/// This is the only way to see a page-side failure in a release build, which is
/// exactly where the "blank window, no error" class of bug hides.
#[tauri::command]
pub fn boot_log(msg: String) {
    eprintln!("[cowry/page] {msg}");
}

/// Which GPU this process actually ended up on (PLAN.md §12 check).
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
        "[cowry] render {}: titlebar={} sidebar={} footer={} stream={} overlay={} text={} bg={} dpr={:.2}",
        if sane { "OK" } else { "SUSPECT" },
        report.titlebar,
        report.sidebar,
        report.footer,
        report.stream_el,
        report.overlay_present,
        report.text_len,
        report.body_bg,
        report.device_pixel_ratio,
    );
}
