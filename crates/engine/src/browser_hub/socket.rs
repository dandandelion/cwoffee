//! The kept browser over a unix socket, for the core whose tools do not live in the app.
//!
//! On the pi core the tool host (`cwtool`) is a fresh process per call. It cannot hold the
//! browser — a browser held there would be a new browser per call, which is the exact thing
//! a kept browser exists to avoid — so the app keeps it and this socket is the door.
//!
//! One request per connection, one line of JSON back, the same discipline as
//! [`crate::mcp::socket`]: a tool process that dies mid-call should cost a broken pipe, not a
//! stuck browser. The frame bytes are the only large thing that crosses here, and only when a
//! *viewer* asked for them.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::{Hub, Snapshot, Who};

/// How long a tool process waits for the app to answer about its browser. Deliberately
/// short: a missing app should turn into a sentence in the tool result, not a hang.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// Hand out the door, or say why there is none. The caller must not name a path that does
/// not answer: a tool process told to ask a dead socket gets a confusing "connection refused"
/// where the truth is that this start has no browser it can reach.
pub async fn serve(hub: std::sync::Arc<Mutex<Hub>>, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let listener = match tokio::net::UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) => {
                return Err(format!("no kept-browser socket at {}: {e}", path.display()))
            }
        };
        // The socket holds a browser the person can see. Only this user's processes may ask
        // it to click things; the default (0o755 on some systems) would let other accounts
        // drive it.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        eprintln!("[cwoffee/browser] kept browser answers on {}", path.display());
        // The accept loop runs on its own task and this returns. A caller that awaited it would
        // never get past mounting the browser — the app would start, print its socket path, and
        // then sit there without ever coming up. Same shape as `crate::mcp::socket::serve`, and
        // for the same reason.
        tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let hub = std::sync::Arc::clone(&hub);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.iter().any(|b| *b == b'\n') || buf.len() > 64 * 1024 {
                        break;
                    }
                }
                let req: Value = match serde_json::from_slice(&buf) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = stream.write_all(format!("{}\n", json!({"ok": false, "why": format!("not JSON: {e}")})).as_bytes()).await;
                        return;
                    }
                };
                let out = dispatch(&hub, &req).await;
                let _ = stream.write_all(format!("{}\n", out).as_bytes()).await;
            });
            }
        });
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (hub, path);
        Err("this platform has no unix sockets, so no tool process can reach the kept browser".to_string())
    }
}

async fn dispatch(hub: &std::sync::Arc<Mutex<Hub>>, req: &Value) -> Value {
    let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("").to_string();
    // Who is on the other end of this socket. A tool process says "model"; the app itself
    // never uses this door for its own clicks. The fact is recorded, not trusted for
    // permission — there is no permission here beyond what the app's own approval already gave.
    let who = match req.get("who").and_then(|w| w.as_str()).unwrap_or("model") {
        "person" => Who::Person,
        _ => Who::Model,
    };
    match cmd.as_str() {
        "state" | "ping" => {
            let mut h = hub.lock().await;
            match h.state().await {
                Ok(s) => state_value(&s),
                Err(e) => json!({"ok": false, "why": e}),
            }
        }
        "open" => {
            let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
            let allow_file = req.get("allow_file").and_then(|a| a.as_bool()).unwrap_or(false);
            let mut h = hub.lock().await;
            match h.open(&url, who, allow_file).await {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "why": e}),
            }
        }
        "act" => {
            let mut act = req.get("act").cloned().unwrap_or_else(|| json!({}));
            if act.get("do").is_none() {
                // Tolerate the flat form, where the act fields sit on the request itself:
                // a tool process building one command line at a time should not have to know
                // which nesting the app prefers.
                for k in ["do", "target", "text", "key", "px", "ms", "code"] {
                    if let Some(v) = req.get(k) {
                        act[k] = v.clone();
                    }
                }
            }
            let mut h = hub.lock().await;
            match h.act(act, who).await {
                Ok(v) => v,
                Err(e) => json!({"ok": false, "why": e}),
            }
        }
        "frame" => {
            let png = req.get("type").and_then(|t| t.as_str()) == Some("png");
            let mut h = hub.lock().await;
            match h.frame(png).await {
                Ok(f) => json!({"ok": true, "b64": f.b64, "mime": f.mime, "bytes": f.bytes, "seq": f.seq, "pic": f.pic, "live": f.live}),
                Err(e) => json!({"ok": false, "why": e}),
            }
        }
        "diagnostics" => {
            // Read-only, and it costs the page nothing: the records were collected as they
            // happened. A borrowed process may ask, for the same reason it may ask `state` — and
            // for the same reason it may not start a stream or reshape the page, this neither
            // changes what the person sees nor moves the page.
            let since = req.get("since").and_then(|s| s.as_i64()).unwrap_or(0);
            let kind = req.get("kind").and_then(|k| k.as_str());
            let level = req.get("level").and_then(|l| l.as_str());
            let limit = req.get("limit").and_then(|l| l.as_u64()).unwrap_or(60) as usize;
            let h = hub.lock().await;
            h.peek().diagnostics_value(since, kind, level, limit)
        }
        "close" => {
            let why = req.get("why").and_then(|w| w.as_str()).unwrap_or("closed over the socket");
            let mut h = hub.lock().await;
            h.close(why).await;
            json!({"ok": true, "why": why})
        }
        other => json!({"ok": false, "why": format!("the kept browser does not answer a `{other}` command")}),
    }
}

pub fn state_value(s: &Snapshot) -> Value {
    json!({
        "ok": true,
        "alive": s.alive,
        "opened": s.opened,
        // The page counter and the newest record id. Neither is sent as records — a tool process
        // that wants them asks for them, and a `state` reply that carried 240 of them would be a
        // reply nobody reads. They are here so a mirror can say "there is more than you have seen".
        "nav": s.nav,
        "diag_seq": s.diag_seq,
        "diagnostics_held": s.diagnostics.len(),
        "url": s.url,
        "title": s.title,
        "opened_by": format!("{:?}", s.opened_by).to_lowercase(),
        "last_actor": format!("{:?}", s.last_actor).to_lowercase(),
        "last_act": s.last_act,
        "why": s.why,
        "idle_secs": s.idle_secs,
        "idle_left": s.idle_left_now(),
        "idle_deadline": s.idle_deadline,
        "console": s.console.iter().map(|l| json!({"kind": l.kind, "text": l.text})).collect::<Vec<_>>(),
        "page_errors": s.page_errors,
        "requests_failed": s.requests_failed,
        "downloads_blocked": s.downloads_blocked,
        "frames_taken": s.frames_taken,
        // Whether a person is watching the page this call is about, and whether the view is live.
        // A tool process on the pi core carries the same mirror as the app, and the sentence it
        // tells the model ("this page is on somebody's screen") reads these, not a guess.
        "live": s.live,
        "live_why": s.live_why,
        // Whose window the page is in. A borrowed process reads this to know the app's browser is
        // a page on someone's screen, not a stream it could ever attach to.
        "native": s.native,
        // Where the page widget is in somebody's window. A tool process reads this to know the page
        // it is driving occupies a rectangle on a screen; it is not information a process can invent.
        "want": { "x": s.want_x, "y": s.want_y, "width": s.want_w, "height": s.want_h },
        "placed": {
            "x": s.placed_x,
            "y": s.placed_y,
            "width": s.placed_w,
            "height": s.placed_h,
            "shown": s.placed_shown,
        },
        "frame_pic": s.frame_pic,
        "seq": s.seq,
        "says": s.says(),
    })
}

/// One blocking call, for a process that is not on an async runtime: `cwtool check` asking
/// whether a browser is open before it builds a risk answer.
pub fn ask_blocking(path: &Path, req: &Value, timeout: Duration) -> Result<Value, String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let mut sock = UnixStream::connect(path).map_err(|e| format!("the app's kept browser did not answer at {}: {e}", path.display()))?;
    sock.write_all(format!("{}\n", serde_json::to_string(req).unwrap_or_default()).as_bytes())
        .map_err(|e| format!("the command to the kept browser never got there: {e}"))?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| format!("could not set a timeout on the kept-browser socket: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = sock.read(&mut chunk).map_err(|e| format!("the kept browser did not answer: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.iter().any(|b| *b == b'\n') {
            break;
        }
        if buf.len() > 32 * 1024 * 1024 {
            return Err("the kept browser's answer was too large to read".to_string());
        }
    }
    let line = String::from_utf8_lossy(&buf).trim().to_string();
    if line.is_empty() {
        return Err("the kept browser answered nothing".to_string());
    }
    serde_json::from_str(&line).map_err(|e| format!("the kept browser's answer was not JSON: {e}"))
}

/// A kept browser's state, read without a hub in this process. Kept beside the socket so the
/// mirror a tool process carries is built by the same code that reads the answer.
pub fn snapshot_from(v: &Value) -> Snapshot {
    Snapshot {
        diagnostics: Vec::new(),
        diag_seq: v.get("diag_seq").and_then(|s| s.as_u64()).unwrap_or(0),
        nav: v.get("nav").and_then(|s| s.as_u64()).unwrap_or(0),
        alive: v.get("alive").and_then(|a| a.as_bool()).unwrap_or(false),
        opened: v.get("opened").and_then(|a| a.as_bool()).unwrap_or(false),
        url: v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
        title: v.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        opened_by: match v.get("opened_by").and_then(|w| w.as_str()).unwrap_or("") {
            "person" => Who::Person,
            "model" => Who::Model,
            _ => Who::Nobody,
        },
        last_actor: match v.get("last_actor").and_then(|w| w.as_str()).unwrap_or("") {
            "person" => Who::Person,
            "model" => Who::Model,
            _ => Who::Nobody,
        },
        last_act: v.get("last_act").and_then(|a| a.as_str()).unwrap_or("").to_string(),
        why: v.get("why").and_then(|w| w.as_str()).unwrap_or("").to_string(),
        idle_secs: v.get("idle_secs").and_then(|s| s.as_u64()).unwrap_or(0),
        idle_left: v.get("idle_left").and_then(|s| s.as_i64()),
        idle_deadline: v.get("idle_deadline").and_then(|s| s.as_i64()),
        console: v
            .get("console")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .map(|l| super::LogLine {
                        kind: l.get("kind").and_then(|k| k.as_str()).unwrap_or("log").to_string(),
                        text: l.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        page_errors: v
            .get("page_errors")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().map(|e| e.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default(),
        requests_failed: v
            .get("requests_failed")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().map(|e| e.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default(),
        downloads_blocked: v
            .get("downloads_blocked")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().map(|e| e.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default(),
        frames_taken: v.get("frames_taken").and_then(|f| f.as_u64()).unwrap_or(0),
        live: v.get("live").and_then(|l| l.as_bool()).unwrap_or(false),
        live_why: v.get("live_why").and_then(|w| w.as_str()).unwrap_or("").to_string(),
        native: v.get("native").and_then(|n| n.as_bool()).unwrap_or(false),
        frame_pic: v.get("frame_pic").and_then(|p| p.as_u64()).unwrap_or(0),
        want_x: v.get("want").and_then(|p| p.get("x")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        want_y: v.get("want").and_then(|p| p.get("y")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        want_w: v.get("want").and_then(|p| p.get("width")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        want_h: v.get("want").and_then(|p| p.get("height")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        placed_x: v.get("placed").and_then(|p| p.get("x")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        placed_y: v.get("placed").and_then(|p| p.get("y")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        placed_w: v.get("placed").and_then(|p| p.get("width")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        placed_h: v.get("placed").and_then(|p| p.get("height")).and_then(|w| w.as_u64()).unwrap_or(0) as u32,
        placed_shown: v.get("placed").and_then(|p| p.get("shown")).and_then(|w| w.as_bool()).unwrap_or(false),
        viewport_w: v
            .get("viewport")
            .and_then(|p| p.get("width"))
            .and_then(|w| w.as_u64())
            .unwrap_or(0) as u32,
        viewport_h: v
            .get("viewport")
            .and_then(|p| p.get("height"))
            .and_then(|h| h.as_u64())
            .unwrap_or(0) as u32,
        seq: v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0),
    }
}

/// The same call from an async tool: connect non-blocking through tokio's unix socket.
pub async fn ask(path: &Path, req: &Value, timeout: Duration) -> Result<Value, String> {
    let req = req.clone();
    let path = path.to_path_buf();
    // The blocking helper is the simpler of the two and this is a short-lived, bounded call
    // on a local socket; running it on a blocking task keeps the runtime free without writing
    // a second reader.
    let joined = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || ask_blocking(&path, &req, timeout)))
        .await
        .map_err(|_| format!("the kept browser did not answer within {}s", timeout.as_secs()))?;
    joined.map_err(|e| format!("the kept-browser call was dropped: {e}"))?
}
