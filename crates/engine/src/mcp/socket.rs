//! The hub, reached from a process that cannot own one.
//!
//! A hub is a long-lived owner of children: an MCP server is a process whose pipes carry
//! JSON-RPC frames, and the tool call that uses it is one request in the middle of that
//! conversation. The Rust core holds the hub in the app. The pi core cannot — `cwtool` is
//! one process per tool call, so a server spawned inside it would be a new server on every
//! call, and a stateful server would answer correctly exactly once. So the app keeps the hub
//! and the tool host asks it, over a unix socket in the app's own state directory.
//!
//! What the socket answers is deliberately small: the catalog, the servers' status lines, the
//! catalog block the prompt is shown, and one tool call. It cannot start a turn, change a
//! model, read the store, or approve anything. The approval gate is upstream of it, in the
//! same place it sits on the Rust core, and a process that can reach this socket can reach
//! nothing else through it. The socket lives in a directory the app made for itself; it is not
//! a network port, and it carries no token, because it carries no authority.
//!
//! One request per connection, one line each way. A client that reused a connection would have
//! to remember which server it was talking to, and `cwtool` does not outlive the call.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use super::{Hub, ToolInfo};

/// How long a client waits for the host before it says so. Longer than a healthy call and
/// shorter than a hang: a host that is gone must produce a sentence, not a frozen turn.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// The serving side. Returns once the listener is bound and the accept loop is running as a
/// task on **the caller's runtime** — a requirement, not a style choice. The servers are tokio
/// children with tokio pipes, and driving the hub from a second runtime leaves those pipes on a
/// reactor nobody polls: a live server would answer nothing and read as a dead one at the
/// timeout. Callers that have no runtime cannot host a hub at all, which is already true of
/// every process that owns one.
pub async fn serve(hub: Arc<Mutex<Hub>>, path: PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    // A socket left behind by a previous app would be accepted by the next one and answer to a
    // hub that no longer exists. Unlinking first means the path always names this hub.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("could not listen on {}: {e}", path.display()))?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let hub = hub.clone();
                    tokio::spawn(async move {
                        let _ = handle(stream, &hub).await;
                    });
                }
                // A connection that died before it could be read is not a reason to stop
                // serving: the next tool call still needs a road to its server.
                Err(_) => continue,
            }
        }
    });
    Ok(())
}

/// One request, one answer. Anything it cannot answer comes back as an error *value*, because
/// the caller is a tool loop reading one JSON line, and an unparseable answer reads to it as
/// "the tool did not run".
async fn handle(stream: UnixStream, hub: &Arc<Mutex<Hub>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let mut stream = reader.into_inner();
    let req: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("the request is not JSON: {e}") }));
    let reply = dispatch(req, hub).await;
    let text = serde_json::to_string(&reply)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"the reply would not serialize\"}".into());
    stream.write_all(text.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await
}

async fn dispatch(req: Value, hub: &Arc<Mutex<Hub>>) -> Value {
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    match cmd.as_str() {
        "ping" => json!({ "ok": true, "servers": hub.lock().await.has_servers() }),
        "catalog" => {
            let tools = hub.lock().await.catalog();
            match serde_json::to_value(&tools) {
                Ok(v) => json!({ "ok": true, "tools": v }),
                Err(e) => json!({ "ok": false, "error": format!("the catalog would not serialize: {e}") }),
            }
        }
        // The words the model is shown about its servers. On the pi core the catalog belongs
        // with the turn's words rather than in the system prompt, and the hub is the one place
        // that knows how the block is capped: a second copy of that budget in a shim would be
        // a number that drifts from the engine's.
        "block" => json!({ "ok": true, "block": hub.lock().await.catalog_block() }),
        "status" => {
            let h = hub.lock().await;
            let servers = h
                .servers_named()
                .into_iter()
                .map(|(name, line)| json!({ "name": name, "line": line }))
                .collect::<Vec<Value>>();
            json!({ "ok": true, "has_servers": h.has_servers(), "servers": servers })
        }
        "call" => {
            let Some(server) = req.get("server").and_then(Value::as_str) else {
                return json!({ "ok": false, "error": "a call needs the server's name" });
            };
            let Some(tool) = req.get("tool").and_then(Value::as_str) else {
                return json!({ "ok": false, "error": "a call needs the tool's name" });
            };
            let args = req.get("args").cloned().unwrap_or_else(|| json!({}));
            // The caller's own limit, and no default that quietly raises it: a tool dump is
            // prompt bytes, and the host that framed the call decided how many.
            let max_bytes = req.get("max_bytes").and_then(Value::as_u64).unwrap_or(24 * 1024) as usize;
            match hub.lock().await.call(server, tool, &args, max_bytes).await {
                Ok(text) => json!({ "ok": true, "text": text }),
                Err(why) => json!({ "ok": false, "error": why }),
            }
        }
        other => json!({
            "ok": false,
            "error": format!(
                "this socket answers ping, catalog, block, status and call; it was asked for \
                 {other:?}. It cannot start a turn, change a model, or read a conversation."
            ),
        }),
    }
}

/// One request to the host, with a deadline. A missing socket is a sentence about the host,
/// not a stack frame: the model reads this text, and "connection refused" tells it nothing it
/// can act on.
pub fn ask(sock: &Path, req: &Value, timeout: Duration) -> Result<Value, String> {
    let stream = std::os::unix::net::UnixStream::connect(sock).map_err(|e| {
        format!(
            "the app's MCP hub is not reachable at {} ({e}). The host process that owns the \
             servers is gone, or this core was started without one; nothing was called.",
            sock.display()
        )
    })?;
    stream.set_write_timeout(Some(timeout)).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    let mut writer = stream.try_clone().map_err(|e| format!("could not dup the socket: {e}"))?;
    let text = serde_json::to_string(req).map_err(|e| format!("the request would not serialize: {e}"))?;
    writer
        .write_all(text.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|e| format!("could not reach the app's MCP hub: {e}"))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line)
        .map_err(|e| format!("the app's MCP hub did not answer ({e}); nothing can be said about the call"))?;
    if line.trim().is_empty() {
        return Err("the app's MCP hub answered with nothing; nothing can be said about the call".into());
    }
    serde_json::from_str(line.trim())
        .map_err(|e| format!("the app's MCP hub answered with something that is not JSON: {e}"))
}

/// The catalog the host is holding, or an error saying the host could not be reached.
pub fn catalog(sock: &Path) -> Result<Vec<ToolInfo>, String> {
    let reply = ask(sock, &json!({ "cmd": "catalog" }), CLIENT_TIMEOUT)?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the hub refused the catalog request")
            .to_string());
    }
    let Some(list) = reply.get("tools").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        match serde_json::from_value::<ToolInfo>(v.clone()) {
            Ok(info) => out.push(info),
            // A catalog entry this process cannot read is an entry it must not pretend exists:
            // registering a half-parsed tool would hand the model a schema that validates
            // nothing the server accepts.
            Err(e) => return Err(format!("a catalog entry could not be read: {e}")),
        }
    }
    Ok(out)
}

/// `(name, one-line state)` for every configured server, dead ones included.
pub fn servers(sock: &Path) -> Result<Vec<(String, String)>, String> {
    let reply = ask(sock, &json!({ "cmd": "status" }), CLIENT_TIMEOUT)?;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the hub refused the status request")
            .to_string());
    }
    let Some(list) = reply.get("servers").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(list
        .iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let line = v.get("line")?.as_str()?.to_string();
            Some((name, line))
        })
        .collect())
}

/// One call, and the hub's own words back. `Err` here is the hub's sentence about a dead or
/// refusing server — the same sentence the in-process route produces — so both cores hand the
/// model one wording for one failure.
pub fn call(sock: &Path, server: &str, tool: &str, args: &Value, max_bytes: usize) -> Result<String, String> {
    let reply = ask(
        sock,
        &json!({ "cmd": "call", "server": server, "tool": tool, "args": args, "max_bytes": max_bytes }),
        CLIENT_TIMEOUT,
    )?;
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(reply.get("text").and_then(Value::as_str).unwrap_or("").to_string());
    }
    Err(reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the hub refused this call")
        .to_string())
}
