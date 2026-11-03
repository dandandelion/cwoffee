
//! Tests for the MCP seam. They spawn `scripts/fake-mcp-server.mjs`, so the framing, the
//! timeouts and the dead-server path are exercised against a real stdio pipe rather than a mock
//! that agrees with whatever the client expects.

/// The approval card leads with whose program runs. A reader who has to decode
/// `mcp__jira__create_issue` into "a program I installed is about to act" has to notice the
/// namespace under time pressure, which is exactly when people stop reading.
#[test]
fn the_approval_card_names_the_server_a_call_is_going_into() {
    let line = crate::approval::summarize_with(
        "mcp__fake__write_note",
        &json!({ "name": "note.txt", "body": "x" }),
        &[],
    );
    assert!(line.starts_with("MCP server fake: "), "{line}");
    assert!(line.contains("note.txt"), "the card still has to say what it writes: {line}");
    // An escape is named on top of the source, not instead of it: both facts change the answer.
    let out = crate::approval::summarize_with(
        "mcp__fake__write_note",
        &json!({ "name": "note.txt" }),
        &["/home/you/.ssh/id_ed25519".to_string()],
    );
    assert!(out.starts_with("MCP server fake: "), "{out}");
    assert!(out.contains("OUTSIDE the workspace"), "{out}");
    // cwoffee's own tools are not relabelled as third-party.
    let own = crate::approval::summarize_with("write_file", &json!({ "path": "a.md" }), &[]);
    assert!(!own.contains("MCP server"), "{own}");
}

use super::*;
use crate::tools::{Registry, Risk, ToolCtx};
use std::path::PathBuf;
use std::time::Duration;

fn script() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../scripts/fake-mcp-server.mjs");
    p.canonicalize().expect("scripts/fake-mcp-server.mjs exists")
}

fn node() -> String {
    std::env::var("CWOFFEE_NODE").unwrap_or_else(|_| "node".to_string())
}

fn cfg(args: Vec<String>, timeout: u64) -> McpCfg {
    McpCfg {
        enabled: true,
        timeout_secs: timeout,
        approval_timeout_secs: None,
        servers: std::collections::BTreeMap::from([(
            "fake".to_string(),
            McpServerCfg {
                command: node(),
                args: {
                    let mut a = vec![script().display().to_string()];
                    a.extend(args);
                    a
                },
                enabled: true,
                env: std::collections::BTreeMap::new(),
            },
        )]),
    }
}

/// Every test that starts a server leaves its children killed; a stray fake server holding a
/// pipe open would make the next test's `initialize` wait on a process nobody is talking to.
async fn start(args: Vec<String>, timeout: u64) -> Arc<Mutex<Hub>> {
    let hub = Arc::new(Mutex::new(Hub::start(&cfg(args, timeout)).await));
    hub
}

#[test]
fn a_server_name_that_cannot_be_a_tool_name_is_refused_not_rewritten() {
    let mut c = McpCfg::default();
    for bad in ["f", "Fake", "my server", "mcp__x", "ok_name"] {
        c.servers.insert(bad.to_string(), McpServerCfg::default());
    }
    assert_eq!(c.bad_names(), vec!["Fake", "f", "mcp__x", "my server"]);
    // `ok_name` is fine and `mcp__x` is not, even though both are letters and underscores: a
    // name carrying our own namespace marker would let a server impersonate a cwoffee tool.
    assert!(is_server_name("ok_name"));
    assert!(!is_server_name("mcp__x"));
}

#[tokio::test]
async fn an_unnamed_server_is_reported_instead_of_spawned() {
    let mut c = McpCfg::default();
    c.servers.insert(
        "Bad Name".to_string(),
        McpServerCfg {
            command: node(),
            args: vec![script().display().to_string()],
            ..Default::default()
        },
    );
    let hub = Hub::start(&c).await;
    let status = hub.status("Bad Name").unwrap().says();
    assert!(status.contains("not usable"), "{status}");
    assert!(hub.catalog().is_empty(), "a dead server offers no tools");
}

#[tokio::test]
async fn a_live_server_lists_tools_that_are_hidden_until_revealed() {
    let hub = start(vec!["--tools".into(), "3".into()], 10).await;
    let status = hub.lock().await.status("fake").unwrap().says();
    assert_eq!(status, "live", "the fake server did not come up");
    let catalog = hub.lock().await.catalog();
    assert_eq!(catalog.len(), 4, "t1..t3 plus `boom`; the bogus name is refused: {catalog:?}");
    assert_eq!(catalog[0].declared, "mcp__fake__t1");
    assert!(catalog[0].says_read_only);
    assert!(!catalog[1].says_read_only);

    let mut reg = Registry::with_v1_tools();
    let before = serde_json::to_string(&reg.defs()).unwrap().len();
    install_into(&mut reg, &catalog, &hub);
    let after = serde_json::to_string(&reg.defs()).unwrap().len();
    // The whole economy of the feature: an MCP server is configured, its tools are registered,
    // and the prompt still costs exactly what it cost before.
    assert_eq!(before, after, "registering MCP tools changed the declared prompt bytes");
    assert_eq!(reg.lazy_names().len(), 4);
    assert!(!reg.defs().iter().any(|d| d.name.starts_with("mcp__")));

    assert!(reg.reveal("mcp__fake__t1"));
    assert!(reg.defs().iter().any(|d| d.name == "mcp__fake__t1"));
    assert!(!reg.reveal("mcp__fake__t1"), "revealing twice is not a thing");
    shutdown(&hub).await;
}

#[tokio::test]
async fn the_catalog_is_bounded_and_says_what_it_left_out() {
    // 200 tools is a real server, and a catalog of it would be a second tool menu in every
    // prompt — which is the cost this feature exists to avoid.
    let hub = start(vec!["--tools".into(), "200".into()], 10).await;
    let block = hub.lock().await.catalog_block();
    assert!(
        block.len() <= CATALOG_BYTE_BUDGET + 400,
        "the catalog block is {} bytes: {CATALOG_BYTE_BUDGET} budget plus the fixed lead-in",
        block.len()
    );
    assert!(block.contains("explore"), "{block}");
    assert!(block.contains("NOT callable yet"), "{block}");
    assert!(block.contains("more tool(s) not listed"), "{block}");
    // Order is stable: the catalog is prompt bytes, and a reshuffle each turn is a cold prefill.
    let again = hub.lock().await.catalog_block();
    assert_eq!(block, again);
    shutdown(&hub).await;
}

#[tokio::test]
async fn a_dead_server_says_so_in_the_catalog_and_a_call_into_it_names_the_server() {
    let hub = start(vec!["--die".into()], 10).await;
    let block = hub.lock().await.catalog_block();
    assert!(block.contains("fake: live"), "it listed its tools, so it was live at startup: {block}");
    // `t1` is read-only per the fake server, so the first call restarts it and it dies again.
    let err = {
        let mut h = hub.lock().await;
        h.call("fake", "t1", &json!({ "text": "hi" }), 4096).await
    };
    let why = err.expect_err("a server that closed its output must not answer Ok");
    assert!(why.contains("fake"), "the message must name the server: {why}");
    assert!(!why.contains("hang"), "{why}");
    // The catalog now tells the reader the server is gone, not that it has three tools.
    let after = hub.lock().await.catalog_block();
    assert!(after.contains("dead"), "{after}");
    shutdown(&hub).await;
}

#[tokio::test]
async fn a_stalled_server_is_a_timeout_not_a_hang() {
    let hub = start(vec!["--slow".into(), "4000".into(), "--tools".into(), "1".into()], 1).await;
    let started = std::time::Instant::now();
    let why = {
        let mut h = hub.lock().await;
        h.call("fake", "t1", &json!({ "text": "hi" }), 4096).await
    }
    .expect_err("a server that stalls past the timeout must fail");
    assert!(why.contains("did not answer"), "{why}");
    assert!(why.contains("timeout_secs"), "{why}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "it hung: {:?}", started.elapsed());
    shutdown(&hub).await;
}

/// The edit form shows an environment's **names** and never its values, so the values have to survive
/// an edit that never saw them. This is the difference between "I changed the command" and "I
/// silently blanked the token that made the program work", and the page reports the first one either
/// way.
#[test]
fn an_edit_that_never_saw_a_token_keeps_it() {
    use std::collections::BTreeMap;
    let held: BTreeMap<String, String> =
        [("JIRA_TOKEN".to_string(), "s3cr3t".to_string()), ("HOME_HINT".to_string(), "/opt".to_string())]
            .into_iter()
            .collect();

    // The prefill a row produces: bare names, no values.
    let kept = McpServerCfg::env_keep_lines(&held, &["JIRA_TOKEN".into(), "HOME_HINT".into()]).unwrap();
    assert_eq!(kept["JIRA_TOKEN"], "s3cr3t", "the token was blanked by an edit that only changed the command");
    assert_eq!(kept["HOME_HINT"], "/opt");

    // A typed value replaces, and an empty one is taken as deliberate.
    let edited = McpServerCfg::env_keep_lines(&held, &["JIRA_TOKEN=new-token".into(), "HOME_HINT=".into()]).unwrap();
    assert_eq!(edited["JIRA_TOKEN"], "new-token");
    assert_eq!(edited["HOME_HINT"], "", "an explicit `KEY=` means empty, not missing");

    // A name the server does not have is refused, not created empty: whoever typed it meant to give
    // a value with it.
    let err = McpServerCfg::env_keep_lines(&held, &["NEW_KEY".into()]).unwrap_err();
    assert!(err.contains("NEW_KEY") && err.contains("NEW_KEY=value"), "{err}");
    // And dropping the line drops the key — the form is the whole list of what the server gets.
    let dropped = McpServerCfg::env_keep_lines(&held, &["JIRA_TOKEN".into()]).unwrap();
    assert_eq!(dropped.len(), 1);
    assert!(dropped.contains_key("JIRA_TOKEN"));
    assert!(!dropped.contains_key("HOME_HINT"));
}

/// A server can be removed from the config while the app is running. The hub forgets it, and so must
/// the registry — otherwise the session keeps handles for a program that is gone, and the model can
/// name one and be shown an approval card for a call that cannot happen.
#[tokio::test]
async fn a_removed_servers_tools_stop_being_callable_in_the_running_session() {
    let hub = Arc::new(Mutex::new(Hub::start(&cfg(vec!["--tools".into(), "3".into()], 10)).await));
    let mut reg = crate::tools::Registry::with_v1_tools();
    let offered: Vec<crate::mcp::ToolInfo> = hub.lock().await.catalog();
    // Read from the catalog rather than assumed: `--tools 3` names three, and the fake server
    // always offers one more (`boom`) so a call that returns an error has a tool of its own.
    assert!(
        offered.iter().filter(|t| t.tool.starts_with('t')).count() == 3,
        "the fake server offered {:?}",
        offered.iter().map(|t| &t.declared).collect::<Vec<_>>()
    );
    for info in offered.iter() {
        reg.register_lazy(Arc::new(crate::mcp::McpTool::new(
            info.clone(),
            Arc::clone(&hub),
        )));
    }
    assert_eq!(reg.lazy_names().iter().filter(|n| n.starts_with("mcp__")).count(), offered.len());

    // Take one away, the way `Hub::apply` does when the file stops listing it.
    let still: std::collections::HashSet<String> = offered
        .iter()
        .filter(|t| t.tool != "t2")
        .map(|t| t.declared.clone())
        .collect();
    let gone = reg.forget_stale_mcp(&still);
    assert_eq!(gone.len(), 1, "{gone:?}");
    assert!(gone[0].ends_with("__t2"), "{gone:?}");
    assert!(reg.get_shared(&gone[0]).is_none(), "the handle is gone");
    assert!(!reg.lazy_names().contains(&gone[0]), "and it is no longer withheld");
    assert!(!reg.defs().iter().any(|d| d.name == gone[0]), "and it is not declared");

    // The guard, which is the part that could do damage: a first-party tool is not the hub's to
    // offer, and `write_file` is not in `offered` either.
    assert!(reg.get_shared("write_file").is_some(), "forget_stale_mcp took a first-party tool");
    assert!(reg
        .defs()
        .iter()
        .any(|d| d.name == "explore"));
    // And what is still offered stays hidden rather than being re-declared on the way through.
    for keep in still.iter() {
        assert!(reg.is_lazy(keep), "{keep} stopped being hidden when a neighbour was dropped");
    }
}

#[tokio::test]
async fn a_call_is_answered_and_a_block_that_is_not_text_is_said_not_dropped() {
    let hub = start(vec!["--tools".into(), "1".into(), "--noise".into()], 10).await;
    let text = {
        let mut h = hub.lock().await;
        h.call("fake", "t1", &json!({ "text": "echo me" }), 4096).await
    }
    .expect("a live server answers");
    assert!(text.contains("echo me"), "{text}");
    // The fake server returns a text block and an image block. Returning only the text half
    // would tell the model the call produced text, which is a claim about content we dropped.
    assert!(text.contains("also returned: image"), "{text}");
    // Interleaved log lines must not be read as answers: `request` skips non-matching frames.
    shutdown(&hub).await;
}

#[tokio::test]
async fn a_server_that_reports_an_error_is_an_error_on_the_card_too() {
    let hub = start(vec!["--tools".into(), "1".into()], 10).await;
    let tool = hub
        .lock()
        .await
        .catalog()
        .into_iter()
        .find(|t| t.tool == "boom")
        .expect("the fake server offers `boom`");
    let hub = Hub::start(&cfg(vec!["--tools".into(), "1".into()], 10)).await;
    let ctx = ToolCtx {
        workspace: std::env::temp_dir(),
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let r = McpTool::new(tool, Arc::new(Mutex::new(hub)))
        .run(json!({}), &ctx, &tx)
        .await;
    assert!(!r.ok, "isError must not arrive as a successful call");
    assert!(r.error.as_deref().unwrap_or("").contains("fake"), "{r:?}");
    assert!(r.output.contains("refused"), "{}", r.output);
}

#[tokio::test]
async fn a_tool_that_writes_writes_somewhere_the_test_can_read() {
    let dir = std::env::temp_dir().join(format!("cwoffee_mcp_write_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let hub = Arc::new(Mutex::new(Hub::start(&McpCfg {
        enabled: true,
        timeout_secs: 10,
        approval_timeout_secs: None,
        servers: std::collections::BTreeMap::from([(
            "fake".to_string(),
            McpServerCfg {
                command: node(),
                args: vec![script().display().to_string(), "--writes".into(), dir.display().to_string()],
                enabled: true,
                env: std::collections::BTreeMap::new(),
            },
        )]),
    })
    .await));
    let text = {
        let mut h = hub.lock().await;
        h.call("fake", "write_note", &json!({ "name": "note.txt", "body": "from the server" }), 4096)
            .await
    }
    .expect("the server writes");
    assert!(text.contains("note.txt"), "{text}");
    let bytes = std::fs::read_to_string(dir.join("note.txt")).expect("the file is really there");
    assert_eq!(bytes, "from the server");
    shutdown(&hub).await;
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_servers_claim_to_be_read_only_lowers_the_bar_and_never_lifts_it() {
    let reads = ToolInfo {
        server: "x".into(),
        tool: "look".into(),
        declared: "mcp__x__look".into(),
        description: String::new(),
        schema: json!({}),
        says_read_only: true,
    };
    let quiet = ToolInfo { says_read_only: false, ..reads.clone() };
    assert_eq!(risk_of(&reads), Risk::Workspace);
    assert_eq!(risk_of(&quiet), Risk::Sensitive);
    // The rule under test is the one the config cannot override: nothing a server says makes a
    // third-party tool free to run.
    assert_ne!(risk_of(&reads), Risk::Harmless);
    assert_ne!(risk_of(&quiet), Risk::Harmless);
}

#[test]
fn a_path_in_the_arguments_outside_the_workspace_is_named_as_an_escape() {
    let ws = std::env::temp_dir().join(format!("cwoffee_mcp_ws_{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let inside = ws.join("note.md");
    std::fs::write(&inside, "x").unwrap();
    let args = json!({
        "file": inside.display().to_string(),
        "other": format!("{}/.ssh/id_ed25519", std::env::var("HOME").unwrap_or_else(|_| "/root".into())),
        "nested": [{ "deep": ws.join("sub").display().to_string() }],
    });
    let found = escapes_in(&ws, &args);
    assert_eq!(found.len(), 1, "only the path outside the workspace: {found:?}");
    assert!(found[0].contains(".ssh"), "{found:?}");
    std::fs::remove_dir_all(&ws).ok();
}

/// The gate half: a grant remembered for the conversation must not cover a server.
#[tokio::test]
async fn a_session_grant_for_a_tool_does_not_cover_an_mcp_call() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // A short gate timeout: the MCP call below is deliberately unanswered, and the point of the
    // claim is that it *asks* rather than riding the exec grant. Waiting out the app's real
    // approval timeout to prove that would make this the slowest test in the crate for no extra
    // information.
    let mut g = crate::approval::ApprovalGate::new(crate::approval::ApprovalMode::Session, tx)
        .with_timeout(std::time::Duration::from_millis(120));
    // Answer the one question the gate is allowed to ask: Session mode means the *first* allow is
    // remembered, so a test that never answers gets a fail-closed denial and grades nothing.
    let answer = async {
        let (req, reply) = rx.recv().await.expect("the gate asks once");
        assert_eq!(req.tool, "exec");
        reply.send(crate::approval::ApprovalAnswer::Allow).ok();
    };
    let exec = CallRisk::at(Risk::Workspace);
    let exec_args = json!({ "command": "ls" });
    let asked = g.check("exec", &exec, &exec_args);
    let (got, ()) = tokio::join!(asked, answer);
    assert!(matches!(got, crate::approval::GateOutcome::Allow), "the first allow must be remembered");
    // `check` asks and the UI here is a channel nobody answers, so the second call for exec is
    // allowed from the grant. The MCP call has to ask anyway.
    let mcp = CallRisk {
        level: Risk::Workspace,
        escapes: Vec::new(),
        per_call: true,
    };
    let mcp_args = json!({ "text": "x" });
    match g.check("mcp__fake__t1", &mcp, &mcp_args).await {
        crate::approval::GateOutcome::Allow => panic!("a session grant must not cover a server"),
        // With no UI to answer, `Ask` cannot happen here; anything other than Allow proves the
        // grant did not cover the call, and the arm is spelled out so a future mode that allows
        // silently has to be thought about rather than silently matching `_`.
        crate::approval::GateOutcome::Ask { .. } => panic!("the gate asked a question nobody was asked to answer"),
        crate::approval::GateOutcome::Deny { reason } => {
            assert!(
                reason.contains("no approval UI") || reason.contains("refusing"),
                "expected a fail-closed refusal, got {reason}"
            );
        }
    }
}

#[tokio::test]
async fn explore_can_search_the_servers_and_names_what_it_found() {
    let hub = start(vec!["--tools".into(), "3".into()], 10).await;
    let mut reg = Registry::with_v1_tools();
    install_into(&mut reg, &hub.lock().await.catalog(), &hub);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // The agent fills `lazy_tools` from its registry every round-trip; a test that leaves it
    // empty is asking `explore` about a host that holds nothing back, which is a different app.
    let ctx = ToolCtx {
        lazy_tools: reg.lazy_views(),
        ..Default::default()
    };
    let r = crate::tools::discover::ExploreTool
        .run(json!({ "need": "mcp: echo back some text" }), &ctx, &tx)
        .await;
    let text = r.text();
    assert!(text.contains("mcp__fake__t1"), "{text}");
    // The card names the server. A model that cannot tell which program a tool reaches into
    // will guess, and a guess about a third party is how a person's home folder gets touched.
    assert!(text.contains("MCP server fake"), "{text}");
    assert!(r.activates.contains(&"mcp__fake__t1".to_string()), "{:?}", r.activates);
    // `mcp:` must not answer with first-party tools: "look this up in jira" is not web_search.
    assert!(!text.contains("- web_search:"), "{text}");
    shutdown(&hub).await;
}

#[tokio::test]
async fn searching_mcp_when_nothing_is_running_says_so() {
    let reg = Registry::with_v1_tools();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let r = crate::tools::discover::ExploreTool
        .run(json!({ "need": "mcp: anything" }), &ToolCtx::default(), &tx)
        .await;
    let text = r.text();
    assert!(text.contains("No MCP server is running"), "{text}");
    assert!(r.activates.is_empty(), "a search with nothing to find unlocks nothing");
    let _ = reg;
}

/// The schemas are held by the process from startup and reach the model only when it asks:
/// `with_arguments` is the expensive option, and a hidden tool's arguments are not in the
/// prompt at all until it is revealed.
#[tokio::test]
async fn a_hidden_tools_schema_costs_nothing_until_asked_for() {
    let hub = start(vec!["--tools".into(), "2".into()], 10).await;
    let mut reg = Registry::with_v1_tools();
    install_into(&mut reg, &hub.lock().await.catalog(), &hub);
    let declared = serde_json::to_string(&reg.defs()).unwrap();
    assert!(!declared.contains("echo back the text"), "a hidden schema is in the prompt: {declared}");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = ToolCtx {
        lazy_tools: reg.lazy_views(),
        ..Default::default()
    };
    let r = crate::tools::discover::ExploreTool
        .run(json!({ "need": "mcp: echo", "with_arguments": true }), &ctx, &tx)
        .await;
    assert!(r.text().contains("\"text\""), "the answer must carry the arguments: {}", r.text());
    shutdown(&hub).await;
}


/// The client calls are blocking syscalls — that is how `cwtool`, a process with no runtime of
/// its own, reaches the hub. In a `#[tokio::test]` the runtime is single-threaded, so running
/// one inline would park the only worker and the accept loop would never answer: the production
/// path runs them on the blocking pool (see `McpTool::run`), and a test that measured a
/// different path would be measuring nothing.
async fn rpc<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.expect("the blocking call ran")
}

async fn get_catalog(p: PathBuf) -> Vec<ToolInfo> {
    rpc(move || socket::catalog(&p)).await.expect("the catalog travels")
}

async fn get_call(p: PathBuf, server: &str, tool: &str, args: Value) -> Result<String, String> {
    let server = server.to_string();
    let tool = tool.to_string();
    rpc(move || socket::call(&p, &server, &tool, &args, 4096)).await
}

async fn get_cmd(p: PathBuf, req: Value) -> Value {
    rpc(move || socket::ask(&p, &req, Duration::from_secs(5)))
        .await
        .expect("the hub answered")
}

/// One socket per test, and the file removed at both ends: a path that still answers to a
/// previous test's hub would grade a call against a server nobody started.
fn sock(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("cwoffee-mcp-sock-{tag}-{}", std::process::id()))
}

fn fresh(tag: &str) -> PathBuf {
    let p = sock(tag);
    let _ = std::fs::remove_file(&p);
    p
}

/// The claim the pi core rests on: a call made from a *different process*, through the socket,
/// is the same call the Rust core makes. Not a similar one with its own wording — one answer,
/// because both roads go to the same hub and the tool body is one function.
#[tokio::test]
async fn a_call_from_another_process_reaches_the_same_hub_and_answers_the_same_words() {
    let hub = start(vec![], 10).await;
    let p = fresh("call");
    socket::serve(Arc::clone(&hub), p.clone()).await.expect("the socket listens");
    let args = json!({ "text": "through the socket" });
    let via_socket = get_call(p.clone(), "fake", "t1", args.clone()).await.expect("a call answers");
    let in_process = hub
        .lock()
        .await
        .call("fake", "t1", &args, 4096)
        .await
        .expect("the hub answers the same call");
    assert_eq!(via_socket, in_process, "one call, two roads, two answers");
    assert!(via_socket.contains("through the socket"), "{via_socket}");
    shutdown(&hub).await;
    let _ = std::fs::remove_file(&p);
}

/// What the prompt pays, handed out by the hub rather than recomputed by whoever asks: a
/// second copy of the byte budget in a shim would be a number that drifts from the engine's.
#[tokio::test]
async fn the_socket_hands_out_the_catalog_and_the_block_and_refuses_anything_else() {
    let hub = start(vec!["--tools".into(), "2".into()], 10).await;
    let p = fresh("catalog");
    socket::serve(Arc::clone(&hub), p.clone()).await.expect("the socket listens");

    let list = get_catalog(p.clone()).await;
    assert!(
        list.iter().any(|t| t.declared == "mcp__fake__t1"),
        "the catalog holds {:?}",
        list.iter().map(|t| t.declared.clone()).collect::<Vec<_>>()
    );

    let block = get_cmd(p.clone(), json!({ "cmd": "block" })).await;
    let text = block["block"].as_str().expect("the block is a sentence");
    assert_eq!(text, hub.lock().await.catalog_block(), "the prompt gets the hub's own block");
    assert!(
        text.len() <= CATALOG_BYTE_BUDGET,
        "the block is {}B against a {CATALOG_BYTE_BUDGET}B ceiling",
        text.len()
    );
    assert!(text.contains("explore"), "the block has to say how a tool becomes callable: {text}");

    // The socket is a road to a server, not a way to drive the app. Anything else is refused
    // with the list of what it does answer, so a caller learns the boundary rather than
    // guessing at it from a bare error code.
    let refused = get_cmd(p.clone(), json!({ "cmd": "start_turn" })).await;
    assert_eq!(refused["ok"], json!(false), "a non-tool request was honoured: {refused}");
    let why = refused["error"].as_str().unwrap_or("");
    assert!(why.contains("cannot start a turn"), "{why}");
    assert!(why.contains("catalog"), "the refusal should say what it does answer: {why}");

    shutdown(&hub).await;
    let _ = std::fs::remove_file(&p);
}

/// A server whose process went away: the model gets one sentence for one fact, whichever core
/// ran the call. Two wordings would be two behaviours, and the pi arm of a test could pass on
/// a phrasing the Rust core never produces.
#[tokio::test]
async fn a_server_whose_process_is_gone_answers_the_same_way_over_the_socket() {
    let hub = start(vec!["--die".into()], 5).await;
    let p = fresh("dead");
    socket::serve(Arc::clone(&hub), p.clone()).await.expect("the socket listens");
    let args = json!({ "text": "anything" });
    let in_process = hub
        .lock()
        .await
        .call("fake", "t1", &args, 4096)
        .await
        .expect_err("the server's pipes were closed, so this cannot succeed");
    let via_socket = get_call(p.clone(), "fake", "t1", args.clone()).await.expect_err("the same call over the socket");
    assert_eq!(via_socket, in_process, "a dead server is described two different ways");
    assert!(in_process.contains("dead") || in_process.contains("closed"), "{in_process}");
    shutdown(&hub).await;
    let _ = std::fs::remove_file(&p);
}

/// A host that is not there is a sentence a model can act on, and it arrives promptly: the
/// whole rule about MCP is that somebody else's program never leaves a turn waiting.
#[test]
fn a_socket_nobody_is_listening_on_names_the_host_and_returns_quickly() {
    let p = fresh("absent");
    let t = std::time::Instant::now();
    let e = socket::call(&p, "fake", "t1", &json!({ "text": "x" }), 4096).expect_err("nothing is listening");
    assert!(t.elapsed() < Duration::from_secs(2), "a dead host took {:?}", t.elapsed());
    assert!(e.contains("not reachable"), "{e}");
    assert!(e.contains("nothing was called"), "{e}");
}

/// The safety half does not change with the road. `per_call` is what stops a "yes for the
/// conversation" from covering a third party, and an escape is what stops a server from
/// reading a file the workspace does not contain — both read from the same code on either
/// core, and this is the assertion that fails if a socket route ever grows its own policy.
#[tokio::test]
async fn the_socket_route_keeps_the_per_call_rule_the_same_risk_and_the_same_escapes() {
    let hub = start(vec![], 10).await;
    let p = fresh("risk");
    socket::serve(Arc::clone(&hub), p.clone()).await.expect("the socket listens");
    let catalog = get_catalog(p.clone()).await;

    let mut in_reg = Registry::with_v1_tools();
    install_into(&mut in_reg, &catalog, &hub);
    let mut out_reg = Registry::with_v1_tools();
    install_socket_into(&mut out_reg, &catalog, &p);

    // A workspace that exists, so the escape check compares two real paths rather than two
    // guesses: canonicalizing a folder nobody created is how a path outside it can read as
    // inside it.
    let ws = std::env::temp_dir().join(format!("cwoffee-mcp-sock-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("a workspace to measure escapes against");
    let outside = std::env::temp_dir().join(format!("cwoffee-mcp-sock-outside-{}.txt", std::process::id()));
    let ctx = ToolCtx {
        workspace: ws.clone(),
        ..Default::default()
    };
    for name in ["mcp__fake__t1", "mcp__fake__t2"] {
        let a = Risk::Harmless;
        let here = in_reg.get(name).expect("registered").risk_for(&json!({ "text": "x" }), &ctx);
        let there = out_reg.get(name).expect("registered").risk_for(&json!({ "text": "x" }), &ctx);
        assert_eq!(here.level, there.level, "{name} is risky differently on the two roads");
        assert_ne!(here.level, a, "{name} is never harmless: a third party runs it");
        assert!(here.per_call && there.per_call, "{name}: a session grant covers neither road");
        assert_eq!(here.escapes, there.escapes, "{name}: escapes are counted two ways");
    }
    // A path in the arguments that lands outside the workspace is an escape on either road: a
    // server cannot grant itself room cwoffee declined to give it, and the card has to name it.
    let with_path = json!({ "text": outside.display().to_string() });
    let here = in_reg.get("mcp__fake__t1").unwrap().risk_for(&with_path, &ctx);
    let there = out_reg.get("mcp__fake__t1").unwrap().risk_for(&with_path, &ctx);
    assert!(!here.escapes.is_empty(), "the in-process route saw no escape: {here:?}");
    assert_eq!(here.escapes, there.escapes, "the socket route disagrees about the escape");
    assert!(here.per_call && there.per_call, "an escape stops being per-call on one road");
    shutdown(&hub).await;
    let _ = std::fs::remove_file(&p);
}

/// A hidden tool stays hidden when it arrives over a socket. The whole economy of the feature
/// is that a configured server costs nothing until asked for, and the pi core registers its
/// tools through this path — so the assertion that matters most on that core is here.
#[tokio::test]
async fn a_server_reached_over_the_socket_declares_nothing_until_it_is_named() {
    let hub = start(vec!["--tools".into(), "3".into()], 10).await;
    let p = fresh("hidden");
    socket::serve(Arc::clone(&hub), p.clone()).await.expect("the socket listens");
    let mut reg = Registry::with_v1_tools();
    let catalog = get_catalog(p.clone()).await;
    install_socket_into(&mut reg, &catalog, &p);
    let declared = serde_json::to_string(&reg.defs()).unwrap();
    assert!(!declared.contains("mcp__"), "a server's tool reached the declared list: {declared}");
    assert_eq!(
        reg.lazy_names().len(),
        catalog.len(),
        "the withheld list is not the catalog the hub handed out"
    );
    for t in catalog.iter() {
        assert!(reg.is_lazy(&t.declared), "{} is not withheld", t.declared);
    }
    // Named, and callable: the same-turn promise the engine's answer makes.
    assert!(reg.reveal("mcp__fake__t1"), "revealing a socket tool did nothing");
    let declared = serde_json::to_string(&reg.defs()).unwrap();
    assert!(declared.contains("mcp__fake__t1"), "a named tool is still out of the prompt: {declared}");
    assert!(!declared.contains("mcp__fake__t2"), "revealing one tool declared the whole server");
    shutdown(&hub).await;
    let _ = std::fs::remove_file(&p);
}

/// What the settings page rests on: the file is written and the running set is made to match it,
/// without a restart. A server that only the boot could start is a control that reports success and
/// changes nothing, so this walks the whole cycle the page offers — add, switch off, remove — against
/// the same hub the boot built and the same `apply` the commands call.
#[tokio::test]
async fn a_server_added_while_the_app_is_open_is_running_without_a_restart() {
    let dir = std::env::temp_dir().join(format!("cwoffee-mcp-apply-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "mode = \"ask\"\n").unwrap();

    let row = |args: Vec<String>| McpServerCfg {
        command: node(),
        args: {
            let mut a = vec![script().display().to_string()];
            a.extend(args);
            a
        },
        enabled: true,
        env: std::collections::BTreeMap::new(),
    };
    let read = |p: &std::path::Path| {
        let (mut c, _) = crate::config::Config::load(Some(p)).unwrap();
        c.apply_env();
        c.mcp_cfg()
    };

    // The boot's own path: the file holds one server, `Hub::start` reads it.
    crate::config::set_server_in(&path, "fake", &row(vec!["--tools".into(), "2".into()]))
        .expect("the first server is written the same way the settings form writes it");
    let hub = Arc::new(Mutex::new(Hub::start(&read(&path)).await));
    assert_eq!(hub.lock().await.status("fake").unwrap().says(), "live");

    // Add a second one while the app is open, and reconcile.
    crate::config::set_server_in(&path, "other", &row(vec!["--tools".into(), "1".into()]))
        .expect("a second server can be added to a file that already has one");
    let says = hub.lock().await.apply(&read(&path)).await;
    let names: Vec<&str> = says.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["fake", "other"], "apply answers for every server the file lists");
    let says_other = says.iter().find(|(n, _)| n == "other").unwrap().1.clone();
    assert!(says_other.starts_with("live"), "the new server says {says_other}");
    let decl: Vec<String> = hub.lock().await.catalog().iter().map(|t| t.declared.clone()).collect();
    assert!(decl.iter().any(|d| d.starts_with("mcp__other__")), "the catalog carries it: {decl:?}");
    assert_eq!(hub.lock().await.server_tools("other").len(), 2, "t1 plus `boom`");

    // Switch it off. The file keeps the row; the process goes, and the catalog with it.
    crate::config::set_server_enabled_in(&path, "other", false).unwrap();
    hub.lock().await.apply(&read(&path)).await;
    assert_eq!(hub.lock().await.status("other").unwrap().says(), "off in config");
    assert_eq!(hub.lock().await.server_tools("other").len(), 0, "a switched-off server offers nothing");
    let decl: Vec<String> = hub.lock().await.catalog().iter().map(|t| t.declared.clone()).collect();
    assert!(!decl.iter().any(|d| d.starts_with("mcp__other__")), "{decl:?}");

    // Remove it, and the hub stops knowing the name at all.
    crate::config::remove_server_in(&path, "other").unwrap();
    let says = hub.lock().await.apply(&read(&path)).await;
    assert!(hub.lock().await.status("other").is_none(), "a name the file dropped is not kept");
    assert!(
        says.iter().any(|(n, s)| n == "other" && s.contains("stopped")),
        "the answer says what happened to the one that just went away: {says:?}"
    );
    // The server nobody touched is still the same live one: an apply that restarted everything would
    // drop a session somebody was already using.
    assert_eq!(hub.lock().await.status("fake").unwrap().says(), "live");

    shutdown(&hub).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_form_sends_one_argument_per_line_and_key_value_environment() {
    // One argument per line, because nothing here splits quotes: a shell-free form that pretended
    // otherwise would turn `--dir /my docs` into two arguments without telling anyone.
    let args = McpServerCfg::args_from_lines(&[
        "  --tools ".into(),
        "3".into(),
        "".into(),
        "   ".into(),
        "--writes /tmp/my docs".into(),
    ]);
    assert_eq!(args, vec!["--tools", "3", "--writes /tmp/my docs"]);

    let env = McpServerCfg::env_from_lines(&[
        "JIRA_URL=https://issues.example.com".into(),
        "".into(),
        "JIRA_TOKEN=sk-new".into(),
    ])
    .unwrap();
    assert_eq!(env.len(), 2);
    assert_eq!(env["JIRA_TOKEN"], "sk-new");

    // A line with no `=` is a half-typed key. Guessing it as a name with an empty value would write
    // an environment entry nobody asked for.
    let e = McpServerCfg::env_from_lines(&["JIRA_URL".into()]).unwrap_err();
    assert!(e.contains("KEY=value"), "{e}");
    let e = McpServerCfg::env_from_lines(&["=oops".into()]).unwrap_err();
    assert!(e.contains("before `=`"), "{e}");
}

/// The page reads this object, not the struct. Both transports used to assemble it by hand and had
/// drifted by one field each, which is the fault `theme`'s payload test names — so the wire is what
/// gets asserted.
#[tokio::test]
async fn the_status_payload_keeps_what_the_file_says_apart_from_what_is_answering() {
    let file = crate::config::McpFileState {
        enabled: true,
        timeout_secs: 20,
        servers: vec![(
            "jira".to_string(),
            McpServerCfg {
                command: "jira-mcp".to_string(),
                args: vec!["--stdio".to_string()],
                enabled: false,
                env: std::collections::BTreeMap::from([("JIRA_URL".to_string(), "https://x".to_string())]),
            },
        )],
        problem: String::new(),
    };
    let mut cfg = McpCfg::default();
    cfg.servers.insert("jira".to_string(), file.servers[0].1.clone());
    let hub = Hub::start(&cfg).await;
    let p = status_payload(
        Some(&hub),
        &cfg,
        "rust",
        std::path::Path::new("/home/u/.config/cwoffee/config.toml"),
        "this process",
        &file,
        true,
    );
    // A hub is mounted, nothing is answering, and the file's own row is visible with its switch off.
    assert_eq!(p["mounted"], json!(true));
    assert_eq!(p["running"], json!(0), "a server switched off in the file is not answering");
    assert_eq!(p["configured"], json!(1), "what this start resolved");
    assert_eq!(p["file_configured"], json!(1), "what the file itself holds");
    assert_eq!(p["servers"][0]["says"].as_str().unwrap(), "off in config (0 tool(s))");
    assert_eq!(p["file"]["servers"][0]["name"], json!("jira"));
    assert_eq!(p["file"]["servers"][0]["enabled"], json!(false));
    assert_eq!(p["file"]["servers"][0]["running"], json!(false));
    assert_eq!(p["file"]["servers"][0]["env"], json!(["JIRA_URL"]));
    assert_eq!(p["writable"], json!(true));
    assert!(p["why"].as_str().unwrap().contains("none is answering"), "{}", p["why"]);

    // The same payload for a transport that may not change anything says so; the page has to be able
    // to explain itself instead of showing controls that do nothing.
    let phone = status_payload(Some(&hub), &cfg, "rust", std::path::Path::new("/x"), "none", &file, false);
    assert_eq!(phone["writable"], json!(false));
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("unused"));
}

/// `CWOFFEE_MCP_SERVERS` replaces the file's server list wholesale. A page that wrote the file in
/// that situation would report a change the next start never reads, so `writable` goes false and the
/// command refuses with the variable's name.
#[test]
fn a_page_is_not_told_it_may_write_when_the_environment_names_the_servers() {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("CWOFFEE_MCP_SERVERS", "{\"fake\":{\"command\":\"node\"}}");
    let p = status_payload(
        None,
        &McpCfg::default(),
        "rust",
        std::path::Path::new("/x.toml"),
        "none",
        &crate::config::McpFileState::default(),
        // The desktop asks for true; the environment is what takes it away.
        true,
    );
    assert!(listed_from_env());
    assert_eq!(p["writable"], json!(false), "a write would say it worked and change nothing");
    assert_eq!(p["env_lists"], json!(true));
    std::env::remove_var("CWOFFEE_MCP_SERVERS");
    assert!(!listed_from_env());
}
