//! LAN / remote access to a running cwoffee session host.
//!
//! This is a transport, not a second engine. It is handed the *same*
//! `SessionHost` the desktop window drives, so a phone shows the turn that is
//! actually running and an approval answered from a phone is the same approval
//! the desktop is waiting on. A separate gateway process with its own host would
//! have looked identical on the surface and been a different conversation.
//!
//! ## What is exposed
//!
//!   * static files: the built `ui/dist`, so the normal UI runs in a browser
//!   * `/ws`: JSON RPC plus a pushed event stream
//!
//! ## Security posture, and its limits
//!
//! A cwoffee session has a sandboxed shell and file writes in a workspace. A
//! network listener on that is not a read-only dashboard, so:
//!
//!   * the default bind is loopback. Binding anywhere else requires a token and
//!     refuses to start without one - there is no "oops, it was open" state.
//!   * tokens are compared without early exit.
//!   * a *watch* token can read; only a *drive* token can send a turn, cancel,
//!     change mode, or answer an approval. Reading and driving are separate
//!     capabilities because the watch token is the one you are likely to type on
//!     a phone in a coffee shop.
//!   * the token is never in a URL. A browser cannot set headers on a WebSocket,
//!     so the token goes in the first frame after connect, and nothing is
//!     streamed and no RPC is served before that frame is accepted. Query
//!     strings end up in proxy logs and browser history.
//!   * approvals still fail closed on the engine's own timeout. A phone that
//!     drops its connection cannot leave the agent waiting forever.
//!
//! What this does **not** do: it is plain HTTP. On a network you do not control,
//! the token and the whole transcript are visible to anyone on the path. Use
//! loopback plus a tunnel (Tailscale, SSH -L) rather than exposing this port.
//! That is a real limitation, stated rather than papered over.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot};
use cwoffee_engine::approval::ApprovalAnswer;
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{HostCmd, SessionHost};
use cwoffee_engine::store::Store;

/// Everything `serve` needs to reach the live engine.
pub struct EngineHandle {
    pub host: Arc<SessionHost>,
    pub store: Arc<Store>,
    /// Fan-out of the same events the desktop webview receives.
    pub events: broadcast::Sender<EngineEvent>,
    /// The same pending map the desktop's `respond_approval` uses, so an
    /// approval answered from either side resolves the one the engine awaits.
    pub pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>>,
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub host: IpAddr,
    pub port: u16,
    /// Absolute path to a built `ui/dist`.
    pub dist: PathBuf,
    /// Grants read *and* drive. This is the token you use yourself.
    pub drive_token: Option<String>,
    /// Grants read only. Optional; a deployment can have no watch token at all.
    pub watch_token: Option<String>,
}

impl GatewayConfig {
    pub fn is_loopback(&self) -> bool {
        self.host.is_loopback()
    }
}

#[derive(Debug)]
pub enum GatewayError {
    /// Refusing to start rather than opening an unauthenticated agent to a LAN.
    Refused(String),
    Other(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Refused(m) => write!(f, "refused: {m}"),
            GatewayError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Constant-time string comparison. `==` on strings returns at the first
/// differing byte, which turns a timing attack from theoretical into practical
/// against a token of any length.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // Lengths differ: still compare the full shorter side and fold the length
    // difference into the result, so the early return is not a timing signal.
    let n = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

fn role_for(token: &str, cfg: &GatewayConfig) -> Option<Role> {
    if let Some(t) = &cfg.drive_token {
        if ct_eq(token, t) {
            return Some(Role::Drive);
        }
    }
    if let Some(t) = &cfg.watch_token {
        if ct_eq(token, t) {
            return Some(Role::Watch);
        }
    }
    None
}

/// Why an auth attempt was refused, in words the client can show.
///
/// "no token" and "wrong token" need completely different fixes, and a phone on
/// Safari has no console to check which one happened. Collapsing both into
/// "gateway rejected the token" is what sent the first report off to edit a
/// config file on the wrong machine.
/// True if the first line of a (possibly partial) request targets `prefix`.
///
/// The upgrade header may not have arrived yet when we have to decide, but the
/// request line always has: it is the first thing any client sends.
fn request_line_targets(head: &str, prefix: &str) -> bool {
    match head.split_whitespace().nth(1) {
        Some(path) => path.starts_with(prefix),
        None => false,
    }
}

fn auth_refusal_reason(token: &str) -> &'static str {
    if token.is_empty() {
        "no token supplied: open the URL with ?token=<CWOFFEE_GATEWAY_TOKEN>"
    } else {
        "token does not match CWOFFEE_GATEWAY_TOKEN"
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Watch,
    Drive,
}

impl Role {
    fn can_drive(self) -> bool {
        matches!(self, Role::Drive)
    }
}

/// Wire format. One struct with optional fields keeps the browser side simple:
/// there is no framing to get wrong, just JSON objects with a `type`.
#[derive(Debug, Deserialize)]
struct RpcReq {
    /// Client-generated id, echoed on the response so the browser can match.
    id: u64,
    cmd: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcRes {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn serve(cfg: GatewayConfig, handle: Arc<EngineHandle>) -> Result<(), GatewayError> {
    // The whole security story rests on this check. An agent with a sandboxed
    // shell must not be reachable by default from anything but this machine.
    if !cfg.is_loopback() && cfg.drive_token.is_none() {
        return Err(GatewayError::Refused(format!(
            "binding {} requires a drive token; refusing to expose a tool-using \
             agent to the network unauthenticated",
            cfg.host
        )));
    }
    if !cfg.dist.is_dir() {
        return Err(GatewayError::Other(format!(
            "no built frontend at {}; run `npm --prefix ui run build`",
            cfg.dist.display()
        )));
    }

    let addr = SocketAddr::new(cfg.host, cfg.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| GatewayError::Other(format!("cannot bind {addr}: {e}")))?;

    eprintln!(
        "[cwoffee] gateway: http://{}:{} ({})",
        cfg.host,
        cfg.port,
        if cfg.is_loopback() {
            "loopback only"
        } else {
            "NETWORK-REACHABLE; plain HTTP, no TLS - use a tunnel"
        }
    );

    let cfg = Arc::new(cfg);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[cwoffee] gateway accept failed: {e}");
                continue;
            }
        };
        // Nagle is actively harmful here. The protocol is a sequence of small
        // messages - hello, auth_ok, ready, then one reply per RPC - and with
        // Nagle plus delayed ACK each back-to-back small write waits for the
        // previous acknowledgement: a measured ~40ms between auth_ok and ready on
        // a link with sub-millisecond RTT. The boot does several such round trips,
        // so on a phone over a VPN the cost compounds into seconds of "connecting".
        let _ = stream.set_nodelay(true);
        let cfg = Arc::clone(&cfg);
        let handle = Arc::clone(&handle);
        tokio::spawn(async move {
            if let Err(e) = connection(stream, peer, cfg, handle).await {
                // Per-connection failures are routine: a browser tab closing
                // mid-response, a scanner probing the port. Loud enough to
                // diagnose, quiet enough not to bury real errors.
                eprintln!("[cwoffee] gateway {peer}: {e}");
            }
        });
    }
}

/// Headers are read before deciding anything, so a request that is not a
/// WebSocket upgrade can be answered as plain HTTP.
const MAX_HEAD: usize = 16 * 1024;
/// A keep-alive connection that goes quiet must not pin a task indefinitely.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

async fn connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    cfg: Arc<GatewayConfig>,
    handle: Arc<EngineHandle>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Decide from a *peek*, not a read. The first version read the header block
    // and then handed the drained stream to accept_async, which sat waiting for a
    // handshake that had already been consumed: every WebSocket connection hung
    // open with no response. peek() leaves the bytes in the socket, so the
    // websocket layer can read its own request.
    //
    // One peek is not enough. A phone on a cellular link, or anything behind a
    // proxy, can deliver the handshake in pieces, and a first peek holding only
    // "GET /ws HTTP/1.1\r\n" would be classified as plain HTTP and answered with
    // a 404 - which the client sees as a socket that closes, and then retries
    // forever. Peek until the header block terminates, and fall back to the
    // request line: a GET for /ws is an upgrade whatever else arrived.
    let mut peeked = vec![0u8; 2048];
    let mut head_peek = String::new();
    let mut head_complete = false;
    // Wait for the header block to actually terminate. The first version gave up
    // after four peeks totalling ~240ms of waiting; on a split first segment that
    // left only "GET /ws HTTP/1.1\r\n" visible, the connection was routed to the
    // websocket layer anyway, and accept_async answered
    // `Junk after client request` and closed. A browser sends the handshake in one
    // segment; a phone on a cellular link does not always.
    for _ in 0..20 {
        let n = stream.peek(&mut peeked).await?;
        if n > 0 {
            head_peek = String::from_utf8_lossy(&peeked[..n]).to_ascii_lowercase();
            if head_peek.contains("\r\n\r\n") {
                head_complete = true;
                break;
            }
            if head_peek.contains("upgrade: websocket") {
                head_complete = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !head_complete && !head_peek.is_empty() {
        eprintln!(
            "[cwoffee/gateway] {peer} sent an incomplete request head ({} byte(s) in 2s)",
            head_peek.len()
        );
    }
    let wants_ws = head_peek.contains("upgrade: websocket") || request_line_targets(&head_peek, "/ws");
    // Staged and timestamped. The first phone report ("gateway did not respond
    // within 15s") could not be localised at all: the only logs were auth results,
    // so a connection that died before auth left no trace, and a connection that
    // never arrived looked identical to one that stalled mid-handshake.
    eprintln!(
        "[cwoffee/gateway] {peer} tcp accepted; peek saw {} byte(s); routing to {}",
        head_peek.len(),
        if wants_ws { "websocket" } else { "http" }
    );
    if wants_ws {
        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("[cwoffee/gateway] {peer} websocket handshake failed: {e}");
                return Ok(());
            }
        };
        eprintln!("[cwoffee/gateway] {peer} websocket handshake complete");
        return websocket(ws, cfg, handle, peer.to_string()).await;
    }

    // Plain HTTP: now actually consume the request head.
    // Served with keep-alive. `Connection: close` forced a fresh TCP connection
    // per asset, and on a high-RTT link each one costs a round trip before a
    // single byte of the app arrives: four assets, four connections.
    let mut tmp = [0u8; 4096];
    // Bytes past the end of one request head belong to the next request. Dropping
    // them (the first version truncated) silently swallowed pipelined requests and
    // left the client waiting for a response that would never be written.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head_end = loop {
            if let Some(pos) = find_double_crlf(&buf) {
                break pos + 4;
            }
            // An unbounded header read is a memory DoS on a reachable port, and a
            // keep-alive connection that goes quiet must not pin a task forever.
            let n = match tokio::time::timeout(READ_TIMEOUT, stream.read(&mut tmp)).await {
                Ok(Ok(n)) => n,
                Ok(Err(_)) | Err(_) => return Ok(()),
            };
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > MAX_HEAD {
                let _ = respond(&mut stream, 431, "text/plain", b"headers too large", false, "no-store").await;
                return Ok(());
            }
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        buf.drain(..head_end);
        let (method, path, headers) = match parse_head(&head) {
            Some(v) => v,
            None => return Ok(()),
        };
        // HTTP/1.1 keeps the connection open unless the client says otherwise.
        let keep_alive = headers
            .get("connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);

        if method != "GET" && method != "HEAD" {
            // The RPC surface is the socket, not a POST endpoint: keeping static
            // serving read-only means a bug in the file server cannot become a way
            // to drive the agent.
            respond(&mut stream, 405, "text/plain", b"method not allowed", keep_alive, "no-store").await?;
            if !keep_alive {
                return Ok(());
            }
            continue;
        }

        match serve_static(&cfg.dist, &path) {
            Some((status, ctype, body)) => {
                respond(&mut stream, status, &ctype, &body, keep_alive, cache_for(&path)).await?
            }
            None => {
                eprintln!("[cwoffee] gateway {peer}: no route for {path}");
                respond(&mut stream, 404, "text/plain", b"not found", keep_alive, "no-store").await?
            }
        }
        if !keep_alive {
            return Ok(());
        }
    }
}

/// Cache policy per resource.
///
/// `no-store` everywhere was costing every real visit: the JS/CSS filenames are
/// content-hashed, so they can never change under the same name, yet each page
/// load re-fetched all of them. On the measured path to a phone (direct on the
/// LAN, but ~200-400ms and lossy because iOS parks the Wi-Fi radio) that is
/// several round trips of pure waste.
fn cache_for(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        // index.html must revalidate, or a rebuilt app is invisible.
        "no-cache"
    }
}

fn find_double_crlf(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_head(head: &str) -> Option<(String, String, HashMap<String, String>)> {
    let mut lines = head.split("\r\n");
    let request = lines.next()?;
    let mut parts = request.splitn(3, ' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Some((method, path, headers))
}

/// Minimal percent-decoding. Only used on the path component, and it deliberately
/// leaves invalid escapes as-is rather than failing, so a stray `%` in a filename
/// does not break asset loading.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Map a URL path to a file under `dist`, refusing anything that escapes it.
///
/// The SPA is a single page, so an unknown path that is not an asset returns
/// index.html - that is what makes a client-side route survive a reload.
fn serve_static(dist: &std::path::Path, url_path: &str) -> Option<(u16, String, Vec<u8>)> {
    let clean = url_path.split('?').next().unwrap_or("/");
    // Decode before confining. Confining first would let `%2e%2e` walk out after
    // the check, because the checked string and the opened string would differ.
    let rel = pct_decode(clean).trim_start_matches('/').to_string();

    let candidate = if rel.is_empty() {
        dist.join("index.html")
    } else {
        // Reject any traversal before touching the filesystem. `Path::join`
        // happily accepts `..`; confine() is what makes it safe.
        match cwoffee_engine::tools::path::confine(dist, rel.as_str()) {
            Ok(p) => p,
            Err(_) => return Some((400, "text/plain".into(), b"bad path".to_vec())),
        }
    };

    if candidate.is_dir() || !candidate.exists() {
        let index = dist.join("index.html");
        let body = std::fs::read(&index).ok()?;
        return Some((200, "text/html; charset=utf-8".into(), body));
    }

    let ext = candidate.extension().and_then(|e| e.to_str()).unwrap_or("");
    let ctype = match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
    .to_string();
    let body = std::fs::read(&candidate).ok()?;
    Some((200, ctype, body))
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    keep_alive: bool,
    cache: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nConnection: {}\r\n\r\n",
        body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// tungstenite 0.26 stores text frames as `Utf8Bytes`, not `String`, so every
/// construction needs the conversion. One helper rather than `.into()` scattered
/// across the file.
fn txt(s: String) -> Message {
    Message::Text(s.into())
}

/// How long a socket may sit unauthenticated. Anything that connects and says
/// nothing gets dropped rather than holding a slot and an event subscription.
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn websocket(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    cfg: Arc<GatewayConfig>,
    handle: Arc<EngineHandle>,
    peer: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut sink, mut stream) = ws.split();

    // Nothing is read or streamed until a role is established.
    let role = match auth_handshake(&mut sink, &mut stream, &cfg, &peer).await {
        Some(r) => r,
        None => return Ok(()),
    };

    let mut rx = handle.events.subscribe();
    // Replay nothing: a late joiner gets the live tail and then loads stored
    // history over RPC. Replaying a turn's events to a second client would mean
    // buffering them, and the transcript is already the durable record.
    let _ = sink
        .send(txt(serde_json::json!({ "type": "ready", "role": if role.can_drive() { "drive" } else { "watch" } }).to_string()))
        .await;

    let mut ping = tokio::time::interval(std::time::Duration::from_secs(25));
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Ok(e) => {
                        let payload = serde_json::to_string(&serde_json::json!({ "type": "event", "d": e })).unwrap_or_default();
                        if sink.send(txt(payload)).await.is_err() { return Ok(()); }
                    }
                    // The engine's event channel closed; nothing left to stream.
                    Err(_) => return Ok(()),
                }
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    // Closed or a protocol error: the browser will reconnect.
                    Some(Err(_)) | None => return Ok(()),
                };
                match msg {
                    Message::Text(t) => {
                        let reply = dispatch(t.as_str(), role, &handle).await;
                        if let Some(payload) = reply {
                            if sink.send(txt(payload)).await.is_err() { return Ok(()); }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(p) => {
                        if sink.send(Message::Pong(p)).await.is_err() { return Ok(()); }
                    }
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() { return Ok(()); }
            }
        }
    }
}

async fn auth_handshake(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>,
    stream: &mut futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<TcpStream>>,
    cfg: &GatewayConfig,
    peer: &str,
) -> Option<Role> {
    let _ = sink
        .send(txt(serde_json::json!({ "type": "hello", "need_auth": cfg.drive_token.is_some() || cfg.watch_token.is_some() }).to_string()))
        .await;

    let first = match tokio::time::timeout(AUTH_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(m))) => m,
        Ok(Some(Err(e))) => {
            eprintln!("[cwoffee/gateway] {peer} frame read failed: {e}");
            return None;
        }
        Ok(None) => {
            eprintln!("[cwoffee/gateway] {peer} closed before sending an auth frame");
            return None;
        }
        Err(_) => {
            eprintln!(
                "[cwoffee/gateway] {peer} sent no auth frame within {}s",
                AUTH_TIMEOUT.as_secs()
            );
            return None;
        }
    };
    let first_txt = match first {
        Message::Text(t) => t,
        _ => return None,
    };
    let v: serde_json::Value = match serde_json::from_str(first_txt.as_str()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[cwoffee/gateway] {peer} first frame was not JSON: {e}");
            return None;
        }
    };
    eprintln!("[cwoffee/gateway] {peer} first frame type={:?}", v.get("type").and_then(|t| t.as_str()));
    if v.get("type").and_then(|t| t.as_str()) != Some("auth") {
        let _ = sink
            .send(txt(serde_json::json!({ "type": "error", "error": "expected an auth frame first" }).to_string()))
            .await;
        return None;
    }
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
    // No tokens configured (loopback-only dev): the connection is accepted as
    // drive, because the bind address is already the boundary.
    if cfg.drive_token.is_none() && cfg.watch_token.is_none() {
        return Some(Role::Drive);
    }
    match role_for(token, cfg) {
        Some(r) => {
            eprintln!("[cwoffee/gateway] {peer} joined as {}", if r.can_drive() { "drive" } else { "watch" });
            let _ = sink
                .send(txt(serde_json::json!({ "type": "auth_ok", "role": if r.can_drive() { "drive" } else { "watch" } }).to_string()))
                .await;
            Some(r)
        }
        None => {
            // The first phone attempt failed and the server said nothing at all,
            // which turned a mistyped token into an hours-worth of "the gateway
            // is broken". Distinguish "no token arrived" from "wrong token": the
            // fixes are completely different, and the client can only show a
            // useful message if the server says which one happened.
            // The token itself is never logged.
            let reason = auth_refusal_reason(token);
            eprintln!(
                "[cwoffee/gateway] {peer} auth refused ({}, {} chars sent)",
                if token.is_empty() { "no token" } else { "wrong token" },
                token.chars().count()
            );
            let _ = sink
                .send(txt(serde_json::json!({ "type": "auth_fail", "reason": reason }).to_string()))
                .await;
            None
        }
    }
}

/// Run one RPC. Returns the serialized response, or None for anything we
/// deliberately do not answer.
async fn dispatch(raw: &str, role: Role, handle: &EngineHandle) -> Option<String> {
    let req: RpcReq = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return Some(
                serde_json::json!({ "type": "rpc_error", "error": format!("malformed frame: {e}") })
                    .to_string(),
            )
        }
    };
    let id = req.id;
    // Tauri passes command arguments as a named map, so `invoke('x', { report })`
    // arrives as `{ report: ... }`. Logging the command and its keys is what
    // turned a "the browser shows nothing" report into a findable shape mismatch.
    if std::env::var("CWOFFEE_GATEWAY_TRACE").is_ok() {
        eprintln!(
            "[cwoffee/gateway] rpc {:#?}",
            req.cmd
        );
    }
    let out = run_cmd(&req.cmd, req.args, role, handle).await;
    Some(
        serde_json::to_string(&RpcRes {
            id,
            ok: out.is_ok(),
            data: out.clone().ok(),
            error: out.err().map(|e| e.to_string()),
        })
        .unwrap_or_default(),
    )
}

type CmdResult = Result<serde_json::Value, String>;

fn need_drive(role: Role) -> CmdResult {
    if role.can_drive() {
        Ok(serde_json::Value::Null)
    } else {
        Err("this token can watch, not drive".into())
    }
}

fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

fn arg_i64(args: &serde_json::Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("missing integer argument `{key}`"))
}

async fn run_cmd(cmd: &str, args: serde_json::Value, role: Role, h: &EngineHandle) -> CmdResult {
    use serde_json::json;
    match cmd {
        // ---- read-only: available to a watch token -------------------------
        "session_state" => serde_json::to_value(h.host.state()).map_err(|e| e.to_string()),
        "list_sessions" => {
            serde_json::to_value(h.store.list_sessions(50).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        }
        "search_messages" => serde_json::to_value(
            h.store
                .search(&arg_str(&args, "query")?, 25)
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string()),
        "session_messages" => {
            let id = arg_i64(&args, "id")?;
            let before = args.get("before").and_then(|v| v.as_i64());
            let page = h
                .store
                .messages_page(id, before, 60)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(page).map_err(|e| e.to_string())
        }
        "migration_notes" => Ok(json!([] as [String; 0])),

        // Everything a cold client needs, in one round trip.
        //
        // Boot used to be four RPCs. Server-side each one costs under 2ms, but a
        // phone on the measured path pays 200-400ms per round trip, so the number
        // of calls - not the work - is what the user experiences as "slow".
        "bootstrap" => {
            let state = serde_json::to_value(h.host.state()).map_err(|e| e.to_string())?;
            let sessions =
                serde_json::to_value(h.store.list_sessions(50).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            // A model list that fails must not blank the app; the client shows the
            // error in the model menu instead.
            let models = match h.host.provider().list_models_info().await {
                Ok(m) => serde_json::to_value(m).unwrap_or_else(|_| json!([])),
                Err(_) => json!([]),
            };
            // Same rule the client applied when it did this in four calls: the
            // host's open session if it has one, otherwise the most recent real
            // conversation. list_sessions is newest-first, so `find` is that rule.
            let open = state.get("session_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let view = if open > 0 {
                Some(open)
            } else {
                sessions.as_array().and_then(|a| {
                    a.iter()
                        .find(|s| s.get("message_count").and_then(|m| m.as_i64()).unwrap_or(0) > 0)
                        .and_then(|s| s.get("id").and_then(|v| v.as_i64()))
                })
            };
            let page = match view {
                Some(id) => serde_json::to_value(
                    h.store.messages_page(id, None, 60).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?,
                None => json!({ "messages": [], "has_older": false }),
            };
            Ok(json!({
                "state": state,
                "sessions": sessions,
                "models": models,
                "session_id": view,
                "page": page,
            }))
        }

        // ---- drive ---------------------------------------------------------
        "start_turn" => {
            need_drive(role)?;
            let text = arg_str(&args, "text")?;
            // Attachments are deliberately not accepted over the gateway in this
            // slice: an image is a large base64 blob on an unauthenticated-by-
            // default channel, and the phone case is watching and answering, not
            // composing with images.
            h.host.submit(text);
            Ok(json!(true))
        }
        "cancel_turn" => {
            need_drive(role)?;
            h.host.cancel();
            Ok(json!(true))
        }
        "set_mode" => {
            need_drive(role)?;
            let m = match arg_str(&args, "mode")?.as_str() {
                "ask" => cwoffee_engine::ApprovalMode::Ask,
                "session" => cwoffee_engine::ApprovalMode::Session,
                "auto" => cwoffee_engine::ApprovalMode::Auto,
                other => return Err(format!("unknown mode {other:?}")),
            };
            h.host.send(HostCmd::SetMode { mode: m });
            Ok(json!(true))
        }
        "set_reasoning" => {
            need_drive(role)?;
            let r = match arg_str(&args, "level")?.as_str() {
                "off" => cwoffee_engine::Reasoning::Off,
                "low" => cwoffee_engine::Reasoning::Low,
                "medium" => cwoffee_engine::Reasoning::Medium,
                "high" => cwoffee_engine::Reasoning::High,
                "xhigh" => cwoffee_engine::Reasoning::XHigh,
                other => return Err(format!("unknown reasoning level {other:?}")),
            };
            h.host.send(HostCmd::SetReasoning { reasoning: r });
            Ok(json!(true))
        }
        "respond_approval" => {
            need_drive(role)?;
            let id = arg_i64(&args, "id")? as u64;
            let answer = match arg_str(&args, "answer")?.as_str() {
                "allow" => ApprovalAnswer::Allow,
                "allow_session" => ApprovalAnswer::AllowSession,
                "deny" => ApprovalAnswer::Deny,
                other => return Err(format!("unknown answer {other:?}")),
            };
            let tx = h.pending.lock().unwrap().remove(&id);
            match tx {
                Some(tx) => tx
                    .send(answer)
                    .map(|_| json!(true))
                    .map_err(|_| "the engine no longer wants this answer".to_string()),
                None => Err(format!("no pending approval with id {id}")),
            }
        }
        "new_session" => {
            need_drive(role)?;
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let id = h
                .store
                .create_session(title, &h.host.state().workspace, &h.host.state().model)
                .map_err(|e| e.to_string())?;
            h.host.send(HostCmd::SwitchSession { id });
            Ok(json!(id))
        }
        "switch_session" => {
            need_drive(role)?;
            let id = arg_i64(&args, "id")?;
            h.host.send(HostCmd::SwitchSession { id });
            Ok(json!(true))
        }
        "clear_session" => {
            need_drive(role)?;
            h.host.send(HostCmd::ClearSession);
            Ok(json!(true))
        }
        "rename_session" => {
            need_drive(role)?;
            h.store
                .rename_session(arg_i64(&args, "id")?, &arg_str(&args, "title")?)
                .map_err(|e| e.to_string())?;
            Ok(json!(true))
        }
        "delete_session" => {
            need_drive(role)?;
            let id = arg_i64(&args, "id")?;
            let active = h.host.state().session_id;
            if active == id && h.host.state().busy {
                return Err("cannot delete the session while its turn is running".into());
            }
            h.store.delete_session(id).map_err(|e| e.to_string())?;
            if active == id {
                h.host.send(HostCmd::ClearSession);
            }
            Ok(json!(true))
        }
        "set_model" => {
            need_drive(role)?;
            h.host.send(HostCmd::SetModel { model: arg_str(&args, "model")? });
            Ok(json!(true))
        }
        "list_models" => serde_json::to_value(
            h.host
                .provider()
                .list_models_info()
                .await
                .map_err(|e| format!("{e}"))?,
        )
        .map_err(|e| e.to_string()),

        // Diagnostics the UI calls on every boot. Answering them is what keeps a
        // browser session's health badge honest instead of permanently red: the
        // first gateway build failed `render_report`, and the UI reasonably read
        // that as "the transport is broken".
        "render_report" => {
            // Tauri passes named arguments as a map, so the payload lives under
            // `report`, not at the top level.
            let r = args.get("report").cloned().unwrap_or(args.clone());
            eprintln!(
                "[cwoffee/gateway] render report titlebar={:?} footer={:?} empty={:?} text={:?}",
                r.get("titlebar").and_then(|v| v.as_bool()),
                r.get("footer").and_then(|v| v.as_bool()),
                r.get("empty_mode").and_then(|v| v.as_bool()),
                r.get("text_len").and_then(|v| v.as_i64())
            );
            Ok(json!(true))
        }
        "boot_log" => {
            eprintln!("[cwoffee/gateway/page] {}", arg_str(&args, "msg")?);
            Ok(json!(true))
        }
        // Honest failure, not a plausible-looking stub. Returning `{note: ...}`
        // here satisfied the call but the UI dereferenced `render_nodes` on it and
        // threw during render, which blanked the whole app: a stub that is shaped
        // almost right is worse than an error.
        "gpu_report" => Err("gpu reporting is desktop-only".into()),
        "open_file" => Err("opening files is desktop-only; the gateway cannot touch your filesystem".into()),
        // Moving the workspace. This is a deliberate widening, and it is worth
        // being blunt about why: the tools are confined to the workspace, so a
        // drive token can aim them at any directory this user can write to. It is
        // gated on the drive token, the engine validates that the path exists and
        // is writable, and the reply names the reason when it refuses. The token
        // is still the whole boundary, and the channel is plain HTTP.
        "set_workspace" => {
            need_drive(role)?;
            let path = arg_str(&args, "path")?;
            let applied = h.host.set_workspace_checked(&path).await?;
            Ok(serde_json::json!(applied))
        }
        // A phone has no folder picker for the host's filesystem, so the client
        // types a path. Listing the host's directories is deliberately not
        // offered: that would turn a leaked token into a file browser.
        "pick_workspace" => {
            let _ = role;
            Err("the folder picker is desktop-only; type the host path instead".into())
        }
        other => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_accepts_equal_and_rejects_different() {
        assert!(ct_eq("hunter2", "hunter2"));
        assert!(!ct_eq("hunter2", "hunter3"));
        assert!(!ct_eq("hunter2", "hunter22"));
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }

    /// A watch token must never satisfy a drive check. This is the whole
    /// separation between "see what the agent is doing" and "make it do things".
    #[test]
    fn watch_and_drive_tokens_grant_different_roles() {
        let cfg = GatewayConfig {
            host: "127.0.0.1".parse().unwrap(),
            port: 8099,
            dist: PathBuf::from("."),
            drive_token: Some("drive-me".into()),
            watch_token: Some("watch-me".into()),
        };
        assert_eq!(role_for("drive-me", &cfg), Some(Role::Drive));
        assert_eq!(role_for("watch-me", &cfg), Some(Role::Watch));
        assert_eq!(role_for("nope", &cfg), None);
        assert!(!Role::Watch.can_drive());
        assert!(Role::Drive.can_drive());
    }

    /// The two tokens must not be interchangeable by prefix either: a shorter
    /// watch token that is a prefix of the drive token must not match.
    #[test]
    fn a_partial_handshake_is_still_recognised_as_a_websocket_upgrade() {
        // The upgrade header is the obvious signal, but on a split first segment
        // only the request line has arrived. Both have to route to the websocket
        // layer, or a phone gets a 404 and retries forever.
        assert!(request_line_targets("get /ws http/1.1\r\n", "/ws"));
        assert!(request_line_targets("get /ws?x=1 http/1.1\r\nhost: x\r\n", "/ws"));
        assert!(!request_line_targets("get /assets/app.js http/1.1\r\n", "/ws"));
        assert!(!request_line_targets("get", "/ws"));
        assert!(!request_line_targets("", "/ws"));
    }

    #[test]
    fn an_absent_token_is_reported_differently_from_a_wrong_one() {
        // Same symptom in the UI, opposite causes.
        assert!(auth_refusal_reason("").contains("no token supplied"));
        assert!(auth_refusal_reason("typo").contains("does not match"));
        assert!(!auth_refusal_reason("typo").contains("no token supplied"));
    }

    #[test]
    fn a_prefix_of_the_drive_token_is_not_accepted() {
        let cfg = GatewayConfig {
            host: "127.0.0.1".parse().unwrap(),
            port: 8099,
            dist: PathBuf::from("."),
            drive_token: Some("superlongdrivetoken".into()),
            watch_token: None,
        };
        assert_eq!(role_for("superlong", &cfg), None);
    }

    /// Refusing to start is the security boundary for a non-loopback bind. If
    /// this ever regresses, the default deployment is an open agent.
    #[tokio::test]
    async fn binding_a_non_loopback_address_without_a_token_is_refused() {
        let cfg = GatewayConfig {
            host: "0.0.0.0".parse().unwrap(),
            port: 8099,
            dist: PathBuf::from("."),
            drive_token: None,
            watch_token: None,
        };
        let handle = Arc::new(EngineHandle {
            host: {
                // Never used: serve() must fail before it touches the engine.
                let p: Arc<dyn cwoffee_engine::provider::Provider> = Arc::new(FakeProvider);
                let store = Arc::new(Store::in_memory().unwrap());
                let h = SessionHost::spawn(
                    p,
                    cwoffee_engine::tools::Registry::new(),
                    Arc::clone(&store),
                    cwoffee_engine::host::HostConfig::new("m", std::env::temp_dir()),
                );
                Arc::new(h)
            },
            store: Arc::new(Store::in_memory().unwrap()),
            events: broadcast::channel(8).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
        });
        let err = serve(cfg, handle).await.unwrap_err();
        assert!(matches!(err, GatewayError::Refused(_)), "got {err:?}");
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl cwoffee_engine::provider::Provider for FakeProvider {
        fn id(&self) -> &str { "fake" }
        fn label(&self) -> &str { "fake" }
        async fn stream(
            &self,
            _req: &cwoffee_engine::provider::ChatRequest,
            _sink: &cwoffee_engine::provider::DeltaSink,
        ) -> Result<(), cwoffee_engine::provider::ProviderError> {
            Ok(())
        }
    }

    /// Path traversal against the static file server.
    #[test]
    fn static_serving_cannot_escape_the_dist_directory() {
        let dir = std::env::temp_dir().join(format!("cwoffee_dist_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), b"<html>ok</html>").unwrap();
        std::fs::write(dir.join("assets/app.js"), b"console.log(1)").unwrap();

        let (status, ctype, body) = serve_static(&dir, "/assets/app.js").unwrap();
        assert_eq!(status, 200);
        assert!(ctype.starts_with("text/javascript"));
        assert_eq!(body, b"console.log(1)");

        // Unknown route -> the SPA shell, so a client-side route survives reload.
        let (status, ctype, body) = serve_static(&dir, "/session/12").unwrap();
        assert_eq!(status, 200);
        assert!(ctype.starts_with("text/html"));
        assert_eq!(body, b"<html>ok</html>");

        // Traversal attempts are refused, not resolved.
        for attack in [
            "/../secret.txt",
            "/assets/../../etc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
        ] {
            let (status, _, _) = serve_static(&dir, attack).unwrap();
            assert_ne!(status, 200, "served content for {attack}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hashed assets are cacheable forever; the shell must revalidate.
    #[test]
    fn hashed_assets_cache_and_the_shell_does_not() {
        assert_eq!(
            cache_for("/assets/index-DhzC1Ss8.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_for("/"), "no-cache");
        assert_eq!(cache_for("/index.html"), "no-cache");
        // A rebuilt app ships new hashed names, so immutability is safe; but the
        // shell is the one file that must never be served stale.
        assert!(!cache_for("/").contains("immutable"));
    }

    /// Static serving must answer more than one request per connection.
    /// `Connection: close` meant a fresh TCP handshake per asset: on the measured
    /// phone path (~200-400ms, lossy) three assets cost more than the app logic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn static_serving_answers_several_requests_on_one_connection() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("cwoffee_dist_ka_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), b"<html>shell</html>").unwrap();
        std::fs::write(dir.join("assets/app-abc.js"), b"console.log(1)").unwrap();

        let cfg = Arc::new(GatewayConfig {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            dist: dir.clone(),
            drive_token: None,
            watch_token: None,
        });
        let handle = test_handle();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, peer) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let svc = tokio::spawn(connection(
            tokio::net::TcpStream::from_std(server).unwrap(),
            peer,
            cfg,
            handle,
        ));

        client
            .write_all(
                b"GET /assets/app-abc.js HTTP/1.1\r\nHost: x\r\n\r\n\
                   GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        // Bounded so a broken loop fails the test instead of hanging it.
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut all = String::new();
        client.read_to_string(&mut all).unwrap();
        // The second request said `Connection: close`, so the task must finish.
        tokio::time::timeout(std::time::Duration::from_secs(5), svc)
            .await
            .expect("connection task outlived a closing client")
            .expect("connection task panicked")
            .expect("connection returned an error");

        assert_eq!(
            all.matches("HTTP/1.1 200 OK").count(),
            2,
            "expected two responses on one connection, got: {all}"
        );
        assert!(all.contains("Connection: keep-alive"), "first reply must persist: {all}");
        assert!(all.contains("Connection: close"), "second reply must honour close: {all}");
        assert!(all.contains("Cache-Control: public, max-age=31536000, immutable"));
        assert!(all.contains("console.log(1)"));
        assert!(all.contains("<html>shell</html>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request head that never terminates must not sit on a task forever.
    #[tokio::test]
    async fn an_idle_connection_is_dropped_rather_than_pinned() {
        let dir = std::env::temp_dir().join(format!("cwoffee_dist_idle_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), b"<html>x</html>").unwrap();
        let cfg = Arc::new(GatewayConfig {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            dist: dir.clone(),
            drive_token: None,
            watch_token: None,
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, peer) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        // READ_TIMEOUT is 20s, far longer than this test: what matters is that a
        // half-open peer cannot keep the task alive indefinitely.
        assert!(READ_TIMEOUT.as_secs() >= 5);
        let svc = tokio::spawn(connection(
            tokio::net::TcpStream::from_std(server).unwrap(),
            peer,
            cfg,
            test_handle(),
        ));
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), svc)
            .await
            .expect("connection task outlived a dropped client")
            .expect("connection task panicked")
            .expect("connection returned an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Boot must be one call, and it must pick the session the client would have
    /// picked: the host's open session, else the newest one with messages.
    #[tokio::test]
    async fn bootstrap_returns_state_sessions_models_and_a_transcript_at_once() {
        let h = test_handle();
        let s1 = h.store.create_session("older chat", "/tmp", "m").unwrap();
        let s2 = h.store.create_session("newest chat", "/tmp", "m").unwrap();
        use cwoffee_engine::provider::Message;
        h.store.append_message(s2, &Message::user("hello there")).unwrap();
        h.store.append_message(s1, &Message::user("older")).unwrap();

        let out = run_cmd("bootstrap", serde_json::json!({}), Role::Watch, &h)
            .await
            .expect("bootstrap failed");
        assert!(out.get("state").is_some(), "no host state");
        assert_eq!(out["sessions"].as_array().unwrap().len(), 2);
        assert!(out["models"].is_array(), "models must be an array even when empty");
        // Neither session is open on the host, so the newest conversation wins.
        assert_eq!(out["session_id"].as_i64(), Some(s2 as i64));
        let page = out.get("page").unwrap();
        assert_eq!(page["messages"].as_array().unwrap().len(), 1);
        assert_eq!(page["messages"][0]["content"], "hello there");

        // An open host session outranks recency.
        run_cmd("switch_session", serde_json::json!({"id": s1}), Role::Drive, &h)
            .await
            .unwrap();
        for _ in 0..50 {
            if h.host.state().session_id as i64 == s1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let out2 = run_cmd("bootstrap", serde_json::json!({}), Role::Watch, &h)
            .await
            .unwrap();
        assert_eq!(
            out2["session_id"].as_i64(),
            out2["state"]["session_id"].as_i64(),
            "an open host session must win over recency"
        );
    }

    /// A watch token must be able to boot: read-only is the whole point.
    #[tokio::test]
    async fn bootstrap_is_available_to_a_watch_only_client() {
        let h = test_handle();
        assert!(run_cmd("bootstrap", serde_json::json!({}), Role::Watch, &h)
            .await
            .is_ok());
    }

    /// The workspace is the sandbox boundary, so moving it is a drive-only act.
    #[tokio::test]
    async fn a_watch_token_cannot_move_the_workspace() {
        let h = test_handle();
        let err = run_cmd(
            "set_workspace",
            serde_json::json!({ "path": "/tmp" }),
            Role::Watch,
            &h,
        )
        .await
        .expect_err("a watch token must not move the workspace");
        assert!(!err.is_empty(), "refusal must say something");
    }

    #[tokio::test]
    async fn a_drive_token_moves_the_workspace_and_a_bad_path_says_why() {
        let h = test_handle();
        let out = run_cmd(
            "set_workspace",
            serde_json::json!({ "path": "/tmp" }),
            Role::Drive,
            &h,
        )
        .await
        .expect("a real writable directory should be accepted");
        assert_eq!(out.as_str(), Some("/tmp"), "the reply is the applied path");

        let err = run_cmd(
            "set_workspace",
            serde_json::json!({ "path": "/nonexistent/definitely-not" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("a non-directory must be refused");
        assert!(err.contains("refused"), "unhelpful error: {err}");
    }

    fn test_handle() -> Arc<EngineHandle> {
        let p: Arc<dyn cwoffee_engine::provider::Provider> = Arc::new(FakeProvider);
        Arc::new(EngineHandle {
            host: Arc::new(SessionHost::spawn(
                p,
                cwoffee_engine::tools::Registry::new(),
                Arc::new(Store::in_memory().unwrap()),
                cwoffee_engine::host::HostConfig::new("m", std::env::temp_dir()),
            )),
            store: Arc::new(Store::in_memory().unwrap()),
            events: broadcast::channel(8).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[test]
    fn header_parsing_handles_a_normal_request() {
        let (method, path, headers) =
            parse_head("GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n").unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/ws");
        // Header names are lower-cased on lookup, so the browser's casing does
        // not decide whether the upgrade is recognised.
        assert_eq!(headers.get("upgrade").map(|s| s.as_str()), Some("websocket"));
        assert!(find_double_crlf(b"GET / HTTP/1.1\r\nA: b\r\n\r\nrest").is_some());
        assert!(find_double_crlf(b"GET / HTTP/1.1\r\nA: b\r\n").is_none());
    }
}
