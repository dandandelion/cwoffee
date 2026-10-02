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
    pub pending_approvals: Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>,
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
        Some(r) => r.send(answer).map_err(|_| "turn already ended".to_string()),
        None => Ok(()),
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
) -> Result<Vec<cowry_engine::store::StoredMessage>, String> {
    state.store.messages(id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn new_session(state: tauri::State<'_, AppState>, title: String) {
    state.host.send(HostCmd::NewSession { title });
}

#[tauri::command]
pub fn switch_session(state: tauri::State<'_, AppState>, id: i64) {
    state.host.send(HostCmd::SwitchSession { id });
}

/// Report which GPU this process actually ended up on (PLAN.md §12 check).
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
pub fn render_report(report: RenderReport) {
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
