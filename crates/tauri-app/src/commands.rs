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
    /// What the browser tool can do in this start, and how the person configured it. The
    /// settings screen reads it through `browser_status` rather than describing it from the
    /// UI's own guess: whether a run will paint a window on this desktop is a claim about the
    /// person's machine, and a screen that paraphrases that policy is a screen that drifts.
    pub browser_cfg: cwoffee_engine::tools::browser::BrowserCfg,
    /// The browser this app is **keeping**: one browser, held by this process, which the
    /// window's sidebar shows and the model's `browse` calls can act inside. `None` only when
    /// the kept browser is switched off or its driver is missing — and then the panel says
    /// that, rather than showing an empty frame and inviting a click that cannot work.
    pub browser: Option<std::sync::Arc<tokio::sync::Mutex<cwoffee_engine::browser_hub::Hub>>>,
    /// A synchronous read of the same browser's state, for verbs that only report it.
    pub browser_state: std::sync::Arc<std::sync::RwLock<cwoffee_engine::browser_hub::Snapshot>>,
    /// Set while `--page-probe` is checking where the page in the window sits. Two things can place
    /// that one widget — the panel's own layout and the probe — and whichever reported last wins, so a
    /// geometry claim would otherwise flake on whether the panel happened to redraw during it. The
    /// panel is not deafened permanently, only for the seconds the check runs.
    pub probe_holds_box: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// How this start was configured for kept browsers (driver path, idle clock, whether the
    /// person switched them off). Held here as well as inside the hub so a status verb can
    /// describe the *policy* without taking the lock a browser call is holding.
    pub browser_keep: cwoffee_engine::browser_hub::SessionCfg,
    /// Which folder a local page may be opened from, for the panel's address box. The workspace is
    /// the boundary the model's file tools already use, so the box uses it too — and it is asked of
    /// the engine rather than copied at startup, because the folder moves when the person switches
    /// conversation and this window does not. See `live_workspace`.
    pub host_for_browser: std::sync::Arc<dyn cwoffee_engine::host::HostOps>,
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
    /// The MCP hub, when this start has one. `None` on the pi core, where no server is
    /// mounted, and the status command says that rather than reporting zero servers as if the
    /// configuration were empty.
    pub mcp: Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
    /// How a tool call reaches those servers. This process on the Rust core; the app's hub
    /// over a socket on the pi core; and a sentence about a hub no tool process can reach when
    /// the socket did not open. A row reading "mounted" while no call can get there is exactly
    /// the claim this prevents.
    pub mcp_route: String,
}

/// What the browser tool will do in this start, in the tool's own words.
///
/// The model is an argument because the answer depends on it: whether the run paints a window
/// on the person's desktop is decided by that model's modalities, and the model can change
/// mid-session. A settings screen that asked once at boot would be describing a start that is
/// no longer happening.
#[tauri::command]
pub async fn browser_status(
    state: tauri::State<'_, AppState>,
    model: String,
) -> Result<serde_json::Value, String> {
    let mut cfg = state.browser_cfg.clone();
    let path = state.config_path.clone();
    cfg.vision = match cwoffee_engine::config::vision_in(&path, model.trim()) {
        Some(true) => cwoffee_engine::tools::browser::Vision::Yes,
        Some(false) => cwoffee_engine::tools::browser::Vision::No,
        None => cwoffee_engine::tools::browser::Vision::Unknown,
    };
    // Whether this start has a page of its own to show. It changes the answer about a window: with
    // the sidebar holding the browser, no walk paints one on this desktop, and a screen that said
    // otherwise would be describing a start that is not happening.
    let window = state
        .browser_state
        .read()
        .map(|g| g.native)
        .unwrap_or(false);
    let (headed, why, shot) = cwoffee_engine::tools::browser::decide(&cfg, "", window);
    Ok(serde_json::json!({
        "mounted": cfg.usable(),
        "driver": cfg.driver.as_ref().map(|p| p.display().to_string()),
        "node": cfg.node,
        "headed": headed,
        "why": why,
        // True when the page in this window *is* the browser a `browse` call gets. The settings row
        // has to say that, because "it will run headless" means something quite different once there
        // is no second browser to be headless about.
        "window_is_browser": window,
        "screenshot_default": shot,
        "vision": match cfg.vision {
            cwoffee_engine::tools::browser::Vision::Yes => "yes",
            cwoffee_engine::tools::browser::Vision::No => "no",
            cwoffee_engine::tools::browser::Vision::Unknown => "unknown",
        },
        "says": cwoffee_engine::tools::browser::status_line(&cfg, window),
        // The kept browser gets the engine's own sentence, not a paraphrase: a settings row
        // that invents its own wording for "does a browser stay open here" is how the panel
        // and the file eventually disagree.
        "keep": cwoffee_engine::browser_hub::status_line(
            &cwoffee_engine::browser_hub::SessionCfg::from_browser_cfg(&cfg),
            &cwoffee_engine::browser_hub::Hub::peek_shared(&state.browser_state),
        ),
        "keep_idle_secs": cwoffee_engine::browser_hub::SessionCfg::from_browser_cfg(&cfg).idle_secs,
    }))
}

/// The kept browser, as the sidebar sees it: what is open, who opened it, who touched it last,
/// how much of the idle clock is left, and what the page's own code complained about.
///
/// The payload is built by the engine (`browser_hub::status_payload`) for the same reason the
/// MCP payload is: the desktop and a phone tab hand-assembling one status object is how two
/// screens end up answering one question differently.
#[tauri::command]
pub async fn browser_panel(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    Ok(browser_payload(&state))
}

fn browser_payload(state: &tauri::State<'_, AppState>) -> serde_json::Value {
    use cwoffee_engine::browser_hub as bh;
    let snap = bh::Hub::peek_shared(&state.browser_state);
    match state.browser.as_ref() {
        // No lock taken: the state mirror exists so that reporting a browser never waits on
        // whoever is driving it, and the config half of the payload is held beside it. A
        // status verb that queued behind a page load would show a stale panel and call it live.
        Some(_) => bh::status_payload(&state.browser_keep, &snap, true, true, "this window's own browser"),
        None => serde_json::json!({
            "mounted": false,
            "alive": false,
            "opened": false,
            "writable": false,
            "url": "",
            "why": snap.why,
            "says": snap.says(),
            "console": [],
            "page_errors": [],
            "requests_failed": [],
            "downloads_blocked": [],
            "idle_secs": 0,
            "idle_left": null,
            "seq": snap.seq,
            "route": "no kept browser is mounted in this start",
        }),
    }
}

/// Which browser a page would open in, and why a panel might refuse to open one at all.
fn kept_hub(
    state: &tauri::State<'_, AppState>,
) -> Result<std::sync::Arc<tokio::sync::Mutex<cwoffee_engine::browser_hub::Hub>>, String> {
    match state.browser.as_ref() {
        Some(h) if state.browser_keep.off_reason.is_none() => Ok(std::sync::Arc::clone(h)),
        Some(_) => Err("kept browsers are switched off in this start ([browser] keep = false), \
                        so there is no browser to open a page in. The one-shot browser tool \
                        still works."
            .to_string()),
        None => Err("no kept browser is mounted here: `browser/session.mjs` is missing, so the \
                     panel has nothing to show. It comes with cwoffee; its dependencies come \
                     from `npm --prefix browser install` and a browser build from \
                     `npx playwright install chromium`."
            .to_string()),
    }
}

/// Open a page in the kept browser, on the person's own say-so.
///
/// No approval card: this is the person acting, not the model. What the person is entitled to
/// is the panel saying plainly whose browser it is and where the page went.
/// What the panel's address box means, turned into an address the browser can be held to.
///
/// A person types `example.com`, or `/home/me/site/index.html`, or `notes/todo.md` next to the
/// workspace, and expects the page to open. So a bare host gets a scheme, a path gets `file://`,
/// and a local page is only allowed **inside the workspace** — the same boundary the model's tool
/// enforces by approval. Refusing every local address made the box useless for the local pages
/// people actually write; refusing every address made it a way to read a home directory off a
/// text field. Both were the wrong side of the same line.
fn panel_target(raw: &str, workspace: &std::path::Path) -> Result<(String, bool), String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("nothing was typed".to_string());
    }
    let scheme = t.find("://").map(|i| (&t[..i]).to_lowercase());
    match scheme.as_deref() {
        Some("http") | Some("https") => Ok((t.to_string(), false)),
        Some("file") => {
            let path = std::path::PathBuf::from(t.trim_start_matches("file://"));
            confine_local(&path, workspace).map(|_| (t.to_string(), true))
        }
        Some(other) => Err(format!(
            "{t} is not a page this browser opens ({other}): addresses are http, https, or a file inside the workspace"
        )),
        None => {
            if t.starts_with('/') || t.starts_with('.') {
                let path = if t.starts_with('/') {
                    std::path::PathBuf::from(t)
                } else {
                    workspace.join(t)
                };
                confine_local(&path, workspace)?;
                Ok((format!("file://{}", path.display()), true))
            } else if t.contains('.') || t == "localhost" || t.starts_with("www.") {
                Ok((format!("https://{t}"), false))
            } else {
                Err(format!(
                    "{t} is not an address: type a site (example.com) or a file inside the workspace"
                ))
            }
        }
    }
}

/// A local page has to be inside the workspace, and the refusal names both sides of that line.
fn confine_local(path: &std::path::Path, workspace: &std::path::Path) -> Result<(), String> {
    let root = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let here = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if here.starts_with(&root) {
        Ok(())
    } else {
        Err(format!(
            concat!(
                "{} is outside the workspace ({}). This box opens a web address — example.com, or https://… — ",
                "or a page inside the workspace folder; a file elsewhere needs a server of your own"
            ),
            here.display(),
            root.display()
        ))
    }
}

/// Open a page in the kept browser, on the person's say-so.
#[tauri::command]
/// The folder the conversation is in, right now, for the panel's address box and the page box's
/// guard. One question to the engine that owns the answer; a copy taken at startup goes stale the
/// first time the person opens a conversation in another folder.
fn live_workspace(state: &tauri::State<'_, AppState>) -> std::path::PathBuf {
    std::path::PathBuf::from(state.host_for_browser.state().workspace)
}

#[tauri::command]
pub async fn browser_open(state: tauri::State<'_, AppState>, url: String) -> Result<serde_json::Value, String> {
    let workspace = live_workspace(&state);
    let (target, allow_file) = panel_target(&url, &workspace)?;
    let hub = kept_hub(&state)?;
    let out = {
        let mut h = hub.lock().await;
        h.open(&target, cwoffee_engine::browser_hub::Who::Person, allow_file).await
    };
    match out {
        Ok(_) => Ok(browser_payload(&state)),
        Err(e) => Err(e),
    }
}

/// A click, a keystroke, a scroll — from the person, into the kept browser.
#[tauri::command]
pub async fn browser_act(
    state: tauri::State<'_, AppState>,
    act: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let hub = kept_hub(&state)?;
    let out = {
        let mut h = hub.lock().await;
        h.act(act, cwoffee_engine::browser_hub::Who::Person).await
    };
    match out {
        Ok(v) => {
            let mut payload = browser_payload(&state);
            // The step's own answer travels with the state, so the panel can say "clicked
            // More" and mean it, rather than inferring it from what the page looks like after.
            payload["act_ok"] = v.get("ok").cloned().unwrap_or(serde_json::json!(true));
            if let Some(w) = v.get("why") {
                payload["act_why"] = w.clone();
            }
            // What the act answered, when it answered with something. The panel does not paint a
            // raw page dump, but it does say "the page gave back N" for a read, and a check that
            // wants the page's own words (did my click land? did my keys arrive?) asks the page
            // rather than trusting the panel's picture.
            if let Some(val) = v.get("value") {
                payload["act_value"] = val.clone();
            }
            if let Some(at) = v.get("at") {
                payload["act_at"] = at.clone();
            }
            Ok(payload)
        }
        Err(e) => Err(e),
    }
}

/// A picture of the page, only if there is a newer one than the caller already holds.
///
/// The `since` comparison is the whole design of this verb: a panel that polls every second
/// should not pay a screenshot every second, and a screenshot of a page nobody changed is
/// bytes for a picture of the same pixels. What it does owe is any frame the browser drew on its
/// own — that is the live view, and a caller learns about it by holding an older picture id.
#[tauri::command]
pub async fn browser_frame(
    state: tauri::State<'_, AppState>,
    since: u64,
) -> Result<serde_json::Value, String> {
    use cwoffee_engine::browser_hub as bh;
    let hub = kept_hub(&state)?;
    let snap = bh::Hub::peek_shared(&state.browser_state);
    if !snap.alive || !snap.opened {
        return Ok(serde_json::json!({"unchanged": true, "seq": snap.seq, "frame_pic": snap.frame_pic, "no_page": true}))
    }
    // Two numbers the caller must not confuse: `since` is the id of the *picture* this window is
    // drawing, `snap.seq` counts page state. A reload bumps the state without drawing anything, so
    // a caller that compared its picture against the state alone is told there is nothing newer —
    // while still showing the page as it was before the reload. `needs_pic` is the engine's rule
    // for both halves of that, and the gateway tab calls the same function.
    let (newest_pic, pictures_state) = {
        let h = hub.lock().await;
        (h.frame_pic(), h.frame_pictures_state(snap.seq))
    };
    if !bh::needs_pic(since, newest_pic, pictures_state) {
        return Ok(serde_json::json!({"unchanged": true, "seq": snap.seq, "frame_pic": newest_pic}))
    }
    let mut h = hub.lock().await;
    // A frame the browser volunteered is already here, so handing it over beats paying for a second
    // picture of the same pixels. That is what makes polling fast a sane thing to do.
    if let Some(f) = h.frame_now() {
        if f.pic > since {
            return Ok(serde_json::json!({
                "unchanged": false, "b64": f.b64, "mime": f.mime, "bytes": f.bytes,
                "seq": f.seq, "pic": f.pic, "live": f.live
            }));
        }
    }
    let png = false;
    match h.frame(png).await {
        Ok(f) => Ok(serde_json::json!({"unchanged": false, "b64": f.b64, "mime": f.mime, "bytes": f.bytes, "seq": f.seq, "pic": f.pic, "live": f.live})),
        Err(e) => Ok(serde_json::json!({"unchanged": true, "why": e, "seq": snap.seq})),
    }
}

/// Ask the kept browser for its live view — its own screencast, sent as the page repaints — or
/// stop asking.
///
/// A load note, not a permission: a screencast is work for the browser process, so it runs while a
/// panel is on screen and stops when the panel closes or the window is hidden. It does not touch
/// the idle clock, either: watching a browser is not the same as using one, and a session that
/// closes itself after `[browser] idle_secs` of nobody acting in it is meant to keep doing that
/// while somebody admires the page.
#[tauri::command]
pub async fn browser_watch(
    state: tauri::State<'_, AppState>,
    on: bool,
    max_width: Option<u32>,
) -> Result<serde_json::Value, String> {
    let hub = kept_hub(&state)?;
    let out = {
        let mut h = hub.lock().await;
        h.watch(on, max_width).await
    }?;
    let mut payload = browser_payload(&state);
    // Reported from the driver's answer, not from the ask: a Chromium with no screencast makes
    // "live" a promise this build cannot keep, and the panel has to say which view it is drawing.
    payload["live"] = serde_json::json!(
        out.get("live").and_then(|l| l.as_bool()).unwrap_or(false) && out.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)
    );
    Ok(payload)
}

/// Where the panel's box is, in CSS pixels from the top left of the window. Only meaningful for a
/// browser that is a page **in this window**: the page is a native rectangle, and the app's own
/// layout is the only thing that knows where the panel is. The panel measures itself and calls
/// this; nothing else decides where the page sits.
#[tauri::command]
pub async fn browser_view_rect(
    state: tauri::State<'_, AppState>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<serde_json::Value, String> {
    if state.probe_holds_box.load(std::sync::atomic::Ordering::SeqCst) {
        // The probe is asking where the page sits; a panel layout tick is not.
        return Ok(serde_json::json!({
            "ok": true,
            "ignored": "a page probe is holding the box for the duration of its geometry check"
        }));
    }
    let hub = kept_hub(&state)?;
    let out = {
        let mut h = hub.lock().await;
        h.view_rect(x, y, width, height).await
    }?;
    Ok(out)
}

/// What the page printed, threw, and failed to load — read from the hub's own record.
///
/// The page is watched the whole time it is open, so this is a memory read: it does not ask the page
/// anything, and it does not reset the idle clock. A drawer that polled the page would keep a
/// person's session alive by looking at it, which is the opposite of what the clock is for.
#[tauri::command]
pub async fn browser_log(
    state: tauri::State<'_, AppState>,
    since: Option<i64>,
    kind: Option<String>,
    level: Option<String>,
    limit: Option<u32>,
) -> Result<serde_json::Value, String> {
    let snap = cwoffee_engine::browser_hub::Hub::peek_shared(&state.browser_state);
    Ok(snap.diagnostics_value(
        since.unwrap_or(0),
        kind.as_deref(),
        level.as_deref(),
        limit.unwrap_or(80) as usize,
    ))
}

/// Reshape the page to the box it is being looked at, so a panel that spans the window gets a page
/// that spans the window instead of a letterbox. The size the browser reports is the size the
/// panel's click arithmetic has to use, which is why this answers with the panel payload rather
/// than with the number it was handed.
///
/// For a page that lives in the window the window owns the size: the panel's box *is* the viewport,
/// and the answer says so instead of resizing something the person is looking at.
#[tauri::command]
pub async fn browser_viewport(
    state: tauri::State<'_, AppState>,
    width: u32,
    height: u32,
) -> Result<serde_json::Value, String> {
    use cwoffee_engine::browser_hub as bh;
    let hub = kept_hub(&state)?;
    let out = {
        let mut h = hub.lock().await;
        h.set_viewport(width, height).await
    }?;
    if !out.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        return Err(out
            .get("why")
            .and_then(|w| w.as_str())
            .unwrap_or("the page would not resize")
            .to_string());
    }
    let mut payload = browser_payload(&state);
    // The panel owes itself the frame that shows the page at the new size, so the picture id it
    // holds has to be behind the one this resize just drew.
    payload["frame_pic"] = serde_json::json!(bh::Hub::peek_shared(&state.browser_state).frame_pic);
    payload["moved"] = out.get("moved").cloned().unwrap_or(serde_json::json!(true));
    payload["viewport"] = out.get("viewport").cloned().unwrap_or(serde_json::json!(null));
    Ok(payload)
}

/// Close the browser now, rather than waiting for the idle clock to do it. The reason is
/// recorded, because "why is there no page in the panel?" deserves an answer better than a
/// blank frame.
#[tauri::command]
pub async fn browser_close(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let hub = kept_hub(&state)?;
    {
        let mut h = hub.lock().await;
        h.close("closed from the panel").await;
    }
    Ok(browser_payload(&state))
}

/// What the third-party servers are doing and what the file says about them: which core is
/// running, whether a hub is mounted here, what each server answered, what `[mcp.servers]` asks
/// for, and how many bytes the catalog costs every prompt.
///
/// Read-only, and the object is assembled by the engine (`mcp::status_payload`) rather than here:
/// the desktop and a phone tab used to build this by hand and had already drifted by one field each,
/// which is the same fault the theme payload test exists to catch.
#[tauri::command]
pub async fn mcp_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let core = state.host.core_id().to_string();
    // Re-read, then re-apply the environment: `[mcp]` in the file and `CWOFFEE_MCP_SERVERS`
    // describe the same servers, and a page that read only the file would tell a person whose
    // servers come from a systemd unit that none are configured. Two bugs of that shape are
    // pinned in the README; this verb is the third place it could have happened.
    let (mut cfg, _) = cwoffee_engine::Config::load(Some(&state.config_path)).unwrap_or_default();
    cfg.apply_env();
    let cfg = cfg.mcp_cfg();
    let file = cwoffee_engine::config::mcp_state_in(&state.config_path);
    let hub = match &state.mcp {
        Some(h) => Some(h.lock().await),
        None => None,
    };
    Ok(cwoffee_engine::mcp::status_payload(
        hub.as_deref(),
        &cfg,
        &core,
        &state.config_path,
        &state.mcp_route,
        &file,
        // This window runs the servers, so this window may change them.
        true,
    ))
}

/// Why a write is refused when the environment names the servers.
///
/// `CWOFFEE_MCP_SERVERS` replaces `[mcp.servers]` wholesale. A page that wrote the file in that
/// situation would report success and change nothing the next start reads — so the refusal names the
/// variable instead of pretending the form worked.
fn mcp_env_refusal() -> String {
    "CWOFFEE_MCP_SERVERS names the third-party servers for this app, and it replaces [mcp.servers]      wholesale: writing the file would say it worked and change nothing the next start reads. Change      that variable, or unset it and let [mcp.servers] in the file answer."
        .to_string()
}

/// The tail every write shares: re-read the file the write just changed, make the running hub match
/// it, tell the core that what the model is offered has changed, and put the two sentences together.
///
/// Two sentences because they are two facts — what the file now holds, and what is answering — and a
/// single "saved" would hide the case where the file was written and the program refused to start.
async fn mcp_after_write(state: &AppState, file_says: String) -> String {
    let (mut cfg, _) = cwoffee_engine::Config::load(Some(&state.config_path)).unwrap_or_default();
    cfg.apply_env();
    let mcfg = cfg.mcp_cfg();
    let live_says = match &state.mcp {
        Some(hub) => {
            let said = cwoffee_engine::mcp::reconcile(hub, &mcfg).await;
            // The Rust core builds the prompt's catalog from a value stored at boot, so it has to be
            // told. The pi core's tool host and its catalog block are both re-read from the hub, and
            // its `mcp_refresh` is empty for that reason rather than by omission.
            state.host.mcp_refresh(Arc::clone(hub));
            said
        }
        None => "no hub is mounted in this app, so nothing was started".to_string(),
    };
    format!("{file_says} {live_says}")
}

/// Add or update one third-party server, start it, and say what happened to both the file and the
/// process. Refusals come back as errors so the row goes red rather than green with a complaint in
/// its body text.
#[tauri::command]
pub async fn mcp_add_server(
    state: tauri::State<'_, AppState>,
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<String>,
    on: bool,
) -> Result<String, String> {
    if cwoffee_engine::mcp::listed_from_env() {
        return Err(mcp_env_refusal());
    }
    // The form shows environment **names**, never values, so a bare `KEY` line means "keep what is
    // there". Resolving it here against the file's own row is what stops an edit of the command from
    // writing empty values over somebody's token — and the refusal of a name the server never had is
    // the part that cannot be guessed at later.
    let held = cwoffee_engine::config::mcp_state_in(&state.config_path)
        .servers
        .iter()
        .find(|(n, _)| *n == name.trim())
        .map(|(_, c)| c.env.clone())
        .unwrap_or_default();
    let row = cwoffee_engine::mcp::McpServerCfg {
        command,
        args: cwoffee_engine::mcp::McpServerCfg::args_from_lines(&args),
        enabled: on,
        env: cwoffee_engine::mcp::McpServerCfg::env_keep_lines(&held, &env)?,
    };
    let file_says = cwoffee_engine::config::add_server_with(&state.config_path, &name, &row);
    if file_says.starts_with("not saved") {
        return Err(file_says);
    }
    Ok(mcp_after_write(&state, file_says).await)
}

/// Switch one server that is already in the file. The switch cannot invent one — a name the file
/// never had is not something a toggle gets to make runnable.
#[tauri::command]
pub async fn mcp_set_server_enabled(
    state: tauri::State<'_, AppState>,
    name: String,
    on: bool,
) -> Result<String, String> {
    if cwoffee_engine::mcp::listed_from_env() {
        return Err(mcp_env_refusal());
    }
    let file_says = cwoffee_engine::config::set_server_enabled_with(&state.config_path, &name, on);
    if file_says.starts_with("not changed") {
        return Err(file_says);
    }
    Ok(mcp_after_write(&state, file_says).await)
}

/// Drop a server: from the file, and from the process table if it was running.
#[tauri::command]
pub async fn mcp_remove_server(state: tauri::State<'_, AppState>, name: String) -> Result<String, String> {
    if cwoffee_engine::mcp::listed_from_env() {
        return Err(mcp_env_refusal());
    }
    let file_says = cwoffee_engine::config::remove_server_with(&state.config_path, &name);
    Ok(mcp_after_write(&state, file_says).await)
}

/// The master switch: whether any third-party program runs in this app at all.
#[tauri::command]
pub async fn mcp_set_enabled(state: tauri::State<'_, AppState>, on: bool) -> Result<String, String> {
    if cwoffee_engine::mcp::listed_from_env() {
        return Err(mcp_env_refusal());
    }
    let file_says = cwoffee_engine::config::set_mcp_enabled_with(&state.config_path, on);
    if file_says.starts_with("not changed") {
        return Err(file_says);
    }
    Ok(mcp_after_write(&state, file_says).await)
}

/// Re-read `[mcp]` and make the running set match it. This is for a person who edited the file in an
/// editor: the app has no file watcher, and a page that never offered anything but its own form
/// would leave a hand edit invisible until the next boot.
#[tauri::command]
pub async fn mcp_reconcile(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let (mut cfg, _) = cwoffee_engine::Config::load(Some(&state.config_path)).unwrap_or_default();
    cfg.apply_env();
    let mcfg = cfg.mcp_cfg();
    match &state.mcp {
        Some(hub) => {
            let said = cwoffee_engine::mcp::reconcile(hub, &mcfg).await;
            state.host.mcp_refresh(Arc::clone(hub));
            Ok(format!("the config file was read again — {said}"))
        }
        None => Ok("no hub is mounted in this app, so there is nothing to reconcile".to_string()),
    }
}

/// What the model is handed right now and what is registered but withheld behind `explore`.
///
/// On the pi core this is a refusal with a reason, and the reason is the useful part: a settings
/// row that showed two zeroes there would say "nothing is hidden" about a core that decides its
/// tool list inside the model process.
#[tauri::command]
pub async fn tool_inventory(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let (declared, hidden) = state.host.tool_inventory().await?;
    Ok(serde_json::json!({ "declared": declared, "hidden": hidden }))
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

/// What the next window-close would do to the gateway. Shown beside the "keep devices working"
/// toggle, so the one case where it cannot be honoured — no standalone gateway binary to start —
/// is said while there is still a window around to say it in.
#[tauri::command]
pub fn gateway_handoff_plan(state: tauri::State<'_, AppState>) -> crate::gateway_ctl::HandoffPlan {
    state.gateway.plan()
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

/// A message typed **while a turn is running**. Same address assertion as `start_turn`: the host
/// refuses a press aimed at a conversation it is not answering, and it refuses one when nothing is
/// running, in words that say to send it as a message instead.
///
/// The answer is the pending item — its id is the handle for taking it back, and its kind is the
/// promise the composer announced. Nothing here claims the model has the message: it is waiting, and
/// the landing receipt arrives on the event stream when a request actually carries it.
#[tauri::command]
pub fn steer_turn(
    state: tauri::State<'_, AppState>,
    text: String,
    attachments: Option<Vec<AttachmentIn>>,
    session: Option<i64>,
) -> Result<serde_json::Value, String> {
    let atts: Vec<cwoffee_engine::provider::Attachment> = attachments
        .unwrap_or_default()
        .into_iter()
        .map(|a| cwoffee_engine::provider::Attachment {
            mime: a.mime,
            data_b64: a.data_b64,
            name: a.name,
        })
        .collect();
    eprintln!(
        "[cwoffee] steer_turn (desktop): session={:?} text={} attachments={}",
        session,
        text.chars().count(),
        atts.len()
    );
    let view = state.host.queue_mid_turn(text, atts, session)?;
    serde_json::to_value(view).map_err(|e| e.to_string())
}

/// Take a waiting message back before it lands. A refusal means it already landed — the row stays,
/// and no control can unsend bytes that went out.
#[tauri::command]
pub fn withdraw_steer(state: tauri::State<'_, AppState>, id: i64) -> Result<serde_json::Value, String> {
    let view = state.host.withdraw_queued(id.max(0) as u64)?;
    serde_json::to_value(view).map_err(|e| e.to_string())
}

/// What is waiting right now. Read from the host rather than remembered by the window, so a second
/// client that reloaded mid-turn paints the lines it missed instead of losing them.
#[tauri::command]
pub fn queue_state(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    serde_json::to_value(state.host.queue_state()).map_err(|e| e.to_string())
}

/// Change what a mid-turn press promises. `rename_all` because Tauri's default would demand
/// `kind` spelled the same — it is one word, but the gateway reads its own snake_case frame and the
/// UI is written once.
#[tauri::command(rename_all = "snake_case")]
pub fn set_mid_turn(state: tauri::State<'_, AppState>, kind: String) -> Result<serde_json::Value, String> {
    let kind = cwoffee_engine::steer::MidTurn::parse(&kind).ok_or_else(|| {
        "that is not a mid-turn promise: expected steer (into this turn) or follow_up (after it)".to_string()
    })?;
    state.host.set_mid_turn(kind);
    serde_json::to_value(state.host.state().mid_turn).map_err(|e| e.to_string())
}

/// Change the **stored** palette, in `config.toml`, which is what the window, a device tab and the
/// terminal all start from. Refused in words when the word is not a palette — `theme` used to be
/// read by nobody, so a bad word and an ignored word looked identical from outside.
#[tauri::command(rename_all = "snake_case")]
pub fn set_theme(state: tauri::State<'_, AppState>, kind: String) -> Result<serde_json::Value, String> {
    let kind = cwoffee_engine::theme::Theme::parse(&kind).ok_or_else(|| {
        "that is not a palette: expected light, dark or system (system follows this client's own \
         colour scheme, live)"
            .to_string()
    })?;
    state.host.set_theme(kind);
    serde_json::to_value(state.host.state().theme).map_err(|e| e.to_string())
}

/// About's palette line. `window` is the override this screen is showing, sent for the wording and
/// stored nowhere: a window's override must not rewrite the host's choice, and the sentence has to
/// be able to say which of the two the person is looking at.
#[tauri::command(rename_all = "snake_case")]
pub fn theme_words(state: tauri::State<'_, AppState>, window: Option<String>) -> Result<String, String> {
    let window = match window.as_deref() {
        Some(w) => Some(cwoffee_engine::theme::Theme::parse(w).ok_or_else(|| "that is not a palette: expected light, dark or system".to_string())?),
        None => None,
    };
    Ok(state.host.theme_words(window))
}

/// What `config.toml` holds, for the armed page harness only.
///
/// Narrow on purpose: the path is the **host's own** config path, not an argument, and the command
/// answers only in an armed run. The theme check needs to prove that a choice made in Settings
/// landed in the file every surface reads at start, and `selftest_file_bytes` is confined to the
/// workspace — a check that could name an arbitrary path would be a test that reads the user's files.
#[tauri::command]
pub fn selftest_config_text(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    if !selftest_armed() {
        return Err("this is not an armed self-test run, so the settings file is not readable here".to_string())
    }
    let path = state.config_path.clone();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| format!("<unreadable: {e}>"));
    Ok(serde_json::json!({ "path": path.display().to_string(), "text": text }))
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
pub fn set_reasoning(
    state: tauri::State<'_, AppState>,
    level: String,
    persist: Option<bool>,
) -> Result<(), String> {
    let r = match level.as_str() {
        "off" => Reasoning::Off,
        "low" => Reasoning::Low,
        "medium" => Reasoning::Medium,
        "high" => Reasoning::High,
        "xhigh" => Reasoning::XHigh,
        other => return Err(format!("unknown reasoning level {other:?}")),
    };
    // `persist` is the difference between the settings screen and the composer dial: true is the
    // default every new conversation starts at (and lands in the config file), false is an override
    // for this conversation alone. A missing flag means false, so an old client cannot move a
    // person's default without asking.
    state.host.send(HostCmd::SetReasoning { reasoning: r, persist: persist.unwrap_or(false) });
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

// ---- the model screen ------------------------------------------------------------------

/// Every endpoint, the models under each, and what is answering. File-based: this answers even
/// when the inference box is down, which is when someone opens this screen.
#[tauri::command]
pub fn provider_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    serde_json::to_value(state.host.provider_status(&state.config_path)).map_err(|e| e.to_string())
}

// `rename_all = "snake_case"` is not decoration. Tauri's default is to expect camelCase
// keys in an `invoke` payload and map them onto snake_case Rust parameters, while the gateway
// — the same verbs for a browser or a phone — reads the snake_case names straight off the
// frame. Two hosts, two spellings, one client: whichever one the UI was written against, the
// other one answers "missing required key ...". The wire dialect is the engine's own, so the
// commands that take more than one word say so here.
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_test(
    state: tauri::State<'_, AppState>,
    base_url: String,
    api_key: Option<String>,
    kind: Option<String>,
    name: Option<String>,
) -> Result<String, String> {
    let req = cwoffee_engine::config::ProviderReq {
        name: name.unwrap_or_default(),
        base_url,
        api_key: api_key.unwrap_or_default(),
        kind: kind.unwrap_or_default(),
    };
    Ok(state.host.provider_test(&state.config_path, &req).await)
}

// See the note on `provider_test`: one wire dialect, spelled the engine's way.
#[tauri::command(rename_all = "snake_case")]
pub async fn provider_probe(
    state: tauri::State<'_, AppState>,
    base_url: String,
    api_key: Option<String>,
    kind: Option<String>,
    name: Option<String>,
) -> Result<serde_json::Value, String> {
    let req = cwoffee_engine::config::ProviderReq {
        name: name.unwrap_or_default(),
        base_url,
        api_key: api_key.unwrap_or_default(),
        kind: kind.unwrap_or_default(),
    };
    let rows = state.host.provider_probe(&state.config_path, &req).await.map_err(|e| e.to_string())?;
    serde_json::to_value(rows).map_err(|e| e.to_string())
}

// See the note on `provider_test`: one wire dialect, spelled the engine's way.
#[tauri::command(rename_all = "snake_case")]
pub fn provider_add(
    state: tauri::State<'_, AppState>,
    base_url: String,
    api_key: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    caps: Option<serde_json::Value>,
) -> Result<String, String> {
    let req = cwoffee_engine::config::ProviderReq {
        name: name.unwrap_or_default(),
        base_url,
        api_key: api_key.unwrap_or_default(),
        kind: kind.unwrap_or_default(),
    };
    let caps: Vec<cwoffee_engine::config::ModelSpec> = caps
        .map(|v| serde_json::from_value(v).map_err(|e| format!("probed models: {e}")))
        .transpose()?
        .unwrap_or_default();
    Ok(state.host.provider_add(&state.config_path, &req, &caps))
}

/// Point this app at the endpoint it answers from — the top-level `[provider]` address, which is the
/// one choice both cores agree on (see `cwoffee_engine::config::set_primary_endpoint_with`).
///
/// This is the "add a model" first step: an install with no endpoint has nothing to answer with, and
/// until this verb existed the only way to say where the model lives was to edit `config.toml` by hand.
#[tauri::command(rename_all = "snake_case")]
pub fn provider_set_primary(
    state: tauri::State<'_, AppState>,
    base_url: String,
    api_key: Option<String>,
    kind: Option<String>,
) -> Result<String, String> {
    if state.host.state().busy {
        return Err(
            "a turn is running, and it is being answered by the endpoint it started with. Say stop              first — repointing the app mid-answer would finish a sentence begun somewhere else."
                .into(),
        );
    }
    let said = cwoffee_engine::config::set_primary_endpoint_with(
        &state.config_path,
        &base_url,
        kind.as_deref().unwrap_or(""),
        api_key.as_deref().unwrap_or(""),
    )?;
    // Write it, then hand it to the running host. The order matters: a host that took up an endpoint
    // the file does not name would be reset by the next start, and the person would be told a change
    // was made that outlived neither the turn nor the reboot.
    state.host.reload_provider()?;
    Ok(said)
}

#[tauri::command]
pub fn provider_remove(state: tauri::State<'_, AppState>, name: String) -> Result<String, String> {
    Ok(state.host.provider_remove(&state.config_path, &name))
}

// See the note on `provider_test`: one wire dialect, spelled the engine's way.
#[tauri::command(rename_all = "snake_case")]
pub fn model_set(
    state: tauri::State<'_, AppState>,
    id: String,
    context_window: Option<usize>,
    max_tokens: Option<usize>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<usize>,
    min_p: Option<f32>,
    seed: Option<u64>,
    efforts: Option<Vec<String>>,
    supports_tools: Option<bool>,
    supports_vision: Option<bool>,
    source: Option<String>,
) -> Result<String, String> {
    let row = cwoffee_engine::config::ModelRow {
        context_window,
        max_tokens,
        temperature,
        top_p,
        top_k,
        min_p,
        seed,
        efforts: efforts.unwrap_or_default(),
        supports_tools,
        supports_vision,
        source: source.unwrap_or_default(),
    };
    Ok(state.host.model_set(&state.config_path, &id, &row))
}

#[tauri::command]
pub fn model_forget(state: tauri::State<'_, AppState>, id: String) -> Result<String, String> {
    Ok(state.host.model_forget(&state.config_path, &id))
}

/// Every conversation filed under one folder — the list a folder delete has to act on.
/// See `Store::sessions_in` for why the sidebar's own lists cannot answer this.
#[tauri::command]
pub fn sessions_in(state: tauri::State<'_, AppState>, path: String) -> Result<Vec<Session>, String> {
    state.store.sessions_in(&path).map_err(|e| e.to_string())
}

/// The display settings, with their labels and defaults, from the Rust list that owns
/// them. They live in the conversation database, so a phone and this window agree; a
/// browser's `localStorage` would be a fourth opinion nobody asked for.
#[tauri::command]
pub fn ui_prefs(state: tauri::State<'_, AppState>) -> Result<Vec<cwoffee_engine::prefs::UiPrefView>, String> {
    cwoffee_engine::prefs::view(state.store.as_ref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_ui_pref(
    state: tauri::State<'_, AppState>,
    key: String,
    value: String,
) -> Result<String, String> {
    cwoffee_engine::prefs::set(state.store.as_ref(), &key, &value)
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
///
/// `session` is the conversation whose transcript the row came from, and it is what the
/// path resolves against first — not the folder the host happens to be working in. A row
/// is a record of where a turn wrote its bytes; the app can restore a conversation from
/// another workspace, or show an old one while a new one is open, and the previous rule
/// ("resolve against wherever we are now") then reported a file sitting intact on disk as
/// `no longer exists`. `save_attachment_as` already resolves through the row for exactly
/// this reason; a click must not disagree with the Save control about where the file is.
/// The host's folder is tried second, because a conversation that moved between folders
/// took its files with it, and a miss names both folders instead of telling a story about
/// deletion.
#[tauri::command]
pub fn open_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session: Option<i64>,
    path: String,
) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;
    let host_ws = std::path::PathBuf::from(&state.host.state().workspace);
    let conv_ws = session
        .filter(|s| *s > 0)
        .and_then(|s| state.store.session(s).ok().flatten())
        .map(|row| std::path::PathBuf::from(row.workspace));
    let full = cwoffee_engine::tools::scratch::turn_file_path(conv_ws.as_deref(), &host_ws, &path)?;
    if !full.is_file() {
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

/// Ask the person where a file should go, offering the name we suggest.
///
/// One implementation for every "save this" control in the app, because two dialogs
/// that drift become two buttons that behave differently while looking identical.
/// `None` is a real answer: the person pressed cancel.
async fn pick_save_path(app: &tauri::AppHandle, name: &str) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(name)
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
    // A dialog the user never closes must not pin a task forever.
    tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "save dialog timed out".to_string())?
        .map_err(|_| "save dialog closed without a result".to_string())
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
    let Some(path) = pick_save_path(&app, &name).await? else {
        return Ok(None);
    };
    std::fs::write(&path, contents.as_bytes()).map_err(|e| format!("cannot write {path}: {e}"))?;
    eprintln!("[cwoffee] share: wrote {path}");
    Ok(Some(path))
}

/// Hand over an attachment: the file the assistant wrote into
/// `.cws/assistant_attachments/`, copied to wherever the person says.
///
/// Desktop-only by construction: the save dialog is a desktop window, so over the
/// gateway the same control downloads (see `attachment_bytes` in the gateway's dispatch).
///
/// The `session` argument is not decorative. The path arrives from a tool result the
/// model wrote, and a conversation can live in any folder the person chose; resolving it
/// against the folder of whichever conversation the host happens to be working on would
/// read the wrong one. The engine's own rule — only a file under this conversation's
/// attachments folder, never an arbitrary path — is what stops a turn from offering some
/// other file on the machine.
#[tauri::command]
pub async fn save_attachment_as(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session: i64,
    path: String,
) -> Result<Option<String>, String> {
    let row = state
        .store
        .session(session)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("session {session} does not exist"))?;
    let full =
        cwoffee_engine::tools::scratch::attachment_path(std::path::Path::new(&row.workspace), &path)?;
    let name = full
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let Some(dest) = pick_save_path(&app, &name).await? else {
        return Ok(None);
    };
    let copied = std::fs::copy(&full, &dest).map_err(|e| format!("cannot write {dest}: {e}"))?;
    eprintln!("[cwoffee] attachment: {path} -> {dest} ({copied} bytes)");
    Ok(Some(dest))
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

// See the note on `provider_test`: one wire dialect, spelled the engine's way.
#[tauri::command(rename_all = "snake_case")]
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

/// Pin or free the ceiling a conversation is measured and folded against.
///
/// `None` means "derive it from the window this model has", which is why the argument
/// is optional rather than a sentinel like `0`: a magic number on the wire is how a
/// setting comes back looking like a value somebody chose.
#[tauri::command]
pub async fn set_context_budget(
    state: tauri::State<'_, AppState>,
    target: Option<usize>,
) -> Result<String, String> {
    state.host.set_context_budget(&state.config_path, target).await
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
    Ok(rows.into_iter().map(ws_row_of).collect())
}

/// One folder row as it leaves this process.
///
/// The names here are the names `WorkspaceRow` in `ui/src/lib/events.ts` reads, and the gateway
/// hands out the engine's own `WorkspaceEntry` for the same list — so this struct is the one place
/// where the desktop can quietly disagree with a phone about what a folder *is*. It has happened:
/// the colour was written to the store, read back from it, and dropped on the way out, which made
/// "colour this folder" a press that did nothing on the desktop while working on a phone.
#[derive(serde::Serialize)]
pub struct WorkspaceRow {
    pub path: String,
    pub label: String,
    /// `#rrggbb`, or empty for "no colour". Validated by the store; see `set_workspace_color`.
    pub color: String,
    pub sessions: i64,
}

fn ws_row_of(w: cwoffee_engine::store::WorkspaceEntry) -> WorkspaceRow {
    WorkspaceRow {
        path: w.path,
        label: w.label,
        color: w.color,
        sessions: w.sessions,
    }
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

/// Grant or take away web reach for one chat conversation.
///
/// One sentence of the answer is the record of what happened; the state itself is read back from
/// `session_state`, which is where every client's idea of the switch comes from. The checks — is this
/// a chat, is a turn running in it — live in the engine, so a phone on the gateway is refused in the
/// same words for the same reason.
#[tauri::command(rename_all = "snake_case")]
pub fn set_session_web(
    state: tauri::State<'_, AppState>,
    session: i64,
    on: bool,
) -> Result<String, String> {
    cwoffee_engine::host::set_conversation_web(
        state.store.as_ref(),
        state.host.as_ref(),
        session,
        on,
    )
}

/// What this build is and where the person's files live. See `cwoffee_engine::About`.
#[tauri::command]
pub fn about(state: tauri::State<'_, AppState>) -> cwoffee_engine::About {
    let (cfg, _) = cwoffee_engine::Config::load(Some(&state.config_path)).unwrap_or_default();
    let conversations = state.store.conversation_count().unwrap_or(0);
    let messages = state.store.message_total().unwrap_or(0);
    let mut a = cwoffee_engine::about::from_config(&cfg, &state.config_path, conversations, messages);
    a.core = state.host.core_id().to_string();
    a.config_path = state.config_path.display().to_string();
    // Asked of the store, not recomputed from the environment: the page is answering "is my history
    // at that path?", and only the thing holding the file open can say what it opened.
    a.db_path = state.store.db_path();
    a.thread_store = state.host.thread_store();
    let g = state.gateway.status();
    a.gateway = if !g.running {
        "off".to_string()
    } else {
        format!(
            "{}:{} · {} device{}",
            g.host,
            g.port,
            g.clients,
            if g.clients == 1 { "" } else { "s" }
        )
    };
    a
}

/// Colour a folder's mark in the sidebar. Empty clears it.
///
/// Same class of edit as the label: it changes what the record says about how the person arranged
/// their own folders, which is why it is a stored field and not localStorage — the phone and a
/// second window see the same coloured list.
#[tauri::command(rename_all = "snake_case")]
pub fn set_workspace_color(
    state: tauri::State<'_, AppState>,
    path: String,
    color: String,
) -> Result<bool, String> {
    let n = state
        .store
        .set_workspace_color(&path, &color)
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

/// What the page-side check chain should run.
///
/// `CWOFFEE_SELFTEST_ONLY=cup,code-chrome` narrows a pass to the groups that were touched; the page
/// matches each term against a group's id and against the words of its own section label, so `cup`
/// catches both cup groups and `follow` catches the scroll one. Empty means every group — which is
/// the only setting whose green means "the suite passed" — and the page puts the count of groups it
/// ran, and the names of the ones it did not, on its final line so a narrowed run can never be read
/// as a full one.
///
/// Gated on the same arming as the rest of the harness: in an ordinary run of the app this answers
/// with an empty filter, because nothing outside a self-test should be able to switch off checks.
#[tauri::command]
pub fn selftest_config() -> serde_json::Value {
    // Which palette this run paints. The surface groups are graded in *both*, and one run can only
    // be in one of them, so `--theme light` is the second arm rather than a bonus: a floating surface
    // whose colour is hard-coded dark passes the dark run and fails the light one, which is the
    // sabotage that proves the light arm bites. Read from the environment and refused if it is not a
    // palette word — a typo here would silently run the suite in whichever palette the app booted in
    // and report a pass for the other one.
    let theme: Option<String> = if selftest_armed() {
        match std::env::var("CWOFFEE_SELFTEST_THEME") {
            Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
            _ => None,
        }
    } else {
        None
    };
    let theme = theme.map(|t| {
        match cwoffee_engine::theme::Theme::parse(&t) {
            Some(cwoffee_engine::theme::Theme::Light) => "light".to_string(),
            Some(cwoffee_engine::theme::Theme::Dark) => "dark".to_string(),
            // `system` would make the arm's palette depend on the machine, which is not a claim the
            // surface groups can grade. The run says so in its own header rather than guessing.
            _ => String::new(),
        }
    });
    let only: Vec<String> = if selftest_armed() {
        std::env::var("CWOFFEE_SELFTEST_ONLY")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    // Groups this launch must not grade, named by the launcher rather than by the check. A kept
    // browser is one shape per process — a relayed picture column or a page painted in this window —
    // so a full pass runs the other shape in a launch of its own and asks this one to stand down
    // instead of grading a page that is not there and reporting a failure nobody caused.
    let skip: Vec<String> = if selftest_armed() {
        std::env::var("CWOFFEE_SELFTEST_SKIP")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    serde_json::json!({ "only": only, "theme": theme, "skip": skip })
}

/// Resize the self-test window, from inside the app.
///
/// This exists because the page cannot: `core:window:allow-set-size` is deliberately not in the
/// capability file — a chat app's page has no business choosing the window's size — so a check that
/// wants to know what the layout does when the window changes shape has to ask the host, which is
/// also the only thing that can answer whether the resize actually landed. Wayland and mutter are
/// free to ignore a resize request, and a check that measured a window that never changed would
/// report a layout bug that never happened.
///
/// Gated on the same arming as the harness: a normal run of the app has no command that resizes its
/// own window. Like the present ask, the window call runs on its own thread, because a Wayland
/// window call round-trips through the compositor queue that the IPC handler's own loop drains.
#[tauri::command]
pub fn selftest_resize_window(app: tauri::AppHandle, width: u32, height: u32) -> Result<String, String> {
    if !selftest_armed() {
        return Err("this process is not an armed self-test run; its window size is not mine to change".into());
    }
    if !(300..=4000).contains(&width) || !(300..=4000).contains(&height) {
        return Err(format!("refusing to resize the window to {width}x{height}: outside 300..4000"));
    }
    std::thread::spawn(move || {
        use tauri::Manager;
        let Some(w) = app.get_webview_window("main") else {
            println!("[selftest] selftest_resize_window was asked and there is no main window");
            return;
        };
        // Frameless (`decorations: false`), so the inner size the page measures and the outer size
        // this call sets are the same box. Stated rather than assumed: a decorated window would need
        // the decoration delta added back or the page's numbers would be off by the titlebar.
        let before = w
            .inner_size()
            .map(|s| format!("{}x{}", s.width, s.height))
            .unwrap_or_else(|_| "unknown".into());
        match w.set_size(tauri::LogicalSize::new(f64::from(width), f64::from(height))) {
            Ok(()) => println!("[selftest] window resize asked: {before} -> {width}x{height} (logical)"),
            Err(e) => println!("[selftest] window resize to {width}x{height} failed: {e}"),
        }
    });
    Ok(format!("resize of {width}x{height} asked"))
}

/// Ask the window manager for the window back, from inside the app.
///
/// This exists because the layout and animation checks can only measure a page the compositor is
/// painting. When something else covers this window, WebKitGTK stops firing requestAnimationFrame,
/// CSS animations freeze at their start value, and a run answers with frame gaps, follow catch-up
/// that never converges and a dozen skipped geometry checks - all of which read as a broken UI
/// rather than a hidden window. The startup code already asks for the window; that ask is one shot,
/// and a fullscreen application that raises itself takes it back mid-run.
///
/// The app presenting its own window is the only raise available on this desktop. Raising it from
/// outside means either an X client faking input - which Wayland answers with a remote-desktop
/// consent dialog, an interruption no test is allowed to cause - or Shell unsafe mode.
///
/// Gated on the same arming as the harness itself: a normal run of the app must never find a
/// command that changes its window stacking.
#[tauri::command]
pub fn present_selftest_window(app: tauri::AppHandle) -> Result<String, String> {
    if !selftest_armed() {
        return Err("this process is not an armed self-test run; the window stacking is not mine to change".into());
    }
    // The ask runs on its own thread, and that is not a style choice. A Wayland window call
    // round-trips through the compositor's event queue, and that queue is drained by the same main
    // loop that is executing this command: asking for focus from inside the IPC handler parks the
    // handler, which parks the webview's message channel with it. The page then sits on an `await
    // invoke(...)` that never settles and stops logging mid-run - twice observed, both times right
    // after a page-side window ask, with the app's own watcher thread still reporting. The startup
    // raise and the run's focus watcher already do their window work on a thread of their own, and
    // that path is proven on this desktop.
    std::thread::spawn(move || {
        // The window lookup lives on the Manager trait; imported here rather than at the top of the
        // file so the rest of the command module keeps its current imports.
        use tauri::Manager;
        let Some(w) = app.get_webview_window("main") else {
            println!("[selftest] present_selftest_window was asked and there is no main window");
            return;
        };
        let mut said: Vec<String> = Vec::new();
        if w.is_minimized().unwrap_or(false) {
            if w.unminimize().is_ok() {
                said.push("un-minimized".into());
            }
        }
        if w.show().is_ok() {
            said.push("shown".into());
        }
        // Not `set_always_on_top`: on this Wayland desktop mutter ignores it for a normal
        // toplevel, and every log line that claimed "pinned above others" was a claim about a
        // stacking order the app does not have. A diagnostic that lies about what it did is worse
        // than no diagnostic, because the next person reads it and stops looking.
        if w.set_focus().is_ok() {
            said.push("focused".into());
        }
        // On Wayland a plain present is a *request*, and a compositor that is guarding against
        // focus stealing answers it by doing nothing. That is the state this run kept hitting:
        // the window is on another workspace or behind another window, mutter never paints what
        // it is not showing, `requestAnimationFrame` goes silent, and twenty-odd layout checks
        // measure a page that does not exist. The one lever an app genuinely owns over its own
        // surface is its minimized state, and mutter honours the restore: it brings the window
        // back onto the current workspace and gives it focus. So if the present above did not
        // actually land, minimize and restore deliberately. It blinks, and only in an armed
        // self-test run - a normal run has no command that can reach here at all.
        // What was tried, so nobody tries it again from the log alone. Asking twice, and
        // escalating, does nothing on this compositor: a plain present, a minimize/restore cycle
        // and a fullscreen request were all issued for a window that stayed `focused=false`, and
        // `xdg_toplevel` gives an app no way to name a workspace or demand a stacking order. The
        // fix for the frame clock is not a window trick - it is `WEBKIT_DISABLE_COMPOSITING_MODE=1`
        // in scripts/selftest.sh, which takes `requestAnimationFrame` off the compositor's frame
        // callbacks entirely. What this command is good for is the state line printed below, which
        // is how a run says "the window was not on screen" in the app's own words.
        let size = w
            .inner_size()
            .map(|s| format!("{}x{}", s.width, s.height))
            .unwrap_or_else(|_| "unknown size".into());
        let how = if said.is_empty() { "nothing took".to_string() } else { said.join(", ") };
        println!(
            "[selftest] the page asked for its own window: {} ({}, focused={} visible={} minimized={})",
            how,
            size,
            w.is_focused().unwrap_or(false),
            w.is_visible().unwrap_or(false),
            w.is_minimized().unwrap_or(false),
        );
    });
    Ok("asked on its own thread, so the ask cannot park the page that asked".into())
}

/// What the window knows about itself, without asking the compositor for anything.
///
/// The run needs this because the frame clock belongs to the compositor: `focused=false` means
/// `requestAnimationFrame` stops, and every layout, motion and cadence claim from that point on is
/// unverifiable. Reading the state is free - tao keeps it locally, since Wayland offers no way to ask
/// a compositor synchronously. *Changing* it is the part this desktop refuses, which is why the ask
/// above and the read here are separate commands: one is a request the app is entitled to make, and
/// the other is the honest answer about whether it landed.
#[tauri::command]
pub fn selftest_window_state(app: tauri::AppHandle) -> Result<String, String> {
    if !selftest_armed() {
        return Err("this process is not an armed self-test run; the window's business is none of ours".into())
    }
    use tauri::Manager;
    let Some(w) = app.get_webview_window("main") else {
        return Err("there is no main window to report on".into())
    };
    Ok(format!(
        "focused={} visible={} minimized={}",
        w.is_focused().unwrap_or(false),
        w.is_visible().unwrap_or(false),
        w.is_minimized().unwrap_or(false),
    ))
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

#[tauri::command(rename_all = "snake_case")]
pub async fn edit_message(
    state: tauri::State<'_, AppState>,
    session: i64,
    message_id: Option<i64>,
    from_end: Option<usize>,
    shown: String,
    next: String,
    resend: bool,
) -> Result<serde_json::Value, String> {
    eprintln!(
        "[cwoffee] edit_message (desktop): session={session} id={message_id:?} from_end={from_end:?} \
         resend={resend} shown={} chars -> {} chars",
        shown.chars().count(),
        next.chars().count()
    );
    let req = cwoffee_engine::EditReq {
        session,
        message_id,
        from_end,
        shown,
        next,
        resend,
    };
    // Everything the verb means is on the other side of this call, and the phone's gateway makes the
    // same call with the same words: the check, the cut, the rewind and the re-send, in that order.
    // The refusal comes back as an error, not as a field: this command used to serialise the whole
    // `Result`, which handed the client `{"Err": "…"}` where it reads `{note, dropped, …}`, so a
    // window asking "which message did you change?" got nothing and said "saved" anyway.
    let out = cwoffee_engine::edit::apply_and_send(state.store.as_ref(), state.host.as_ref(), &req)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Branch a conversation from one of the messages on screen.
///
/// Same shape as `edit_message`, and for the same reason: the caller names the row it means and
/// repeats the words it painted, so a window that has drifted cannot branch a message it never
/// showed. `role` says which kind of row `from_end` counts — the pencil only ever works on your own
/// message, while a branch can also be asked at an answer, and "the newest answer" and "the newest
/// message" are different rows.
///
/// `rename_all` because the parameters are two words: Tauri's default would demand `messageId`
/// while the gateway reads `message_id` straight off the frame, and the UI is written once.
#[tauri::command(rename_all = "snake_case")]
pub async fn branch_conversation(
    state: tauri::State<'_, AppState>,
    session: i64,
    message_id: Option<i64>,
    from_end: Option<usize>,
    role: Option<String>,
    shown: String,
) -> Result<serde_json::Value, String> {
    eprintln!(
        "[cwoffee] branch_conversation (desktop): session={session} id={message_id:?} \
         from_end={from_end:?} role={role:?} shown={} chars",
        shown.chars().count()
    );
    let req = cwoffee_engine::BranchReq {
        session,
        message_id,
        from_end,
        role: role.unwrap_or_default(),
        shown,
    };
    // The check, the copy and the adoption are one function, and the phone's gateway calls that
    // same function with the same arguments. A refusal is an error, never a field: a client that
    // read `{"Err": "…"}` as a payload would tell the person their branch exists when nothing was
    // created.
    let out = cwoffee_engine::branch::apply_and_open(state.store.as_ref(), state.host.as_ref(), &req)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Who is whose parent, where each branch was cut, and which node the next answer reads.
///
/// The desktop and the phone call the same `crate::tree::read`, so one tree is drawn from one set of
/// facts: a sidebar that showed lineage the phone's tree did not, or a node list that agreed with the
/// transcript but not with pi's thread, would be two answers to one question about what the model was
/// told.
#[tauri::command(rename_all = "snake_case")]
pub async fn session_tree(
    state: tauri::State<'_, AppState>,
    session: i64,
) -> Result<serde_json::Value, String> {
    let out = cwoffee_engine::tree::read(state.store.as_ref(), state.host.as_ref(), session)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Move the conversation's leaf — the node its next answer reads through.
///
/// Same function as the gateway's `switch_leaf`, so the mid-turn refusal and the proof of which node
/// was meant are one text on both sides of the wire, and a stale phone tab cannot move a leaf the
/// desktop would refuse. A refusal is an error, never a field: a client that read `{"Err": "…"}` as a
/// payload would tell the person the leaf moved when nothing moved.
#[tauri::command(rename_all = "snake_case")]
pub async fn switch_leaf(
    state: tauri::State<'_, AppState>,
    session: i64,
    message_id: Option<i64>,
    from_end: Option<usize>,
    role: Option<String>,
    shown: String,
) -> Result<serde_json::Value, String> {
    eprintln!(
        "[cwoffee] switch_leaf (desktop): session={session} id={message_id:?} from_end={from_end:?} \
         role={role:?} shown={} chars",
        shown.chars().count()
    );
    let req = cwoffee_engine::tree::LeafReq {
        session,
        message_id,
        from_end,
        role: role.unwrap_or_default(),
        shown,
    };
    let out = cwoffee_engine::tree::move_leaf(state.store.as_ref(), state.host.as_ref(), &req)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Every file this conversation's turns touched, grouped by turn, with what each turn did to it
/// and whether it can still be put back.
///
/// The gateway's `files_ledger` calls the same function, so a phone and a desktop cannot be shown
/// two different answers to "what did this conversation write". Read-only, and it answers while a
/// turn is running — asking what a conversation has done to a folder is not a way to change it.
#[tauri::command(rename_all = "snake_case")]
pub async fn files_ledger(
    state: tauri::State<'_, AppState>,
    session: i64,
) -> Result<serde_json::Value, String> {
    let out = cwoffee_engine::ledger::read(state.store.as_ref(), session).map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// One stored copy against the bytes on disk now.
///
/// The copy is named by index row, and `shown` repeats the path the card painted: the pair is what
/// stops a window that has been open through two more turns from diffing whatever that path belongs
/// to by now. A mismatch is an error that names what the record says instead, and diffs nothing.
#[tauri::command(rename_all = "snake_case")]
pub async fn file_diff(
    state: tauri::State<'_, AppState>,
    session: i64,
    checkpoint: i64,
    shown: String,
) -> Result<serde_json::Value, String> {
    let out = cwoffee_engine::ledger::diff_of(state.store.as_ref(), session, checkpoint, &shown)
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// What this conversation's copies cost — rows, blobs counted once, and the bytes in the store that
/// no index row points at.
///
/// Deliberately its own verb rather than a field of the ledger: the index's total and the store's
/// total are two answers, and a card that printed one number for both would be a disk usage nobody
/// can reconcile with `du`.
#[tauri::command(rename_all = "snake_case")]
pub async fn snapshot_stats(
    state: tauri::State<'_, AppState>,
    session: i64,
) -> Result<serde_json::Value, String> {
    let out = cwoffee_engine::ledger::stats(state.store.as_ref(), session).map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Drop this conversation's snapshots, keeping any blob another conversation points at.
///
/// Drive-side and audited on the gateway, like the revert next to it: this deletes files. `shown`
/// is the copy count the card printed, so a window holding an old list changes nothing and is told
/// what the record says now. Refused while a turn runs — that turn is taking copies right now.
#[tauri::command(rename_all = "snake_case")]
pub async fn forget_copies(
    state: tauri::State<'_, AppState>,
    session: i64,
    shown: usize,
) -> Result<serde_json::Value, String> {
    eprintln!("[cwoffee] forget_copies (desktop): session={session} shown={shown}");
    let out = cwoffee_engine::ledger::forget(state.store.as_ref(), state.host.as_ref(), session, shown)
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// What reverting a turn's file changes would do, without doing it.
///
/// The transcript asks this before it shows a confirmation, so the person reads the actual
/// path list and the actual "edited since" flags before they press anything. It is a preview of
/// the same plan `checkpoint_revert` will execute, computed from the same index rows: showing one
/// plan and running another is how an "undo" ends up overwriting a file the reader never saw.
///
/// `whole_turn` is the difference between "undo this answer's files" and "put the folder back the
/// way it was before this message" — the second takes the later turns' changes with it.
#[tauri::command(rename_all = "snake_case")]
pub fn checkpoint_preview(
    state: tauri::State<'_, AppState>,
    session: i64,
    message_id: i64,
    through_latest: bool,
) -> Result<serde_json::Value, String> {
    let req = cwoffee_engine::checkpoint::RevertReq {
        session,
        message_id,
        through_latest,
        shown: Vec::new(),
        overwrite_edited: false,
    };
    let out = cwoffee_engine::checkpoint::preview(state.store.as_ref(), &req)
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// Undo one turn's file changes. The transcript only calls this after a confirmation naming the
/// paths, and the engine re-reads its own scope before touching anything.
///
/// The transcript rows are not touched: this is about files on disk, and a conversation that
/// silently lost its messages when someone pressed "undo the files" would be a worse surprise
/// than a folder that still holds what was written.
#[tauri::command(rename_all = "snake_case")]
pub async fn checkpoint_revert(
    state: tauri::State<'_, AppState>,
    session: i64,
    message_id: i64,
    through_latest: bool,
    shown: Vec<String>,
    overwrite_edited: bool,
) -> Result<serde_json::Value, String> {
    eprintln!(
        "[cwoffee] checkpoint_revert (desktop): session={session} message={message_id} \
         through_latest={through_latest} overwrite_edited={overwrite_edited} \
         shown={} path(s)",
        shown.len()
    );
    let req = cwoffee_engine::checkpoint::RevertReq {
        session,
        message_id,
        through_latest,
        shown,
        overwrite_edited,
    };
    // An error, not a payload: a phone or window that read `{"Err": …}` as a result would
    // report "reverted" for a revert that refused.
    let out = cwoffee_engine::checkpoint::revert(state.store.as_ref(), state.host.as_ref(), &req)
        .map_err(|e| e.to_string())?;
    serde_json::to_value(out).map_err(|e| e.to_string())
}

/// The bytes behind one message's attachments, for a reader who opens a picture that arrived in an
/// earlier conversation.
///
/// A transcript page carries name and size only — deliberately, because reading a conversation
/// should not drag every attached megabyte across IPC for pictures nobody is looking at. This is the
/// other half of that choice: when someone does click one, the single row they asked about comes
/// back with its payload.
///
/// `rename_all` because the parameter is two words: Tauri's default would demand `messageId` while
/// the gateway reads `message_id` straight off the frame, and the UI is written once.
#[tauri::command(rename_all = "snake_case")]
pub fn message_attachments(
    state: tauri::State<'_, AppState>,
    message_id: i64,
) -> Result<Vec<cwoffee_engine::provider::Attachment>, String> {
    state
        .store
        .message_attachments(message_id)
        .map_err(|e| e.to_string())
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

/// Only the page harness asks for this: it puts the board's own clipboard fixture back on the
/// selection at the moment the paste check needs it. Refuses unless the board named the file
/// in `CWOFFEE_SELFTEST_CLIP_IMG`, so a running app cannot be talked into replacing what the
/// user copied.
#[tauri::command]
pub fn selftest_stage_clipboard() -> Result<serde_json::Value, String> {
    crate::clipboard::stage_fixture()
}

/// Read one workspace file's bytes, for the harness only.
///
/// "The turn is undone on disk" is a claim about a file, and the UI has no file API — a check
/// that only reads the revert card's own arithmetic would be grading the app's own report card,
/// not the folder. So the harness reads the bytes the way an editor would, through the same
/// `confine` the tools use: it cannot be pointed at a file outside the conversation's folder.
///
/// The `bytes` field is capped, because the harness asks about small fixtures and a reader that
/// slurps a whole video would be a harness that hangs.
#[tauri::command]
pub fn selftest_file_bytes(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<serde_json::Value, String> {
    const CAP: u64 = 64 * 1024;
    let ws = state.host.state().workspace;
    let full = cwoffee_engine::tools::path::confine(std::path::Path::new(&ws), &path)
        .map_err(|e| e.to_string())?;
    let md = std::fs::metadata(&full).map_err(|_| format!("{path} is not there"))?;
    if !md.is_file() {
        return Err(format!("{path} is not a file"));
    }
    if md.len() > CAP {
        return Err(format!("{path} is {} bytes, more than this reader returns", md.len()));
    }
    let text = std::fs::read_to_string(&full)
        .map_err(|e| format!("{path} is not readable as text: {e}"))?;
    Ok(serde_json::json!({ "path": path, "bytes": md.len(), "text": text }))
}

/// Write one workspace file's bytes, for the harness only.
///
/// The checkpoint claims that matter are the ones where a person changed a file after the turn:
/// the app has to notice and leave it alone. There is no other way to make that state without
/// spending a model turn to do it, and a turn would add checkpoints of its own to the very
/// scope being measured. Confined to the folder, like every other file path in this app.
#[tauri::command]
pub fn selftest_write_file(
    state: tauri::State<'_, AppState>,
    path: String,
    text: String,
) -> Result<serde_json::Value, String> {
    let ws = state.host.state().workspace;
    let full = cwoffee_engine::tools::path::confine(std::path::Path::new(&ws), &path)
        .map_err(|e| e.to_string())?;
    std::fs::write(&full, text.as_bytes()).map_err(|e| format!("could not write {path}: {e}"))?;
    Ok(serde_json::json!({ "path": path, "bytes": text.len() }))
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
    /// The pinned identity row at the foot of the sidebar (`Settings`, then the mark, the name and the
    /// three icons). This was `footer`, reporting the bottom toolbar; the toolbar was removed to give
    /// the transcript and the sidebar its height back, and the signal the watchdog wants - the shell's
    /// chrome rendered - moved with the controls. Defaulted so an older UI bundle still parses; a
    /// bundle that cannot report it is a bundle this line cannot vouch for, which is what SUSPECT means.
    #[serde(default)]
    pub sidefoot: bool,
    pub stream_el: bool,
    /// True when the view is showing the draft/empty state. Reported because a
    /// hidden transcript is expected there, and a diagnostic that lies about it
    /// trains you to ignore the line.
    pub empty_mode: bool,
    pub body_bg: String,
    /// The app's own surface — `.app-shell`, which carries the desk colour or gradient the whole
    /// window sits on. This is the painted-surface signal now: the desktop window is deliberately
    /// transparent at its corners (a frameless window rounds against the desktop, not against a
    /// page), so an empty body background is what a correctly painted window looks like and can no
    /// longer carry the check. Defaulted so an older UI bundle still parses — and an older bundle
    /// reports nothing here, which is exactly when this line should say SUSPECT.
    #[serde(default)]
    pub desk_bg: String,
    pub overlay_present: bool,
    pub text_len: usize,
    pub device_pixel_ratio: f64,
    /// Which picture the opening screen loaded, or `none` when that screen is not on show.
    /// An asset that is not in the installed bundle fails silently: no console line, no broken
    /// icon, just a screen missing its one drawing. Defaulted so an older UI bundle still parses.
    #[serde(default)]
    pub cup: String,
}

#[tauri::command]
pub fn render_report(state: tauri::State<'_, AppState>, report: RenderReport) {
    // The watchdog in main.rs keys off this; without it a blank window looks
    // healthy from every outside signal.
    state.rendered.store(true, std::sync::atomic::Ordering::SeqCst);
    let sane = render_is_sane(&report);
    eprintln!(
        "[cwoffee] render {}: titlebar={} sidebar={} sidefoot={} stream={} empty={} overlay={} text={} bg={} desk={} dpr={:.2} cup={}",
        if sane { "OK" } else { "SUSPECT" },
        report.titlebar,
        report.sidebar,
        report.sidefoot,
        report.stream_el,
        report.empty_mode,
        report.overlay_present,
        report.text_len,
        report.body_bg,
        report.desk_bg,
        report.device_pixel_ratio,
        report.cup,
    );
}

/// Does this report describe a window that actually painted something?
///
/// The surface test is on the app's own shell, not on `document.body`: the desktop window is
/// transparent at the corners now, so a hollow body background is the correct state and a webview
/// that painted nothing is a hollow *shell*. Black still counts as a failure, because the desk is
/// never pure black in either theme.
fn render_is_sane(r: &RenderReport) -> bool {
    fn hollow(s: &str) -> bool {
        let s = s.trim();
        s.is_empty() || s == "none" || s == "rgba(0, 0, 0, 0)" || s == "rgb(0, 0, 0)"
    }
    r.titlebar && r.sidefoot && !r.overlay_present && r.text_len > 0 && !hollow(&r.desk_bg)
}

#[cfg(test)]
mod tests {
    use super::panel_target;
    use super::picked_to_path;
    use super::ws_row_of;
    use super::render_is_sane;
    use super::RenderReport;
    use std::path::PathBuf;
    use tauri_plugin_dialog::FilePath;

    /// Under Wayland/GNOME the desktop portal returns a URL, not a path, and
    /// `FilePath`'s `Display` prints that URL verbatim. `to_string()` was the
    /// whole bug: a folder the user had just picked became `file:///home/...`,
    /// which is not a directory, so the engine refused it and the workspace
    /// control snapped back to whatever the config said.
    /// `list_workspaces` maps the engine's row into this crate's own struct, which is where a field
    /// can go missing without anything failing: the store had the colour, the sidebar drew a list of
    /// colourless folders, and a phone reading the same store saw the colour fine. So the claim is
    /// about the wire — every name `WorkspaceRow` in `ui/src/lib/events.ts` reads has to be there,
    /// with the value the record was given.
    #[test]
    fn a_folder_row_reaching_the_window_carries_every_name_the_window_reads() {
        let row = ws_row_of(cwoffee_engine::store::WorkspaceEntry {
            path: "/work/notes".into(),
            label: "Notes".into(),
            color: "#3f9a72".into(),
            sessions: 3,
        });
        let j = serde_json::to_value(&row).expect("a folder row serializes");
        for key in ["path", "label", "color", "sessions"] {
            assert!(j.get(key).is_some(), "the sidebar reads {key:?} and the wire does not carry it: {j}");
        }
        assert_eq!(j["color"].as_str(), Some("#3f9a72"), "the colour was dropped at the seam: {j}");
        assert_eq!(j["path"].as_str(), Some("/work/notes"));
        assert_eq!(j["sessions"].as_i64(), Some(3));
    }

    /// The window is transparent at the corners now, and the launch check used to read a hollow
    /// body background as "the webview never painted". It must not: the shell is the surface.
    #[test]
    fn a_transparent_body_with_a_painted_shell_is_not_a_blank_window() {
        let mut r = RenderReport {
            titlebar: true,
            sidebar: true,
            sidefoot: true,
            stream_el: true,
            empty_mode: false,
            body_bg: "rgba(0, 0, 0, 0)".into(),
            desk_bg: "gradient".into(),
            overlay_present: false,
            text_len: 3_505,
            device_pixel_ratio: 2.0,
            cup: "cup.webp:480x480".into(),
        };
        assert!(render_is_sane(&r), "a rounded window has an empty body");
        r.desk_bg = "rgba(0, 0, 0, 0)".into();
        assert!(!render_is_sane(&r), "a shell that painted nothing is still blank");
        r.desk_bg = "rgb(0, 0, 0)".into();
        assert!(!render_is_sane(&r), "and a black shell is the old symptom, not a theme");
    }

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

    // --- what the panel's address box accepts ---------------------------------------------

    fn ws() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cwoffee-panel-target-{}", std::process::id()))
    }

    /// A typed address is not a URL a person wrote in a config file; it is whatever they typed.
    /// A bare host gets a scheme, and a bare word is refused rather than guessed at — a box that
    /// sends `pi` to `https://pi` is a box that wastes somebody's afternoon.
    #[test]
    fn a_typed_address_becomes_the_page_the_person_meant() {
        let root = ws();
        std::fs::create_dir_all(&root).unwrap();
        let (url, file) = panel_target("example.com", &root).unwrap();
        assert_eq!((url.as_str(), file), ("https://example.com", false));
        let (url, _) = panel_target("https://x.dev/a?b=1", &root).unwrap();
        assert_eq!(url, "https://x.dev/a?b=1");
        assert!(panel_target("pi", &root).is_err(), "a bare word is not an address");
        assert!(panel_target("", &root).is_err());
    }

    /// A local page inside the workspace opens. The old box refused every local address, which
    /// made it useless for the local pages people write, and the fix must not become a way to read
    /// a home directory off a text field: the refusal names both sides of the line.
    #[test]
    fn a_local_page_opens_inside_the_workspace_and_not_outside_it() {
        let root = ws();
        std::fs::create_dir_all(root.join("site")).unwrap();
        std::fs::write(root.join("site/index.html"), "<p>hi</p>").unwrap();
        let (url, file) = panel_target(&format!("{}/site/index.html", root.display()), &root).unwrap();
        assert!(file, "a local page is a file address, and the caller is told so");
        assert!(url.starts_with("file://"), "{url}");
        let (url, _) = panel_target("site/index.html", &root).unwrap();
        assert!(url.contains("site/index.html"), "a relative path is read against the workspace: {url}");

        let outside = std::env::temp_dir().join(format!("outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.html"), "<p>no</p>").unwrap();
        let why = panel_target(&format!("{}/secret.html", outside.display()), &root).unwrap_err();
        assert!(why.contains("outside the workspace"), "{why}");
        assert!(why.contains(&root.display().to_string()), "and the refusal names the folder it stayed out of: {why}");
        assert!(why.contains("secret.html"), "and the path it refused: {why}");
    }

    /// A scheme that is not a page — `javascript:`, `data:`, `ftp:` — is refused as itself, by
    /// name. A box that hands `javascript:` to a webview is a box that runs a page's script with
    /// the person's own keyboard.
    #[test]
    fn a_scheme_that_is_not_a_page_is_refused_by_name() {
        let root = ws();
        let why = panel_target("javascript:alert(1)", &root).unwrap_err();
        assert!(why.contains("javascript"), "{why}");
        let why = panel_target("data:text/html,<script>1</script>", &root).unwrap_err();
        assert!(why.contains("data"), "{why}");
    }
}
