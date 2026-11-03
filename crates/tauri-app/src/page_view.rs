//! The embedded browser's driver: the far end of the line protocol, answered by a page in this
//! window instead of by a browser in a child process.
//!
//! The page is a real WebKit view sitting in the app's own window (see [`page_embed`]), so the
//! person is looking at the document the engine is driving: a `browse` click repaints the page they
//! are watching, and their own click is the next thing the model reads. There is one page, one
//! cookie jar and one URL, which is the whole point of doing it this way.
//!
//! It answers the **same line protocol** as `browser/session.mjs`, which is what makes it a kept
//! browser rather than a different kind of thing: the idle clock, the panel's sentences about who
//! opened it and when it closes, the gateway's driver/watcher roles and the `cwtool` socket door are
//! one set of code. The engine's tests prove that with a driver that answers in-process.
//!
//! What is deliberately *not* here:
//!
//! * **No app privileges in the page.** The view is built with wry directly, never through Tauri's
//!   `WebviewBuilder`, so a website loaded in that rectangle has no `window.__TAURI__`, no commands,
//!   no capability. The only door is this protocol.
//! * **No callbacks trusted across navigations.** wry documents that an `eval_with_callback`
//!   registered against a webview that is still loading has its callback dropped. So each step is
//!   *polled for* through the bridge's own job table: a lost answer becomes a timeout with a
//!   sentence, never a hang.
//! * **No separate human and agent state.** The bridge taps real pointer, key and wheel input, so a
//!   person touching the page rewinds the idle clock exactly as a `browse` act does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use cwoffee_engine::browser_hub::NativeDriver;

use crate::page_embed::{self, Rect};

/// The script installed in the page before any of its own code runs, on every navigation.
pub const BRIDGE: &str = include_str!("page_bridge.js");

/// A step's answer, before the driver admits the page did not reply.
/// How long one step may wait for the page, when the caller asked for nothing specific.
const STEP_WAIT: Duration = Duration::from_secs(12);
/// How long the bridge is given to come up after a navigation.
const BRIDGE_WAIT: Duration = Duration::from_secs(20);
/// The driver's heartbeat: how often it reads what the page collected.
///
/// This is the "collected automatically, without the model polling" part. The read happens on this
/// timer; the model reads what the hub kept.
const POLL: Duration = Duration::from_millis(900);
/// How long a step may wait for the page to finish arriving before it is issued at all. Short on
/// purpose: a page that takes more than this to settle is a page the person is watching load, and a
/// step held back much longer just looks like a hung tool.
const SETTLE: Duration = Duration::from_secs(4);
/// How long a step that is expected to leave the page waits for the new document. A link click in
/// WebKit answers before the navigation commits, so the boot id can still read the page being left
/// for a few hundred ms — answering in that moment is how a working click is reported as one that
/// did nothing. Long enough for a slow server to start answering, short enough that a dead link is
/// not a stall.
const NAV_WAIT: Duration = Duration::from_millis(1_600);

/// A command from the hub, already turned into something the worker can act on.
enum Msg {
    Open { id: u64, url: String, who: String },
    /// `step_ms` is how long this step may take: the caller's remaining budget, clamped. See
    /// [`step_budget`].
    Act { id: u64, verb: String, step_ms: u64, req: Value },
    State { id: u64 },
    Place { id: u64, rect: Rect },
    Watch { id: u64 },
    Frame { id: u64 },
    Viewport { id: u64 },
    Ping { id: u64 },
    Close { id: u64 },
    Quit,
}

pub struct PageDriver {
    tx: Mutex<Option<Sender<Msg>>>,
    app: tauri::AppHandle,
    /// Which folder a local page may be opened from — asked of the engine when it is needed, because
    /// the folder moves when the person switches conversation and this window does not.
    ws_of: page_embed::WsOf,
}

impl PageDriver {
    pub fn new(app: &tauri::AppHandle, ws_of: page_embed::WsOf) -> Arc<Self> {
        Arc::new(Self { tx: Mutex::new(None), app: app.clone(), ws_of })
    }
}

impl NativeDriver for PageDriver {
    fn attach(&self, out: UnboundedSender<String>) {
        let (tx, rx) = mpsc::channel::<Msg>();
        *self.tx.lock().expect("page driver lock") = Some(tx);
        let app = self.app.clone();
        let ws_of = self.ws_of.clone();
        thread::Builder::new()
            .name("cwoffee-page".into())
            .spawn(move || {
                worker(app, ws_of, rx, out);
            })
            .expect("spawn the embedded page worker");
    }

    fn send(&self, line: &str) {
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => return,
        };
        let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let cmd = value
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rect = Rect {
            x: value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as u32,
            y: value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as u32,
            // The hub sends these as JSON numbers built from `f64`, and `as_u64` answers `None` for
            // a float — which is how a panel box of 300x200 becomes 0x0 and the page sits wherever
            // it likes, with nothing said about it.
            w: value.get("width").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as u32,
            h: value.get("height").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as u32,
        };
        let msg = match cmd.as_str() {
            "open" => Msg::Open {
                id,
                url: value
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("about:blank")
                    .to_string(),
                who: value
                    .get("who")
                    .and_then(|v| v.as_str())
                    .unwrap_or("the model")
                    .to_string(),
            },
            "act" => Msg::Act {
                id,
                verb: value.get("verb").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                step_ms: step_budget(value.get("timeout_ms")),
                req: value,
            },
            "state" => Msg::State { id },
            "view_rect" => Msg::Place { id, rect },
            "watch" | "unwatch" => Msg::Watch { id },
            "frame" => Msg::Frame { id },
            "viewport" => Msg::Viewport { id },
            "ping" => Msg::Ping { id },
            "close" => Msg::Close { id },
            "quit" => Msg::Quit,
            other => {
                eprintln!("[cwoffee/browser] the page driver was asked for {other}, which it does not answer");
                return;
            }
        };
        let tx = self.tx.lock().expect("page driver lock").clone();
        if let Some(tx) = tx {
            let _ = tx.send(msg);
        }
    }

    fn quit(&self) {
        let tx = self.tx.lock().expect("page driver lock").take();
        if let Some(tx) = tx {
            let _ = tx.send(Msg::Quit);
        }
    }
}

/// How long one step may take, in the page's own terms.
///
/// The walk's caller sends its remaining budget with every step, and the page box has its own
/// deadline for an answer from the document. Left at a constant, a walk with 10 seconds left had
/// 30 seconds to hear back from the page: the box answered "the page did not answer within 30s"
/// *after* the walk had already been stopped for time, and the model was left holding two
/// contradictory sentences and no page report. Clamping to the caller's clock leaves one deadline
/// and one report.
fn step_budget(given: Option<&Value>) -> u64 {
    const FLOOR: u64 = 500;
    const DEFAULT: u64 = STEP_WAIT.as_millis() as u64;
    const ANSWERABLE: u64 = 25_000;
    match given.and_then(|v| v.as_f64()) {
        Some(ms) if ms.is_finite() && ms > 0.0 => (ms as u64).clamp(FLOOR, ANSWERABLE),
        _ => DEFAULT,
    }
}

/// What the driver knows about the page it is standing in front of.
#[derive(Default)]
struct Know {
    open: bool,
    rect: Rect,
    url: String,
    title: String,
    /// The bridge's per-document id. A different boot means a new document — which is how a link
    /// the person clicked, or a `location.href` the page ran, reaches the hub.
    boot: String,
    nav: u64,
    /// The driver's cursor into the page's diagnostics buffer.
    cursor: i64,
    touched: i64,
    size: (u32, u32),
}

fn worker(app: tauri::AppHandle, ws_of: page_embed::WsOf, rx: Receiver<Msg>, out: UnboundedSender<String>) {
    let know = Arc::new(Mutex::new(Know::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let say = |v: Value, out: &UnboundedSender<String>| {
        let _ = out.send(v.to_string());
    };

    // One tag per step, so an answer left in a document that is being thrown away can never be
    // read as the answer to the step now running.
    let mut job_seq: u64 = 0;
    while !stop.load(Ordering::Relaxed) {

        let msg = match rx.recv_timeout(POLL) {
            Ok(m) => Some(m),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Some(Msg::Quit),
        };
        match msg {
            Some(Msg::Quit) => {
                let _ = page_embed::close(&app);
                break;
            }
            Some(Msg::Close { id }) => {
                let _ = page_embed::close(&app);
                let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
                k.open = false;
                k.url.clear();
                k.title.clear();
                k.boot.clear();
                k.cursor = 0;
                say(json!({"id": id, "ok": true, "alive": false}), &out);
                continue;
            }
            Some(Msg::Open { id, url, who }) => {
                let rect = know.lock().unwrap_or_else(|p| p.into_inner()).rect;
                if let Err(e) = page_embed::open(&app, &url, rect, BRIDGE, out.clone(), ws_of.clone()) {
                    say(
                        json!({"id": id, "ok": false, "alive": false, "why": e}),
                        &out,
                    );
                    continue;
                }
                // Whether this address can produce a new document at all. The same page with a
                // different anchor stays in the document that is already up, so waiting for a new one
                // would wait forever; any other address means a document is on its way, and the step
                // that follows has to be aimed at that one — clicking the page that is still on screen
                // while a new one loads is a click on a page that is about to disappear.
                let anchor_only = {
                    let k = know.lock().unwrap_or_else(|q| q.into_inner());
                    !k.url.is_empty()
                        && k.url.split('#').next().unwrap_or("") == url.split('#').next().unwrap_or("")
                        && k.url != url
                };
                let was = {
                    // The document that is up right now, **kept** rather than cleared. Two things
                    // depend on the real answer: the wait below has to see a *different* document id
                    // before it accepts a navigation, and the reader that pulls this page's records
                    // resets its cursor exactly when the document id changes. Taking the id here made
                    // both wrong — an open accepted the page that was already on screen, and the
                    // cursor reset re-sent records the previous document had already handed over, so
                    // the drawer showed the same console line and the same failed request twice, under
                    // new ids and with identical timestamps.
                    let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
                    k.open = true;
                    k.boot.clone()
                };
                // The page has to load and its bridge has to come up before it can be described.
                // Waiting here is what makes the answer name the page that is really on screen.
                let ready = wait_bridge(&app, &know, &out, &was, &url, !anchor_only);
                let now = read_state(&app, &know);
                if !ready {
                    // A document that never produced a bridge is a navigation failure, and the only
                    // honest report of one: WebKit's own `load-failed` signal is not reachable from
                    // wry, so the absence of a page is what we can actually observe.
                    say(
                        json!({"ev": "diag", "diag": {
                            "t": now_ms(), "kind": "navigation", "level": "error",
                            "text": "the page never finished loading: no document came up at all",
                            "url": url
                        }}),
                        &out,
                    );
                }
                {
                    let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some((u, t)) = now.as_ref() {
                        k.url = u.clone();
                        k.title = t.clone();
                    }
                }
                let (u, t) = now.unwrap_or_else(|| (url.clone(), String::new()));
                let size = know.lock().unwrap_or_else(|p| p.into_inner()).size;
                say(
                    json!({
                        "id": id,
                        "ok": ready,
                        "alive": ready,
                        "url_before": url,
                        "url_after": u,
                        "title": t,
                        "native": true,
                        "who": who,
                        "viewport": {"width": size.0, "height": size.1},
                        "placed": tell_placed(&app, &out).unwrap_or(Value::Null),
                        "why_not": if ready { Value::Null } else { json!("the page never finished loading") }
                    }),
                    &out,
                );
                continue;
            }
            Some(Msg::Place { id, rect }) => {
                let shown = rect.usable();
                let applied = page_embed::place(&app, rect).unwrap_or(rect);
                let _ = page_embed::visible(&app, shown);
                {
                    let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
                    k.rect = applied;
                }
                let got = tell_placed(&app, &out);
                // And told to the hub, as an event rather than only as the answer to this command.
                // The panel's box is the page's viewport here, and the number has to be in the
                // snapshot: an answer that reaches only the window that asked leaves every other
                // reader — a phone, the model — sizing a page it cannot see.
                let _ = out.send(
                    json!({"ev": "viewport", "viewport": {"width": applied.w, "height": applied.h}}).to_string(),
                );
                say(
                    json!({"id": id, "ok": true, "x": applied.x, "y": applied.y, "width": applied.w, "height": applied.h,
                        "placed": got.unwrap_or_else(|| json!({"x": applied.x, "y": applied.y, "width": applied.w, "height": applied.h, "shown": shown}))}),
                    &out,
                );
                continue;
            }
            Some(Msg::Watch { id }) => {
                // There is nothing to stream: the panel is looking at the page, not at a picture of
                // it. Saying so is what stops the panel drawing a live mark it did not earn.
                say(
                    json!({"id": id, "ok": true, "live": false, "why": "the page is the page: this browser is on screen, so there is no stream of pictures"}),
                    &out,
                );
                continue;
            }
            Some(Msg::Frame { id }) => {
                say(
                    json!({"id": id, "ok": false, "why": "there is no picture of a page that is already on screen. This browser is the page itself: read it with text, snapshot, a11y or eval"}),
                    &out,
                );
                continue;
            }
            Some(Msg::Viewport { id }) => {
                say(
                    json!({"id": id, "ok": false, "why": "the page fills the box the panel gives it; the panel sets that box, so the page is not resized out from under it"}),
                    &out,
                );
                continue;
            }
            Some(Msg::Ping { id }) => {
                let open = know.lock().unwrap_or_else(|p| p.into_inner()).open;
                say(json!({"id": id, "ok": true, "alive": open}), &out);
                continue;
            }
            Some(Msg::State { id }) => {
                let read = read_state(&app, &know);
                let k = know.lock().unwrap_or_else(|p| p.into_inner());
                let (u, t) = read.unwrap_or_else(|| (k.url.clone(), k.title.clone()));
                say(
                    json!({
                        "id": id, "ok": true, "alive": k.open, "url": u, "title": t,
                        "nav": k.nav, "native": true,
                        "viewport": {"width": k.size.0, "height": k.size.1}
                    }),
                    &out,
                );
                continue;
            }
            Some(Msg::Act { id, req, step_ms, .. }) => {
                // Never run a step in a document that is on its way out. A click that starts a
                // navigation leaves the old page alive for a moment, and a step issued into that
                // moment answers perfectly — about a page that no longer exists. That is the whole
                // "the browser is lying to me" complaint: `location.href` came back as the old url
                // after the link had been clicked, and the model reasonably concluded the click had
                // not worked when it had.
                settle(&app, SETTLE);
                let boot0 = page_boot(&app);
                job_seq += 1;
                let answer = run_job(&app, &req, &format!("j{job_seq}"), step_ms, &boot0);
                // Flat, at the top level, with the id: that is how session.mjs answers a command,
                // and the hub reads `ok`, `value` and `why` off the answer itself. Nesting the
                // reply under a key is how a working click quietly counts as a failure.
                let mut reply = match answer {
                    Value::Object(map) => map,
                    other => serde_json::Map::from_iter([("value".to_string(), other)]),
                };
                reply.insert("id".to_string(), json!(id));
                // Did this step move the page? If it did, the step's own answer came from the page it
                // started in, and the next step of the walk must not be issued into a document that is
                // still arriving. Say which of the two happened, and wait for the page that is coming.
                let mut boot1 = page_boot(&app);
                if boot1 == boot0 && reply.get("may_navigate").and_then(|v| v.as_bool()).unwrap_or(false) {
                    // The step said it was meant to leave: wait for the document it is leaving for.
                    let until = Instant::now() + NAV_WAIT;
                    while Instant::now() < until {
                        let now = page_boot(&app);
                        if !now.is_empty() && now != boot0 {
                            boot1 = now;
                            break;
                        }
                        thread::sleep(Duration::from_millis(60));
                    }
                }
                if !boot0.is_empty() && !boot1.is_empty() && boot0 != boot1 {
                    reply.insert("navigated".to_string(), json!(true));
                    reply.insert("url_from".to_string(), json!(know.lock().unwrap_or_else(|p| p.into_inner()).url));
                    settle(&app, BRIDGE_WAIT);
                    reply.insert("url_after".to_string(), json!(page_url(&app)));
                }
                let _ = out.send(Value::Object(reply).to_string());
                // The page may have moved somewhere else as a result of that step.
                sync(&app, &ws_of, &know, &out);
                continue;
            }
            None => {}
        }
        // The heartbeat. This is what makes diagnostics arrive without anybody asking for them.
        sync(&app, &ws_of, &know, &out);
    }
}

/// Read the page's state: (url, title), or None if the bridge is not answering.
fn read_state(app: &tauri::AppHandle, know: &Arc<Mutex<Know>>) -> Option<(String, String)> {
    let raw = eval_wait(app, "window.__cw ? JSON.stringify(window.__cw.state()) : ''", Duration::from_millis(2_500))?;
    if raw.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&raw).ok()?;
    let u = v.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let t = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(sz) = v.get("size") {
        k.size = (
            sz.get("w").and_then(|w| w.as_u64()).unwrap_or(0) as u32,
            sz.get("h").and_then(|h| h.as_u64()).unwrap_or(0) as u32,
        );
    }
    Some((u, t))
}

/// Pull the page's diagnostics and its navigation, and hand them to the hub.
fn sync(
    app: &tauri::AppHandle,
    ws_of: &page_embed::WsOf,
    know: &Arc<Mutex<Know>>,
    out: &UnboundedSender<String>,
) {
    let raw = match eval_wait(
        app,
        "window.__cw ? JSON.stringify(window.__cw.poll(0)) : ''",
        Duration::from_millis(2_500),
    ) {
        Some(raw) if !raw.is_empty() => raw,
        _ => return,
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let boot = v.get("boot").and_then(|b| b.as_str()).unwrap_or("").to_string();
    let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let touched = v.get("touched").and_then(|t| t.as_i64()).unwrap_or(0);
    let cursor = v.get("cursor").and_then(|c| c.as_i64()).unwrap_or(0);
    let size = v
        .get("size")
        .map(|s| (
            s.get("w").and_then(|w| w.as_u64()).unwrap_or(0) as u32,
            s.get("h").and_then(|h| h.as_u64()).unwrap_or(0) as u32,
        ))
        .unwrap_or((0, 0));
    let items: Vec<Value> = v
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();

    // Decide everything under the lock, send after it: the hub's reader is this same thread, and
    // a lock held across a send is a lock held across somebody else's bookkeeping.
    let mut nav: Option<Value> = None;
    let mut moved: Option<Value> = None;
    let mut activity = false;
    let mut fresh: Vec<Value> = Vec::new();
    {
        let mut k = know.lock().unwrap_or_else(|p| p.into_inner());
        // A new document, whoever caused it. The hub's `nav` event is what keeps its state from
        // going stale on a link the person clicked.
        let new_document = !boot.is_empty() && boot != k.boot;
        if new_document {
            k.boot = boot.clone();
            k.nav += 1;
            // A new document counts its own records from one again. Left alone, the old cursor would
            // decide the new page's first records were already seen, and the drawer would quietly
            // stop filling after a navigation.
            k.cursor = 0;
            k.url = url.clone();
            k.title = title.clone();
            nav = Some(json!({"ev": "nav", "url": url, "title": title}));
        }
        // The same document at another address: a fragment, `history.pushState`, a Back through
        // either. The page counter stays where it was — the document did not change — but the url the
        // hub holds has to follow, or a person pressing Back sees a panel that insists nothing moved.
        if !new_document && !boot.is_empty() && !url.is_empty() && url != k.url {
            k.url = url.clone();
            k.title = title.clone();
            moved = Some(json!({"ev": "url", "url": url, "title": title}));
        }
        if size.0 > 0 && size.1 > 0 {
            k.size = size;
        }
        // A hand on the page is activity, and the idle clock is about activity. Watching a page is
        // not using one; clicking in it is — and here, the click is real.
        if touched > k.touched {
            k.touched = touched;
            activity = true;
        }
        for item in items {
            let seq = item.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
            if seq > k.cursor {
                k.cursor = seq;
                fresh.push(item);
            }
        }
    }
    let _ = cursor;
    if let Some(nav) = nav {
        let _ = out.send(nav.to_string());
    }
    if let Some(moved) = moved {
        let _ = out.send(moved.to_string());
    }
    if activity {
        let _ = out.send(json!({"ev": "activity"}).to_string());
    }
    for item in fresh {
        // **A file the page offered.** WebKitGTK stopped telling its embedder about downloads, so a
        // `download` link in this browser is a plain navigation and nothing is ever saved. The bridge
        // cancels that navigation and leaves this record instead; fetching the bytes is the app's
        // job, and the answer goes back into the same log so the person and the model can both see
        // where the file went.
        if is_offer(&item) {
            save_offered(&item, ws_of, out);
        }
        let _ = out.send(json!({"ev": "diag", "diag": item}).to_string());
    }
}

/// A record the page left asking the app to fetch a file. The bridge owns the words; this only owns
/// the shape, and the test next to it keeps the two agreeing.
fn is_offer(item: &Value) -> bool {
    item.get("kind").and_then(|k| k.as_str()) == Some("download")
        && item.get("save").and_then(|s| s.as_bool()).unwrap_or(false)
}

/// Fetch what a page offered and report it. Spawned, because a page that points at a slow server
/// must not stop the reader that carries everything else about that page.
fn save_offered(item: &Value, ws_of: &page_embed::WsOf, out: &UnboundedSender<String>) {
    let url = match item.get("url").and_then(|u| u.as_str()) {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => {
            let _ = out.send(diag_line("download", "error", "a download was offered with no address to fetch"));
            return;
        }
    };
    let ws = (ws_of)();
    let out = out.clone();
    tauri::async_runtime::spawn(async move {
        let said = match cwoffee_engine::browser_hub::downloads::save(&url, &ws).await {
            Ok(path) => {
                let rel = path
                    .strip_prefix(&ws)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                diag_line(
                    "download",
                    "info",
                    &format!("saved {bytes} bytes to {rel}"),
                )
            }
            Err(why) => diag_line("download", "error", &format!("would not save {url}: {why}")),
        };
        let _ = out.send(said);
    });
}

/// One record, in the shape the page's own records arrive in. The hub numbers it and says which
/// page it belongs to, the same as any other line in the log.
fn diag_line(kind: &str, level: &str, text: &str) -> String {
    json!({"ev": "diag", "diag": {"t": now_ms(), "kind": kind, "level": level, "text": text}}).to_string()
}

/// Wait until the bridge answers in the current document.
/// Where the page widget really ended up, sent to the hub as an event.
///
/// The panel asked for a rectangle; this is what the widget tree made of that. A page nobody placed
/// has no box, and a page with a box it was not shown paints nothing — either one leaves the panel
/// with an empty frame and nothing to say, which is the failure this line exists to make audible.
fn tell_placed(app: &tauri::AppHandle, out: &UnboundedSender<String>) -> Option<Value> {
    let (r, shown) = page_embed::placed(app).ok()??;
    let v = json!({"x": r.x, "y": r.y, "width": r.w, "height": r.h, "shown": shown});
    let _ = out.send(json!({"ev": "placed", "placed": v.clone()}).to_string());
    Some(v)
}

/// Wait until there is a page to describe: a document that was not there before (`was`), or the
/// address we asked for in the document that is already there — an anchor jump stays in one
/// document, so waiting for a new one would wait forever.
fn wait_bridge(
    app: &tauri::AppHandle,
    know: &Arc<Mutex<Know>>,
    out: &UnboundedSender<String>,
    was: &str,
    want: &str,
    new_document: bool,
) -> bool {
    let started = Instant::now();
    while started.elapsed() < BRIDGE_WAIT {
        if let Some(raw) = eval_wait(
            app,
            "window.__cw ? window.__cw.boot + '|' + location.href : ''",
            Duration::from_millis(2_000),
        ) {
            if let Some((boot, href)) = raw.split_once('|') {
                if boot.is_empty() {
                    continue;
                }
                if boot != was {
                    return true;
                }
                // Only an anchor jump keeps its document. Anything else has to arrive as a new one,
                // or the page this reply describes is the page the last navigation left behind.
                if !new_document && href == want {
                    return true;
                }
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
    let _ = (know, out);
    false
}

fn eval_wait(app: &tauri::AppHandle, script: &str, budget: Duration) -> Option<String> {
    let rx = page_embed::eval(app, script).ok()?;
    let raw = rx.recv_timeout(budget).ok()?;
    Some(unquote(&raw))
}

/// wry hands back the JSON encoding of whatever the expression evaluated to. A string comes back
/// as a JSON string, so the value has to be taken off once before it is the text or the JSON we asked for.
fn unquote(raw: &str) -> String {
    match serde_json::from_str::<String>(raw) {
        Ok(s) => s,
        Err(_) => raw.to_string(),
    }
}

/// One step in the page, answered the way `session.mjs` answers it: `{ok, ...}` or `{ok:false, why}`.
///
/// The command object is handed to the bridge as it arrived: the verb and its arguments are the
/// same names the Playwright driver reads, so `target`, `text`, `x`, `code` and the rest mean the
/// same thing in both.
/// The bridge's id for the document that is on screen right now, empty when there is no bridge.
fn page_boot(app: &tauri::AppHandle) -> String {
    eval_wait(
        app,
        "window.__cw ? window.__cw.boot : ''",
        Duration::from_millis(2_000),
    )
    .unwrap_or_default()
}

/// The url of the document that is on screen right now.
fn page_url(app: &tauri::AppHandle) -> String {
    eval_wait(app, "location.href", Duration::from_millis(2_000)).unwrap_or_default()
}

/// Wait until the page is a settled document with a bridge in it — `document.readyState ===
/// 'complete'` and `window.__cw` present. Issuing a step before that is what makes a walk read a
/// page that has already been thrown away: the old document is still scriptable while the new one is
/// arriving, so the step succeeds and tells the truth about the wrong page.
fn settle(app: &tauri::AppHandle, budget: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < budget {
        if let Some(raw) = eval_wait(
            app,
            "window.__cw ? document.readyState : ''",
            Duration::from_millis(2_000),
        ) {
            if raw == "complete" {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
    false
}

fn run_job(app: &tauri::AppHandle, req: &Value, tag: &str, step_ms: u64, boot0: &str) -> Value {
    // The caller's clock, not a constant. A walk with 10 seconds left must not spend 30 waiting on
    // a page that cannot possibly still answer in time.
    let wait = Duration::from_millis(step_ms);
    let script = run_script(req, tag);
    let Some(id) = eval_wait(app, &script, Duration::from_millis(4_000)) else {
        return json!({"ok": false, "why": "the page did not take the step"});
    };
    if id == "no bridge" {
        return json!({"ok": false, "why": "there is no cwoffee bridge in this page (the page is still loading, or it took the frame away from us)"});
    }
    // `a|` is the answer, `b|` is "not answered yet, and here is the id of the document that is up".
    // The second half is the whole point: a step that is still waiting when the page is replaced has
    // no answer left to wait for — the table it was sitting in went away with the document — and
    // polling that table for the caller's whole budget is how a step that worked (a Back, a form
    // submit, a link click) gets reported as "the page did not answer within 20s".
    let poll = poll_script(&id);
    let started = Instant::now();
    loop {
        match eval_wait(app, &poll, Duration::from_millis(3_000)) {
            Some(raw) if raw.starts_with("a|") => {
                return serde_json::from_str::<Value>(&raw[2..])
                    .unwrap_or(json!({"ok": false, "why": "the page's answer was not readable"}))
            }
            Some(raw) if raw.starts_with("b|") => {
                if moved_since(boot0, &raw[2..]) {
                    return lost_to_navigation(app, req);
                }
            }
            // The poll itself got no answer. The common reason is not a dead page: it is that the
            // document the poll was aimed at is being replaced underneath it — a Back, a submit, a
            // link click — and wry drops the callback of an eval aimed at a page on its way out.
            // Concluding "the page stopped answering" from that is how a step that worked gets
            // reported as a failure, so the new document is asked who it is first.
            Some(_) => {}
            None => {
                let now = page_boot(app);
                if !now.is_empty() {
                    if moved_since(boot0, &now) {
                        return lost_to_navigation(app, req);
                    }
                } else if started.elapsed() > wait {
                    return json!({"ok": false, "why": "the page stopped answering"});
                }
            }
        }
        if started.elapsed() > wait {
            // Say the number, and say it in the sentence: a model reading "within {}s" learns
            // nothing about how long it had, which is the one fact that tells it whether the page
            // was slow or the box was broken.
            return json!({
                "ok": false,
                "why": format!("the page did not answer within {}s", wait.as_secs()),
                "step_wait_s": wait.as_secs()
            });
        }
        thread::sleep(Duration::from_millis(30));
    }
}

/// The document up now is not the document the step was issued into.
fn moved_since(boot0: &str, boot_now: &str) -> bool {
    !boot0.is_empty() && !boot_now.is_empty() && boot_now != boot0
}

/// Hand the step to the page. The page's answer comes later, into a table, under the key it was
/// given; all this returns is the key back, which is how the app learns a page took the step at all.
fn run_script(req: &Value, tag: &str) -> String {
    format!(
        "window.__cw ? (window.__cw.run({req}, '{tag}'), '{tag}') : 'no bridge'",
        req = serde_json::to_string(req).unwrap_or_else(|_| "{}".into()),
        tag = tag
    )
}

/// Ask the page for the answer. `a|` is the answer; `b|` is "not answered yet, and here is the id of
/// the document that is up". The second half is the whole point: a step that is still waiting when
/// the page is replaced has no answer left to wait for — the table it was sitting in went away with
/// the document — and polling that table for the caller's whole budget is how a step that worked (a
/// Back, a submit, a link click) gets reported as "the page did not answer within 20s".
fn poll_script(id: &str) -> String {
    format!(
        "window.__cwR && '{id}' in window.__cwR ? (function(){{var r = window.__cwR['{id}']; window.__cwDone('{id}'); return 'a|' + (typeof r === 'string' ? r : JSON.stringify(r))}})() : (window.__cw ? 'b|' + window.__cw.boot : '')"
    )
}

/// A step whose answer lived in a document that has been replaced. This is not "it failed" and it is
/// not silently "it worked": it is "the page moved, and here is where it moved to" — which is the
/// only honest thing left to say, and far better than the 20-second timeout it used to be.
///
/// When the step *asked* for that move — Back, Forward, a reload, a navigate — a new document is the
/// outcome, so the step is reported as succeeding, with a note about the answer that was lost. For any
/// other step the move was not asked for, and the reader is told it cannot conclude anything about
/// the step until it reads the page that is up now.
fn lost_to_navigation(app: &tauri::AppHandle, req: &Value) -> Value {
    let now = page_url(app);
    let where_now = if now.is_empty() { "somewhere else".to_string() } else { now };
    if asked_to_move(req) {
        return json!({
            "ok": true,
            "navigated": true,
            "url_after": where_now,
            "note": format!(
                "the page moved to {where_now}, which is what this step asked for; the step's own answer \
                 went away with the document it replaced, so the address above is the app's reading of \
                 the move, not the page's own answer",
            ),
        });
    }
    json!({
        "ok": false,
        "why": format!(
            "the page moved to {where_now} while this step was still running, and the step's own answer \
             went away with the document it was asked in; the step may or may not have taken effect — \
             read the page that is up now before deciding",
        ),
        "navigated": true,
        "lost_to_navigation": true,
    })
}

/// Whether a step's whole job was to leave the page. For those verbs a new document on the other side
/// of the step is the outcome, not an accident.
fn asked_to_move(req: &Value) -> bool {
    matches!(
        req.get("do").and_then(|d| d.as_str()).unwrap_or(""),
        "back" | "forward" | "reload" | "navigate" | "open"
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::BRIDGE;
    use super::asked_to_move;
    use super::is_offer;
    use super::moved_since;
    use super::poll_script;
    use super::run_script;
    use super::step_budget;
    use super::STEP_WAIT;
    use serde_json::json;

    /// The step protocol is spelled in two languages — the page's own script and the JavaScript this
    /// crate builds — and joined by three names: the answer table, the cleanup hook, and the id of the
    /// document that is up. Nothing else in the repo notices when one half renames one of them: the
    /// page answers into a table the app then polls under a different key, and every step in a walk
    /// "times out" in a browser that answered instantly. So the agreement is asserted here, where a
    /// rename on either side makes it fail.
    #[test]
    fn the_page_and_the_app_agree_on_where_an_answer_lives() {
        let run = run_script(&json!({ "do": "click", "target": "Pay" }), "cw1");
        assert!(run.contains("window.__cw.run("), "the step is handed to the bridge the page carries");
        assert!(BRIDGE.contains("(window.__cw = {})"), "the page publishes that bridge");
        assert!(run.contains("{\"do\":\"click\",\"target\":\"Pay\"}"), "the step travels as the JSON it was given");

        let poll = poll_script("cw1");
        assert!(poll.contains("__cwR"), "the app reads the answer out of the page's own table");
        assert!(BRIDGE.contains("window.__cwR[key] = answer"), "and the page leaves it there");
        assert!(poll.contains("__cwDone"), "the app clears the row once it has read it");
        assert!(BRIDGE.contains("window.__cwDone ="), "and the page hands over that hook");
        assert!(poll.contains("__cw.boot"), "a step that has not answered names the document that is up");
        assert!(BRIDGE.contains("cw.boot ="), "and the page keeps that id on the bridge it installs");
        assert!(poll.contains("'a|'"), "an answer is marked as one");
        assert!(poll.contains("'b|'"), "a step still outstanding says so, and not with a timeout");
        assert!(
            BRIDGE.contains("window.__cwR[key] = answer") && !BRIDGE.contains("'b|'"),
            "the page only ever writes answers into the table; the difference between an answer and a wait is the app's to make"
        );
    }

    /// The page marks an offer; the app acts on that mark. The names live in two files, so the
    /// agreement is a test rather than a hope — a rename on one side would otherwise leave a download
    /// that records a promise and never fetches the file.
    #[test]
    fn a_download_offer_means_the_same_thing_on_both_sides() {
        assert!(
            is_offer(&json!({"kind": "download", "save": true, "url": "http://x/a.zip"})),
            "the shape the bridge writes"
        );
        assert!(
            !is_offer(&json!({"kind": "download", "url": "http://x/a.zip"})),
            "a record that only says a download happened does not ask for a fetch"
        );
        assert!(!is_offer(&json!({"kind": "console", "save": true})), "nor does any other kind");
        assert!(
            BRIDGE.contains("record('download', 'info'") && BRIDGE.contains("save: true"),
            "and the page really does write that shape"
        );
        assert!(
            BRIDGE.contains("'url', 'status', 'method', 'where', 'nav', 'save'"),
            "the bridge passes the flag out of its record, or the offer would arrive without it"
        );
    }

    /// A step is issued into one document and answered in whichever is up when the poll runs. The two
    /// boot ids disagree only when a real navigation happened; an empty id on either side means a page
    /// that has no bridge (still loading, or gone), which is not evidence of a move.
    #[test]
    fn a_step_is_only_lost_when_a_different_document_is_up() {
        assert!(moved_since("aaa", "bbb"), "a new document arrived");
        assert!(!moved_since("aaa", "aaa"), "the same page is not a move");
        assert!(!moved_since("", "bbb"), "the step was issued into a page with no bridge id");
        assert!(!moved_since("aaa", ""), "the new document has not come up yet");
    }

    /// A navigation that swallows a step's answer is an answer for the verbs that asked for it and no
    /// evidence at all for the others: Back that lands on a new document worked, a `wait` interrupted
    /// by somebody else's navigation proved nothing either way.
    #[test]
    fn a_move_answers_for_the_steps_that_asked_for_it() {
        for verb in ["back", "forward", "reload", "navigate", "open"] {
            assert!(asked_to_move(&json!({ "do": verb })), "{verb} asked to leave: a new document is its outcome");
        }
        for verb in ["wait", "wait_for", "eval", "click", "fill", "text", ""] {
            assert!(!asked_to_move(&json!({ "do": verb })), "{verb} did not ask to leave: the move says nothing about it");
        }
        assert!(!asked_to_move(&json!({ "target": "#go" })), "a job with no verb asks for nothing");
    }

    /// The page box used to wait on its own constant regardless of what the walk asked for. A
    /// `browse` walk with 10 seconds left therefore had a step that could still be running 30
    /// seconds later — long after the tool had said "the browser did not answer within 10s" — and
    /// the two answers disagreed about what had happened. One clock, the caller's.
    #[test]
    fn a_step_is_given_the_time_the_walk_has_left_and_never_more_than_it_can_answer_inside() {
        assert_eq!(step_budget(Some(&json!(10_000))), 10_000, "the caller's budget, taken");
        assert_eq!(step_budget(Some(&json!(90_000))), 25_000, "clamped to what the box can answer inside");
        assert_eq!(step_budget(Some(&json!(10))), 500, "and floored: a 10ms step is a guaranteed failure");
        assert_eq!(step_budget(None), STEP_WAIT.as_millis() as u64, "asked for nothing: the default");
        assert_eq!(
            step_budget(Some(&json!("not a number"))),
            STEP_WAIT.as_millis() as u64,
            "a budget that is not a number is no budget, not a zero"
        );
    }
}
