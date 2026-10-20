//! Network tools: `web_fetch` and `web_search`.
//!
//! These are the **only** network egress in v1. The exec sandbox runs with
//! `--unshare-net`, so arbitrary commands cannot reach the network; everything
//! that does goes through two narrow, auditable, schema-checked calls. That is
//! a much smaller surface than "the agent may run any command, and some of them
//! happen to use curl".
//!
//! Limits stated honestly:
//!   * IP-literal and metadata-endpoint blocking is done on the resolved URL.
//!     DNS rebinding (a name that resolves to a public IP for the check and a
//!     private one for the request) is **not** mitigated without a custom
//!     resolver. Documented rather than pretended away.
//!   * The HTML-to-text pass is a stripper, not a parser. It is good enough to
//!     read articles and will mangle pathological markup. `pulldown-cmark` is
//!     the planned replacement in M2.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{CallRisk, ExecSink, OutputFile, Risk, Tool, ToolCtx, ToolResult};
use tokio::io::AsyncWriteExt;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) Cwoffee/0.1 (+local agent)";

#[derive(Debug)]
pub enum WebError {
    Http(reqwest::Error),
    Url(String),
    Blocked(String),
    TooBig,
    Unsupported(String),
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebError::Http(e) => write!(f, "request failed: {e}"),
            WebError::Url(m) => write!(f, "invalid url: {m}"),
            WebError::Blocked(m) => write!(f, "blocked: {m}"),
            WebError::TooBig => write!(f, "response exceeded the byte limit"),
            WebError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}
impl From<reqwest::Error> for WebError {
    fn from(e: reqwest::Error) -> Self {
        WebError::Http(e)
    }
}

/// Reject URLs that should not be fetched regardless of what the model asked.
fn check_url(raw: &str) -> Result<url::Url, WebError> {
    let u = url::Url::parse(raw).map_err(|e| WebError::Url(e.to_string()))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err(WebError::Blocked(format!(
            "scheme {:?} is not allowed; only http and https",
            u.scheme()
        )));
    }
    let host = u
        .host_str()
        .ok_or_else(|| WebError::Url("url has no host".into()))?
        .to_ascii_lowercase();

    // Cloud metadata and link-local: the classic SSRF target, and never
    // something a chat assistant has a legitimate reason to fetch.
    const BLOCKED_HOSTS: &[&str] = &[
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.goog",
        "100.100.100.200", // alibaba cloud metadata
    ];
    if BLOCKED_HOSTS.contains(&host.as_str()) {
        return Err(WebError::Blocked(format!("{host} is a cloud metadata endpoint")));
    }
    if let Some(host) = u.host() {
        let ip: Option<std::net::IpAddr> = match host {
            url::Host::Ipv4(v4) => Some(std::net::IpAddr::V4(v4)),
            url::Host::Ipv6(v6) => Some(std::net::IpAddr::V6(v6)),
            url::Host::Domain(_) => None,
        };
        if let Some(ip) = ip {
            if ip.is_unspecified() || ip.is_multicast() {
                return Err(WebError::Blocked(format!("{ip} is a special-purpose address")));
            }
            // std's is_link_local only covers IPv4 169.254/16; IPv6 fe80::/10
            // has to be checked by hand.
            let link_local = match ip {
                std::net::IpAddr::V4(v4) => v4.is_link_local(),
                std::net::IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
            };
            if link_local {
                return Err(WebError::Blocked(format!("{ip} is link-local")));
            }
        }
    }
    Ok(u)
}

pub struct WebFetchTool {
    pub client: reqwest::Client,
    pub max_bytes: usize,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(UA)
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .expect("reqwest client"),
            max_bytes: 200_000,
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over http/https and return its text content with HTML markup \
         removed. Use this to read a specific page; use web_search to find pages."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL" }
            },
            "required": ["url"]
        })
    }

    fn risk(&self) -> Risk {
        // Reading a public web page changes nothing on this machine, and research
        // is a core use of the product. It used to be gated like exec, which meant
        // every browsing task interrupted the user several times - the opposite of
        // a prompt people can weigh, because the fortieth prompt is clicked without
        // being read. The URL is still shown, in the tool card rather than in a
        // dialog, and the sandbox is what protects the machine.
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let raw = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolResult::err("web_fetch: missing required string field \"url\""),
        };
        let url = match check_url(raw) {
            Ok(u) => u,
            Err(e) => return ToolResult::err(e.to_string()),
        };

        let resp = match self.client.get(url.as_str()).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("fetch {url} failed: {e}")),
        };
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            return ToolResult::err(format!("HTTP {status} from {url}"));
        }

        // Stream with a cap so a 2 GB response cannot exhaust memory before we
        // notice the Content-Length is missing.
        let mut acc: Vec<u8> = Vec::new();
        let mut truncated_body = false;
        let mut stream = resp;
        loop {
            match stream.chunk().await {
                Ok(Some(bytes)) => {
                    if acc.len() + bytes.len() > self.max_bytes {
                        let room = self.max_bytes - acc.len();
                        acc.extend_from_slice(&bytes[..room.min(bytes.len())]);
                        truncated_body = true;
                        break;
                    }
                    acc.extend_from_slice(&bytes);
                }
                Ok(None) => break,
                Err(e) => return ToolResult::err(format!("read failed: {e}")),
            }
        }

        let text = if ctype.contains("html") {
            let title = html_title(&String::from_utf8_lossy(&acc));
            let body = html_to_text(&String::from_utf8_lossy(&acc));
            format!("title: {title}\nurl: {url}\n\n{body}")
        } else if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") {
            String::from_utf8_lossy(&acc).into_owned()
        } else {
            return ToolResult::err(format!(
                "{ctype} is not text; web_fetch returned {} bytes of it and did not parse it",
                acc.len()
            ));
        };

        let mut res = ToolResult::ok(clamp_text(&text, ctx.max_output_bytes));
        res.truncated = truncated_body || text.len() > ctx.max_output_bytes;
        res
    }
}

// ---------------------------------------------------------------------------
// HTML -> text
// ---------------------------------------------------------------------------

/// Pull the <title> out, for a readable tool card.
/// Download a file verbatim into the workspace.
///
/// `web_fetch` is a *reader*: it strips markup and clamps to a few tens of
/// kilobytes because its output goes into the prompt. That is the right shape
/// for reading a page and the wrong shape for obtaining a file, and the two
/// must not be conflated: a truncated docs dump fed back into the context costs
/// real prefill time on a local model and is useless afterwards. This writes
/// bytes to disk and reports only the path, size and type.
pub struct DownloadTool {
    pub client: reqwest::Client,
    pub max_bytes: u64,
}

impl Default for DownloadTool {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(UA)
                .timeout(std::time::Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .expect("reqwest client"),
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[async_trait]
impl Tool for DownloadTool {
    fn name(&self) -> &str {
        "download_file"
    }

    fn description(&self) -> &str {
        "Download a URL and save it verbatim into the workspace. Use this instead \
         of web_fetch when the goal is to obtain a file (docs dump, PDF, archive, \
         CSV, JSON blob) rather than to read a page: web_fetch strips markup and \
         truncates, this preserves bytes. Read the result back with read_file. \
         Returns the saved path, byte count and content type."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http(s) URL to download" },
                "path": {
                    "type": "string",
                    "description": "Workspace-relative destination. Defaults to the URL's file name."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace the destination if it already exists. Default false."
                }
            },
            "required": ["url"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Workspace
    }

    /// Fetching is harmless; *putting a file on disk over one that already
    /// exists* is not, and neither is writing outside the workspace. Same split
    /// as write_file: creating is the job, replacing needs a yes.
    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string())
            .unwrap_or_default();
        if raw.is_empty() {
            // No path given means "the URL's file name, inside the workspace",
            // which confine will resolve; be conservative and treat it as a write.
            return CallRisk::at(Risk::Workspace);
        }
        let resolved = super::path::lexical_resolve(&ctx.workspace, &raw);
        if resolved != super::path::lexical_resolve(&ctx.workspace, ".")
            && !resolved.starts_with(super::path::lexical_resolve(&ctx.workspace, "."))
        {
            return CallRisk {
                level: Risk::Sensitive,
                escapes: vec![resolved.display().to_string()],
            };
        }
        if resolved.exists() {
            CallRisk::at(Risk::Workspace)
        } else {
            CallRisk::harmless()
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let Some(raw) = args.get("url").and_then(|v| v.as_str()) else {
            return ToolResult::err("download_file needs a `url` string");
        };
        let url = match check_url(raw) {
            Ok(u) => u,
            Err(e) => return ToolResult::err(e.to_string()),
        };

        let requested = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => default_filename(&url),
        };
        let dest = match super::path::confine_permitted(&ctx.workspace, &requested, &ctx.permits) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if dest.exists() && !args.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false) {
            return ToolResult::err(format!(
                "{} already exists. Pass overwrite: true to replace it, or choose another path.",
                super::path::display_rel(&ctx.workspace, &dest)
            ));
        }

        let resp = match self.client.get(url.clone()).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("request failed: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            return ToolResult::err(format!("server returned {status} for {url}"));
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        // Write to a sibling `.part` and rename. A download that dies halfway
        // must not leave a half-file that looks like a completed one.
        let mut part = dest.clone();
        part.set_extension(format!(
            "{}.part",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("file")
        ));
        if let Some(parent) = dest.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
            }
        }

        let mut written: u64 = 0;
        let mut truncated = false;
        // reqwest and tokio::fs produce different error types; the caller only
        // needs a sentence, so collapse both here rather than at every `?`.
        let write = async {
            let mut fh = tokio::fs::File::create(&part)
                .await
                .map_err(|e| e.to_string())?;
            let mut stream = resp;
            loop {
                let chunk = stream.chunk().await.map_err(|e| e.to_string())?;
                let Some(chunk) = chunk else { break };
                if written + chunk.len() as u64 > self.max_bytes {
                    truncated = true;
                    break;
                }
                fh.write_all(&chunk).await.map_err(|e| e.to_string())?;
                written += chunk.len() as u64;
            }
            fh.flush().await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        }
        .await;
        if let Err(e) = write {
            let _ = tokio::fs::remove_file(&part).await;
            return ToolResult::err(format!("write failed: {e}"));
        }
        if truncated {
            let _ = tokio::fs::remove_file(&part).await;
            return ToolResult::err(format!(
                "response exceeds the {} byte download limit; nothing was saved",
                self.max_bytes
            ));
        }
        if let Err(e) = tokio::fs::rename(&part, &dest).await {
            let _ = tokio::fs::remove_file(&part).await;
            return ToolResult::err(format!("could not finalise the file: {e}"));
        }

        let rel = super::path::display_rel(&ctx.workspace, &dest);
        let file = OutputFile {
            path: rel.clone(),
            name: dest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel.clone()),
            mime,
            bytes: written,
            // A download is a deliverable whatever it is named: the name came from
            // a URL or the model's choice, so extension-guessing is the weaker fact
            // here and the act of fetching is the stronger one.
            kind: super::FileKind::Artifact,
        };
        ToolResult::ok(format!(
            "saved {} to {} ({})",
            human_bytes(written),
            rel,
            file.mime
        ))
        .with_file(file)
    }
}

/// Turn a URL into a safe workspace-relative file name. Query strings, leading
/// dots and path separators all have to go: the name comes from the network.
fn default_filename(url: &url::Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
        .unwrap_or("");
    let cleaned: String = last
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        url.domain().unwrap_or("download").to_string() + ".download"
    } else {
        cleaned
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} kB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

pub fn html_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else { return String::new() };
    let Some(gt) = lower[start..].find('>') else { return String::new() };
    let content_start = start + gt + 1;
    let Some(end) = lower[content_start..].find("</title>") else { return String::new() };
    decode_entities(html[content_start..content_start + end].trim())
}

/// Strip markup into readable text.
///
/// Not a parser: it builds no tree, executes nothing, and has no backtracking
/// anywhere, so malformed markup cannot make it blow up. All slicing happens at
/// byte offsets known to be ASCII boundaries.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Drop the contents of these entirely.
            let mut dropped = false;
            for tag in ["script", "style", "textarea", "noscript", "svg"] {
                if ci_starts_with(&html[i..], &format!("<{tag}")) {
                    let close = format!("</{tag}");
                    match ci_find(&html[i..], &close) {
                        Some(end) => {
                            let tail = &html[i + end..];
                            let gt = tail.find('>').map(|g| end + g + 1).unwrap_or(end);
                            i += gt;
                        }
                        // Unterminated: everything after this is inside the
                        // dropped block, so stop rather than emit its contents.
                        None => return finish_text(out),
                    }
                    dropped = true;
                    break;
                }
            }
            if dropped {
                continue;
            }
            // Ordinary tag: skip to '>', emitting a break for block elements.
            let Some(gt) = html[i..].find('>') else {
                break;
            };
            let name = ascii_ident(&html[i + 1..i + gt]);
            if matches!(
                name.as_str(),
                "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                    | "section" | "article" | "header" | "footer" | "table" | "ul" | "ol" | "pre"
                    | "blockquote" | "hr" | "dd" | "dt"
            ) {
                out.push('\n');
            }
            i += gt + 1;
            continue;
        }

        // Entity? Bounded scan; never slices past the end or mid-char.
        if bytes[i] == b'&' {
            let tail = &html[i..];
            if let Some(semi) = tail.find(';').filter(|s| *s <= 10) {
                if let Some(rep) = entity(&tail[..semi]) {
                    out.push_str(&rep);
                    i += semi + 1;
                    continue;
                }
            }
        }

        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    finish_text(out)
}

/// Collapse whitespace runs while keeping paragraph breaks.
fn finish_text(out: String) -> String {
    let mut cleaned = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            continue;
        }
        if blank_run > 0 && !cleaned.is_empty() {
            cleaned.push_str(if blank_run > 1 { "\n\n" } else { "\n" });
        } else if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        blank_run = 0;
        let mut prev_space = false;
        for ch in t.chars() {
            if ch == ' ' || ch == '\t' {
                if !prev_space {
                    cleaned.push(' ');
                }
                prev_space = true;
            } else {
                cleaned.push(ch);
                prev_space = false;
            }
        }
    }
    cleaned.trim().to_string()
}

fn ci_starts_with(hay: &str, needle: &str) -> bool {
    hay.len() >= needle.len()
        && hay.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// Case-insensitive substring search returning a byte offset.
///
/// The scan runs over bytes, not over `&hay[i..]`. Slicing at every byte offset
/// panics the moment the haystack contains a multi-byte character, because most
/// offsets land inside one. This crashed the app on a real page: the scan for
/// `</script>` walked into a `▾` and took the whole process with it.
///
/// The returned offset is still a char boundary: every needle here is ASCII, so
/// a match cannot begin on a UTF-8 continuation byte.
fn ci_find(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let hb = hay.as_bytes();
    let nb = needle.as_bytes();
    (0..=hb.len() - nb.len()).find(|&i| hb[i..i + nb.len()].eq_ignore_ascii_case(nb))
}

/// Leading run of ASCII alphanumerics: the tag name.
fn ascii_ident(s: &str) -> String {
    s.chars().take_while(|c| c.is_ascii_alphanumeric()).collect()
}

fn entity(name: &str) -> Option<String> {
    // Numeric: &#39; and &#x27; both appear constantly in real feeds and search
    // snippets, so named-only decoding leaves visible garbage in the output.
    if let Some(digits) = name.strip_prefix("&#") {
        let (radix, digits) = match digits.strip_prefix('x').or_else(|| digits.strip_prefix('X')) {
            Some(hex) => (16, hex),
            None => (10, digits),
        };
        let cp = u32::from_str_radix(digits, radix).ok()?;
        return char::from_u32(cp).map(|c| c.to_string());
    }
    Some(match name {
        "&amp" => "&",
        "&lt" => "<",
        "&gt" => ">",
        "&quot" => "\"",
        "&#39" | "&apos" => "'",
        "&nbsp" => " ",
        "&mdash" | "&ndash" => "-",
        "&hellip" => "...",
        "&copy" => "(c)",
        "&rsquo" | "&lsquo" => "'",
        "&rdquo" | "&ldquo" => "\"",
        _ => return None,
    }
    .to_string())
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        if let Some(semi) = after.find(';') {
            if let Some(rep) = entity(&after[..semi]) {
                out.push_str(&rep);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

fn clamp_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[... truncated at {cut} of {} bytes]", &s[..cut], s.len())
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Which backend to use. SearXNG has a real JSON API and is the right answer for
/// repeated searching; DuckDuckGo's HTML endpoint is the keyless fallback that
/// starts returning bot challenges after a handful of queries. `Auto` means
/// "Cwoffee runs its own SearXNG, and says so if it cannot".
#[derive(Default, Debug, Clone)]
pub enum SearchBackend {
    SearXNG { base: String },
    #[default]
    DuckDuckGo,
    Auto(crate::tools::searxng::AutoSearXNG),
}

impl SearchBackend {
    /// One line for logs and the settings card. Never a struct dump: this is what
    /// a person reads to know which search they are getting.
    pub fn describe(&self) -> String {
        match self {
            SearchBackend::SearXNG { base } => {
                if base.trim().is_empty() {
                    "SearXNG (no URL configured)".into()
                } else {
                    format!("SearXNG at {}", base.trim_end_matches('/'))
                }
            }
            SearchBackend::DuckDuckGo => "DuckDuckGo (keyless HTML endpoint)".into(),
            SearchBackend::Auto(a) => format!("SearXNG managed by Cwoffee on port {}", a.port),
        }
    }

    /// The spelling the settings UI sends and the config file uses, so there is
    /// one vocabulary rather than three.
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchBackend::SearXNG { .. } => "searxng",
            SearchBackend::DuckDuckGo => "duckduckgo",
            SearchBackend::Auto(_) => "auto",
        }
    }
}

pub struct WebSearchTool {
    pub client: reqwest::Client,
    /// Interior mutability, not a rebuild: the tool's name, description and schema
    /// are prompt bytes, and switching providers must not change them. The order of
    /// the tool list is pinned elsewhere for the same reason.
    pub backend: std::sync::Mutex<SearchBackend>,
    pub max_results: usize,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        let base = std::env::var("CWOFFEE_SEARXNG_URL").ok().filter(|s| !s.is_empty());
        Self {
            client: reqwest::Client::builder()
                .user_agent(UA)
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            backend: std::sync::Mutex::new(
                base.map(|base| SearchBackend::SearXNG { base })
                    .unwrap_or(SearchBackend::DuckDuckGo),
            ),
            max_results: 8,
        }
    }
}

impl WebSearchTool {
    pub fn from_backend(backend: SearchBackend) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(UA)
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            backend: std::sync::Mutex::new(backend),
            max_results: 8,
        }
    }

    pub fn backend(&self) -> SearchBackend {
        self.backend.lock().map(|b| b.clone()).unwrap_or(SearchBackend::DuckDuckGo)
    }

    pub fn set_backend(&self, b: SearchBackend) -> SearchBackend {
        let mut guard = self.backend.lock().expect("search backend mutex");
        std::mem::replace(&mut *guard, b)
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns titles, URLs and snippets. Follow up with \
         web_fetch on the most promising URL to read it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" }
            },
            "required": ["query"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let q = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q.trim().to_string(),
            None => return ToolResult::err("web_search: missing required string field \"query\""),
        };
        if q.is_empty() {
            return ToolResult::err("web_search: query is empty");
        }

        // The mutex is released before any await: a lock held across `.await` is
        // how a search that hangs ends up blocking the settings card too.
        let (note, results) = self.search_with(&q).await;
        let results = match results {
            Ok(r) => r,
            Err(e) => return ToolResult::err(e),
        };
        if results.is_empty() {
            return ToolResult::ok(match note {
                Some(n) => format!("{n}\n\nno results"),
                None => "no results".into(),
            });
        }
        let body = results
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
            .collect::<Vec<_>>()
            .join("\n\n");
        let body = match note {
            // The note leads, so the model cannot report "searched the web" without
            // knowing which search answered.
            Some(n) => format!("{n}\n\n{body}"),
            None => body,
        };
        let mut res = ToolResult::ok(clamp_text(&body, ctx.max_output_bytes));
        res.truncated = body.len() > ctx.max_output_bytes;
        res
    }
}

impl WebSearchTool {
    /// One query, one answer, plus a note when the answer did not come from the
    /// backend that was configured. Shared by the tool and by the settings panel so
    /// the two can never disagree about what a search would do.
    pub async fn search_with(
        &self,
        q: &str,
    ) -> (Option<String>, Result<Vec<SearchResult>, String>) {
        let backend = self.backend();
        match &backend {
            SearchBackend::SearXNG { base } => {
                if base.trim().is_empty() {
                    // Configured "searxng" with no URL: name the missing key rather
                    // than fetching an empty host and reporting a network error no
                    // one can act on.
                    return (
                        None,
                        Err(
                            "web_search: backend is \"searxng\" but no URL is set; add \
                             searxng_url under [search], or set backend = \"auto\" to \
                             have Cwoffee run one"
                                .into(),
                        ),
                    );
                }
                (None, self.searxng(base, q).await.map_err(|e| e.to_string()))
            }
            SearchBackend::DuckDuckGo => (None, self.duckduckgo(q).await.map_err(|e| e.to_string())),
            SearchBackend::Auto(auto) => match auto.ensure_up().await {
                Ok(()) => match self.searxng(&auto.base(), q).await {
                    Ok(r) => {
                        auto.touch();
                        (None, Ok(r))
                    }
                    Err(e) => {
                        // The instance answered the health check but not the search
                        // (JSON off, engines all down). Fall back, and tell the model
                        // which search it is actually reading.
                        (
                            Some(format!(
                                "note: the SearXNG instance on {} answered but not for this \
                                 search ({e}); these results came from DuckDuckGo instead",
                                auto.base()
                            )),
                            self.duckduckgo(q).await.map_err(|e| e.to_string()),
                        )
                    }
                },
                Err(e) => (
                    Some(format!(
                        "note: Cwoffee could not start its own SearXNG ({e}); these results \
                         came from DuckDuckGo, which starts answering with a bot challenge \
                         after a handful of queries"
                    )),
                    self.duckduckgo(q).await.map_err(|e| e.to_string()),
                ),
            },
        }
    }

    async fn searxng(&self, base: &str, q: &str) -> Result<Vec<SearchResult>, WebError> {
        let url = format!(
            "{}/search?format=json&q={}",
            base.trim_end_matches('/'),
            urlencoded(q)
        );
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(WebError::Http(resp.error_for_status().unwrap_err()));
        }
        // SearXNG only serves JSON if the instance has enabled it.
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !ctype.contains("json") {
            return Err(WebError::Unsupported(format!(
                "SearXNG at {base} did not return JSON (content-type {ctype:?}); \
                 enable format=json in settings.yml, or unset CWOFFEE_SEARXNG_URL to \
                 fall back to DuckDuckGo"
            )));
        }
        #[derive(Deserialize)]
        struct SxResp {
            #[serde(default)]
            results: Vec<SxResult>,
        }
        #[derive(Deserialize)]
        struct SxResult {
            #[serde(default)]
            title: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            content: String,
        }
        let parsed: SxResp = resp.json().await?;
        Ok(parsed
            .results
            .into_iter()
            .take(self.max_results)
            .map(|r| SearchResult {
                // SearXNG returns markup in title as well as content; decoding
                // only one of them leaves "&amp;" visible in the results list.
                title: html_to_text(&r.title),
                url: r.url,
                snippet: html_to_text(&r.content),
            })
            .collect())
    }

    async fn duckduckgo(&self, q: &str) -> Result<Vec<SearchResult>, WebError> {
        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(q));
        let resp = self.client.get(&url).send().await?;
        let status = resp.status();
        let html = resp.text().await?;

        // DDG's unofficial HTML endpoint answers bot-ish traffic with a challenge
        // page: HTTP 202 containing "anomaly"/"challenge", with no results in it.
        // Reporting that as "no results" is the worst possible outcome — the
        // model then tells the user nothing exists. Say what actually happened
        // and how to fix it.
        if status.as_u16() == 202 || is_ddg_challenge(&html) {
            return Err(WebError::Unsupported(
                "the DuckDuckGo HTML endpoint returned a bot challenge instead of \
                 results (HTTP 202). This is expected for automated traffic and may \
                 clear on retry. For reliable search, set CWOFFEE_SEARXNG_URL to a \
                 SearXNG instance (with format=json enabled in settings.yml)."
                    .into(),
            ));
        }
        if !status.is_success() {
            return Err(WebError::Unsupported(format!("DuckDuckGo returned HTTP {status}")));
        }
        Ok(parse_ddg_html(&html, self.max_results))
    }
}

/// True when the HTML is DDG's bot challenge rather than a results page.
pub fn is_ddg_challenge(html: &str) -> bool {
    html.contains("anomaly_modal") || html.contains("challenge-captcha") || html.contains("anomaly.js")
}

/// Best-effort parse of DuckDuckGo's HTML endpoint.
///
/// Brittle by nature: it is unofficial HTML with no contract. Kept separate and
/// fixture-tested so breakage is obvious and local.
pub fn parse_ddg_html(html: &str, max: usize) -> Vec<SearchResult> {
    let mut out = Vec::new();
    let mut rest = html;
    while out.len() < max {
        let Some(a) = rest.find("result__a") else { break };
        let Some(href_i) = rest[a..].find("href=\"") else { break };
        let href_start = a + href_i + 6;
        let Some(href_end) = rest[href_start..].find('"') else { break };
        let href = decode_entities(&rest[href_start..href_start + href_end]);

        let Some(gt) = rest[href_end + href_start..].find('>') else { break };
        let title_start = href_start + href_end + gt + 1;
        let Some(close) = rest[title_start..].find("</a>") else { break };
        let title = html_to_text(&rest[title_start..title_start + close]);

        // snippet, if present
        let mut snippet = String::new();
        if let Some(s) = rest[title_start..].find("result__snippet") {
            let sstart = title_start + s;
            if let Some(gt2) = rest[sstart..].find('>') {
                let bstart = sstart + gt2 + 1;
                if let Some(end) = rest[bstart..].find("</a>") {
                    snippet = html_to_text(&rest[bstart..bstart + end]);
                }
            }
        }

        // DDG wraps outbound links in a redirect: /l/?uddg=<encoded>
        let real = unwrap_ddg_redirect(&href).unwrap_or(href);
        out.push(SearchResult { title, url: real, snippet });
        rest = &rest[title_start + close..];
    }
    out
}

fn unwrap_ddg_redirect(href: &str) -> Option<String> {
    let marker = "uddg=";
    let i = href.find(marker)? + marker.len();
    let tail = &href[i..];
    let end = tail.find('&').unwrap_or(tail.len());
    percent_decode(&tail[..end])
}

fn urlencoded(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // An invalid escape is passed through rather than aborting the decode:
        // losing the rest of a URL is worse than keeping a literal '%'.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use tokio::sync::mpsc;

    fn sink() -> ExecSink {
        mpsc::unbounded_channel().0
    }
    fn ctx() -> ToolCtx {
        ToolCtx { workspace: ".".into(), max_output_bytes: 4096, ..Default::default() }
    }

    // ---- url policy -------------------------------------------------------

    // The crash that closed the app mid-session: a real page whose <script>
    // body contained a multi-byte character. The scan for the closing tag
    // sliced the string at every byte offset and panicked inside the tool,
    // which took the whole process down.
    #[test]
    fn multibyte_content_inside_a_dropped_block_does_not_panic() {
        let html = "<div>a</div><script>var x = '\u{25be}\u{25be}\u{25be}'; // \u{4e2d}\u{6587} </script><p>kept</p>";
        let text = html_to_text(html);
        assert!(text.contains("kept"), "{text}");
        assert!(!text.contains('\u{25be}'), "script content leaked: {text}");
    }

    #[test]
    fn multibyte_everywhere_does_not_panic() {
        let html = format!("\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}\u{25be}");
        let html = html.repeat(50) + "<STYLE>\u{1f600}\u{1f600}</style>tail";
        let text = html_to_text(&html);
        assert!(text.ends_with("tail"), "{text}");
    }

    #[test]
    fn unterminated_script_with_multibyte_stops_cleanly() {
        let html = "before<script>\u{25be}\u{25be} never closed";
        let text = html_to_text(html);
        assert_eq!(text.trim(), "before", "{text}");
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        for u in [
            "file:///etc/passwd",
            "gopher://x/y",
            "ftp://x/y",
            "data:text/html,hi",
            "javascript:alert(1)",
            "/not-a-url",
        ] {
            let r = check_url(u);
            assert!(r.is_err(), "{u} was accepted");
        }
    }

    #[test]
    fn metadata_and_link_local_endpoints_are_rejected() {
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/x",
            "http://[fe80::1]/x",
            "http://0.0.0.0/x",
        ] {
            let r = check_url(u);
            assert!(r.is_err(), "{u} was accepted");
        }
    }

    #[test]
    fn ordinary_urls_pass() {
        for u in ["https://example.com", "http://localhost:5173/x", "https://a.b/c?d=e#f"] {
            assert!(check_url(u).is_ok(), "{u} rejected: {:?}", check_url(u).err());
        }
    }

    // ---- html -> text -----------------------------------------------------

    #[test]
    fn strips_scripts_and_styles_entirely() {
        let html = "<p>keep</p><script>var evil = 'gone';</script><style>.x{}</style><p>also keep</p>";
        let t = html_to_text(html);
        assert!(t.contains("keep"));
        assert!(!t.contains("evil"), "script body leaked: {t:?}");
        assert!(!t.contains(".x{}"), "style body leaked: {t:?}");
    }

    #[test]
    fn block_tags_become_line_breaks() {
        let t = html_to_text("<h1>Title</h1><p>one</p><p>two</p><ul><li>a</li><li>b</li></ul>");
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines.len() >= 5, "expected line breaks, got {lines:?}");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(html_to_text("a &amp; b &lt;c&gt; &quot;d&quot;"), "a & b <c> \"d\"");
        // An entity at the very end must not slice past the end of the string.
        // This decoder requires the terminating ';': `&amp` with no semicolon is
        // left verbatim rather than guessed at, which is the safe reading.
        assert_eq!(html_to_text("tail &amp"), "tail &amp");
        assert_eq!(html_to_text("tail &amp;"), "tail &");
        assert_eq!(html_to_text("&"), "&");
        assert_eq!(html_to_text("&notanentityatall"), "&notanentityatall");
    }

    #[test]
    fn nested_and_broken_markup_does_not_panic() {
        for h in [
            "<p",
            "<>",
            "<<<>>>",
            "<div><span>text",
            "</p></p></p>",
            "&amp",
            "&#99999999;",
            "<script>",
            "<script>unterminated",
            "",
            "<a href=x>y</a".repeat(50).as_str(),
            "&".repeat(100).as_str(),
        ] {
            let _ = html_to_text(h);
        }
    }

    #[test]
    fn numeric_entities_are_decoded() {
        assert_eq!(html_to_text("a&#39;b"), "a'b");
        assert_eq!(html_to_text("a&#x27;b"), "a'b");
        assert_eq!(html_to_text("a&#X27;b"), "a'b");
        assert_eq!(html_to_text("caf&#233; caf&#xE9; caf&#xe9;"), "café café café");
        assert_eq!(html_to_text("&#128512;"), "😀");
        // The named table is deliberately small; an unrecognised named entity
        // passes through verbatim rather than being guessed at. pulldown-cmark
        // (M2) replaces both this and the tag stripper.
        assert_eq!(html_to_text("caf&eacute;"), "caf&eacute;");
        // out of range / invalid must not panic and must not invent a char
        assert_eq!(html_to_text("&#x110000;"), "&#x110000;");
        assert_eq!(html_to_text("&#;"), "&#;");
        assert_eq!(html_to_text("&#xZZ;"), "&#xZZ;");
    }

    #[test]
    fn title_extraction() {
        assert_eq!(html_title("<html><head><title> Hello &amp; Bye </title></head>"), "Hello & Bye");
        assert_eq!(html_title("<TITLE>up</TITLE>"), "up");
        assert_eq!(html_title("no title here"), "");
    }

    #[test]
    fn whitespace_is_collapsed_but_paragraphs_survive() {
        let t = html_to_text("<div>a</div>\n\n\n\n<div>b</div>");
        assert_eq!(t, "a\n\nb");
    }

    // ---- ddg parsing ------------------------------------------------------

    const DDG_FIXTURE: &str = r#"
    <div class="result">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//html.duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc">
          Example <b>Page</b> Title
        </a>
      </h2>
      <a class="result__snippet" href="//html.duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">
        A useful &amp; informative snippet.
      </a>
    </div>
    <div class="result">
      <h2 class="result__title">
        <a class="result__a" href="https://second.example.org/x">Second Result</a>
      </h2>
      <a class="result__snippet">Second snippet</a>
    </div>
    "#;

    #[test]
    fn ddg_fixture_yields_decoded_urls() {
        let rs = parse_ddg_html(DDG_FIXTURE, 8);
        assert_eq!(rs.len(), 2, "got {rs:?}");
        assert_eq!(rs[0].url, "https://example.com/page", "redirect not unwrapped");
        assert!(rs[0].title.contains("Example"), "title={:?}", rs[0].title);
        assert!(rs[0].snippet.contains("useful"), "snippet={:?}", rs[0].snippet);
        assert_eq!(rs[1].url, "https://second.example.org/x");
    }

    #[test]
    fn ddg_challenge_is_detected_not_reported_as_no_results() {
        // Real shape of the response: HTTP 202, a challenge script, zero results.
        let challenge = r#"<html><head><script src="/challenge/anomaly.js"></script></head>
            <body><div class="anomaly_modal">Please verify you are not a robot</div></body></html>"#;
        assert!(is_ddg_challenge(challenge), "challenge not detected");
        // an ordinary empty results page must NOT be treated as a challenge
        assert!(!is_ddg_challenge("<html><body>No results</body></html>"));
        assert!(!is_ddg_challenge(DDG_FIXTURE));
    }

    #[test]
    fn ddg_parser_survives_garbage_and_respects_max() {
        assert!(parse_ddg_html("no results here", 8).is_empty());
        assert!(parse_ddg_html("", 8).is_empty());
        assert!(parse_ddg_html("<a class=\"result__a\"", 8).is_empty());
        assert_eq!(parse_ddg_html(DDG_FIXTURE, 1).len(), 1);
    }

    #[test]
    fn percent_and_url_encoding_round_trip() {
        assert_eq!(urlencoded("hello world&x=1"), "hello+world%26x%3D1");
        assert_eq!(percent_decode("a%20b").unwrap(), "a b");
        assert_eq!(percent_decode("%C3%A9").unwrap(), "é");
        assert!(percent_decode("%ZZ").is_some()); // passes through, not a crash
    }

    #[test]
    fn clamp_respects_char_boundaries() {
        let s = "héllo wörld café".repeat(10);
        for max in [1, 3, 5, 7, 20, 100] {
            let c = clamp_text(&s, max);
            assert!(c.is_char_boundary(c.len().min(c.len())));
            assert!(!c.is_empty());
        }
        assert_eq!(clamp_text("short", 100), "short");
    }

    #[tokio::test]
    async fn missing_args_are_reported_not_panicked() {
        let t = WebFetchTool::default();
        let r = t.run(json!({}), &ctx(), &sink()).await;
        assert!(!r.ok && r.error.as_deref().unwrap().contains("url"));

        let s = WebSearchTool::default();
        let r = s.run(json!({"query": "   "}), &ctx(), &sink()).await;
        assert!(!r.ok && r.error.as_deref().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn fetch_refuses_file_scheme_without_touching_the_disk() {
        let t = WebFetchTool::default();
        let r = t.run(json!({"url": "file:///etc/passwd"}), &ctx(), &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("scheme"));
    }
}
