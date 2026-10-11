//! End-to-end agent loop against a real model, with a real sandboxed tool.
//!
//!   cargo run -p cwoffee-engine --example live_agent -- http://127.0.0.1:8090/v1
//!
//! Unit tests use a scripted provider. This proves the parts a script cannot:
//! that the model actually emits a tool call our accumulator + repair pipeline
//! can parse, that the loop feeds the result back, and that the second round-trip
//! still hits the prefix cache.

use std::sync::atomic::{AtomicU64, Ordering};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalGate, ApprovalMode};
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::provider::{openai::{Flavor, OpenAiProvider}, Reasoning};
use cwoffee_engine::tools::{exec::ExecTool, ExecCfg, Registry, ToolCtx};
use cwoffee_engine::{Agent, AgentConfig};
use tokio::sync::mpsc;

static APPROVALS: AtomicU64 = AtomicU64::new(0);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8090/v1".into());
    let mode = std::env::args()
        .nth(2)
        .and_then(|s| ApprovalMode::parse(&s))
        .unwrap_or(ApprovalMode::Session);

    let provider = OpenAiProvider::new("llama.cpp", Flavor::LlamaCpp, base)?;

    let mut reg = Registry::new();
    reg.register(Box::new(ExecTool::new(&ExecCfg {
        timeout_secs: 30,
        ..Default::default()
    })));

    let (areq, mut ars) = mpsc::unbounded_channel();
    let gate = ApprovalGate::new(mode, areq);

    let workspace = std::env::current_dir()?;
    let ctx = ToolCtx { workspace: workspace.clone(), max_output_bytes: 16 * 1024, ..Default::default() };

    let mut agent = Agent::new(
        std::sync::Arc::new(provider),
        reg,
        gate,
        AgentConfig {
            model: "qwen3.8-flash-next-iq3_s".into(),
            max_tool_iters: 6,
            reasoning: Reasoning::Low,
            max_tokens: Some(900),
            ..Default::default()
        },
        ctx,
    );
    agent.prompt_mut().set_env(format!(
        "<env>\nos: linux\nworkspace: {}\nshell: sh\n</env>",
        workspace.display()
    ));

    // The approval responder: prints what it was asked and decides.
    tokio::spawn(async move {
        while let Some((req, reply)) = ars.recv().await {
            APPROVALS.fetch_add(1, Ordering::Relaxed);
            println!("\n[approval #{}] tool={} risk={:?}\n  {}", req.id, req.tool, req.risk, req.summary);
            // Auto-approve for the demo, but exercise the real round-trip.
            reply.send(ApprovalAnswer::AllowSession).ok();
        }
    });

    let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
    let cancel = agent.cancel_handle();

    let render = tokio::spawn(async move {
        render_events(&mut erx).await;
    });

    let task = "List the files in the current directory with `ls -la`, then tell me \
                how many entries there are and what language this project is most \
                likely written in. Use the exec tool.";

    println!("mode={:?}  task: {task}", agent.mode());
    let tel = agent.run_turn(task, vec![], &etx, 1).await;
    drop(etx);
    render.await?;

    println!("\n=== turn totals ===");
    println!("approvals asked : {}", APPROVALS.load(Ordering::Relaxed));
    println!("prompt tokens   : {} (cached {} = {:.0}%)",
        tel.prompt_tokens, tel.cached_tokens, tel.cache_hit_pct());
    println!("completion      : {}", tel.completion_tokens);
    println!("ttft            : {} ms", tel.ttft_ms);
    println!("raw prefill     : {:.0} tok/s", tel.server_prefill_tps);
    println!("effective prefill: {:.0} tok/s", tel.effective_prefill_tps());
    println!("decode          : {:.1} tok/s", tel.server_decode_tps);
    println!("spec accept     : {}/{} ({:.0}%)",
        tel.draft_accepted, tel.draft_n, tel.spec_accept_pct());
    println!("compactions     : {}", agent.prompt_mut().compactions);
    let _ = cancel;
    Ok(())
}

async fn render_events(rx: &mut mpsc::UnboundedReceiver<EngineEvent>) {
    use std::io::Write;
    let mut in_reasoning = false;
    let mut in_text = false;

    while let Some(e) = rx.recv().await {
        match e {
            EngineEvent::SessionChanged { session_id, .. } => {
                println!("(session changed -> {session_id})");
            }
            // Live mid-turn estimates: the examples print the final telemetry line.
            EngineEvent::TurnStats(_) => {}
            // The example has no UI to answer with, so the tool's own timeout
            // is what ends the wait.
            EngineEvent::QuestionRequested { question, .. } => {
                println!("(the model asks: {question} - nobody here to answer)")
            }
            EngineEvent::QuestionAnswered { .. } => {}
            EngineEvent::QuestionExpired { id } => {
                println!("[ask] question {id} closed without an answer");
            }
            EngineEvent::TurnStart { turn_id, model, .. } => {
                println!("=== turn {turn_id} ({model}) ===");
            }
            EngineEvent::TurnFiles { files, .. } => {
                for f in &files {
                    println!("(file) {} - {} bytes, {}", f.path, f.bytes, f.mime);
                }
            }
            EngineEvent::Reasoning(r) => {
                if !in_reasoning {
                    print!("\n[thinking] ");
                    in_reasoning = true;
                }
                print!("{r}");
                std::io::stdout().flush().ok();
            }
            EngineEvent::Text(t) => {
                if in_reasoning {
                    println!();
                    in_reasoning = false;
                }
                if !in_text {
                    print!("\n[answer] ");
                    in_text = true;
                }
                print!("{t}");
                std::io::stdout().flush().ok();
            }
            EngineEvent::ToolCallBegin { id, name } => {
                if in_reasoning { println!(); in_reasoning = false; }
                println!("\n[tool call] {name} ({id})");
            }
            EngineEvent::ToolCallReady { args, .. } => {
                println!("[parsed args] {}", args);
            }
            EngineEvent::ToolCallArgs(_) => {}
            EngineEvent::ToolCallParseError { raw, message, .. } => {
                println!("[PARSE FAIL] {message}\n  raw: {raw}");
            }
            EngineEvent::ToolExecStart { .. } => println!("[running...]"),
            EngineEvent::ToolExecOutput { stream, chunk, .. } => {
                let tag = match stream {
                    cwoffee_engine::ExecStream::Stdout => "",
                    cwoffee_engine::ExecStream::Stderr => "[err] ",
                };
                print!("{tag}{chunk}");
                std::io::stdout().flush().ok();
            }
            EngineEvent::ToolExecEnd { ok, exit_code, .. } => {
                println!("[tool done] ok={ok} exit={exit_code:?}");
            }
            EngineEvent::ApprovalRequested { id, tool, risk, summary, .. } => {
                println!("\n[approval #{id}] tool={tool} risk={risk:?}\n  {summary}");
            }
            EngineEvent::Usage(u) => {
                println!(
                    "[usage] prompt={} cached={} completion={} raw_prefill={:.0}/s decode={:.1}/s ttft={}ms",
                    u.prompt_tokens, u.cached_tokens, u.completion_tokens,
                    u.server_prefill_tps, u.server_decode_tps, u.ttft_ms
                );
            }
            EngineEvent::Note { message } => println!("\n  [note] {message}"),
            EngineEvent::Error { message, fatal } => {
                println!("\n[{}] {message}", if fatal { "FATAL" } else { "warn" });
            }
            EngineEvent::TurnEnd { finish, .. } => {
                println!("\n=== turn end: {finish:?} ===");
            }
        }
    }
}
