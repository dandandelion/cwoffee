//! Live check of the network tools. Parsing is unit-tested; this proves the
//! tools actually work against real servers, which no fixture can show.
//!
//!   cargo run -p cwoffee-engine --example live_web

use cwoffee_engine::tools::Tool;
use cwoffee_engine::tools::web::{WebFetchTool, WebSearchTool};
use cwoffee_engine::tools::ToolCtx;
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

    println!("\n=== download_file: the chakra-ui docs dump that web_fetch truncated ===");
    let dl = cwoffee_engine::tools::web::DownloadTool::default();
    let r = dl
        .run(
            json!({ "url": "https://chakra-ui.com/llms.txt" }),
            &ctx,
            &tx,
        )
        .await;
    show(&r);
    println!("  files: {:?}", r.files.iter().map(|f| (&f.path, f.bytes, &f.mime)).collect::<Vec<_>>());

    println!("\n=== download_file: refuse to clobber ===");
    let r2 = dl.run(json!({ "url": "https://chakra-ui.com/llms.txt" }), &ctx, &tx).await;
    show(&r2);

    println!("\n=== download_file: oversized response is refused, not truncated ===");
    let small = cwoffee_engine::tools::web::DownloadTool {
        max_bytes: 200,
        ..Default::default()
    };
    let r3 = small
        .run(
            json!({ "url": "https://chakra-ui.com/llms.txt", "path": "too-big.txt" }),
            &ctx,
            &tx,
        )
        .await;
    show(&r3);
    let leftover = std::path::Path::new(&ctx.workspace).join("too-big.txt");
    println!("  partial file left behind: {}", leftover.exists());

    println!("\n=== download_file: path escape is refused ===");
    let r4 = dl
        .run(
            json!({ "url": "https://chakra-ui.com/llms.txt", "path": "../escaped.txt" }),
            &ctx,
            &tx,
        )
        .await;
    show(&r4);

    println!("\n=== web_search ===");
    let r = search.run(json!({"query": "llama.cpp --cache-reuse prefix caching"}), &ctx, &tx).await;
    show(&r);

    println!("\n=== web_search: hostile query ===");
    let r = search.run(json!({"query": "what is \"AND\" OR NOT (a:b)"}), &ctx, &tx).await;
    show(&r);
}

fn show(r: &cwoffee_engine::tools::ToolResult) {
    if r.ok {
        let head: String = r.output.chars().take(700).collect();
        let indented = head.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
        let clamp = if r.truncated { ", clamped" } else { "" };
        println!("  ok ({} bytes{clamp}):\n{indented}\n", r.output.len());
    } else {
        println!("  err: {}", r.error.as_deref().unwrap_or(""));
    }
}
