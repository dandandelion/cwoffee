//! Network reach for the exec sandbox, and the one place that decides what an
//! agent is allowed to download.
//!
//! Three levels, because "give it internet" and "keep it contained" are both real
//! requirements and no single setting serves both:
//!
//! ```text
//! off     no network at all (bwrap --unshare-net). What the sandbox used to be.
//! proxy   network reachable; well-behaved tooling goes through a proxy in this
//!         process that allowlists hosts, counts bytes and logs refusals. DEFAULT.
//! open    network, no proxy. Simplest, least contained.
//! ```
//!
//! The sentence that has to stay true: **`proxy` is a guardrail, not a boundary.**
//! npm, yarn, pip, cargo, git, curl and wget all honour proxy variables, so their
//! traffic is decided here in practice. A process that ignores those variables and
//! dials an IP directly is not stopped, and cannot be stopped in this design:
//! enforcing egress at the network layer needs its own network namespace with a
//! veth pair plus `nftables`/`tc`, i.e. root. The boundary is still what it was -
//! filesystem binds, user namespace, PID namespace, IPC namespace, and the
//! denylist. What `proxy` adds on top of that is three things that do not need
//! root: **decisions by hostname**, **a byte budget**, and **a log of what was
//! asked for** - and a refusal the model can read, name and configure around.
//!
//! One hard constraint worth knowing before reaching for a knob: with
//! `--unshare-net` there is no network at all, so a proxy is unreachable by
//! construction. Sharing the namespace is what makes the proxy reachable, and the
//! moment the namespace is shared the allowlist can only be cooperative. That is
//! why these are three modes and not one switch with two settings.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetMode {
    Off,
    #[default]
    Proxy,
    Open,
}

impl NetMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "unshare" | "isolated" => Some(NetMode::Off),
            "proxy" | "allowlist" => Some(NetMode::Proxy),
            "open" | "full" | "all" | "on" => Some(NetMode::Open),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NetMode::Off => "off",
            NetMode::Proxy => "proxy",
            NetMode::Open => "open",
        }
    }
}

/// What `[sandbox] network` hands to the exec tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetCfg {
    pub mode: NetMode,
    /// Hosts the proxy admits. An entry is exact (`registry.npmjs.org`) or a
    /// subdomain wildcard (`*.example.com`). `*` admits everything, which is
    /// `open` with the proxy's logging still running - a useful way to find out
    /// what a build actually reaches before tightening the list.
    pub allow: Vec<String>,
    /// Bytes through the proxy per exec call. Not a security limit - a 2 GB
    /// download is slow, not dangerous - but a model that mirrors a registry pays
    /// for it in wall clock and disk, and a stop that names itself beats a wait
    /// that never ends.
    pub max_bytes_per_call: u64,
}

impl NetCfg {
    /// One line for the boot log: what the sandbox will do, and with which budget.
    /// A user who edits `[sandbox] network` can then see the app honour it instead
    /// of guessing from whether a command happened to work.
    pub fn describe(&self) -> String {
        match self.mode {
            NetMode::Off => "off: no network in the sandbox".into(),
            NetMode::Open => "open: the sandbox shares the machine's network".into(),
            NetMode::Proxy => format!(
                "proxy: {} allowlisted host pattern(s), {} MB per call",
                self.allow.len(),
                self.max_bytes_per_call / (1 << 20)
            ),
        }
    }
}

impl Default for NetCfg {
    fn default() -> Self {
        Self {
            mode: NetMode::Proxy,
            allow: default_allow(),
            max_bytes_per_call: 512 * 1024 * 1024,
        }
    }
}

/// The default list is "install a dependency", not "browse the web". Every entry
/// is a host a package manager dials, and the pairs matter: pip uses one host for
/// metadata and a different one for the files, cargo an index and a download
/// server, and forgetting either is the classic "it resolved but could not
/// download" half-setup.
pub fn default_allow() -> Vec<String> {
    const LIST: [&str; 23] = [
        // npm / yarn / node
        "registry.npmjs.org",
        "registry.yarnpkg.com",
        "nodejs.org",
        // python
        "pypi.org",
        "pypi.python.org",
        "files.pythonhosted.org",
        // rust
        "crates.io",
        "static.crates.io",
        "index.crates.io",
        // git forges and raw fetches
        "github.com",
        "codeload.github.com",
        "raw.githubusercontent.com",
        "objects.githubusercontent.com",
        "api.github.com",
        "gitlab.com",
        // other ecosystems
        "proxy.golang.org",
        "sum.golang.org",
        "rubygems.org",
        "huggingface.co",
        "cdn.jsdelivr.net",
        "unpkg.com",
        "deno.land",
        // model weights / datasets on the hub share the apex host
        "hf.co",
    ];
    LIST.iter().map(|s| s.to_string()).collect()
}

/// Should bwrap get `--unshare-net`?
pub fn unshare_net(mode: NetMode) -> bool {
    matches!(mode, NetMode::Off)
}

/// Environment for the sandboxed command.
///
/// Every variable here exists because a different tool reads a different one: npm
/// has its own `npm_config_*` pair, pip reads `PIP_PROXY`, cargo
/// `CARGO_HTTP_PROXY`, git and curl `ALL_PROXY`. `NO_PROXY` must name loopback, or
/// a command inside the sandbox that talks to the app's own gateway, or to a local
/// llama.cpp endpoint, would be pushed through a proxy that will not carry it.
pub fn proxy_env(port: u16) -> Vec<(String, String)> {
    let url = format!("http://127.0.0.1:{port}");
    vec![
        ("HTTP_PROXY".into(), url.clone()),
        ("http_proxy".into(), url.clone()),
        ("HTTPS_PROXY".into(), url.clone()),
        ("https_proxy".into(), url.clone()),
        ("ALL_PROXY".into(), url.clone()),
        ("all_proxy".into(), url.clone()),
        ("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()),
        ("no_proxy".into(), "localhost,127.0.0.1,::1".into()),
        ("npm_config_proxy".into(), url.clone()),
        ("npm_config_https_proxy".into(), url.clone()),
        ("CARGO_HTTP_PROXY".into(), url.clone()),
        ("PIP_PROXY".into(), url),
    ]
}

/// Does the allowlist admit `host`? Case-insensitive. A `*.` entry covers
/// subdomains but not the apex, which is how CDNs and forges are usually split.
pub fn admitted(allow: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allow.iter().any(|entry| {
        let e = entry.trim().to_ascii_lowercase();
        if e == "*" {
            return true;
        }
        if let Some(suffix) = e.strip_prefix("*.") {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else {
            host == e
        }
    })
}

/// The default port policy. A proxy that will happily tunnel port 25 or 6667 is
/// not an egress allowlist, it is a VPN with extra steps.
pub const WEB_PORTS: [u16; 2] = [80, 443];

pub fn allowed_port(port: u16) -> bool {
    WEB_PORTS.contains(&port)
}

/// State shared between the exec tool and the proxy task.
#[derive(Default)]
pub struct EgressState {
    bytes: AtomicU64,
    limit: AtomicU64,
    denied: Mutex<Vec<String>>,
}

impl EgressState {
    pub fn bytes(self: &Arc<Self>) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn hosts_denied(self: &Arc<Self>) -> Vec<String> {
        self.denied.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Start of an exec call: zero the counters so the note attached to a result
    /// describes that call, not everything since boot.
    pub fn begin_call(self: &Arc<Self>, limit: u64) -> CallMark {
        self.bytes.store(0, Ordering::Relaxed);
        self.limit.store(limit, Ordering::Relaxed);
        if let Ok(mut d) = self.denied.lock() {
            d.clear();
        }
        CallMark { over_budget: false }
    }

    pub fn note_over_budget(self: &Arc<Self>) {
        if let Ok(mut d) = self.denied.lock() {
            if !d.iter().any(|h| h.starts_with("#bytes")) {
                d.push("#bytes".into());
            }
        }
    }
}

pub struct CallMark {
    pub over_budget: bool,
}

/// A running proxy.
pub struct EgressProxy {
    pub port: u16,
    pub state: Arc<EgressState>,
}

/// Start the proxy on loopback, on a port the OS picks. Loopback-only is not a
/// detail: a proxy reachable from the LAN is an open relay for anyone who can see
/// the machine, and this one has no credentials.
pub async fn start(
    allow: Vec<String>,
    max_bytes_per_call: u64,
) -> Result<EgressProxy, String> {
    start_with_ports(allow, max_bytes_per_call, &WEB_PORTS).await
}

/// `ports` is a parameter rather than a constant because the tests need to tunnel
/// to an origin on a loopback port they chose at random, and a test that cannot
/// reach the success path only proves the refusal path.
pub async fn start_with_ports(
    allow: Vec<String>,
    max_bytes_per_call: u64,
    ports: &[u16],
) -> Result<EgressProxy, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("egress proxy: cannot bind loopback: {e}"))?;
    let addr: SocketAddr = listener
        .local_addr()
        .map_err(|e| format!("egress proxy: no local address: {e}"))?;
    let state = Arc::new(EgressState {
        bytes: AtomicU64::new(0),
        limit: AtomicU64::new(max_bytes_per_call),
        denied: Mutex::new(Vec::new()),
    });
    let shared = Arc::clone(&state);
    // Owned before the spawn: the accept loop outlives this function, so a borrow
    // of the caller's slice cannot ride along in it.
    let ports = Arc::new(ports.to_vec());
    tokio::spawn(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                let allow = allow.clone();
                let ports = Arc::clone(&ports);
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let _ = serve_one(sock, &allow, &ports, &state).await;
                });
            }
        }
    });
    Ok(EgressProxy { port: addr.port(), state })
}

async fn serve_one(
    mut sock: TcpStream,
    allow: &[String],
    ports: &[u16],
    state: &Arc<EgressState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read the request head one byte at a time. Deliberate: buffering here could
    // swallow the start of a request body, and this stream is split into halves
    // after the decision is made.
    let mut head: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < 64 * 1024 {
        if sock.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let first = text.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("");

    let (host, port, connect) = match decide_target(&method, target) {
        Some(t) => t,
        None => {
            return reject(sock, 400, "cwoffee egress proxy: unsupported request").await;
        }
    };

    if !ports.contains(&port) || !admitted(allow, &host) {
        if let Ok(mut d) = state.denied.lock() {
            if !d.iter().any(|h| h == &host) {
                d.push(host.clone());
            }
        }
        return reject(
            sock,
            403,
            &format!(
                "cwoffee egress proxy: {host}:{port} is not in the sandbox egress allowlist. \
                 Add it in config.toml as allow = [\"{host}\"] under [sandbox], or run this \
                 command outside Cwoffee."
            ),
        )
        .await;
    }

    let mut peer = match TcpStream::connect((host.as_str(), port)).await {
        Ok(p) => p,
        Err(e) => {
            return reject(
                sock,
                502,
                &format!("cwoffee egress proxy: cannot reach {host}:{port}: {e}"),
            )
            .await;
        }
    };

    if connect {
        // 200 and then a blind tunnel. Nothing is inspected past here - it is TLS -
        // which is exactly why the decision had to be made on the CONNECT hostname.
        sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
    } else {
        // Plain HTTP: the origin wants a path, not the absolute URI the proxy was
        // given. Everything else is forwarded untouched. To the peer - writing this
        // back to the caller would hand it its own request and stall the origin
        // forever, which is exactly what this line used to do.
        peer.write_all(&rewrite_absolute_uri(&head)).await?;
    }

    // Four halves, two pumps: a package manager uploads and downloads on one
    // connection, and a single-direction copy stalls the other.
    let (mut sr, mut sw) = sock.into_split();
    let (mut pr, mut pw) = peer.into_split();
    let up = pump(&mut sr, &mut pw, state);
    let down = pump(&mut pr, &mut sw, state);
    // select!, not join!: when either direction ends - origin closed the response,
    // the caller hung up, or the byte budget cut the pipe - the other direction has
    // nothing left to wait for. join! would keep a connection open forever for a
    // client that sent its request and is simply waiting to read.
    let over = tokio::select! { r = up => r.over, r = down => r.over };
    // Returning here drops all four halves, which closes both ends: the caller sees
    // FIN, the origin sees FIN, and no task is left waiting on a half that will
    // never produce another byte.
    if over {
        state.note_over_budget();
    }
    Ok(())
}

struct Pumped {
    over: bool,
}

async fn pump<R, W>(mut read: R, mut write: W, state: &Arc<EgressState>) -> Pumped
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = [0u8; 32 * 1024];
    let mut over = false;
    loop {
        let n = match read.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if write.write_all(&buf[..n]).await.is_err() {
            break;
        }
        if state.bytes.fetch_add(n as u64, Ordering::Relaxed) + n as u64
            > state.limit.load(Ordering::Relaxed)
        {
            over = true;
            break;
        }
    }
    Pumped { over }
}

/// `(host, port, is_connect)` from the request line.
fn decide_target(method: &str, target: &str) -> Option<(String, u16, bool)> {
    match method {
        "CONNECT" => {
            let (h, p) = target.rsplit_once(':')?;
            Some((h.to_string(), p.parse().ok()?, true))
        }
        "GET" | "POST" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "PATCH" => {
            let rest = target.strip_prefix("http://")?;
            let authority = rest.split('/').next()?;
            let (h, p) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse().ok()?),
                None => (authority, 80),
            };
            Some((h.to_string(), p, false))
        }
        _ => None,
    }
}

fn rewrite_absolute_uri(head: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(head).into_owned();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            let mut parts = line.splitn(3, ' ');
            let m = parts.next().unwrap_or("GET");
            let target = parts.next().unwrap_or("/");
            let ver = parts.next().unwrap_or("HTTP/1.1");
            let path = match target.strip_prefix("http://") {
                // Everything from the first `/` after the authority onward:
                // "http://h/a/b" is "/a/b", not "/a".
                Some(rest) => match rest.find('/') {
                    Some(i) => rest[i..].to_string(),
                    None => "/".into(),
                },
                None => target.to_string(),
            };
            out.push_str(&format!("{m} {path} {ver}\r\n"));
        } else if !line.is_empty() {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    out.into_bytes()
}

async fn reject(
    mut sock: TcpStream,
    code: u16,
    why: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body = format!("{why}\n");
    let reason = match code {
        403 => "Forbidden",
        502 => "Bad Gateway",
        _ => "Bad Request",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Proxy-Status: cwoffee\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allow(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_default_list_covers_the_hosts_a_real_install_needs() {
        // Each pair is a host people forget: pypi metadata and pypi files are
        // different hosts, cargo index and cargo downloads are different hosts.
        for host in [
            "registry.npmjs.org",
            "registry.yarnpkg.com",
            "pypi.org",
            "files.pythonhosted.org",
            "index.crates.io",
            "static.crates.io",
            "github.com",
            "codeload.github.com",
            "nodejs.org",
        ] {
            assert!(admitted(&NetCfg::default().allow, host), "{host} missing");
        }
        // And it is not the whole internet.
        for host in ["evil.example.com", "pypi.org.evil.net", "198.51.100.7"] {
            assert!(!admitted(&NetCfg::default().allow, host), "{host} admitted");
        }
    }

    #[test]
    fn allow_matching_is_case_insensitive_and_subdomain_aware() {
        let a = allow(&["*.gitlab.com", "Example.COM"]);
        assert!(admitted(&a, "gitlab.com"));
        assert!(admitted(&a, "code.gitlab.com"));
        assert!(admitted(&a, "EXAMPLE.com"));
        assert!(!admitted(&a, "notgitlab.com"));
        assert!(!admitted(&a, "sub.example.com"));
        let star = allow(&["*"]);
        assert!(admitted(&star, "anything.at.all"));
    }

    #[test]
    fn only_web_ports_tunnel() {
        assert!(allowed_port(443) && allowed_port(80));
        for p in [25u16, 135, 139, 445, 3389, 6667, 8080] {
            assert!(!allowed_port(p), "{p} tunneled");
        }
    }

    #[test]
    fn proxy_env_covers_the_tools_that_read_different_names_and_keeps_loopback_direct() {
        let env = proxy_env(9999);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "npm_config_https_proxy",
            "PIP_PROXY",
            "CARGO_HTTP_PROXY",
        ] {
            assert_eq!(get(k).as_deref(), Some("http://127.0.0.1:9999"), "{k}");
        }
        let no = get("NO_PROXY").unwrap();
        assert!(no.contains("127.0.0.1") && no.contains("localhost"), "{no}");
    }

    #[test]
    fn mode_words_parse_and_only_off_is_no_network() {
        assert_eq!(NetMode::parse("off"), Some(NetMode::Off));
        assert_eq!(NetMode::parse("Proxy"), Some(NetMode::Proxy));
        assert_eq!(NetMode::parse("open"), Some(NetMode::Open));
        assert_eq!(NetMode::parse("maybe"), None);
        assert!(unshare_net(NetMode::Off));
        assert!(!unshare_net(NetMode::Proxy));
        assert!(!unshare_net(NetMode::Open));
    }

    #[test]
    fn request_lines_become_host_and_port() {
        assert_eq!(
            decide_target("CONNECT", "registry.npmjs.org:443"),
            Some(("registry.npmjs.org".into(), 443, true))
        );
        assert_eq!(
            decide_target("GET", "http://example.com/a/b"),
            Some(("example.com".into(), 80, false))
        );
        assert_eq!(
            decide_target("GET", "http://example.com:8080/a"),
            Some(("example.com".into(), 8080, false))
        );
        assert_eq!(decide_target("PROPFIND", "http://example.com/"), None);
    }

    #[test]
    fn a_plain_request_loses_the_authority_before_the_origin_sees_it() {
        let head = b"GET http://example.com/a/b HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let out = String::from_utf8(rewrite_absolute_uri(head)).unwrap();
        assert!(out.starts_with("GET /a/b HTTP/1.1\r\n"), "{out}");
        assert!(out.contains("Host: example.com"), "{out}");
    }

    /// End to end over a real socket, with a local origin server: the allowed
    /// request gets the body, the refused one gets a text answer that names the
    /// host and the config line that would admit it, and the refusal is recorded
    /// for the exec result to repeat.
    #[tokio::test]
    async fn the_proxy_serves_an_admitted_host_and_refuses_the_rest_by_name() {
        // A stand-in origin on loopback.
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = origin.accept().await {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf).await;
                let body = "hello from the origin";
                let _ = c
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });

        let proxy =
            start_with_ports(vec!["127.0.0.1".into()], 1 << 20, &[80, 443, origin_port])
                .await
                .unwrap();

        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(
            format!("GET http://127.0.0.1:{origin_port}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).await.unwrap();
        assert!(got.contains("200 OK"), "{got}");
        assert!(got.contains("hello from the origin"), "{got}");

        // A port outside 80/443 never reaches the origin at all.
        let mut c2 = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c2.write_all(b"CONNECT 127.0.0.1:25 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut got2 = String::new();
        c2.read_to_string(&mut got2).await.unwrap();
        assert!(got2.starts_with("HTTP/1.1 403"), "{got2}");
        assert!(got2.contains("127.0.0.1:25"), "{got2}");
        assert!(got2.contains("allow = [\"127.0.0.1\"]"), "{got2}");
        assert!(got2.contains(":25"), "{got2}");
        assert!(proxy.state.hosts_denied().contains(&"127.0.0.1".to_string()));
    }

    /// The byte budget has to stop the copy, not just count it.
    #[tokio::test]
    async fn a_call_that_moves_more_than_its_budget_is_stopped_and_said_so() {
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = origin.accept().await {
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf).await;
                let body = vec![b'x'; 400_000];
                let _ = c
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = c.write_all(&body).await;
            }
        });
        let proxy =
            start_with_ports(vec!["127.0.0.1".into()], 100_000, &[80, 443, port])
                .await
                .unwrap();
        let _mark = proxy.state.begin_call(100_000);
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(
            format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: h\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got).await;
        let moved = proxy.state.bytes();
        assert!(moved > 0, "nothing counted");
        assert!(
            proxy.state.hosts_denied().iter().any(|h| h == "#bytes"),
            "no budget note: {:?}",
            proxy.state.hosts_denied()
        );
    }

    #[allow(dead_code)]
    fn unused_args() -> serde_json::Value {
        json!({})
    }
}
