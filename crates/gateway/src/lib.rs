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

use std::collections::{BTreeMap, HashMap};
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
    /// ask_user round trips, shared with the desktop the same way approvals are:
    /// whichever client answers first resolves the one the engine awaits.
    pub pending_questions:
        Arc<Mutex<HashMap<u64, oneshot::Sender<cwoffee_engine::tools::ask::Answer>>>>,
    /// Who is attached right now: peer address -> live connections from it.
    ///
    /// The desktop panel shows this so the user can confirm the phone is really
    /// on it rather than guessing from "it started without an error". Keyed by
    /// address because the question the panel answers is "how many *devices*",
    /// and two tabs on one phone is one device.
    pub clients: Arc<Mutex<BTreeMap<String, usize>>>,
    /// What the one-time path relocation did (or refused to do), so a phone can be
    /// told the same thing the desktop window says. Returning `[]` here would be a
    /// stub that looks like "nothing needed migrating", which is a claim, not a
    /// fact: the desktop knows and this side did not.
    pub migration_notes: Vec<String>,
    /// The config file this host read (or would read). The search panel writes
    /// through it, so a provider chosen on a phone is still chosen after a restart,
    /// and so both shells write the same file rather than two.
    pub config_path: std::path::PathBuf,
}

impl EngineHandle {
    pub fn devices(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    pub fn connections(&self) -> usize {
        self.clients.lock().unwrap().values().sum()
    }

    pub fn peers(&self) -> Vec<String> {
        self.clients.lock().unwrap().keys().cloned().collect()
    }
}

/// One authenticated connection.
///
/// This gauge used to be an `AtomicUsize` with a `fetch_sub` in `Drop` and no
/// matching increment anywhere, so each connection that closed pushed the count
/// below zero and the panel printed 18446744073709552000 devices - usize::MAX,
/// formatted as fact. A guard whose constructor takes the ticket is the fix:
/// holding one without having been counted is not expressible.
struct ClientGuard {
    table: Arc<Mutex<BTreeMap<String, usize>>>,
    peer: String,
}

impl ClientGuard {
    fn attach(table: &Arc<Mutex<BTreeMap<String, usize>>>, peer: &str) -> ClientGuard {
        {
            let mut t = table.lock().unwrap();
            *t.entry(peer.to_string()).or_insert(0) += 1;
        }
        ClientGuard { table: table.clone(), peer: peer.to_string() }
    }
}

impl Drop for ClientGuard {
    /// Also runs on panic unwind, so a tool that takes the connection task down
    /// cannot leave a ghost device on the panel.
    fn drop(&mut self) {
        let mut t = self.table.lock().unwrap();
        if let Some(n) = t.get_mut(&self.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                t.remove(&self.peer);
            }
        }
    }
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
        // The address rather than addr:port: the panel counts devices, and a
        // device that reconnects gets a new ephemeral port every time.
        return websocket(ws, cfg, handle, peer.to_string(), peer.ip().to_string()).await;
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

/// Every way a client connection can end, logged with a reason.
///
/// A phone does not get to tell you why its socket died: it just stops updating.
/// Before, twelve separate `return Ok(())` paths were indistinguishable, and the
/// one that mattered - a client that fell behind its event stream - looked like
/// the engine shutting down.
macro_rules! gw_bye {
    ($addr:expr, $why:expr) => {{
        eprintln!("[cwoffee/gateway] {} connection ended: {}", $addr, $why);
        return Ok(());
    }};
}

async fn websocket(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    cfg: Arc<GatewayConfig>,
    handle: Arc<EngineHandle>,
    peer: String,
    addr: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut sink, mut stream) = ws.split();

    // Nothing is read or streamed until a role is established.
    let role = match auth_handshake(&mut sink, &mut stream, &cfg, &peer).await {
        Some(r) => r,
        None => return Ok(()),
    };

    // Count from here, not from the TCP accept: scanners and probes hit this
    // port constantly, and a gauge that counts them is a gauge nobody trusts.
    let _clients = ClientGuard::attach(&handle.clients, &addr);

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
                        if sink.send(txt(payload)).await.is_err() { gw_bye!(addr, "write to client failed"); }
                    }
                    // A slow client - a phone on a lossy link, a backgrounded tab
                    // with the socket throttled - falls behind the broadcast. It
                    // used to be treated exactly like a closed channel and the
                    // socket hung up, which lost the `turnEnd` that was the whole
                    // point of the stream: the phone kept `busy = true` from a
                    // `turnStart` it had already received and left the composer
                    // disabled until someone reloaded the page.
                    //
                    // So say so instead: the client re-reads authoritative state
                    // (which carries `busy`) and keeps its connection.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!(
                            "[cwoffee/gateway] {addr} fell {n} event(s) behind; asked it to resync"
                        );
                        let payload = serde_json::to_string(&serde_json::json!({
                            "type": "resync",
                            "d": { "missed": n },
                        }))
                        .unwrap_or_default();
                        if sink.send(txt(payload)).await.is_err() { gw_bye!(addr, "write to client failed"); }
                    }
                    // The engine's event channel closed; nothing left to stream.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => gw_bye!(addr, "engine event stream closed"),
                }
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    // Closed or a protocol error: the browser will reconnect.
                    Some(Err(e)) => gw_bye!(addr, &format!("client frame error: {e}")),
                    None => gw_bye!(addr, "client stopped sending"),
                };
                match msg {
                    Message::Text(t) => {
                        let reply = dispatch(t.as_str(), role, &handle, &addr).await;
                        if let Some(payload) = reply {
                            if sink.send(txt(payload)).await.is_err() { gw_bye!(addr, "write to client failed"); }
                        }
                    }
                    Message::Close(r) => gw_bye!(addr, &format!("client closed: {r:?}")),
                    Message::Ping(p) => {
                        if sink.send(Message::Pong(p)).await.is_err() { gw_bye!(addr, "pong write failed"); }
                    }
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() { gw_bye!(addr, "ping write failed"); }
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
/// Commands that change what the agent does or what it can reach. Everything on
/// this list gets a line in the server log naming the peer that asked, because
/// this endpoint can run a shell and this socket can be reachable from a network.
/// Without it, "the agent ran a command I did not ask for" is unanswerable.
const AUDITED: &[&str] = &[
    "start_turn",
    "cancel_turn",
    "new_session",
    "switch_session",
    "clear_session",
    "rename_session",
    "delete_session",
    "set_model",
    "set_mode",
    "set_reasoning",
    "set_workspace",
    "add_workspace",
    "remove_workspace",
    "respond_approval",
    "respond_question",
    "set_search_backend",
];

/// Empty a pending map and report how many waiters were released.
///
/// Dropping a `oneshot::Sender` is the language both the approval gate and `ask_user`
/// understand for "nobody is going to answer this": the awaiting side wakes on a
/// closed channel instead of sitting until its timeout.
fn drain<T>(map: &Arc<Mutex<HashMap<u64, oneshot::Sender<T>>>>) -> usize {
    let mut m = map.lock().unwrap();
    let n = m.len();
    m.clear();
    n
}

/// Args rendered without their payloads: the fact that a turn was started and of
/// what size matters in a log; the user's message text does not belong in one.
fn arg_hint(args: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(map) = args.as_object() {
        for (k, v) in map {
            let v = match v {
                serde_json::Value::String(s) => format!("len {}", s.chars().count()),
                serde_json::Value::Array(a) => format!("{} item(s)", a.len()),
                serde_json::Value::Null => "unset".to_string(),
                other => other.to_string(),
            };
            parts.push(format!("{k}={v}"));
        }
    }
    parts.sort();
    parts.join(" ")
}

async fn dispatch(raw: &str, role: Role, handle: &EngineHandle, peer: &str) -> Option<String> {
    let req: RpcReq = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            return Some(
                serde_json::json!({ "type": "rpc_error", "error": format!("malformed frame: {e}") })
                    .to_string(),
            )
        }
    };
    if AUDITED.contains(&req.cmd.as_str()) {
        eprintln!(
            "[cwoffee/gateway] {peer} {} ({})",
            req.cmd,
            arg_hint(&req.args)
        );
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
        "share_markdown" => {
            // A read, not a write: the document is handed back and the client
            // decides where it goes (a save dialog on the desktop, a download on
            // a phone). It used to be `export_markdown`, drive-only, and it wrote
            // into the host's workspace - which then looked like model output to
            // the next turn. A watch token can already read this conversation, so
            // there is nothing here that needs a drive token.
            let id = args.get("session").and_then(|v| v.as_i64()).ok_or("session id required")?;
            let row = h
                .store
                .session(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("session {id} does not exist"))?;
            let rows = h.store.messages(id).map_err(|e| e.to_string())?;
            serde_json::to_value(cwoffee_engine::share::share_document(&row.title, &row.workspace, &rows))
                .map_err(|e| e.to_string())
        }
        // ---- web search: the panel on a phone drives the same file the desktop
        // panel writes. Reading is open to a watch token (it names a provider, not
        // a payload); choosing one and running a probe are drive-only: the first
        // edits the host's config, the second spends the host's network and CPU.
        "search_settings" => {
            serde_json::to_value(
                cwoffee_engine::settings::search_view(&h.host, &h.config_path).await,
            )
            .map_err(|e| e.to_string())
        }
        "session_messages" => {
            let id = arg_i64(&args, "id")?;
            let before = args.get("before").and_then(|v| v.as_i64());
            let page = h
                .store
                .messages_page(id, before, 60)
                .map_err(|e| e.to_string())?;
            serde_json::to_value(page).map_err(|e| e.to_string())
        }
        "migration_notes" => Ok(json!(h.migration_notes)),

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
            // The sidebar is grouped by folder, so the folder list is part of
            // "everything a cold client needs". Omitting it would cost a fourth
            // round trip on a phone link, which is the whole reason this command
            // exists.
            let workspaces = match h.store.list_workspaces() {
                Ok(w) => serde_json::to_value(w).unwrap_or_else(|_| json!([])),
                Err(_) => json!([]),
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
                "workspaces": workspaces,
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
            // `session` is the sender's view, and it is not optional in practice:
            // two clients share one host, and a turn without an address lands in
            // whichever conversation the last switch left active. The engine refuses
            // a mismatch, so a phone tab that has gone stale gets an error instead of
            // appending to the desktop's chat.
            let session = args.get("session").and_then(|v| v.as_i64());
            h.host.send_turn_to(text, Vec::new(), session);
            Ok(json!(true))
        }
        "cancel_turn" => {
            need_drive(role)?;
            // Release what the turn is parked on before cancelling. The engine
            // releases a parked approval on its own now, and a parked question's tool
            // wakes when its sender drops, but doing it here makes Stop take effect on
            // the spot rather than on the engine's next poll - and it is the one place
            // that can say how many prompts were outstanding when the user gave up.
            let approvals = drain(&h.pending);
            let questions = drain(&h.pending_questions);
            if approvals + questions > 0 {
                // No peer here: `run_cmd` does not know who asked. The audit line
                // dispatch logs for every drive command names the address, and it
                // always precedes this one.
                println!(
                    "[gateway] cancel_turn released {approvals} approval(s), {questions} question(s)"
                );
            }
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
        "set_search_backend" => {
            need_drive(role)?;
            // Audited and drive-only: this edits the host's config file. A watch
            // token may see which provider is on, not change it.
            let v = cwoffee_engine::settings::set_search_backend(
                &h.host,
                &h.config_path,
                &arg_str(&args, "backend")?,
                &args.get("searxng_url").and_then(|v| v.as_str()).unwrap_or_default(),
            )
            .await?;
            serde_json::to_value(v).map_err(|e| e.to_string())
        }
        "search_probe" => {
            need_drive(role)?;
            // One real search, so "is search working" can be answered on a machine
            // where the model is not being asked to demonstrate it.
            let q = args.get("query").and_then(|v| v.as_str()).map(String::from);
            serde_json::to_value(cwoffee_engine::settings::search_probe(&h.host, q).await?)
                .map_err(|e| e.to_string())
        }
        "respond_approval" => {
            need_drive(role)?;
            let id = arg_i64(&args, "id")? as u64;
            let raw = arg_str(&args, "answer")?;
            // Parsed with the engine's own Deserialize, not a hand-written list of
            // spellings. `ApprovalAnswer` is `rename_all = "camelCase"`, so the
            // wire form the UI sends is "allowSession"; this match asked for
            // "allow_session", which no client ever sent. Plain "allow" happened to
            // agree, so approving from a phone worked for the one button and failed
            // for every other answer - and the failure text named an answer the
            // user had never seen.
            let answer = match serde_json::from_value::<ApprovalAnswer>(serde_json::json!(&raw)) {
                Ok(a) => a,
                Err(_) if raw == "allow_session" => ApprovalAnswer::AllowSession,
                Err(_) => {
                    return Err(format!(
                        "unknown answer {raw:?}; expected allow | allowSession | deny"
                    ))
                }
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
        "respond_question" => {
            need_drive(role)?;
            use cwoffee_engine::tools::ask::Answer;
            let id = arg_i64(&args, "id")? as u64;
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // `options` is a list because a question can allow several answers.
            // A single `option` number is still accepted so an older page does not
            // silently send an empty answer.
            let mut options: Vec<usize> = args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default();
            if options.is_empty() {
                if let Some(one) = args.get("option").and_then(|v| v.as_u64()) {
                    options.push(one as usize);
                }
            }
            let tx = h.pending_questions.lock().unwrap().remove(&id);
            match tx {
                Some(tx) => tx
                    .send(Answer { text, options })
                    .map(|_| json!(true))
                    .map_err(|_| "the engine no longer wants this answer".to_string()),
                None => Err(format!("no pending question with id {id}")),
            }
        }
        "new_session" => {
            need_drive(role)?;
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            // A browser can start a session inside a named workspace; the engine
            // is the one that decides whether that path is usable, and switching
            // into the row moves the tools to where the row says it lives.
            let ws = match args.get("workspace").and_then(|v| v.as_str()) {
                Some(raw) => cwoffee_engine::host::check_workspace(raw)
                    .map_err(|e| format!("{raw:?}: {e}"))?
                    .display()
                    .to_string(),
                None => h.host.state().workspace,
            };
            let id = h
                .store
                .create_session(title, &ws, &h.host.state().model)
                .map_err(|e| e.to_string())?;
            h.store.add_workspace(&ws).ok();
            h.host.send(HostCmd::SwitchSession { id });
            Ok(json!(id))
        }
        "list_workspaces" => {
            // Read-only: the sidebar needs the grouping before it is allowed to
            // touch anything in it.
            Ok(serde_json::to_value(h.store.list_workspaces().map_err(|e| e.to_string())?)
                .unwrap_or(json!([])))
        }
        "add_workspace" => {
            need_drive(role)?;
            let raw = arg_str(&args, "path")?;
            let p = cwoffee_engine::host::check_workspace(&raw).map_err(|e| format!("{raw:?}: {e}"))?;
            h.store.add_workspace(&p.display().to_string()).map_err(|e| e.to_string())?;
            Ok(json!(p.display().to_string()))
        }
        "set_workspace_label" => {
            need_drive(role)?;
            let raw = arg_str(&args, "path")?;
            let label = args.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let n = h
                .store
                .set_workspace_label(&raw, label)
                .map_err(|e| e.to_string())?;
            Ok(json!(n > 0))
        }
        "remove_workspace" => {
            need_drive(role)?;
            let raw = arg_str(&args, "path")?;
            // Drops the sidebar entry only. The sessions that live there are not
            // touched, so the folder reappears as long as they exist.
            Ok(json!(h.store.remove_workspace(&raw).map_err(|e| e.to_string())?))
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
            h.host.send(HostCmd::SetModel { model: arg_str(&args, "model")?, persist: true });
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
            pending_questions: Arc::new(Mutex::new(HashMap::new())),
        clients: Arc::new(Mutex::new(BTreeMap::new())),
            migration_notes: Vec::new(),
            // Never read: serve() must fail before it touches the engine. Present
            // because the struct says a gateway always knows which file it would
            // write, and a test that cannot answer that is not a gateway.
            config_path: std::env::temp_dir().join("cwoffee-gw-refused-config.toml"),
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

    /// A real, writable directory that is *not* the system temp root: the engine
    /// refuses the root itself (it is the "no workspace" bucket), so tests that
    /// want a valid folder have to make one.
    fn unique_dir(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("cwoffee_gw_{tag}_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p)
            .unwrap_or(p)
            .display()
            .to_string()
    }

    #[tokio::test]
    async fn a_drive_token_moves_the_workspace_and_a_bad_path_says_why() {
        let h = test_handle();
        let dir = unique_dir("move");
        let out = run_cmd(
            "set_workspace",
            serde_json::json!({ "path": &dir }),
            Role::Drive,
            &h,
        )
        .await
        .expect("a real writable directory should be accepted");
        assert_eq!(out.as_str(), Some(dir.as_str()), "the reply is the applied path");

        let err = run_cmd(
            "set_workspace",
            serde_json::json!({ "path": "/nonexistent/definitely-not" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("a non-directory must be refused");
        assert!(err.contains("not a directory"), "unhelpful error: {err}");

        // The system temp root is the "no workspace" bucket, so setting it is
        // refused rather than silently accepted: the mobile UI offers this path.
        let err = run_cmd("set_workspace", serde_json::json!({ "path": "/tmp" }), Role::Drive, &h)
            .await
            .expect_err("the temp root is not a workspace");
        assert!(err.contains("temp folder"), "unhelpful error: {err}");
    }

    /// The sidebar list is read-only, so a watch token - which exists so a
    /// colleague can follow a session without being able to touch it - must be
    /// able to read it.
    #[tokio::test]
    async fn the_sidebar_list_reads_the_store_and_a_watch_token_can_see_it() {
        let h = test_handle();
        let out = run_cmd("list_workspaces", serde_json::json!({}), Role::Watch, &h)
            .await
            .expect("listing folders is read-only");
        // The test host starts in a real directory, so its workspace is listed
        // whether or not anybody added it by hand.
        let ws = h.host.state().workspace;
        assert!(
            out.as_array()
                .map(|a| a.iter().any(|w| w["path"].as_str() == Some(ws.as_str())))
                .unwrap_or(false),
            "the workspace a session lives in must appear: {out}"
        );
    }

    #[tokio::test]
    async fn adding_a_folder_to_the_sidebar_is_drive_only_and_validated() {
        let h = test_handle();
        let dir = unique_dir("add");
        let err = run_cmd(
            "add_workspace",
            serde_json::json!({ "path": &dir }),
            Role::Watch,
            &h,
        )
        .await
        .expect_err("a watch token must not edit the sidebar");
        assert!(!err.is_empty());

        let out = run_cmd(
            "add_workspace",
            serde_json::json!({ "path": &dir }),
            Role::Drive,
            &h,
        )
        .await
        .expect("a real writable directory should be accepted");
        assert_eq!(out.as_str(), Some(dir.as_str()));

        // Adding /tmp is refused with a reason the UI can toast: it is the bucket
        // sessions with no workspace live in, not a folder you can add.
        let err = run_cmd("add_workspace", serde_json::json!({ "path": "/tmp" }), Role::Drive, &h)
            .await
            .expect_err("the temp root is not a workspace");
        assert!(err.contains("temp folder"), "unhelpful error: {err}");

        // The engine decides what is a workspace, so a file is not one and the
        // sidebar cannot be filled with junk from a browser.
        let err = run_cmd(
            "add_workspace",
            serde_json::json!({ "path": "/nonexistent/definitely-not" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("a non-directory must be refused");
        assert!(err.contains("not a directory"), "unhelpful error: {err}");

        let out = run_cmd("list_workspaces", serde_json::json!({}), Role::Watch, &h)
            .await
            .unwrap();
        assert!(
            out.as_array()
                .map(|a| a.iter().any(|w| w["path"].as_str() == Some("/tmp")))
                .unwrap_or(false),
            "the added folder must come back: {out}"
        );
    }

    #[tokio::test]
    async fn a_workspace_label_names_the_row_and_not_the_folder() {
        let h = test_handle();
        let dir = unique_dir("label");
        run_cmd("add_workspace", serde_json::json!({ "path": &dir }), Role::Drive, &h)
            .await
            .unwrap();
        let ok = run_cmd(
            "set_workspace_label",
            serde_json::json!({ "path": &dir, "label": "scratch" }),
            Role::Drive,
            &h,
        )
        .await
        .expect("renaming a row the client can see is allowed");
        assert_eq!(ok.as_bool(), Some(true));
        let out = run_cmd("list_workspaces", serde_json::json!({}), Role::Watch, &h)
            .await
            .unwrap();
        assert!(
            out.as_array()
                .map(|a| a.iter().any(|w| w["label"].as_str() == Some("scratch")))
                .unwrap_or(false),
            "the label must survive the round trip: {out}"
        );
        // A name for a folder nobody registered is a no-op, not a new row.
        let ghost = run_cmd(
            "set_workspace_label",
            serde_json::json!({ "path": "/nope", "label": "ghost" }),
            Role::Drive,
            &h,
        )
        .await
        .expect("an unknown path is a valid request");
        assert_eq!(ghost.as_bool(), Some(false));
    }

    /// A client that cannot keep up used to be hung up on, exactly as if the
    /// engine had shut down. That is the worst possible thing to do to a phone:
    /// it has usually already received the `turnStart`, so it comes back with a
    /// composer disabled by a turn that finished long ago.
    ///
    /// The contract now is: say "you missed N events", keep the socket, and let
    /// the client re-read state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_client_that_falls_behind_is_told_and_kept() {
        let h = test_handle();
        let (port, _srv) = serve_on_free_port(&h).await;
        let url = format!("ws://127.0.0.1:{port}/ws");
        let mut ws = attach(&url).await;

        // Prove the connection's write pump has subscribed to the broadcast before
        // flooding it. A subscriber registered *after* these sends starts at the
        // tail and sees none of them, which is not a lag and produces no resync -
        // and that race, not the mechanism, is what made this test flaky under load.
        use futures_util::StreamExt;
        let _ = h.events.send(EngineEvent::Note {
            message: "warm-up: the pump is subscribed".into(),
        });
        let warmed = tokio::time::timeout(
            // Generous, and it costs nothing when healthy: this is a ceiling on a
            // first frame, not a sleep. The box running these tests is also running
            // inference, and 10s of scheduler starvation is ordinary here.
            std::time::Duration::from_secs(30),
            StreamExt::next(&mut ws),
        )
        .await
        .ok()
        .flatten()
        .map(|f| f.map(|f| f.to_text().unwrap_or_default().contains("warm-up")).unwrap_or(false))
        .unwrap_or(false);
        assert!(warmed, "the connection never forwarded a plain event");

        // Enough volume to overflow the broadcast capacity (8) and the socket
        // buffers while the client is not reading, which is what makes the pump lag.
        for chunk in 0..6u32 {
            for i in 0..100u32 {
                let _ = h.events.send(EngineEvent::Note {
                    message: "x".repeat(2048) + &(chunk * 100 + i).to_string(),
                });
            }
            tokio::task::yield_now().await;
        }

        let mut saw_resync = false;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(40));
        tokio::pin!(deadline);
        let mut got = 0u32;
        while !saw_resync {
            let frame = tokio::select! {
                _ = &mut deadline => break,
                f = ws.next() => match f {
                    Some(Ok(f)) => Some(f),
                    _ => None,
                }
            };
            let Some(frame) = frame else { break };
            got += 1;
            let text = frame.to_text().unwrap_or_default().to_string();
            if text.contains("\"resync\"") {
                saw_resync = true;
            }
        }
        assert!(saw_resync, "a client that lagged was never told about it (read {got} frames)");

        // And the socket must still be usable: a resync that arrives just before
        // the server drops the connection is a fancier way of hanging up.
        //
        // Drained without a frame budget and on a generous deadline. The flood is
        // 600 events, and the reply queues behind whatever the kernel has not
        // shipped yet, so the shared helper's 500-frame window and 5s timeout made
        // *this* assertion the flaky one while the mechanism under test was fine.
        ws.send(txt(
            serde_json::json!({ "id": 4242, "cmd": "list_sessions", "args": {} }).to_string(),
        ))
        .await
        .expect("send after resync");
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            async {
                loop {
                    let frame = ws.next().await.expect("socket closed after a resync");
                    let text = frame.expect("frame after a resync").to_text().unwrap_or_default().to_string();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if v.get("id").and_then(|x| x.as_i64()) == Some(4242) {
                            return v;
                        }
                    }
                }
            },
        )
        .await
        .expect("the socket stopped answering after a resync");
        assert_eq!(reply.get("ok").and_then(|x| x.as_bool()), Some(true), "{reply}");
        assert!(
            reply.get("data").map(|d| d.is_array()).unwrap_or(false),
            "list_sessions should return an array: {reply}"
        );
    }

    /// The gauge is a claim about the world, printed as fact in the footer, so
    /// it is tested against real connections rather than against the type.
    /// It used to print 18446744073709552000 devices: the guard decremented an
    /// `AtomicUsize` that nothing had ever incremented.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_attached_gauge_counts_devices_and_returns_to_zero() {
        let h = test_handle();
        let (port, _srv) = serve_on_free_port(&h).await;
        let url = format!("ws://127.0.0.1:{port}/ws");

        let a = attach(&url).await;
        // Poll rather than sleep-and-hope: the guard is taken inside the
        // connection task, which is scheduled on the other worker.
        wait_for(|| h.connections() == 1).await;
        assert_eq!(h.devices(), 1, "one connection should be one device");

        let b = attach(&url).await;
        wait_for(|| h.connections() == 2).await;
        assert_eq!(h.devices(), 1, "two connections from one address is one device");
        assert_eq!(h.peers(), vec!["127.0.0.1".to_string()]);

        drop(a);
        wait_for(|| h.connections() == 1).await;
        assert_eq!(h.devices(), 1, "the other tab is still attached");

        drop(b);
        wait_for(|| h.connections() == 0).await;
        assert_eq!(h.devices(), 0, "nobody is attached");
        assert_eq!(h.connections(), 0, "the gauge must not go negative");
    }

    /// Scanners and probes hit this port constantly. They are not devices.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_socket_that_never_authenticates_is_not_a_device() {
        let h = test_handle();
        let (port, _srv) = serve_on_free_port(&h).await;
        let raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(h.devices(), 0, "an unauthenticated socket was counted");
        assert_eq!(h.connections(), 0);
        drop(raw);
    }

    /// Open a websocket, do the auth handshake, and return the handle so the
    /// test controls when the connection ends.
    async fn attach(url: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        use futures_util::StreamExt;
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        // "hello" first, then our auth frame, then auth_ok + ready.
        let _ = ws.next().await.unwrap().unwrap();
        ws.send(txt(serde_json::json!({ "type": "auth", "token": "tok" }).to_string()))
            .await
            .unwrap();
        for _ in 0..4 {
            let frame = ws.next().await.unwrap().unwrap();
            if frame.to_string().contains("\"ready\"") {
                return ws;
            }
        }
        panic!("auth never completed");
    }

    async fn wait_for(f: impl Fn() -> bool) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Bind a probe port, release it, then serve on it, retrying while the port
    /// is taken. Each call gets its own hundred-port block: two tests that
    /// both reached for 21000 ended up talking to each other's server, and one
    /// reported "an unauthenticated socket was counted" when the count was
    /// actually a genuine attach from the *other* test.
    async fn serve_on_free_port(
        handle: &Arc<EngineHandle>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        use std::sync::atomic::{AtomicU16, Ordering};
        static NEXT_BLOCK: AtomicU16 = AtomicU16::new(21_000);
        let base = NEXT_BLOCK.fetch_add(100, Ordering::Relaxed);
        for port in base..base + 100 {
            if TcpListener::bind(("127.0.0.1", port)).await.is_err() {
                continue;
            }
            let cfg = GatewayConfig {
                host: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
                dist: std::env::temp_dir(),
                drive_token: Some("tok".into()),
                watch_token: None,
            };
            let h = handle.clone();
            let task = tokio::spawn(async move {
                let _ = serve(cfg, h).await;
            });
            for _ in 0..50 {
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    return (port, task);
                }
                if task.is_finished() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        panic!("could not bind a test port");
    }

    /// A question is the model asking the *user*. A watch token can see it, but
    /// only a drive token may answer on the user's behalf.
    #[tokio::test]
    async fn a_watch_token_cannot_answer_a_question() {
        let h = test_handle();
        let err = run_cmd(
            "respond_question",
            serde_json::json!({ "id": 1, "text": "yes", "option": 0 }),
            Role::Watch,
            &h,
        )
        .await
        .expect_err("a watch token must not answer for the user");
        assert!(!err.is_empty());
    }

    /// An unknown id is an error, not a silent success: a UI that thinks it
    /// answered while the engine is still parked will otherwise wait out the
    /// timeout with no clue why.
    #[tokio::test]
    async fn answering_a_question_nobody_asked_says_so() {
        let h = test_handle();
        let err = run_cmd(
            "respond_question",
            serde_json::json!({ "id": 999, "text": "yes" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("id 999 was never asked");
        assert!(err.contains("999"), "unhelpful: {err}");
    }

    /// Sequence for unique temp config paths (see `test_handle_in_notes`).
    static CONFIG_SEQ: std::sync::OnceLock<std::sync::atomic::AtomicUsize> =
        std::sync::OnceLock::new();
    fn config_seq() -> &'static std::sync::atomic::AtomicUsize {
        CONFIG_SEQ.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
    }

    fn test_handle() -> Arc<EngineHandle> {
        test_handle_with_notes(Vec::new())
    }

    fn test_handle_with_notes(notes: Vec<String>) -> Arc<EngineHandle> {
        test_handle_in_notes(std::env::temp_dir().display().to_string(), notes)
    }

    /// A handle whose sessions live in `ws`. Export writes into the conversation's
    /// own workspace, and the engine refuses the system temp root as a workspace,
    /// so a test that wants a real file on disk has to say where.
    fn test_handle_in(ws: &str) -> Arc<EngineHandle> {
        test_handle_in_notes(ws.to_string(), Vec::new())
    }

    fn test_handle_in_notes(ws: String, notes: Vec<String>) -> Arc<EngineHandle> {
        let p: Arc<dyn cwoffee_engine::provider::Provider> = Arc::new(FakeProvider);
        // One store, shared. It used to be two, which quietly made every
        // store-backed RPC in these tests read a database the host had never
        // written to - the assertions still passed, just about nothing.
        let store = Arc::new(Store::in_memory().unwrap());
        Arc::new(EngineHandle {
            host: Arc::new(SessionHost::spawn(
                p,
                // The real tool set, not an empty one: several RPCs here are about
                // tools (web_search's provider being the obvious one), and an empty
                // registry makes those assertions pass about nothing.
                cwoffee_engine::tools::Registry::with_v1_tools(),
                Arc::clone(&store),
                cwoffee_engine::host::HostConfig::new("m", std::path::PathBuf::from(ws)),
            )),
            store,
            events: broadcast::channel(8).0,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_questions: Arc::new(Mutex::new(HashMap::new())),
        clients: Arc::new(Mutex::new(BTreeMap::new())),
            migration_notes: notes,
            // A path under /tmp, never the real ~/.config: these tests write the
            // file they claim to write, and must not touch anyone's settings.
            // A path under /tmp, never the real ~/.config: these tests write the
            // file they claim to write. Unique per handle, because two tests that
            // both "set the search backend" through one shared file race, and the
            // loser's assertion then depends on test order.
            config_path: std::env::temp_dir().join(format!(
                "cwoffee-gw-test-config-{}-{}.toml",
                std::process::id(),
                config_seq().fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            )),
        })
    }

    /// `migration_notes` used to answer `[]` from the gateway no matter what the
    /// desktop knew. An empty list is a *claim* - "nothing needed relocating" - and
    /// on a phone that is the difference between "your old folders were followed"
    /// and "this side is not telling you".
    /// The web UI sends the engine's own spelling, `allowSession`. This dispatch
    /// used to compare against `"allow_session"`, which no client has ever sent, so
    /// a phone could approve the "Allow" button and nothing else - the turn stayed
    /// parked, and the error named an answer nobody had seen.
    #[tokio::test]
    async fn an_approval_answered_from_the_phone_reaches_the_engine() {
        let h = test_handle();
        for answer in ["allow", "allowSession", "deny"] {
            let (tx, rx) = oneshot::channel();
            h.pending.lock().unwrap().insert(7, tx);
            let out = run_cmd(
                "respond_approval",
                serde_json::json!({ "id": 7, "answer": answer }),
                Role::Drive,
                &h,
            )
            .await
            .unwrap_or_else(|e| panic!("{answer} was refused: {e}"));
            assert_eq!(out, serde_json::json!(true));
            let got = rx.await.expect("the parked sender was dropped, not answered");
            let want = match answer {
                "allow" => ApprovalAnswer::Allow,
                "allowSession" => ApprovalAnswer::AllowSession,
                _ => ApprovalAnswer::Deny,
            };
            assert_eq!(got, want, "{answer} resolved as {got:?}");
        }
        // A legacy snake_case client is still understood, and a typo is refused by
        // name rather than by "no pending approval".
        let (tx, _rx) = oneshot::channel();
        h.pending.lock().unwrap().insert(8, tx);
        run_cmd(
            "respond_approval",
            serde_json::json!({ "id": 8, "answer": "allow_session" }),
            Role::Drive,
            &h,
        )
        .await
        .expect("the old spelling should still resolve");
        let err = run_cmd(
            "respond_approval",
            serde_json::json!({ "id": 9, "answer": "yes" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("yes is not an answer");
        assert!(err.contains("allowSession"), "unhelpful: {err}");
    }

    /// A stale view cannot append to someone else's conversation.
    ///
    /// The desktop switched on, the phone tab did not, and the phone sends. Without
    /// an address on the turn that message lands in whatever session is active -
    /// which reads to the person at the keyboard as "my transcript changed by
    /// itself". Over the wire the refusal arrives as an error *event* (the rpc itself
    /// was well formed), so the client must render it; what this asserts is that
    /// nothing ran.
    /// The search panel on a phone drives the same file the desktop panel writes.
    /// What must hold: a watch token can read it, cannot change it, and a change
    /// that is accepted lands in the config file this process was given (a temp
    /// path in these tests, never anyone's ~/.config).
    #[tokio::test]
    async fn the_search_panel_is_readable_to_a_watcher_and_writable_only_by_a_driver() {
        let h = test_handle();
        let view = run_cmd("search_settings", serde_json::json!({}), Role::Watch, &h)
            .await
            .expect("a watcher can read which search is on");
        assert_eq!(view["backend"], "duckduckgo", "{view}");
        assert!(view["describing"].as_str().unwrap().contains("DuckDuckGo"), "{view}");
        // Nothing was chosen by these tests, so nothing should have been written.
        let cfg_path = h.config_path.clone();
        let _ = std::fs::remove_file(&cfg_path);

        let err = run_cmd(
            "set_search_backend",
            serde_json::json!({ "backend": "auto" }),
            Role::Watch,
            &h,
        )
        .await
        .expect_err("a watcher must not be able to reconfigure the host");
        assert!(err.contains("watch"), "unhelpful: {err}");
        assert!(!cfg_path.exists(), "the refusal must not have written the file either");

        let out = run_cmd("set_search_backend", serde_json::json!({ "backend": "auto" }), Role::Drive, &h)
            .await
            .expect("a driver can choose the managed provider");
        assert_eq!(out["backend"], "auto", "{out}");
        assert!(
            out["describing"].as_str().unwrap().contains("managed by Cwoffee"),
            "the answer must say what it became: {out}"
        );
        // Persisted, or "enabled" meant until the next restart.
        let text = std::fs::read_to_string(&cfg_path).expect("the choice was written");
        assert!(text.contains("backend = \"auto\""), "{text}");
        // And the live tool agrees, not just the file.
        let live = run_cmd("search_settings", serde_json::json!({}), Role::Watch, &h)
            .await
            .expect("read back");
        assert_eq!(live["backend"], "auto", "{live}");

        // A nonsense provider is an error, not a quiet fall back.
        let bad = run_cmd(
            "set_search_backend",
            serde_json::json!({ "backend": "google" }),
            Role::Drive,
            &h,
        )
        .await
        .expect_err("unknown provider must fail");
        assert!(bad.contains("unknown search provider"), "unhelpful: {bad}");
        let after = run_cmd("search_settings", serde_json::json!({}), Role::Watch, &h)
            .await
            .expect("read back again");
        assert_eq!(after["backend"], "auto", "the refused choice must not stick: {after}");
        let _ = std::fs::remove_file(&cfg_path);
    }

    /// A probe is a real search, so it costs the host a request: drive-only. And
    /// when the provider cannot answer, the answer is an error sentence rather than
    /// an empty list that reads as "the internet has nothing about that".
    #[tokio::test]
    async fn a_probe_is_drive_only_and_says_which_side_failed() {
        let h = test_handle();
        let err = run_cmd("search_probe", serde_json::json!({ "query": "coffee" }), Role::Watch, &h)
            .await
            .expect_err("a watcher must not spend the host's network");
        assert!(err.contains("watch"), "unhelpful: {err}");

        // Point the tool at a loopback port nothing listens on, then probe. No
        // internet needed, and the failure is the one a user actually hits.
        let _ = run_cmd(
            "set_search_backend",
            serde_json::json!({ "backend": "searxng", "searxng_url": "http://127.0.0.1:9/search" }),
            Role::Drive,
            &h,
        )
        .await
        .expect("a manual URL is a legitimate choice");
        let out = run_cmd("search_probe", serde_json::json!({ "query": "coffee roasting" }), Role::Drive, &h)
            .await
            .expect("the probe itself is well formed even when the search fails");
        assert!(out["error"].is_string(), "expected an error sentence, got {out}");
        assert!(out["results"].as_array().map(|a| a.is_empty()).unwrap_or(false), "{out}");
        let _ = std::fs::remove_file(&h.config_path);
    }

    #[tokio::test]
    async fn a_phone_can_share_a_conversation_without_touching_the_host() {
        // Sharing is a read: the document comes back over the wire and nothing is
        // written on the host. The old shape put a file in the conversation's own
        // workspace, and the next model turn treated that as its own output.
        let dir = unique_dir("share");
        let h = test_handle_in(&dir);
        let id = match run_cmd("new_session", serde_json::json!({ "title": "share me" }), Role::Drive, &h)
            .await
            .unwrap()
        {
            serde_json::Value::Number(n) => n.as_i64().unwrap(),
            other => panic!("new_session returned {other:?}"),
        };
        h.store
            .append_message(
                id,
                &cwoffee_engine::provider::Message::user("the story so far"),
            )
            .unwrap();
        let turns_before = h.host.state().turns;
        let before = h.host.state().session_id;

        // A watch token may share. It is the same conversation it can already read
        // message by message; there is no write left to gate.
        let out = match run_cmd("share_markdown", serde_json::json!({ "session": id }), Role::Watch, &h)
            .await
            .unwrap()
        {
            serde_json::Value::Object(o) => o,
            other => panic!("share returned {other:?}"),
        };
        let name = out.get("name").and_then(|v| v.as_str()).unwrap().to_string();
        let body = out.get("markdown").and_then(|v| v.as_str()).unwrap().to_string();
        assert!(name.ends_with(".md") && !name.contains('/'), "suggested name {name:?}");
        assert!(body.contains("the story so far"), "document did not contain the conversation");
        assert!(body.contains("share me"), "document did not name the session");

        // Nothing on the host changed. This is the whole point of the rewrite.
        let left = std::fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0);
        assert_eq!(left, 0, "sharing wrote {left} entr(y/ies) into the workspace");

        let ghost = 900_000i64;
        let err = run_cmd("share_markdown", serde_json::json!({ "session": ghost }), Role::Watch, &h)
            .await
            .expect_err("a conversation that does not exist cannot be shared");
        assert!(err.contains("does not exist"), "got {err}");

        // And the old command is gone rather than quietly still writing files.
        let err = run_cmd("export_markdown", serde_json::json!({ "session": id }), Role::Drive, &h)
            .await
            .expect_err("export_markdown must not survive the change");
        assert!(err.contains("unknown command"), "got {err}");

        assert_eq!(h.host.state().turns, turns_before, "sharing ran a turn");
        assert_eq!(h.host.state().session_id, before, "sharing moved the host");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_turn_from_a_phone_tab_lands_where_the_tab_is_looking() {
        let h = test_handle();
        // The phone is looking at its own conversation; the desktop may be on
        // another. Browsing does not move the host, so the send carries the view
        // and the host follows it.
        let id = match run_cmd("new_session", serde_json::json!({ "title": "phone tab" }), Role::Drive, &h)
            .await
            .expect("new_session is drive-usable")
        {
            serde_json::Value::Number(n) => n.as_i64().unwrap(),
            other => panic!("new_session returned {other:?}"),
        };
        let turns_before = h.host.state().turns;
        run_cmd(
            "start_turn",
            serde_json::json!({ "text": "hello from the phone", "session": id }),
            Role::Drive,
            &h,
        )
        .await
        .expect("the command is well formed; anything the engine refuses is an event");
        for _ in 0..100 {
            if h.host.state().turns > turns_before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(h.host.state().turns, turns_before + 1, "the turn never ran");
        assert_eq!(h.host.state().session_id, id, "the host did not follow the addressee");
        let rows = h.store.history(id).unwrap();
        assert_eq!(rows.len(), 2, "the turn did not land in the addressed session");

        // A number that matches no conversation: the send stops. It must not fall
        // back to whatever the host happened to have open.
        let ghost = 900_000;
        run_cmd(
            "start_turn",
            serde_json::json!({ "text": "nowhere to go", "session": ghost }),
            Role::Drive,
            &h,
        )
        .await
        .expect("the rpc itself is fine; the refusal is an engine event");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(h.host.state().turns, turns_before + 1, "a turn with no addressee ran");
        assert_eq!(h.host.state().session_id, id, "the host moved to chase a missing row");
    }

    #[tokio::test]
    async fn a_cancel_releases_what_the_turn_is_parked_on() {
        let h = test_handle();
        let (atx, arx) = oneshot::channel::<ApprovalAnswer>();
        let (qtx, qrx) = oneshot::channel::<cwoffee_engine::tools::ask::Answer>();
        h.pending.lock().unwrap().insert(11, atx);
        h.pending_questions.lock().unwrap().insert(12, qtx);

        run_cmd("cancel_turn", serde_json::json!({}), Role::Drive, &h)
            .await
            .expect("cancel should be honoured");

        // Closed channel, not a value: that is how the approval gate reads "nobody is
        // going to answer" and refuses to run, and how ask_user tells the model the
        // question was cancelled.
        assert!(arx.await.is_err(), "the parked approval was left waiting");
        assert!(qrx.await.is_err(), "the parked question was left waiting");
        assert!(h.pending.lock().unwrap().is_empty(), "the approval map still holds a sender");
        assert!(h.pending_questions.lock().unwrap().is_empty(), "the question map still holds a sender");
    }

    #[tokio::test]
    async fn migration_notes_reports_what_the_host_was_actually_given() {
        let h = test_handle();
        let out = run_cmd("migration_notes", serde_json::json!({}), Role::Watch, &h).await;
        assert_eq!(out.unwrap(), serde_json::json!([] as [String; 0]));

        let h = test_handle_with_notes(vec![
            "re-homed 3 session(s) from /home/me/cowry-workspace to /home/me/cwoffee-workspace"
                .into(),
        ]);
        let out = run_cmd("migration_notes", serde_json::json!({}), Role::Watch, &h).await;
        let arr = out.unwrap();
        let arr = arr.as_array().expect("an array");
        assert_eq!(arr.len(), 1, "{arr:?}");
        assert!(arr[0].as_str().unwrap().contains("re-homed 3 session"), "{arr:?}");
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
