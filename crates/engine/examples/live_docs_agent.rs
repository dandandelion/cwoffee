//! Live end-to-end: real model -> tools -> files on disk.
//!
//! Unit tests prove the writers produce valid packages. This proves the model can
//! actually drive them: choosing write_xlsx over write_file, producing plausible
//! arguments, and leaving files that open afterwards. The output is read back
//! with pdftotext, so "the tool ran" is not the success criterion - "a person can
//! read the file" is.
use std::path::PathBuf;

use cwoffee_engine::agent::{Agent, AgentConfig};
use cwoffee_engine::approval::{ApprovalAnswer, ApprovalGate, ApprovalMode};
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::provider::openai::{Flavor, OpenAiProvider};
use cwoffee_engine::provider::Reasoning;
use cwoffee_engine::tools::{Registry, ToolCtx};
use tokio::sync::mpsc;

const TASK: &str = "Make two deliverables in the workspace:\n\
1) budget.xlsx with a header row (item, cost) and three rows of numbers: rent 1200, food 300, power 90.\n\
2) summary.pdf: a short document with a heading and one sentence stating the total.\n\
Use the write_xlsx tool for the spreadsheet and write_file with a .pdf path for the document.";

async fn render(rx: &mut mpsc::UnboundedReceiver<EngineEvent>) {
    // Args arrive separately from the call, so keep the id -> name mapping to
    // print them together.
    let mut names: std::collections::HashMap<String, String> = Default::default();
    while let Some(ev) = rx.recv().await {
        match ev {
            EngineEvent::ToolCallBegin { id, name } => {
                println!("tool ▸ {name}");
                names.insert(id, name);
            }
            EngineEvent::ToolCallReady { id, args } => {
                let n = names.get(&id).cloned().unwrap_or_else(|| "?".into());
                println!("  {n} args {}", args.to_string());
            }
            EngineEvent::ToolCallParseError { raw, message, .. } => {
                let r: String = raw.chars().take(140).collect();
                println!("  parse error: {message} raw={r}");
            }
            EngineEvent::ToolExecEnd { id, ok, exit_code, .. } => {
                let n = names.get(&id).cloned().unwrap_or_else(|| "?".into());
                println!("  {n} -> {} exit={exit_code:?}", if ok { "ok" } else { "FAIL" });
            }
            EngineEvent::Text(t) => print!("{t}"),
            EngineEvent::Error { message, .. } => println!("\n[error] {message}"),
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8090/v1".into());

    let workspace: PathBuf =
        std::env::temp_dir().join(format!("cwoffee_docs_agent_{}", std::process::id()));
    std::fs::create_dir_all(&workspace)?;

    let provider = OpenAiProvider::new("llama.cpp", Flavor::LlamaCpp, base)?;
    let registry = Registry::with_v1_tools();

    let (areq, mut ars) = mpsc::unbounded_channel();
    let gate = ApprovalGate::new(ApprovalMode::Auto, areq);

    let ctx = ToolCtx { workspace: workspace.clone(), max_output_bytes: 16 * 1024 };

    let mut agent = Agent::new(
        std::sync::Arc::new(provider),
        registry,
        gate,
        AgentConfig {
            model: "qwen3.8-flash-next-iq3_s".into(),
            max_tool_iters: 8,
            reasoning: Reasoning::Low,
            max_tokens: Some(1400),
        },
        ctx,
    );
    agent.prompt_mut().set_env(format!(
        "<env>\nos: linux\nworkspace: {}\nshell: sh\n</env>",
        workspace.display()
    ));

    tokio::spawn(async move {
        while let Some((req, reply)) = ars.recv().await {
            println!("\n[approval] tool={} risk={:?}\n  {}", req.tool, req.risk, req.summary);
            reply.send(ApprovalAnswer::AllowSession).ok();
        }
    });

    let (etx, mut erx) = mpsc::unbounded_channel::<EngineEvent>();
    let render = tokio::spawn(async move { render(&mut erx).await });

    println!("task: {TASK}\n");
    let tel = agent.run_turn(TASK, vec![], &etx).await;
    drop(etx);
    render.await?;

    println!("\n=== files in {} ===", workspace.display());
    let mut names: Vec<String> = Vec::new();
    for e in std::fs::read_dir(&workspace)? {
        let p = e?.path();
        let meta = std::fs::metadata(&p)?;
        names.push(p.file_name().unwrap().to_string_lossy().to_string());
        println!("  {} ({} bytes)", names.last().unwrap(), meta.len());
    }

    // Read the artifacts back: a file that exists is not a file a person can open.
    let pdf = workspace.join("summary.pdf");
    if pdf.exists() {
        println!("\n=== pdftotext summary.pdf ===");
        match std::process::Command::new("pdftotext").arg(&pdf).arg("-").output() {
            Ok(o) => println!("{}", String::from_utf8_lossy(&o.stdout)),
            Err(e) => println!("pdftotext unavailable: {e}"),
        }
    } else {
        println!("\nsummary.pdf was NOT produced (found: {names:?})");
    }

    println!(
        "prompt={} cached={} decode={:.1}/s",
        tel.prompt_tokens,
        tel.cached_tokens,
        tel.server_decode_tps
    );
    println!("kept at {}", workspace.display());
    Ok(())
}
