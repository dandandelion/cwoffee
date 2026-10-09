// One sandboxed exec call, with the egress mode chosen on the command line.
//
//   cargo run -p cwoffee-engine --example exec_probe -- proxy <dir> "curl -sS .../"
//   cargo run -p cwoffee-engine --example exec_probe -- off   <dir> "wget -q .../"
//
// This exists because network behaviour cannot be checked from the config file: the
// only question that matters is whether a command inside the sandbox, with the
// environment the tool really gives it, reaches something. Everything in here is the
// tool's own path - ExecCfg, ExecTool::new, the same bwrap argument list - so a
// result here is a result the model would get.

use cwoffee_engine::tools::exec::ExecTool;
use cwoffee_engine::tools::net::{self, NetCfg, NetMode};
use cwoffee_engine::tools::{ExecChunk, Tool, ToolCtx};
use serde_json::json;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| {
        eprintln!("usage: exec_probe <off|proxy|open> [allow,comma] <workspace> <command>");
        std::process::exit(2);
    });
    let second = args.next().unwrap_or_default();
    let (allow, workspace, cmd) = if second.starts_with('/') {
        (None, second, args.next().unwrap_or_default())
    } else {
        (
            Some(second.split(',').map(|s| s.trim().to_string()).collect::<Vec<_>>()),
            args.next().unwrap_or_default(),
            args.next().unwrap_or_default(),
        )
    };
    let mode = match NetMode::parse(&mode) {
        Some(m) => m,
        None => {
            eprintln!("not a mode: {mode} (off, proxy, open)");
            std::process::exit(2);
        }
    };
    if cmd.is_empty() || workspace.is_empty() {
        eprintln!("need a workspace and a command");
        std::process::exit(2);
    }

    let cfg = cwoffee_engine::tools::ExecCfg {
        timeout_secs: 120,
        sandbox_enabled: true,
        net: NetCfg {
            mode,
            allow: allow.unwrap_or_else(net::default_allow),
            max_bytes_per_call: 512 << 20,
        },
    };
    println!("egress: {}", cfg.net.describe());

    let tool = ExecTool::new(&cfg);
    let ctx = ToolCtx {
        workspace: std::path::PathBuf::from(&workspace),
        max_output_bytes: 64 * 1024,
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ExecChunk>();
    let pump = tokio::spawn(async move {
        while let Some(c) = rx.recv().await {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), c.chunk.as_bytes());
        }
    });
    let r = tool.run(json!({"cmd": cmd}), &ctx, &tx).await;
    drop(tx);
    let _ = pump.await;

    println!(
        "ok={} exit={:?} truncated={}\n--- output ---\n{}",
        r.ok,
        r.exit_code,
        r.truncated,
        r.output
    );
    if let Some(e) = &r.error {
        println!("--- error ---\n{e}");
    }
}
