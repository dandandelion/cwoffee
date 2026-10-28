
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
