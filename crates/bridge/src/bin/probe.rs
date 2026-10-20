//! cwoffee-bridge-probe — the smallest client that can prove the bridge works.
//!
//! It is a *client*, not a test: it talks to the bridge through `HostOps` and
//! `StoreOps`, the same two traits the desktop window and the phone use, and it
//! answers approval cards and questions the way a UI does. So the thing it proves
//! is not "pi streams" — it is that a streamed turn, its approvals, its tool
//! calls, and its transcript survive the translation without a single line of UI
//! being written.
//!
//! Run against cwoffee's own scripted endpoint:
//!
//! ```text
//! cd ../cwoffee && node scripts/mock-llm.mjs --port 8190            # one terminal
//! cargo run -p cwoffee-bridge --bin cwoffee-bridge-probe -- \
//!     --url http://127.0.0.1:8190/v1 --model cwoffee-mock \
//!     --prompt "Use read_file to read the file /etc/hostname" \
//!     --ext ../../spike/pi-extension-cwoffee-tools.ts --approve allow
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cwoffee_bridge::host::{PiHost, PiHostConfig};
use cwoffee_bridge::process::PiLaunch;
use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode};
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{HostCmd, HostOps};
use cwoffee_engine::provider::Reasoning;
use cwoffee_engine::store::{Store, StoreOps};

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| -> String {
        argv.iter()
            .position(|a| a.as_str() == k)
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_else(|| d.to_string())
    };
    let flag = |k: &str| argv.iter().any(|a| a.as_str() == k);
    let many = |k: &str| -> Vec<String> {
        argv.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == k)
            .filter_map(|(i, _)| argv.get(i + 1))
            .cloned()
            .collect()
    };

    let url = get("--url", "http://127.0.0.1:8190/v1");
    let provider = get("--provider", "cwoffee-mock");
    let model = get("--model", "cwoffee-mock");
    let pi_bin = PathBuf::from(get("--pi", "pi"));
    let workdir = std::fs::canonicalize(get("--workspace", "/tmp/pi-ws")).unwrap_or_else(|_| PathBuf::from("/tmp/pi-ws"));
    let agent_dir = PathBuf::from(get("--agent-dir", "/tmp/cwoffee-bridge-probe"));
    let session_dir = PathBuf::from(get("--session-dir", "/tmp/cwoffee-bridge-probe/sessions"));
    let db = PathBuf::from(get("--db", "/tmp/cwoffee-bridge-probe/store.db"));
    let prompts = if many("--prompt").is_empty() {
        vec!["Count from 1 to 15.".to_string()]
    } else {
        many("--prompt")
    };
    let approve = get("--approve", "allow");
    // Two ways a card closes with no answer, and they are different code paths:
    // the viewer disappears (the oneshot receiver is dropped), or the viewer just
    // never answers (pi's own dialog timeout denies). Both must end with the tool
    // not running.
    let no_answer = flag("--no-answer");
    let drop_card = flag("--drop-card");
    let approval_timeout = get("--approval-timeout-secs", "20").parse::<u64>().unwrap_or(20);
    let idle_reap = get("--idle-reap-secs", "600").parse::<u64>().unwrap_or(600);
    let gap = get("--gap-secs", "0").parse::<u64>().unwrap_or(0);
    // Open an existing conversation before the first prompt. Running the probe
    // twice against one --db does that, which is how the resume path (pi's own
    // session file) gets exercised rather than asserted.
    let switch_to = argv
        .iter()
        .position(|a| a.as_str() == "--switch")
        .and_then(|i| argv.get(i + 1))
        .and_then(|s| s.parse::<i64>().ok());
    // Pin policy (locked decision 13): the bridge names the pi it was built
    // against. --pi-version exists so a developer pointing at a checkout can say
    // "this one, exactly", and --any-pi drops the check for exploration only.
    let pinned = if flag("--any-pi") { None } else { Some(get("--pi-version", "1.2.0")) };

    std::fs::create_dir_all(&agent_dir)?;
    std::fs::create_dir_all(&session_dir)?;
    std::fs::create_dir_all(db.parent().unwrap_or(std::path::Path::new(".")))?;
    if !flag("--keep-models") {
        // The mock endpoint speaks the OpenAI wire, which is what pi's `openai-completions`
        // api means; a real endpoint's kind comes from cwoffee's config instead.
        let path =
            cwoffee_bridge::mount::write_dev_provider(&agent_dir, &provider, &url, &model, "mock", "openai-completions", None)?;
        eprintln!("[probe] wrote {provider} -> {url} in {}", path.display());
    }

    // Extensions are copied into the isolated agent dir and pi's own
    // node_modules is linked beside them, so their imports resolve without
    // touching the `pi` the user has in ~/.pi. Same trick as the capture script.
    let ext_paths = cwoffee_bridge::mount::mount_extensions(
        &agent_dir,
        &pi_bin,
        &many("--ext").into_iter().map(PathBuf::from).collect::<Vec<_>>(),
    )?;

    let store = Arc::new(Store::open(&db)?);
    let launch = PiLaunch {
        bin: pi_bin.clone(),
        workdir: workdir.clone(),
        provider: provider.clone(),
        model: model.clone(),
        agent_dir: agent_dir.clone(),
        session_dir,
        session_file: None,
        extensions: ext_paths,
        // Empty means pi's defaults. A coworker product would name every built-in
        // coding tool as `-read,-bash,-edit,-write` and add ours with `+`; that
        // switch is what Phase 2 turns on, and the probe reports what is on.
        tools: many("--tools"),
        // The probe drives the coworker shape: tool set chosen by --tools above, pi's
        // own prompt left alone. Chat mode is a different process and the gateway and
        // the desktop window are where that switch is user-visible.
        no_tools: false,
        system_prompt: None,
        extra_env: Vec::new(),
        pinned_version: pinned.clone(),
    };

    let mut host = PiHost::spawn(PiHostConfig {
        launch,
        store: Arc::clone(&store) as Arc<dyn StoreOps>,
        mode: ApprovalMode::Ask,
        reasoning: Reasoning::Off,
        approval_timeout: Duration::from_secs(approval_timeout),
        question_timeout: Duration::from_secs(30),
        idle_reap: Duration::from_secs(idle_reap),
        context_budget: 8_000,
        budget_pin: Some(8_000),
        model_window: None,
        session_map: agent_dir.join("cwoffee-pi-sessions.json"),
        db_path: None,
        lean_tools: false,
        persona: cwoffee_engine::host::Persona::Cowork,
        cfg_path: None,
        max_tool_iters: 0,
        compact_timeout: std::time::Duration::from_secs(1800),
    })
    .await?;
    println!("pi {} at {}", host.pi_version, pi_bin.display());

    let (mut ev_rx, mut ap_rx) = host.take_receivers().expect("receivers");
    let mut q_rx = host.take_questions().expect("question receiver");

    // The client half: print what the core emits, and answer what it asks. This
    // is the same shape as the gateway's adapter task, which is the point.
    let printer = tokio::spawn(async move {
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        let mut usage: Option<cwoffee_engine::telemetry::TurnTelemetry> = None;
        let mut text = String::new();
        let mut answers = Vec::new();
        let mut ended = 0usize;
        let mut fatal: Option<String> = None;
        loop {
            tokio::select! {
                ev = ev_rx.recv() => match ev {
                    Some(e) => {
                        *kinds.entry(kind_name(&e)).or_insert(0) += 1;
                        match e {
                            EngineEvent::Text(t) => { print!("{t}"); use std::io::Write; let _ = std::io::stdout().flush(); text.push_str(&t); }
                            EngineEvent::Reasoning(t) => text.push_str(&t),
                            EngineEvent::TurnStart { model, session_id, turn_id, .. } => {
                                println!("\n[turn {turn_id}] {model} into conversation {session_id}");
                            }
                            EngineEvent::ToolCallBegin { name, id, .. } => println!("\n[tool] {name} ({id})"),
                            EngineEvent::ToolCallReady { args, .. } => println!("[args] {}", trim(&args.to_string())),
                            EngineEvent::ToolExecEnd { ok, changes, .. } => {
                                println!("[tool done] ok={ok} changes={}", changes.len());
                            }
                            EngineEvent::Usage(u) => {
                                println!("\n[usage] prompt={} cached={} completion={} ttft={}ms decode={:.0} tok/s",
                                    u.prompt_tokens, u.cached_tokens, u.completion_tokens, u.ttft_ms, u.server_decode_tps);
                                usage = Some(u);
                            }
                            EngineEvent::TurnStats(s) => {
                                if std::env::var("CWOFFEE_PROBE_STATS").is_ok() {
                                    println!("[stats] ttft={}ms stream={}ms chars={} tps={:.0} thinking={}",
                                        s.ttft_ms, s.streaming_ms, s.chars, s.est_decode_tps, s.thinking);
                                }
                            }
                            EngineEvent::Note { message, .. } => println!("\n[note] {message}"),
                            EngineEvent::Error { message, fatal: f } => {
                                println!("\n[{}] {message}", if f { "FATAL" } else { "error" });
                                text.push_str(&format!("\n[error] {message}\n"));
                                if f { fatal = Some(message); }
                            }
                            EngineEvent::TurnEnd { turn_id, finish } => {
                                println!("\n[turn {turn_id} ended: {finish:?}]");
                                ended += 1;
                            }
                            EngineEvent::SessionChanged { session_id, title } => {
                                println!("[session] {session_id} {:?}", title);
                            }
                            EngineEvent::QuestionExpired { id } => println!("\n[question {id} closed without an answer]"),
                            EngineEvent::QuestionAnswered { id } => println!("[question {id} answered]"),
                            _ => {}
                        }
                    }
                    None => break,
                },
                req = ap_rx.recv() => match req {
                    Some((req, reply)) => {
                        println!("\n[approval #{}] {} — {} escapes={:?}", req.id, req.tool, req.summary, req.escapes);
                        if drop_card {
                            // The viewer is gone: dropping the reply is the only
                            // honest signal there is.
                            println!("[probe] dropping the card: nobody will answer");
                            drop(reply);
                            answers.push(format!("#{}:dropped", req.id));
                        } else if no_answer {
                            // Keep the card alive and say nothing: pi's timeout
                            // must be what releases the turn.
                            std::mem::forget(reply);
                            answers.push(format!("#{}:silence", req.id));
                        } else {
                            let ans = match approve.as_str() {
                                "deny" => ApprovalAnswer::Deny,
                                "session" => ApprovalAnswer::AllowSession,
                                _ => ApprovalAnswer::Allow,
                            };
                            answers.push(format!("#{}:{}", req.id, approve));
                            let _ = reply.send(ans);
                        }
                    }
                    None => {}
                },
                q = q_rx.recv() => match q {
                    Some((q, reply)) => {
                        println!("\n[question #{}] {} options={:?}", q.id, q.question,
                            q.options.iter().map(|o| o.label.clone()).collect::<Vec<_>>());
                        let pick = q.recommended.or(if q.options.is_empty() { None } else { Some(0) });
                        let answer = match pick {
                            Some(i) => cwoffee_engine::tools::ask::Answer { text: q.options[i].label.clone(), options: vec![i] },
                            None => cwoffee_engine::tools::ask::Answer { text: "the probe typed this".into(), options: vec![] },
                        };
                        answers.push(format!("q{}:{}", q.id, answer.text));
                        let _ = reply.send(answer);
                    }
                    None => {}
                },
            }
        }
        (kinds, usage, text, answers, ended, fatal)
    });

    // Open an existing conversation first if asked: that is the switch path, and
    // — when no model process is running yet — the resume path.
    if let Some(id) = switch_to {
        println!("\n[probe] switching to conversation {id}");
        host.send(HostCmd::SwitchSession { id });
        // Fire-and-forget by contract, so wait on the observable condition the way
        // a poller does instead of sleeping a fixed amount.
        for _ in 0..40 {
            if host.state().session_id == id {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Count turns from before anything is sent: reading `turns` afterwards races
    // the first turn settling, and a goal that can never be reached looks like a
    // hang rather than like a bug in the probe.
    let want = host.state().turns + prompts.len() as u64;

    for (i, p) in prompts.iter().enumerate() {
        if i > 0 && gap > 0 {
            println!("\n[probe] waiting {gap}s so the idle reaper can close the model process");
            tokio::time::sleep(Duration::from_secs(gap)).await;
            let st = host.state();
            println!(
                "[probe] before prompt {}: turns={} context={} busy={}",
                i + 1,
                st.turns,
                st.context_tokens,
                st.busy
            );
        }
        println!("\n=== prompt {} ===\n{}", i + 1, p);
        host.send_turn_to(p.clone(), Vec::new(), None);
    }

    // Wait on `turns`, the way the engine's own tests do: `busy` going false is
    // not observable if the turn finished fast, and a sleep is not a condition.
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(90);
    loop {
        let st = host.state();
        if st.turns >= want {
            break;
        }
        // A turn that never started must not hang the probe for a minute and a
        // half: if the core is idle again with nothing settled, nothing is coming.
        if !st.busy && st.turns == 0 && started.elapsed() > Duration::from_secs(20) {
            println!("\n[probe] the core went idle without settling a turn");
            break;
        }
        if std::time::Instant::now() > deadline {
            println!("\n[probe] timed out waiting for the turn to settle");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Let the last events drain before tearing the stream down.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = host.state();
    println!(
        "\n[state] conversation={} turns={} context={} tokens, budget={} window={:?} busy={}",
        st.session_id, st.turns, st.context_tokens, st.context_budget, st.model_window, st.busy
    );

    host.send(HostCmd::Shutdown);
    drop(host);
    let (kinds, usage, text, answers, ended, fatal) = printer.await?;

    println!("\n--- what the core emitted ---");
    for (k, n) in &kinds {
        println!("  {n:>3}  {k}");
    }
    println!("answered cards: {}", if answers.is_empty() { "none".into() } else { answers.join(", ") });
    println!("turns ended: {ended}");

    println!("\n--- the transcript the store kept ---");
    for s in store.list_sessions(20)? {
        let rows = store.messages(s.id)?;
        println!("  session {} {:?} ({} rows, model {})", s.id, s.title, rows.len(), s.model);
        for r in rows.iter().take(12) {
            println!("    {:<9} {}", r.role, trim(&r.content));
        }
    }
    println!("\nanswer text length: {} chars", text.chars().count());
    if let Some(u) = &usage {
        let total_prompt = u.prompt_tokens;
        println!(
            "prefix cache: {} of {} prompt tokens were cached read ({:.0}%)",
            u.cached_tokens,
            total_prompt,
            100.0 * u.cached_tokens as f64 / total_prompt.max(1) as f64
        );
    }
    match fatal {
        Some(m) => anyhow::bail!("the turn failed: {m}"),
        None if ended == 0 => anyhow::bail!("no turn ever ended: the stream produced no TurnEnd"),
        None => Ok(()),
    }
}

/// Copy an extension into the isolated agent dir and make sure pi's own
/// `node_modules` resolves for it. An extension that cannot import its types is a
/// silent no-op at best and a confusing startup error at worst, so the probe does
/// the setup the product will do in Phase 2.

/// Walk up from the installed `pi` binary looking for the package it came from.

fn which(name: &str) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for p in path.split(':') {
            let cand = PathBuf::from(p).join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn trim(s: &str) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() > 90 {
        let cut: String = one.chars().take(90).collect();
        format!("{cut}…")
    } else {
        one
    }
}

/// Event names as the front end knows them, for the summary. Written by hand
/// rather than serializing so the list reads like the protocol, not like Rust.
fn kind_name(e: &EngineEvent) -> String {
    match e {
        EngineEvent::SessionChanged { .. } => "sessionChanged",
        EngineEvent::TurnStart { .. } => "turnStart",
        EngineEvent::Text(_) => "text",
        EngineEvent::Reasoning(_) => "reasoning",
        EngineEvent::ToolCallBegin { .. } => "toolCallBegin",
        EngineEvent::ToolCallArgs(_) => "toolCallArgs",
        EngineEvent::ToolCallReady { .. } => "toolCallReady",
        EngineEvent::ToolCallParseError { .. } => "toolCallParseError",
        EngineEvent::TurnFiles { .. } => "turnFiles",
        EngineEvent::ToolExecStart { .. } => "toolExecStart",
        EngineEvent::ToolExecOutput { .. } => "toolExecOutput",
        EngineEvent::ToolExecEnd { .. } => "toolExecEnd",
        EngineEvent::ApprovalRequested { .. } => "approvalRequested",
        EngineEvent::QuestionRequested { .. } => "questionRequested",
        EngineEvent::QuestionAnswered { .. } => "questionAnswered",
        EngineEvent::QuestionExpired { .. } => "questionExpired",
        EngineEvent::TurnStats(_) => "turnStats",
        EngineEvent::Usage(_) => "usage",
        EngineEvent::TurnEnd { .. } => "turnEnd",
        EngineEvent::Note { .. } => "note",
        EngineEvent::Error { .. } => "error",
    }
    .to_string()
}
