//! cwoffee tool shim — spike version.
//!
//! Purpose: prove that in a pi-cwoffee world the *tools stay Rust*. pi's tool loop
//! calls this binary (through a ~30-line extension), passes JSON args on stdin, and
//! gets a JSON envelope back. All policy lives HERE, where we can test it and ship it,
//! not in a tool description and not in a JS extension.
//!
//! Production shape is the same contract per tool, with the real engine's tools behind
//! it (exec under bwrap, sheet_sql with SQLITE_LIMIT_ATTACHED=0 + a progress-handler
//! wall clock, files/docs/net/xlsx, ask).

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool = args.get(1).cloned().unwrap_or_default();

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_else(|_| panic!("args must be a JSON object on stdin"));

    match tool.as_str() {
        "read_file" => {
            let path = v["path"].as_str().unwrap_or_else(|| panic!("read_file needs a string `path`"));
            read_file(path);
        }
        // Unknown tool: fail LOUDLY. A canned "ok" here would let the harness pass while
        // testing nothing — the same reason scripts/mock-llm.mjs 400s on an unknown prompt.
        other => panic!("no such rust tool: {}", other),
    }
}

fn read_file(path: &str) {
    // The guardrail is the engine's, not a promise in a prompt. This is the part that
    // must not move to the extension.
    if !path.starts_with("/etc/") || path.contains("..") {
        print!(r#"{{"ok":false,"error":"read_file refused: {}"}}"#, path);
        return;
    }
    match std::fs::read_to_string(path) {
        Ok(s) => print!(
            r#"{{"ok":true,"tool":"read_file","engine":"rust","bytes":{},"content":"{}"}}"#,
            s.len(),
            s.trim().replace('\\', "\\\\").replace('"', "\\\"")
        ),
        Err(e) => print!(r#"{{"ok":false,"error":"{}"}}"#, e),
    }
}
