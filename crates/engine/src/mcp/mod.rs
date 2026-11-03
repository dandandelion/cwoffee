//! Third-party MCP servers, kept out of the prompt until the model asks for them.
//!
//! MCP is the one capability here that someone else writes. A server is a program cwoffee
//! spawns and talks to over stdio JSON-RPC; its tools can do anything that program can do,
//! which is everything on this machine. So the cost question and the safety question are the
//! same question, and both are answered by making third-party tools ride the seam that already
//! exists for cwoffee's own: declared **inactive** until `explore` names them, hydrated on
//! lookup, callable from the next call on.
//!
//! The cost is the reason it is lazy at all. Twelve first-party declarations cost ~12 KB of
//! every prompt (`scripts/gateway-prompt-shape.mjs` reads the bytes that leave the process);
//! a server with thirty tools can cost more than that on its own, in every turn, for a feature
//! the conversation may never use. What is always in the prompt is a catalog of names and one
//! line each — a few hundred bytes, and `CATALOG_BYTE_BUDGET` is the ceiling on it, asserted by
//! a test against a live server.
//!
//! Four rules hold the safety half:
//!
//! * A tool from a server is **never** `Risk::Harmless`. The server's own `readOnlyHint`
//!   annotation is the only downgrade we take, and it goes to `Workspace` — a third party's
//!   claim about its own behaviour is not evidence, and a tool that can call anything can write
//!   anything. No annotation ⇒ `Sensitive`, always asked.
//! * A session grant does not cover an MCP call. `CallRisk::per_call` says so to the gate: "yes
//!   to exec for this conversation" is about a tool whose reach is the workspace, and a server
//!   is not that.
//! * A path in the arguments that lands outside the workspace is an **escape**, which needs
//!   per-call approval in every mode but Auto, exactly like `exec`. A server cannot grant itself
//!   room cwoffee declined to give it.
//! * A dead server is a sentence, not a hang. Timeouts are `timeout_secs` from `[mcp]` and the
//!   app's existing `approval_timeout_secs`; there is no third number, because a magic timeout
//!   is a hang with a decimal point in it.
//!
//! Both cores mount the same catalog, and the difference between them is only how a call
//! reaches the servers. The Rust core holds the hub in the app's process. The pi core cannot:
//! its tool host is one process per call, so a server it spawned would restart on every call.
//! There the app still owns the hub, and the tool host asks it over a socket - see [`socket`],
//! and `Route` below, which is the one place those two roads to the same hub diverge.

pub mod socket;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

use crate::tools::{clamp, CallRisk, ExecSink, Risk, Tool, ToolCtx, ToolResult};

/// The ceiling on what the catalog may cost every prompt, in bytes. Named as a constant and
/// asserted in a test because "a few hundred bytes" is a hope, and the whole reason MCP is
/// lazy is that an unbounded catalog would cost more than the tools it stands in for.
pub const CATALOG_BYTE_BUDGET: usize = 1024;

/// One JSON-RPC round trip: handshake, `tools/list`, or a tool call. Shorter than exec's 120s
/// because a server that has not answered in 20 seconds is not thinking, it is stuck.
pub const DEFAULT_MCP_TIMEOUT: u64 = 20;

/// The protocol version we offer. Servers may answer with their own; `initialize` reports what
/// the server picked, and the catalog says it, because a tool list from a server speaking a
/// different dialect is a claim about compatibility nobody checked.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// One server to spawn. `command` + `args`, stdio — the shape MCP actually speaks over a local
/// pipe. A `url` (HTTP/SSE transport) is not supported, and saying so here rather than parsing
/// and ignoring it is deliberate: a key that parses and does nothing is how a person believes
/// they have a feature.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerCfg {
    /// The program to run. Empty means "not configured", which is reported, not spawned.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// `false` keeps a server in the file without starting it: the honest way to park one.
    pub enabled: bool,
    /// Extra environment for this server only. A server often needs a token, and putting it in
    /// the process-wide environment would hand it to every other tool call.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Default for McpServerCfg {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            enabled: true,
            env: BTreeMap::new(),
        }
    }
}

impl McpServerCfg {
    /// Read the environment a form sends: `KEY=value` lines, in order.
    ///
    /// Refused rather than guessed. A line with no `=` is a half-typed key, and a key containing
    /// `=` (which the split would put in the value) hands a token to a name nobody wrote down.
    /// Blank lines are ignored, because a text area always seems to end with one.
    pub fn env_from_lines(lines: &[String]) -> Result<BTreeMap<String, String>, String> {
        let mut out = BTreeMap::new();
        for l in lines {
            let l = l.trim();
            if l.is_empty() {
                continue;
            }
            let Some((k, v)) = l.split_once('=') else {
                return Err(format!(
                    "{l:?} is not KEY=value: an environment entry needs the equals sign between the name and the value"
                ));
            };
            let k = k.trim();
            if k.is_empty() || k.contains(' ') {
                return Err(format!("{l:?}: the part before `=` has to be a name with no spaces in it"));
            }
            out.insert(k.to_string(), v.trim().to_string());
        }
        Ok(out)
    }

    /// Read the environment a form sends, keeping the values the form was never shown.
    ///
    /// A settings row cannot echo a token back — that puts a secret in the next screenshot somebody
    /// posts — so the edit form lists the keys it found and lets a person type over the ones they mean
    /// to change. `KEY` alone therefore means "this one stays as it is"; only `KEY=value` writes a
    /// value, and an empty one is a deliberate empty (`KEY=`).
    ///
    /// Without that rule, opening a server to change its command would blank every token it had: the
    /// form prefills from a row that holds names only, and the write would report a successful edit
    /// that broke the program. A name the server does not already have is refused rather than created
    /// empty, because the person who typed it meant to give a value.
    pub fn env_keep_lines(
        existing: &BTreeMap<String, String>,
        lines: &[String],
    ) -> Result<BTreeMap<String, String>, String> {
        let mut out = BTreeMap::new();
        for l in lines {
            let l = l.trim();
            if l.is_empty() {
                continue;
            }
            let Some((k, v)) = l.split_once('=') else {
                let k = l;
                if k.contains(' ') || k.is_empty() {
                    return Err(format!("{l:?}: an environment entry is a name, or NAME=value"));
                }
                match existing.get(k) {
                    Some(v) => {
                        out.insert(k.to_string(), v.clone());
                    }
                    None => {
                        return Err(format!(
                            "{k:?} is not one of this server's keys, so there is no value to keep: write {k}=value to give it one"
                        ))
                    }
                }
                continue;
            };
            let k = k.trim();
            if k.is_empty() || k.contains(' ') {
                return Err(format!("{l:?}: the part before `=` has to be a name with no spaces in it"));
            }
            out.insert(k.to_string(), v.trim().to_string());
        }
        Ok(out)
    }

    /// Read the arguments a form sends: one per line.
    ///
    /// One per line rather than one shell-looking line, because there is no shell here: nothing
    /// splits quotes, and a form that pretended otherwise would quietly turn `--dir "/my docs"`
    /// into two arguments. A person who needs a space inside one argument gets one line.
    pub fn args_from_lines(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(|a| a.to_string())
            .collect()
    }
}

/// `[mcp]`. Absent means no servers, which is the default: nothing spawns, nothing costs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpCfg {
    /// `false` starts no server even when the file lists some. The section-level switch exists
    /// because `[mcp]` is about third-party programs running on this machine, and a person who
    /// turned them off expects that to be a fact about this start, not a suggestion.
    pub enabled: bool,
    /// Seconds one JSON-RPC round trip may take.
    pub timeout_secs: u64,
    /// Filled from the top-level `approval_timeout_secs` by `Config::mcp_cfg`, so an MCP
    /// approval waits exactly as long as every other approval in this app.
    pub approval_timeout_secs: Option<u64>,
    /// Keyed by server name; the name becomes part of every tool name it offers.
    pub servers: BTreeMap<String, McpServerCfg>,
}

impl Default for McpCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: DEFAULT_MCP_TIMEOUT,
            approval_timeout_secs: None,
            servers: BTreeMap::new(),
        }
    }
}

impl McpCfg {
    pub fn has_servers(&self) -> bool {
        self.enabled && self.servers.values().any(|s| s.enabled && !s.command.trim().is_empty())
    }

    /// A server name has to survive being embedded in a tool name that a provider will accept
    /// and a person will read, so it is checked rather than sanitized: `mcp__My Server__run` is
    /// a name no provider guarantees, and quietly rewriting a name is how a model calls a tool
    /// nobody typed.
    pub fn bad_names(&self) -> Vec<&str> {
        self.servers.keys().filter(|k| !is_server_name(k)).map(|k| k.as_str()).collect()
    }
}

pub fn is_server_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes.len() > 32 {
        return false;
    }
    if !matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9') {
        return false;
    }
    if s.contains("__") {
        // The declared name is `mcp__<server>__<tool>`. A server whose own name contains the
        // separator makes that ambiguous to split, and a name that carries our marker can read
        // like a cwoffee tool to a reader skimming the approval card.
        return false;
    }
    bytes
        .iter()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// What one server offered at startup. The schema is kept because a discovered tool has to be
/// callable with the arguments it described; what is *not* kept in the prompt is that schema,
/// until the model asks for the tool — which is the whole economy of this design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub server: String,
    /// The name the server gave it.
    pub tool: String,
    /// The name cwoffee declares: `mcp__<server>__<tool>`. Namespaced because two servers both
    /// offering `search` would otherwise answer with one tool and a mystery.
    pub declared: String,
    pub description: String,
    pub schema: Value,
    /// The server's own `readOnlyHint`, kept as what it is: a claim, not a fact.
    pub says_read_only: bool,
}

/// What the risk of a server's tool is, and why. Kept in one function so the rule "never
/// Harmless" has exactly one place to be true.
pub fn risk_of(info: &ToolInfo) -> Risk {
    if info.says_read_only {
        Risk::Workspace
    } else {
        Risk::Sensitive
    }
}

/// The server a declared tool name belongs to, or `None` if the name is not an MCP one.
///
/// Kept next to the naming rule rather than duplicated in the approval gate: the namespace is
/// `mcp__<server>__<tool>`, and a second parser that drifts from the first would label somebody's
/// own tool as third-party or miss a third-party tool entirely.
pub fn server_of(declared: &str) -> Option<&str> {
    let rest = declared.strip_prefix("mcp__")?;
    let server = rest.split("__").next()?;
    if server.is_empty() {
        None
    } else {
        Some(server)
    }
}

/// The fixed sentence every catalog opens with, and the tag that closes it. Both are constants,
/// so the budget can be stated as "what scales with the number of servers" without pretending the
/// explanation is free.
const CATALOG_CLOSE: &str = "\n</mcp>";

pub fn catalog_head() -> &'static str {
    "<mcp>\nThird-party MCP servers. Their tools are NOT callable yet: ask `explore` with need \
     \"mcp: <what you need>\" to name the ones you want, and they are callable from your next call \
     on. Their arguments arrive with that answer, so ask again with with_arguments before calling \
     one.\n"
}

/// What a server looks like to a reader: alive with N tools, parked, or dead with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Live,
    Disabled,
    Dead(String),
}

impl Status {
    pub fn says(&self) -> String {
        match self {
            Status::Live => "live".to_string(),
            Status::Disabled => "off in config".to_string(),
            Status::Dead(why) => format!("dead: {why}"),
        }
    }
}

/// One running server: the child, its pipes, and what it listed.
struct Live {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    /// What the server said it speaks. Kept for the status line, not for branching logic: we
    /// send one version and a server that needs another says so when `tools/list` fails.
    protocol: String,
}

struct Server {
    cfg: McpServerCfg,
    status: Status,
    tools: Vec<ToolInfo>,
    live: Option<Live>,
    /// Restarts attempted after a death. Bounded so a server that dies on every call does not
    /// turn a turn into a respawn loop.
    restarts: usize,
}

/// One server, from the config keys to a live child or a reason there is not one.
///
/// Split out of `Hub::start` because `Hub::apply` needs the identical answer: a server enabled
/// from the settings page must be started the same way, refused the same way for the same reasons,
/// and land in the same status vocabulary. Two copies of that logic is how the boot log and the
/// settings page start disagreeing about one server.
async fn boot_server(
    name: &str,
    scfg: &McpServerCfg,
    master_on: bool,
    timeout: std::time::Duration,
) -> Server {
    // Why this server will not be talked to, if it will not — written as a reason rather
    // than as a pre-set status. A status that starts out dead and is only replaced after
    // a successful connect makes every server dead on its way in, and "did this server
    // start?" is the one question the catalog exists to answer.
    let refusal = if scfg.command.trim().is_empty() {
        Some("no command".to_string())
    } else if !is_server_name(name) {
        Some(format!(
            "the name {name:?} is not usable: server names are 2-32 characters of a-z, \
             0-9, '_' or '-', starting with a letter or digit, and never containing a \
             double underscore — that is cwoffee's namespace marker"
        ))
    } else {
        None
    };
    let mut srv = Server {
        cfg: scfg.clone(),
        status: if !master_on || !scfg.enabled {
            Status::Disabled
        } else if let Some(why) = &refusal {
            Status::Dead(why.clone())
        } else {
            // Optimistic until `connect` answers; every path below replaces it.
            Status::Live
        },
        tools: Vec::new(),
        live: None,
        restarts: 0,
    };
    if refusal.is_some() || !master_on || !scfg.enabled {
        // Nothing to spawn: with no command, a name that cannot be a tool name, or a
        // switch turned off, `tools` stays empty so the catalog cannot offer a ghost.
    } else {
        match connect(name, &srv.cfg, timeout).await {
            Ok((live, tools, protocol)) => {
                srv.live = Some(Live { child: live.child, stdin: live.stdin, stdout: live.stdout, next_id: live.next_id, protocol });
                srv.tools = tools;
                srv.status = Status::Live;
            }
            Err(Dead(why)) => srv.status = Status::Dead(why),
        }
    }
    srv
}

/// Close one server's child. Shared by `apply` (a switch off, or a name the file dropped) and
/// `shutdown` (the app is going away), because a dropped child that nobody closed keeps a pipe
/// open and a process running for the rest of the session.
async fn close_server(s: &mut Server) {
    if let Some(live) = s.live.as_mut() {
        let _ = live.stdin.shutdown().await;
        match tokio::time::timeout(std::time::Duration::from_secs(2), live.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                live.child.start_kill().ok();
                eprintln!("[cwoffee] mcp: a server did not exit within 2s and was killed");
            }
        }
    }
    s.live = None;
    s.tools.clear();
    s.status = Status::Disabled;
}

/// Reconcile the running hub with the config and say what came of it, in the words a status row
/// has room for.
///
/// One function rather than a sentence assembled in each transport: "what did that switch do to the
/// process" must not mean two things depending on whether the click came from the desktop or a
/// re-read after a hand edit.
pub async fn reconcile(hub: &Arc<Mutex<Hub>>, cfg: &McpCfg) -> String {
    let says = {
        let mut h = hub.lock().await;
        h.apply(cfg).await
    };
    if says.is_empty() {
        return "nothing is listed under [mcp.servers], so nothing is running and nothing is offered".to_string();
    }
    let live = says.iter().filter(|(_, s)| s.starts_with("live")).count();
    format!(
        "and now: {} — {live} of {} answering",
        says.iter()
            .map(|(n, s)| format!("{n}: {s}"))
            .collect::<Vec<_>>()
            .join("; "),
        says.len()
    )
}

/// Whether this start's server list came from the environment rather than the file.
///
/// `CWOFFEE_MCP_SERVERS` replaces the file's `[mcp.servers]` wholesale. A settings page that wrote
/// the file in that situation would report success and change nothing the next boot reads, which is
/// the shape this project keeps calling a lie — so a write refuses and names the variable instead.
pub fn listed_from_env() -> bool {
    std::env::var("CWOFFEE_MCP_SERVERS")
        .ok()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// The status payload, assembled in one place.
///
/// Both transports used to build this by hand and had already drifted by one field each when the
/// theme payload test caught the pattern (`parsed` is not the same as `published`). The desktop and
/// a phone tab now read the same object, and a cargo test reads the wire rather than the struct.
pub fn status_payload(
    hub: Option<&Hub>,
    cfg: &McpCfg,
    core: &str,
    config_path: &std::path::Path,
    route: &str,
    file: &crate::config::McpFileState,
    writable: bool,
) -> Value {
    let servers: Vec<Value> = match hub {
        Some(h) => h
            .servers_named()
            .iter()
            .map(|(n, s)| {
                let tools = h.server_tools(n);
                serde_json::json!({
                    "name": n,
                    "says": s,
                    "tools": tools
                        .iter()
                        .map(|(d, desc)| serde_json::json!({ "declared": d, "description": desc }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect(),
        None => Vec::new(),
    };
    let (block, body, lead) = hub.map(|h| h.catalog_parts()).unwrap_or((0, 0, 0));
    let listed: Vec<Value> = file
        .servers
        .iter()
        .map(|(n, c)| {
            serde_json::json!({
                "name": n,
                "command": c.command,
                "args": c.args,
                "enabled": c.enabled,
                "env": c.env.keys().cloned().collect::<Vec<_>>(),
                "running": hub.map(|h| matches!(h.status(n), Some(Status::Live))).unwrap_or(false),
                "says": hub.and_then(|h| h.status(n)).map(|s| s.says()).unwrap_or_else(|| "not started in this app".to_string()),
                "tools": hub.map(|h| h.server_tools(n).len()).unwrap_or(0),
            })
        })
        .collect();
    serde_json::json!({
        "core": core,
        "mounted": hub.is_some(),
        // Two numbers, because they answer two different questions: what the prompt pays in total,
        // and how much of that grows when somebody adds a server.
        "catalog_bytes": block,
        "catalog_body_bytes": body,
        "catalog_lead_bytes": lead,
        "catalog_budget": CATALOG_BYTE_BUDGET,
        "tools": hub.map(|h| h.catalog().len()).unwrap_or(0),
        "running": hub.map(|h| h.live_count()).unwrap_or(0),
        "servers": servers,
        // What this start resolved — the file *or* `CWOFFEE_MCP_SERVERS` — and what the file itself
        // holds. Both, because the person reading this page is asking about the file.
        "configured": cfg.servers.len(),
        "file_configured": file.servers.len(),
        "env_lists": listed_from_env(),
        // Whether *this* transport may change any of it. A phone tab reads the same page: the
        // servers run on the machine that started them, and a control there that wrote the desktop's
        // config would be a remote launch of somebody else's program.
        "writable": writable && !listed_from_env(),
        "config_path": config_path.display().to_string(),
        "route": route,
        // The file's own facts, kept apart from the hub's: what may be started, what is answering.
        "file": {
            "enabled": file.enabled,
            "timeout_secs": file.timeout_secs,
            "servers": listed,
            "problem": file.problem,
        },
        // Which servers exist is one fact and whether a call can reach them is another. The second
        // is answered from the config, not from a guess about the core: the pi core mounts the same
        // servers the Rust core does, so a `why` that named the core would be a stale claim about a
        // capability that now exists.
        "why": match hub {
            Some(h) if h.live_count() > 0 => String::new(),
            Some(h) if h.servers.is_empty() => "no [mcp] servers configured".to_string(),
            Some(_) if !cfg.enabled => "[mcp] enabled = false: nothing is started in this start".to_string(),
            Some(_) => "[mcp] lists servers and none is answering".to_string(),
            None if file.servers.is_empty() => "no [mcp] servers configured".to_string(),
            None => "[mcp] lists servers and none is mounted here".to_string(),
        },
    })
}

/// The catalog of third-party tools, and the only thing that talks to the servers.
///
/// Shared behind `Arc<Mutex<..>>` because a tool call runs on its own task (a panic in one tool
/// must not take the process with it) and because two MCP calls in one turn must not interleave
/// JSON-RPC frames on the same pipe.
pub struct Hub {
    timeout: std::time::Duration,
    servers: BTreeMap<String, Server>,
}

/// A server that cannot be talked to, with the reason a person can act on.
pub struct Dead(String);

impl Hub {
    /// Spawn every enabled server and read its tool list. Servers start before the first prompt
    /// is built, so the catalog in the env block is a fact about this start rather than a hope.
    ///
    /// One slow or broken server does not stop the others: it is recorded as dead with the
    /// reason, and the catalog says so. That is also why this is sequential — parallel spawns
    /// would make the first prompt wait for the slowest server instead of the sum of them, and
    /// a startup that hangs on someone else's program is not a startup.
    pub async fn start(cfg: &McpCfg) -> Hub {
        let timeout = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
        let mut servers = BTreeMap::new();
        for (name, scfg) in cfg.servers.iter() {
            let srv = boot_server(name, scfg, cfg.enabled, timeout).await;
            servers.insert(name.clone(), srv);
        }
        Hub { timeout, servers }
    }

    /// Make the running set match the config file: start what is new or switched on, stop what is
    /// switched off or gone, and leave a server that is unchanged and alive exactly as it is.
    ///
    /// This is what a settings write ends with, so "I pressed Enable" has one answer: the file says
    /// it and the process is now what the file says. A person who edits `config.toml` in an editor
    /// reaches the same place by pressing the page's re-read control.
    ///
    /// Returns `(name, what it says now)` for every server the file lists, plus the names that were
    /// dropped because the file no longer mentions them.
    pub async fn apply(&mut self, cfg: &McpCfg) -> Vec<(String, String)> {
        let timeout = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
        self.timeout = timeout;
        // Stop and forget what the file no longer lists. Doing this before starting anything keeps
        // a renamed server from holding its old child while the new one spawns.
        let gone: Vec<String> = self
            .servers
            .keys()
            .filter(|n| !cfg.servers.contains_key(*n))
            .cloned()
            .collect();
        for name in &gone {
            if let Some(mut s) = self.servers.remove(name) {
                close_server(&mut s).await;
            }
        }
        let mut says = Vec::new();
        for (name, scfg) in cfg.servers.iter() {
            let keep = self
                .servers
                .get(name)
                .map(|s| s.cfg == *scfg && s.live.is_some() && cfg.enabled && scfg.enabled)
                .unwrap_or(false);
            if keep {
                // Nothing moved: the child is alive and the keys it was started with are the keys
                // the file holds. Restarting it here would drop a session somebody's already using.
                says.push((
                    name.clone(),
                    format!("{} ({} tool(s))", self.servers[name].status.says(), self.servers[name].tools.len()),
                ));
                continue;
            }
            let mut fresh = boot_server(name, scfg, cfg.enabled, timeout).await;
            // A server that is being re-read after a death keeps its restart count, so `call`'s
            // bound still means "twice per conversation" rather than resetting on every settings
            // write. Anything else starts at zero, including a deliberate enable.
            if let Some(old) = self.servers.get(name) {
                if matches!(old.status, Status::Dead(_)) && fresh.live.is_some() {
                    fresh.restarts = old.restarts;
                }
            }
            if let Some(mut old) = self.servers.insert(name.clone(), fresh) {
                close_server(&mut old).await;
            }
            let s = &self.servers[name];
            says.push((name.clone(), format!("{} ({} tool(s))", s.status.says(), s.tools.len())));
        }
        for name in &gone {
            says.push((name.clone(), "removed from the file, so it is stopped".to_string()));
        }
        says.sort_by(|a, b| a.0.cmp(&b.0));
        says
    }

    /// Stop every child and forget the catalog. Used when the master switch goes off.
    pub async fn stop_all(&mut self) {
        for (_, s) in self.servers.iter_mut() {
            close_server(s).await;
        }
    }

    /// How many servers are answering right now.
    pub fn live_count(&self) -> usize {
        self.servers.values().filter(|s| matches!(s.status, Status::Live)).count()
    }

    /// What one server offered, as `(declared name, first line of its description)`.
    ///
    /// The settings page needs this to answer "what does this server actually give me?" without
    /// asking the model to go looking, and it is read off the same list the catalog is built from
    /// rather than from a second read of the server.
    pub fn server_tools(&self, server: &str) -> Vec<(String, String)> {
        self.servers
            .get(server)
            .map(|s| {
                s.tools
                    .iter()
                    .map(|t| {
                        (
                            t.declared.clone(),
                            t.description.lines().next().unwrap_or("").trim().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    /// Every tool the servers offered, in server-name then tool order. Stable order matters:
    /// the catalog is prompt bytes, and a catalog whose order changes between turns costs a
    /// cold prefill for every session.
    pub fn catalog(&self) -> Vec<ToolInfo> {
        let mut out = Vec::new();
        for (name, s) in self.servers.iter() {
            for t in s.tools.iter() {
                let mut t = t.clone();
                t.server = name.clone();
                out.push(t);
            }
        }
        out
    }

    /// `(name, what a reader needs to know)` for every configured server, for the startup log.
    /// A server that failed to start has to be visible in the log the launcher writes to the
    /// journal, because a menu-launched app has no terminal to show it in.
    pub fn servers_named(&self) -> Vec<(String, String)> {
        self.servers
            .iter()
            .map(|(n, s)| {
                (
                    n.clone(),
                    format!("{} ({} tool(s))", s.status.says(), s.tools.len()),
                )
            })
            .collect()
    }

    pub fn status(&self, server: &str) -> Option<&Status> {
        self.servers.get(server).map(|s| &s.status)
    }

    /// The catalog as it enters the env block: names and one line each, bounded, and saying
    /// plainly that none of it is callable yet. It rides the env block rather than the system
    /// prompt because the env block is rewritten every turn (it names the folder and the
    /// sandbox, which can change) while the system prompt is a cache prefix — so adding and
    /// dropping servers costs history bytes, not a permanently edited prompt.
    /// The catalog's two halves: the fixed sentence and the tool list the budget applies to.
    ///
    /// Split out because `mcp_status` answers "what does this cost every prompt?", and the answer
    /// has two parts with different properties — one is a constant, the other is what grows when
    /// somebody adds a server. Reporting only the total would make the budget look violated the
    /// moment the sentence is added, and reporting only the body would understate the prompt.
    pub fn catalog_parts(&self) -> (usize, usize, usize) {
        if self.servers.is_empty() {
            return (0, 0, 0);
        }
        let block = self.catalog_block();
        let head = catalog_head().len() + CATALOG_CLOSE.len();
        // The closer is counted with the head because it is fixed; the body is what scales.
        (block.len(), block.len().saturating_sub(head), head)
    }

    pub fn catalog_block(&self) -> String {
        if self.servers.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        for (name, s) in self.servers.iter() {
            // A server answering a different protocol version is worth the bytes: it is the one
            // fact about a third party that changes what its tool list can be trusted to mean,
            // and it costs nothing when the server answers the version we offered.
            let dialect = match s.live.as_ref() {
                Some(l) if l.protocol != PROTOCOL_VERSION => format!(", speaks {}", l.protocol),
                _ => String::new(),
            };
            lines.push(format!(
                "- {name}: {}{dialect} ({} tool(s))",
                s.status.says(),
                s.tools.len()
            ));
            for t in s.tools.iter() {
                let one = t.description.lines().next().unwrap_or("").trim();
                let short: String = one.chars().take(70).collect();
                lines.push(format!("  {} — {short}", t.declared));
            }
        }
        let body = lines.join("\n");
        let head = catalog_head();

        // The budget is on the catalog, not on the sentence explaining it: the sentence is a
        // fixed ~250 bytes paid once per turn, and a tool list squeezed below readability is
        // worse than a list that names fewer tools and says how many it left out.
        let room = CATALOG_BYTE_BUDGET.saturating_sub(1);
        let (shown, dropped) = if body.len() <= room {
            (body.clone(), 0usize)
        } else {
            let mut kept = String::new();
            let mut dropped = 0usize;
            for l in lines.iter() {
                if kept.len() + l.len() + 1 > room {
                    dropped += 1;
                    continue;
                }
                if !kept.is_empty() {
                    kept.push('\n');
                }
                kept.push_str(l);
            }
            (kept, dropped)
        };
        let mut out = format!("{head}{shown}");
        if dropped > 0 {
            out.push_str(&format!(
                "\n… and {dropped} more tool(s) not listed here: ask `explore` with \
                 \"mcp:\" to search all of them."
            ));
        }
        // The env block ends with `</env>`; the caller appends this before that closer.
        out.push_str(CATALOG_CLOSE);
        out
    }

    /// Run one tool call. A server that died is restarted once per call site (`restarts` is
    /// bounded), and a failure names the server — a call into a dead server that returns an
    /// empty string is how a model concludes the tool worked.
    pub async fn call(&mut self, server: &str, tool: &str, args: &Value, max_bytes: usize) -> Result<String, String> {
        let timeout = self.timeout;
        let Some(s) = self.servers.get_mut(server) else {
            return Err(format!(
                "no MCP server named {server:?}. The servers this start knows about: {}",
                if self.servers.is_empty() {
                    "none".to_string()
                } else {
                    self.servers.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            ));
        };
        if !matches!(s.status, Status::Live) {
            // One honest retry: a server that died between turns is very often restartable, and
            // telling the model to give up when a spawn would fix it is a refusal nobody can
            // repair. Bounded, so a server that dies on every call cannot loop here.
            if s.restarts >= 2 {
                return Err(format!(
                    "MCP server {server} is {}, and it has already been restarted twice this \
                     conversation. Fix or remove it in [mcp] rather than calling it again.",
                    s.status.says()
                ));
            }
            s.restarts += 1;
            match connect(server, &s.cfg, timeout).await {
                Ok((child, tools, protocol)) => {
                    s.live = Some(Live {
                        child: child.child,
                        stdin: child.stdin,
                        stdout: child.stdout,
                        next_id: child.next_id,
                        protocol,
                    });
                    s.tools = tools;
                    s.status = Status::Live;
                }
                Err(Dead(why)) => {
                    s.status = Status::Dead(why.clone());
                    return Err(format!("MCP server {server} is dead: {why}"));
                }
            }
        }
        let wants_schema = s
            .tools
            .iter()
            .find(|t| t.tool == tool)
            .map(|t| t.declared.clone())
            .unwrap_or_else(|| format!("mcp__{server}__{tool}"));
        if !s.tools.iter().any(|t| t.tool == tool) {
            return Err(format!(
                "{server} has no tool {tool:?}. It offers: {} (call them by the cwoffee name, \
                 e.g. {wants_schema})",
                if s.tools.is_empty() {
                    "nothing".to_string()
                } else {
                    s.tools.iter().map(|t| t.tool.as_str()).collect::<Vec<_>>().join(", ")
                }
            ));
        }
        let live = match s.live.as_mut() {
            Some(l) => l,
            None => {
                s.status = Status::Dead("its pipe closed".to_string());
                return Err(format!("MCP server {server} has no open pipe to call {tool:?} on"));
            }
        };
        match tokio::time::timeout(timeout, request(live, "tools/call", json!({ "name": tool, "arguments": args.clone() }))).await
        {
            Err(_) => Err(format!(
                "MCP server {server} did not answer the call to {tool:?} within {}s. Treat it as \
                 failed and tell the user which server stalled; the timeout is \
                 `[mcp] timeout_secs`.",
                timeout.as_secs()
            )),
            Ok(Err(why)) => {
                // The pipe being gone is a different failure from the tool returning an error,
                // and it is worth saying: the next call will restart the server, so the model
                // should not retry the same call on the assumption it will behave the same.
                s.status = Status::Dead(why.clone());
                s.live = None;
                Err(format!("MCP server {server} failed the call to {tool:?}: {why}"))
            }
            Ok(Ok(v)) => Ok(render_call_result(&v, max_bytes)),
        }
    }

    /// Ask a server for its tool list again. Nothing calls this yet; it is here because a
    /// server whose tools change mid-session is a real thing, and the alternative is a catalog
    /// that quietly lies about what exists.
    #[allow(dead_code)]
    pub async fn reload(&mut self, server: &str) -> Result<usize, String> {
        let timeout = self.timeout;
        let Some(s) = self.servers.get_mut(server) else {
            return Err(format!("no MCP server named {server}"));
        };
        let Some(live) = s.live.as_mut() else {
            return Err(format!("MCP server {server} is {}", s.status.says()));
        };
        let v = tokio::time::timeout(timeout, request(live, "tools/list", json!({})))
            .await
            .map_err(|_| format!("MCP server {server} did not answer tools/list within {}s", timeout.as_secs()))?
            .map_err(|e| format!("MCP server {server}: {e}"))?;
        let tools = parse_tools(server, &v);
        let n = tools.len();
        s.tools = tools;
        Ok(n)
    }
}

/// Shut every server down politely. Called on the way out; a killed MCP server left running
/// behind the app is a process the person did not know they had.
pub async fn shutdown(hub: &Arc<Mutex<Hub>>) {
    let mut h = hub.lock().await;
    for (name, s) in h.servers.iter_mut() {
        if let Some(live) = s.live.as_mut() {
            let _ = live.stdin.shutdown().await;
            match tokio::time::timeout(std::time::Duration::from_secs(2), live.child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    live.child.start_kill().ok();
                    eprintln!("[cwoffee] mcp: {name} did not exit within 2s and was killed");
                }
            }
        }
    }
}

/// Send one request and read its answer, skipping notifications and unrelated ids.
///
/// A server is free to log, notify, or answer out of order; reading "the next line" would
/// eventually grade a `notifications/message` as a tool result, which is a wrong answer with an
/// `ok` stamp on it.
async fn request(live: &mut Live, method: &str, params: Value) -> Result<Value, String> {
    live.next_id += 1;
    let id = live.next_id;
    let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let line = format!("{frame}\n");
    live.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("could not write to the server: {e}"))?;
    live.stdin.flush().await.map_err(|e| format!("could not flush: {e}"))?;
    let mut buf = String::new();
    loop {
        buf.clear();
        let read = live.stdout.read_line(&mut buf).await.map_err(|e| format!("read failed: {e}"))?;
        if read == 0 {
            return Err("the server closed its output".to_string());
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // a server's own log line on stdout; not our frame
        };
        if v.get("id").and_then(Value::as_i64) != Some(id) {
            continue;
        }
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string();
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            return Err(format!("the server answered error {code}: {msg}"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
}

async fn notify(live: &mut Live, method: &str) -> Result<(), String> {
    let frame = json!({ "jsonrpc": "2.0", "method": method, "params": {} });
    let line = format!("{frame}\n");
    live.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("could not write: {e}"))?;
    live.stdin.flush().await.map_err(|e| format!("could not flush: {e}"))
}

struct Started {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

/// Spawn, `initialize`, `initialized`, `tools/list`. Returns the pipes plus what it offers.
async fn connect(
    name: &str,
    cfg: &McpServerCfg,
    timeout: std::time::Duration,
) -> Result<(Started, Vec<ToolInfo>, String), Dead> {
    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    // stderr goes to the log, not to the pipe: a server's own diagnostics are for the person who
    // started it, and MCP frames belong on stdout only.
    cmd.stderr(std::process::Stdio::inherit());
    for (k, val) in cfg.env.iter() {
        cmd.env(k, val);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Dead(format!("could not start `{}`: {e}", cfg.command)))?;
    let stdin = child.stdin.take().ok_or_else(|| Dead("no stdin pipe".to_string()))?;
    let stdout = BufReader::new(child.stdout.take().ok_or_else(|| Dead("no stdout pipe".to_string()))?);
    let mut live = Live {
        child,
        stdin,
        stdout,
        next_id: 0,
        protocol: String::new(),
    };
    let hello = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "cwoffee", "version": env!("CARGO_PKG_VERSION") },
    });
    let res = tokio::time::timeout(timeout, request(&mut live, "initialize", hello))
        .await
        .map_err(|_| Dead(format!("no `initialize` answer within {}s", timeout.as_secs())))?
        .map_err(Dead)?;
    let protocol = res
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION)
        .to_string();
    if let Err(why) = tokio::time::timeout(timeout, notify(&mut live, "notifications/initialized")).await {
        let _ = why;
        return Err(Dead("could not send the initialized notification".to_string()));
    }
    let listed = tokio::time::timeout(timeout, request(&mut live, "tools/list", json!({})))
        .await
        .map_err(|_| Dead(format!("no `tools/list` answer within {}s", timeout.as_secs())))?
        .map_err(Dead)?;
    let tools = parse_tools(name, &listed);
    let started = Started {
        child: live.child,
        stdin: live.stdin,
        stdout: live.stdout,
        next_id: live.next_id,
    };
    Ok((started, tools, protocol))
}

/// Turn a `tools/list` result into catalog entries.
///
/// Names are checked, not sanitized: a server offering a tool named `search file` or an empty
/// string would produce a declared name no provider guarantees, and the model would be told a
/// tool that cannot be called.
fn parse_tools(server: &str, v: &Value) -> Vec<ToolInfo> {
    let mut out = Vec::new();
    let Some(list) = v.get("tools").and_then(Value::as_array) else {
        return out;
    };
    for t in list {
        let Some(tool) = t.get("name").and_then(Value::as_str) else {
            continue;
        };
        if tool.is_empty() || tool.len() > 64 || !tool.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
            eprintln!(
                "[cwoffee] mcp: {server} offered a tool named {tool:?}, which is not a usable \
                 name; it is not mounted"
            );
            continue;
        }
        let description = t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("(the server gave no description)")
            .to_string();
        let schema = t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {}, "additionalProperties": false }));
        let says_read_only = t
            .get("annotations")
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push(ToolInfo {
            server: server.to_string(),
            tool: tool.to_string(),
            declared: format!("mcp__{server}__{tool}"),
            description,
            schema,
            says_read_only,
        });
    }
    out
}

/// A `tools/call` answer is a content array; flatten the text, and say what could not be
/// flattened rather than dropping it. Truncation is the tool's job, so `max_bytes` arrives here.
fn render_call_result(v: &Value, max_bytes: usize) -> String {
    let mut text = String::new();
    let mut dropped: Vec<String> = Vec::new();
    if let Some(items) = v.get("content").and_then(Value::as_array) {
        for it in items {
            match it.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(s) = it.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(s);
                    }
                }
                Some(other) => dropped.push(other.to_string()),
                None => dropped.push("a block with no type".to_string()),
            }
        }
    }
    if !dropped.is_empty() {
        text.push_str(&format!(
            "\n[this call also returned: {}, which cwoffee cannot show as text]",
            dropped.join(", ")
        ));
    }
    if v.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        text.push_str("\n[the server reported this call as an error]");
    }
    let (clamped, cut) = clamp(text, max_bytes);
    if cut {
        format!("{clamped}\n[truncated by cwoffee: the server's answer is longer than the tool's output budget]")
    } else {
        clamped
    }
}

/// Absolute paths in a server's arguments that land outside the workspace.
///
/// An MCP server runs wherever its own program runs, so this is not a sandbox: it is the
/// approval card naming the paths a call is about to touch. Scanning arguments is the only
/// signal available — the server decides what it does with them — so the card says "the
/// arguments name these paths outside your workspace" rather than promising enforcement.
pub fn escapes_in(workspace: &Path, args: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(args, &mut found, workspace);
    found.sort();
    found.dedup();
    found
}

fn walk(v: &Value, out: &mut Vec<String>, workspace: &Path) {
    match v {
        Value::Object(map) => {
            for (_, val) in map.iter() {
                walk(val, out, workspace);
            }
        }
        Value::Array(items) => {
            for it in items.iter() {
                walk(it, out, workspace);
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if t.len() < 2 {
                return;
            }
            if !(t.starts_with('/') || t.starts_with('~') || (t.len() > 2 && t.as_bytes()[1] == b':')) {
                return;
            }
            let expanded = if let Some(rest) = t.strip_prefix("~/") {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into())).join(rest)
            } else if t.starts_with('~') {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
            } else {
                PathBuf::from(t)
            };
            let real = expanded.canonicalize().unwrap_or_else(|_| expanded.clone());
            let root = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
            if !real.starts_with(&root) {
                out.push(real.display().to_string());
            }
        }
        _ => {}
    }
}

/// How a call reaches the server.
///
/// Two routes, one behaviour: the risk, the escapes, `per_call`, the truncation limit and the
/// wording of a failure are all decided *before* this is consulted, so a call through the
/// socket is the same call the Rust core makes - it only takes a different road to the same
/// hub. The socket route exists because `cwtool` is one process per tool call and cannot own
/// a long-lived server; it does not reach a server itself, it asks the host that owns one.
pub enum Route {
    Hub(Arc<Mutex<Hub>>),
    Socket(PathBuf),
}

/// One third-party tool, declared only after `explore` names it.
pub struct McpTool {
    info: ToolInfo,
    route: Route,
}

impl McpTool {
    pub fn new(info: ToolInfo, hub: Arc<Mutex<Hub>>) -> Self {
        Self {
            info,
            route: Route::Hub(hub),
        }
    }
    /// The pi core's route: the hub lives in the host process, and this tool asks it over
    /// the socket it was given.
    pub fn via_socket(info: ToolInfo, sock: &Path) -> Self {
        Self {
            info,
            route: Route::Socket(sock.to_path_buf()),
        }
    }
    pub fn info(&self) -> &ToolInfo {
        &self.info
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.info.declared
    }

    /// The server's own words, prefixed with where they came from. A model that cannot tell
    /// which program a tool reaches into will guess, and a guess about a third party's
    /// behaviour is how a person's `~` gets touched.
    fn description(&self) -> &str {
        // Boxed leak for the same reason the other tools get away with `&str`: the description
        // is prompt bytes and must outlive the call that builds them.
        let mut s = String::new();
        s.push_str("[MCP] From server ");
        s.push_str(&self.info.server);
        s.push_str(": ");
        s.push_str(&self.info.description);
        if self.info.says_read_only {
            s.push_str(" (the server says it only reads; cwoffee still asks, once per call)");
        }
        Box::leak(s.into_boxed_str())
    }

    fn schema(&self) -> Value {
        self.info.schema.clone()
    }

    fn source(&self) -> Option<String> {
        // Named, not implied: the approval card and the discovery card both have to say that a
        // different program is about to run, and the server's own description is not that fact.
        Some(format!("MCP server {}", self.info.server))
    }

    fn risk(&self) -> Risk {
        risk_of(&self.info)
    }

    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        CallRisk {
            level: risk_of(&self.info),
            escapes: escapes_in(&ctx.workspace, args),
            // A "yes to this tool for the conversation" must not cover a server: its reach is
            // whatever that program can do, not the workspace.
            per_call: true,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        // One body, two roads. The socket road blocks on a syscall, so it goes to the
        // blocking pool rather than holding the async loop that is serving the turn.
        let reached = match &self.route {
            Route::Hub(hub) => hub
                .lock()
                .await
                .call(&self.info.server, &self.info.tool, &args, ctx.max_output_bytes)
                .await,
            Route::Socket(sock) => {
                let server = self.info.server.clone();
                let tool = self.info.tool.clone();
                let max = ctx.max_output_bytes;
                let sock = sock.clone();
                match tokio::task::spawn_blocking(move || socket::call(&sock, &server, &tool, &args, max)).await {
                    Ok(r) => r,
                    // The blocking task died rather than answering. The model gets the same
                    // shape of sentence it gets for a dead server, because what it can do with
                    // either one is the same: stop and say what could not be reached.
                    Err(e) => Err(format!("the call could not be carried to the host: {e}")),
                }
            }
        };
        match reached {
            Ok(text) => {
                let failed = text.contains("[the server reported this call as an error]");
                let mut r = ToolResult::ok(text);
                if failed {
                    r.ok = false;
                    r.error = Some(format!("server {} reported this call as an error", self.info.server));
                }
                r
            }
            Err(why) => {
                // Repairable, and worded as repairable: the model needs to know whether to
                // change the call, change the server, or stop and tell the person. The text is
                // also handed back as `output`, because a tool_result that only carries an error
                // code leaves the model to guess what to change.
                let mut r = ToolResult::ok(format!("ERROR: {why}"));
                r.ok = false;
                r.exit_code = Some(1);
                r.error = Some(why);
                r
            }
        }
    }
}

/// Register every catalog entry as a **hidden** tool: it exists, it is callable once revealed,
/// and it costs no prompt bytes until then.
///
/// The caller hands over the catalog rather than this function reading the hub, because the
/// registry is built on a thread where blocking on an async lock is not allowed — and a startup
/// that deadlocks on its own lock is a worse failure than a two-argument function.
pub fn install_into(reg: &mut crate::tools::Registry, catalog: &[ToolInfo], hub: &Arc<Mutex<Hub>>) {
    for info in catalog.iter() {
        reg.register_lazy(Arc::new(McpTool::new(info.clone(), hub.clone())));
    }
}

/// The pi core's registration: the same hidden tools, reaching the hub through the host's
/// socket. Nothing here decides what is hidden - `register_lazy` is the one answer to that,
/// and both cores go through it, so "a server nobody uses costs no prompt bytes" is one
/// implementation rather than two that drift.
pub fn install_socket_into(reg: &mut crate::tools::Registry, catalog: &[ToolInfo], sock: &Path) {
    for info in catalog.iter() {
        reg.register_lazy(Arc::new(McpTool::via_socket(info.clone(), sock)));
    }
}

#[cfg(test)]
mod tests;
