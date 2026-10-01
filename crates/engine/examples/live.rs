//! Live smoke test against a real llama.cpp server.
//!
//!   cargo run -p cowry-engine --example live -- http://127.0.0.1:8090/v1
//!
//! Unit tests prove the parser; this proves the parser against the actual
//! bytes on the wire, including prefix-cache behaviour across two turns.

use cowry_engine::Provider;
use cowry_engine::provider::{
    ChatRequest, Delta, Message, Reasoning, ToolDef,
    openai::{Flavor, OpenAiProvider},
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8090/v1".into());

    let p = OpenAiProvider::new("llama.cpp", Flavor::LlamaCpp, base)?;
    println!("provider={} endpoint={}", p.id(), p.label());

    let tools = vec![ToolDef {
        name: "exec".into(),
        description: "Run a shell command and return stdout".into(),
        parameters: json!({
            "type": "object",
            "properties": { "cmd": { "type": "string", "description": "command to run" } },
            "required": ["cmd"]
        }),
    }];

    // ---- turn 1: plain answer, thinking on -------------------------------
    let mut req = ChatRequest::new("qwen3.8-flash-next-iq3_s", vec![
        Message::system("You are Cowry, a concise local assistant."),
        Message::user("Name three JSON parsers, one line each."),
    ]);
    req.tools = tools.clone();
    req.reasoning = Reasoning::Low;
    req.max_tokens = Some(400);

    println!("\n--- turn 1 (thinking on) ---");
    let tel1 = run(&p, &req).await?;

    // ---- turn 2: same prefix + one more turn ------------------------------
    // The point of this second call is the cache-hit number: if the prefix is
    // byte-stable, llama.cpp should report a large cached_tokens here.
    req.messages.push(Message::assistant("noted"));
    req.messages.push(Message::user("Now name two. Same format."));
    println!("\n--- turn 2 (same prefix, expect cache hit) ---");
    let tel2 = run(&p, &req).await?;

    println!("\n=== cache behaviour ===");
    println!(
        "turn1: prompt={} cached={} raw_prefill={:.0}/s decode={:.1}/s spec={}/{}",
        tel1.prompt_tokens, tel1.cached_tokens, tel1.server_prefill_tps,
        tel1.server_decode_tps, tel1.draft_accepted, tel1.draft_n
    );
    println!(
        "turn2: prompt={} cached={} ({:.0}% hit) raw_prefill={:.0}/s decode={:.1}/s ttft={}ms eff_prefill={:.0}/s",
        tel2.prompt_tokens, tel2.cached_tokens, tel2.cache_hit_pct(),
        tel2.server_prefill_tps, tel2.server_decode_tps, tel2.ttft_ms,
        tel2.effective_prefill_tps()
    );
    if tel2.cached_tokens == 0 {
        println!("!! no cache hit on turn 2 - prefix is not byte-stable, investigate");
    }
    Ok(())
}

async fn run(
    p: &OpenAiProvider,
    req: &ChatRequest,
) -> Result<cowry_engine::TurnTelemetry, Box<dyn std::error::Error>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // No spawn: the provider is borrowed, and `join!` polls both futures on this
    // task. We terminate on Delta::Done rather than channel close, because `tx`
    // is owned by this scope and therefore never drops while we are reading.
    let stream_fut = p.stream(req, &tx);

    let mut text = String::new();
    let mut reasoning_len = 0usize;
    let mut tel = cowry_engine::TurnTelemetry::default();

    let recv_fut = async {
        while let Some(d) = rx.recv().await {
            match d {
                Delta::Text(t) => {
                    print!("{t}");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                    text.push_str(&t);
                }
                Delta::Reasoning(r) => reasoning_len += r.len(),
                Delta::ToolCallBegin { id, name } => println!("\n[tool begin] {name} ({id})"),
                Delta::ToolCallArgs(a) => print!("[args] {a}"),
                Delta::ToolCallReady { name, arguments, .. } => {
                    println!("\n[tool ready] {name} args={arguments}")
                }
                Delta::ReasoningEnd { .. } => {}
                Delta::Telemetry(t) => tel = t,
                Delta::Done(f) => {
                    println!("\n[done] {f:?}");
                    break;
                }
            }
        }
    };

    let (res, ()) = tokio::join!(stream_fut, recv_fut);
    res?;

    println!("[reasoning chars] {reasoning_len}  [answer chars] {}", text.len());
    Ok(tel)
}
