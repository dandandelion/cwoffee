//! Live check of the network tools. Parsing is unit-tested; this proves the
//! tools actually work against real servers, which no fixture can show.
//!
//!   cargo run -p cowry-engine --example live_web

use cowry_engine::tools::Tool;
use cowry_engine::tools::web::{WebFetchTool, WebSearchTool};
use cowry_engine::tools::ToolCtx;
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let ctx = ToolCtx { workspace: ".".into(), max_output_bytes: 1200 };

    let fetch = WebFetchTool::default();
    let search = WebSearchTool::default();

    println!("=== web_fetch: example.com ===");
    let r = fetch.run(json!({"url": "https://example.com/"}), &ctx, &tx).await;
    show(&r);

    println!("\n=== web_fetch: a real docs page (HTML -> text) ===");
    let r = fetch
        .run(json!({"url": "https://doc.rust-lang.org/book/ch01-00-getting-started.html"}), &ctx, &tx)
        .await;
    show(&r);

    println!("\n=== web_fetch: JSON endpoint ===");
    let r = fetch.run(json!({"url": "http://127.0.0.1:8090/health"}), &ctx, &tx).await;
    show(&r);

    println!("\n=== web_fetch: SSRF guards (must all be refused) ===");
    for url in [
        "file:///etc/passwd",
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]/",
        "gopher://example.com",
    ] {
        let r = fetch.run(json!({"url": url}), &ctx, &tx).await;
        println!(
            "  {url}\n    -> {}",
            if r.ok { "!!! ALLOWED !!!".to_string() } else { format!("refused: {}", r.error.unwrap()) }
        );
    }

    println!("\n=== web_search ===");
    let r = search.run(json!({"query": "llama.cpp --cache-reuse prefix caching"}), &ctx, &tx).await;
    show(&r);

    println!("\n=== web_search: hostile query ===");
    let r = search.run(json!({"query": "what is \"AND\" OR NOT (a:b)"}), &ctx, &tx).await;
    show(&r);
}

fn show(r: &cowry_engine::tools::ToolResult) {
    if r.ok {
        let head: String = r.output.chars().take(700).collect();
        let indented = head.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
        let clamp = if r.truncated { ", clamped" } else { "" };
        println!("  ok ({} bytes{clamp}):\n{indented}\n", r.output.len());
    } else {
        println!("  err: {}", r.error.as_deref().unwrap_or(""));
    }
}
