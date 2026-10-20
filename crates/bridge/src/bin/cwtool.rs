//! One Rust process, the whole cwoffee tool set, and none of the policy in JavaScript.
//!
//! pi's tool loop reaches the real tools through this binary: the tool name on argv,
//! JSON args on stdin, one JSON envelope on stdout. The extension around it is ~40 lines
//! of plumbing because there is nothing else to put in it.
//!
//!   cwtool specs
//!   cwtool check <tool> [--permit p]... < args.json
//!   cwtool run   <tool> [--permit p]... < args.json
//!
//! Three commands, and each exists to stop a specific drift:
//!
//! * `specs` — the extension registers what the *registry* says, not a copy of it. A tool
//!   description is prompt bytes; two copies of them means two prefix caches and one
//!   guaranteed lie.
//! * `check` — the risk of *this call*, from the tool's own `risk_for`, plus the engine's
//!   own one-line summary. An approval card built from a dialog title cannot say "outside
//!   the workspace: /home/x/.ssh/id_rsa"; this can, in the same words the Rust core uses.
//! * `run` — execution, under the workspace confinement, sandbox config, net policy and
//!   search backend read from cwoffee's own `config.toml`. The desktop's Rust core and a
//!   pi-backed session therefore disagree about nothing except who ran the loop.
//!
//! `--permit` carries the paths the user approved for *this one call*. It is a flag, not
//! an environment variable, because an approved escape that survives into the next call
//! is a hole with a long memory.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use cwoffee_engine::approval::ApprovalMode;
use cwoffee_engine::config::Config;
use cwoffee_engine::tools::web::SearchBackend;
use cwoffee_engine::store::Store;
use cwoffee_engine::tools::{ExecCfg, ExecChunk, Registry, Risk, ToolCtx};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn out(v: Value) {
    println!("{}", serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()));
}

fn die(msg: String) -> ! {
    // On stdout, as an envelope: the caller is a tool loop reading one JSON line, and a
    // tool loop that cannot parse the answer treats it as "the tool did not run" at best
    // and "the tool ran and said nothing" at worst.
    out(json!({ "ok": false, "error": msg }));
    std::process::exit(1);
}

fn args_from_stdin() -> Value {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).ok();
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        return json!({});
    }
    match serde_json::from_str(&trimmed) {
        Ok(v) => v,
        Err(e) => die(format!("args must be one JSON object on stdin: {e}")),
    }
}

/// The registry, built the way the desktop builds it.
///
/// `Config::load` is the whole point: sandbox on/off, exec timeout, net policy and the
/// search backend come from the same file the Rust core reads. If the config cannot be
/// read, say so loudly on stderr and fall back — a silently defaulted sandbox is the
/// failure mode this has to avoid.
fn registry() -> Registry {
    match Config::load(None) {
        Ok((cfg, path)) => {
            if let Some(p) = &path {
                eprintln!("[cwtool] config {} | search: {}", p.display(), cfg.search_backend().describe());
            }
            Registry::with_v1_tools_exec(cfg.search_backend(), cfg.exec_cfg())
        }
        Err(e) => {
            // Loud, because a silently defaulted sandbox or net policy is the one
            // divergence a user cannot see from the transcript.
            eprintln!("[cwtool] config could not be read ({e}); engine defaults, search DuckDuckGo - this session may differ from the Rust core");
            Registry::with_v1_tools_exec(SearchBackend::DuckDuckGo, ExecCfg::default())
        }
    }
}

fn workspace() -> PathBuf {
    match std::env::var("CWOFFEE_WORKSPACE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim().to_string()),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn mode() -> ApprovalMode {
    match std::env::var("CWOFFEE_MODE") {
        Ok(v) => ApprovalMode::parse(&v).unwrap_or(ApprovalMode::Ask),
        Err(_) => ApprovalMode::Ask,
    }
}

/// The app's conversation store, when the host said where it lives.
///
/// `recall` reads the user's own history, and that history is a SQLite file the *host*
/// opened. A tool host that was never handed the path has to say so: the alternative is a
/// tool that reports "nothing matched" when the truth is "nobody was able to look", and a
/// model that believes the first one stops looking.
///
/// The file must already exist. `Store::open` creates one, so a typo in an env var would
/// otherwise produce an empty database beside the user's real history and a confident
/// answer of "you have never discussed that".
fn store_handle() -> Option<Arc<Store>> {
    let raw = std::env::var("CWOFFEE_DB").unwrap_or_default();
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        eprintln!(
            "[cwtool] CWOFFEE_DB is not set: the conversation store is unreachable, so `recall` refuses rather than reporting nothing found"
        );
        return None;
    }
    if !std::path::Path::new(&raw).is_file() {
        eprintln!("[cwtool] CWOFFEE_DB points at {} which is not a database file: `recall` refuses", raw);
        return None;
    }
    match Store::open(&raw) {
        Ok(st) => Some(Arc::new(st)),
        Err(e) => {
            eprintln!("[cwtool] the conversation store at {raw} could not be opened ({e}): `recall` refuses");
            None
        }
    }
}

/// Which conversation is asking, resolved from pi's own session file through the bridge's
/// session map. "Unknowable" is a legitimate answer — one pi process serves several
/// conversations, and the map may not know this one yet — so the tool says which claim it
/// cannot make rather than guessing at one.
fn asking_session() -> Option<i64> {
    let file = std::env::var("CWOFFEE_PI_SESSION_FILE").unwrap_or_default();
    let map = std::env::var("CWOFFEE_PI_SESSION_MAP").unwrap_or_default();
    if file.trim().is_empty() || map.trim().is_empty() {
        return None;
    }
    let me = std::fs::canonicalize(file.trim()).ok()?;
    let text = std::fs::read_to_string(map.trim()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let rows = v.get("by_row").and_then(Value::as_object)?;
    for (row, path) in rows {
        let Some(p) = path.as_str() else { continue };
        if std::fs::canonicalize(p).ok().as_deref() == Some(me.as_path()) {
            if let Ok(id) = row.trim().parse::<i64>() {
                return Some(id);
            }
        }
    }
    None
}

/// The search provider and exec policy, read the way the host reads them. `explore` builds
/// its tool cards from these, so a card has to describe the tool the user actually mounted
/// (a SearXNG search described as DuckDuckGo is a wrong claim in a tool result).
fn cfg_for_tools() -> ExecCfg {
    let (c, _) = cwoffee_engine::config::Config::load(None).unwrap_or_default();
    c.exec_cfg()
}

fn ctx(permits: Vec<PathBuf>) -> ToolCtx {
    let store = store_handle();
    let search_backend = {
        let (c, _) = cwoffee_engine::config::Config::load(None).unwrap_or_default();
        c.search_backend()
    };
    ToolCtx {
        workspace: workspace(),
        // The Rust core's host uses 24 KiB (`SessionHost`'s ToolCtx). Match it exactly:
        // a tool dump is prompt bytes, so a second core that lets three times as much in
        // per read is a core with a different context meter, a different prefix cache and
        // a larger process. The number belongs to the host, not to whoever wrote a runner.
        max_output_bytes: 24 * 1024,
        ask: None,
        events: None,
        question_timeout: None,
        store,
        session: asking_session(),
        search_backend,
        exec_cfg: cfg_for_tools(),
        permits,
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut it = std::env::args().skip(1);
    let Some(sub) = it.next() else {
        eprintln!(
            "usage: cwtool specs | questions <tool> | check <tool> | run <tool> [--permit p]...              [--host-answers json] < args.json"
        );
        return std::process::ExitCode::from(2);
    };
    let tool = it.next().unwrap_or_default();
    let mut permits: Vec<PathBuf> = Vec::new();
    // Answers a *person* gave, in the order the tool asks its questions. They arrive as a
    // flag rather than inside `args` on purpose: the model authors `args`, and answers
    // authored by the model would defeat the point of a tool whose whole job is to ask.
    let mut host_answers: Option<String> = None;
    while let Some(a) = it.next() {
        if a == "--permit" {
            if let Some(p) = it.next() {
                permits.push(PathBuf::from(p));
            }
        } else if a == "--host-answers" {
            if let Some(j) = it.next() {
                host_answers = Some(j);
            }
        }
    }
    let reg = registry();

    match sub.as_str() {
        "specs" => {
            // In registry order on purpose: that order is prompt bytes, and the Rust core
            // appends new tools rather than reshuffling them.
            let list: Vec<Value> = reg
                .defs()
                .into_iter()
                .map(|d| {
                    // Which tools want a human to answer, so the extension around them can
                    // ask without deciding anything. It reads this flag and then asks
                    // `cwtool questions` for the list; it never guesses from the tool name.
                    let host_ask = reg
                        .get(&d.name)
                        .map(|t| t.host_questions(&json!({})).is_some())
                        .unwrap_or(false);
                    json!({
                        "name": d.name,
                        "description": d.description,
                        "parameters": d.parameters,
                        "host_ask": host_ask,
                    })
                })
                .collect();
            out(json!({ "ok": true, "count": list.len(), "tools": list }));
            std::process::ExitCode::SUCCESS
        }
        "questions" => {
            if tool.is_empty() {
                die("questions needs a tool name".into());
            }
            let Some(t) = reg.get(&tool) else {
                die(format!("no such cwoffee tool: {tool}"));
            };
            let args = args_from_stdin();
            // The questions this call would ask, from the tool's own parse. A host that
            // runs the tool in a separate process cannot hold a question open inside it,
            // so it has to ask from this list and hand the answers back with --host-answers.
            // Same parse, same ids: the card the user sees and the answers the tool scores
            // are one set, not two things that happen to look alike.
            match t.host_questions(&args) {
                None => out(json!({ "ok": false, "tool": tool, "error": format!("{tool} does not ask the user anything") })),
                Some(Err(e)) => out(json!({ "ok": false, "tool": tool, "error": e })),
                Some(Ok(qs)) => {
                    let list: Vec<Value> = qs
                        .iter()
                        .map(|q| serde_json::to_value(q).unwrap_or_else(|_| json!({ "error": "a question would not serialize" })))
                        .collect();
                    out(json!({ "ok": true, "tool": tool, "count": list.len(), "questions": list }));
                }
            }
            std::process::ExitCode::SUCCESS
        }
        "check" => {
            if tool.is_empty() {
                die("check needs a tool name".into());
            }
            let Some(t) = reg.get(&tool) else {
                die(format!("no such cwoffee tool: {tool}"));
            };
            let args = args_from_stdin();
            let ctx = ctx(permits);
            let risk = t.risk_for(&args, &ctx);
            let summary = cwoffee_engine::approval::summarize_with(&tool, &args, &risk.escapes);
            // Mirrors the engine's gate: `Ask` stops anything that is not plainly
            // harmless and inside the workspace; the wider modes have already made up
            // their mind for the session. The gate stays the authority for the Rust core.
            let needs = match mode() {
                ApprovalMode::Ask => !matches!(risk.level, Risk::Harmless) || !risk.escapes.is_empty(),
                _ => false,
            };
            out(json!({
                "ok": true,
                "tool": tool,
                "risk": format!("{:?}", risk.level).to_lowercase(),
                "escapes": risk.escapes,
                "summary": summary,
                "needs_approval": needs,
                "mode": mode().as_str(),
            }));
            std::process::ExitCode::SUCCESS
        }
        "run" => {
            if tool.is_empty() {
                die("run needs a tool name".into());
            }
            let Some(t) = reg.get(&tool) else {
                die(format!("no such cwoffee tool: {tool}"));
            };
            let args = args_from_stdin();
            let mut ctx = ctx(permits);
            if let Some(raw) = host_answers.as_deref() {
                let answers: Vec<Option<cwoffee_engine::tools::ask::Answer>> = match serde_json::from_str(raw) {
                    Ok(a) => a,
                    Err(e) => die(format!(
                        "--host-answers must be a JSON array with one entry per question, in the order                          `cwtool questions` listed them: {e}"
                    )),
                };
                let (qtx, mut qrx) = mpsc::unbounded_channel::<cwoffee_engine::tools::ask::QuestionRx>();
                tokio::spawn(async move {
                    // Match by position, not id: the extension asked in the order this tool
                    // builds its questions, and each `cwtool` run numbers a fresh set, so an
                    // id compared across two processes would be noise. A null slot means the
                    // person left that one unanswered — dropping the reply sender is how the
                    // engine already says so, which is why the model is told nobody answered
                    // instead of being handed an empty sentence.
                    let mut i = 0usize;
                    while let Some((_asked, reply)) = qrx.recv().await {
                        match answers.get(i).cloned().flatten() {
                            Some(a) => {
                                let _ = reply.send(a);
                            }
                            None => drop(reply),
                        }
                        i += 1;
                    }
                });
                ctx.ask = Some(qtx);
            }
            // Stream chunks out on stderr rather than dropping them: `exec` produces
            // output while it runs, and a runner that swallows it makes a long command
            // look like a hang.
            let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<ExecChunk>();
            let pump = tokio::spawn(async move {
                while let Some(c) = sink_rx.recv().await {
                    eprintln!("[{:?}] {}", c.stream, c.chunk);
                }
            });
            let res = t.run(args, &ctx, &sink_tx).await;
            drop(sink_tx);
            let _ = pump.await;
            // Pass the *whole* ToolResult through, not the three fields that look
            // interesting: `error` is how a model learns what it got wrong, `files` is
            // how produced files reach the UI as attachments, and `changes` is the
            // +N -M diff on the tool card. Dropping them is a silent feature loss the
            // transcript cannot explain.
            let mut v = serde_json::to_value(&res)
                .unwrap_or_else(|e| json!({ "ok": false, "error": format!("could not serialize the tool result: {e}") }));
            if let Some(o) = v.as_object_mut() {
                o.insert("tool".to_string(), json!(tool));
                // The sentence the model should read, rendered by the engine itself. The
                // envelope carries the structured result (files, changes, telemetry) and
                // rides in pi's `details`, which never enters the prompt; this field is
                // what the extension puts in `content`. Keeping the rendering here rather
                // than in ~40 lines of TypeScript is what keeps the two cores saying the
                // same thing to the endpoint.
                o.insert(
                    "rendered".to_string(),
                    json!(cwoffee_engine::agent::render_result(&res)),
                );
            }
            out(v);
            if res.ok {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::SUCCESS
            }
        }
        other => die(format!("unknown cwtool command: {other}")),
    }
}
