//! Drive the spreadsheet tools on one file, from the command line, with no model
//! in the loop.
//!
//! This exists because the interesting failures are not crashes: they are a model
//! giving up on a tool and unzipping the workbook instead. To see why, run the
//! tool the model ran and look at what came back.
//!
//!   cargo run -p cwoffee-engine --example sheet_probe -- <dir> <file> [json args]
//!
//! Repeats the calls the scripted scenarios assert, with no scenario: pass the
//! args the model would pass.

use std::path::PathBuf;

use cwoffee_engine::tools::web::SearchBackend;
use cwoffee_engine::tools::{ExecCfg, Registry, ToolCtx};
use serde_json::json;

#[tokio::main]
async fn main() {
    let mut argv = std::env::args().skip(1);
    let (Some(dir), Some(file)) = (argv.next(), argv.next()) else {
        eprintln!("usage: sheet_probe <workspace dir> <file> [json args ...]");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let registry = Registry::with_v1_tools_exec(SearchBackend::DuckDuckGo, ExecCfg::default());
    let ctx = ToolCtx {
        workspace: dir.clone(),
        max_output_bytes: 64 * 1024,
        ..Default::default()
    };

    let default_args = vec![
        json!({"path": file, "mode": "structure"}),
        json!({"path": file, "mode": "values", "sheet": "*", "max_cells": 4000}),
    ];
    let mut calls: Vec<(String, serde_json::Value)> = Vec::new();
    let raw: Vec<String> = argv.collect();
    if raw.is_empty() {
        for a in default_args {
            calls.push((a["mode"].as_str().unwrap_or("values").to_string(), a));
        }
        calls.push((
            "sheet_query".into(),
            json!({"path": file, "query": "SELECT COUNT(*) AS rows FROM t"}),
        ));
    } else {
        for a in raw {
            let v: serde_json::Value = serde_json::from_str(&a).expect("args must be JSON");
            let name = if v.get("query").is_some() { "sheet_query" } else { "sheet_read" };
            calls.push((name.to_string(), v));
        }
    }

    for (name, args) in calls {
        let tool = registry.get(&name).expect("tool exists");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = tool.run(args.clone(), &ctx, &tx).await;
        println!(
            "\n===== {name} {} -> ok={} truncated={} =====",
            args, r.ok, r.truncated
        );
        let body = if r.ok { r.output } else { r.error.unwrap_or_else(|| "<no error>".into()) };
        println!("{body}");
    }
}
