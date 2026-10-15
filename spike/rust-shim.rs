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

/// The guardrail: the file must live inside the workspace this process was started for.
///
/// `CWOFFEE_WORKSPACE` is set by the bridge on every spawn (`PiHost::ensure_child`), so
/// confinement is a named fact and not a guess from an inherited cwd. An out-of-workspace
/// path is refused **naming the root that refused it**: a refusal that does not say what
/// it is confined to sends a model flailing through ten variants of one path, and a
/// recorded transcript from the desktop app shows exactly that waste.
///
/// The earlier spike version allowed only `/etc/*`, which read as "the engine guardrail
/// is broken" to a user whose files were nowhere near /etc. A demo rule that a user cannot
/// distinguish from a real one is the failure mode to design out.
fn read_file(path: &str) {
    let root = match std::env::var("CWOFFEE_WORKSPACE") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => match std::env::current_dir() {
            Ok(d) => d.to_string_lossy().to_string(),
            Err(e) => return deny(&format!("there is no workspace root to read within: {e}")),
        },
    };
    if path.contains("..") {
        return deny(&format!("{path}: path traversal is refused (workspace root: {root})"));
    }
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{root}/{path}")
    };
    // canonicalize resolves symlinks, which is the point: a link out of the workspace is
    // an out-of-workspace read, whatever the path the model was given looked like.
    let canon = match std::path::PathBuf::from(&joined).canonicalize() {
        Ok(c) => c,
        Err(e) => return deny(&format!("{joined}: {e} (workspace root: {root})")),
    };
    if !canon.starts_with(&root) {
        return deny(&format!(
            "{} is outside the workspace root {root}",
            canon.display()
        ));
    }
    let path = canon.to_string_lossy().to_string();
    match std::fs::read_to_string(&path) {
        Ok(s) => print!(
            r#"{{"ok":true,"tool":"read_file","engine":"rust","bytes":{},"content":"{}"}}"#,
            s.len(),
            s.trim().replace('\\', "\\\\").replace('"', "\\\"")
        ),
        Err(e) => print!(r#"{{"ok":false,"error":"{}"}}"#, e),
    }
}
fn deny(msg: &str) {
    print!(r#"{{"ok":false,"error":"read_file refused: {}"}}"#, msg);
}

