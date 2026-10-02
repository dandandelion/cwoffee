//! Tools and the tool loop's view of them (PLAN.md §6).
//!
//! v1 ships four: `exec`, `read_file`, `write_file`, `web_fetch`, plus
//! `web_search`. Deliberately few: every extra tool is more schema in the prompt
//! (which costs prefill on a slow local model) and more chances for a quantized
//! model to emit a call it shouldn't.

pub mod docs;
pub mod exec;
pub mod files;
pub mod path;
pub mod repair;
pub mod web;
pub mod xlsx;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::event::ExecStream;

/// One piece of live tool output on its way to the front end. The tool layer
/// produces these; the agent loop turns them into `EngineEvent::ToolExecOutput`.
#[derive(Debug, Clone)]
pub struct ExecChunk {
    pub stream: ExecStream,
    pub chunk: String,
}

/// Live output while a tool runs (stdout/stderr lines, progress).
pub type ExecSink = mpsc::UnboundedSender<ExecChunk>;
/// How much a tool is allowed to hurt you. Drives the approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    /// No side effects at all. Always allowed.
    ReadOnly,
    /// Writes inside the workspace.
    Mutating,
    /// Runs arbitrary code, or reaches the network.
    Exec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    /// Fed back to the model. Must be truncated by the tool, not the caller:
    /// a 200k-line `ls` will blow the context window and cost real prefill time.
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// True when `output` was truncated before reaching the model.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { ok: true, output: output.into(), exit_code: None, truncated: false, error: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let m = msg.into();
        Self { ok: false, output: String::new(), exit_code: None, truncated: false, error: Some(m) }
    }

    /// Whatever the tool actually said, success or failure. Keeps call sites and
    /// tests from repeating the ok/output-vs-error dance.
    pub fn text(&self) -> String {
        match (&self.error, self.output.is_empty()) {
            (Some(e), _) => e.clone(),
            (None, false) => self.output.clone(),
            (None, true) => String::new(),
        }
    }
}

/// Live output while a tool runs (stdout/stderr lines, progress).
/// What a tool is allowed to touch.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// Directory the agent operates in. write_file is confined to it.
    pub workspace: std::path::PathBuf,
    /// Hard cap on bytes returned to the model.
    pub max_output_bytes: usize,
}

impl Default for ToolCtx {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            max_output_bytes: 32 * 1024,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn risk(&self) -> Risk;

    /// `args` has already been through `repair::parse_or_repair`.
    async fn run(&self, args: Value, ctx: &ToolCtx, sink: &ExecSink) -> ToolResult;
}

/// Name-indexed tool set.
#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, Box<dyn Tool>>,
    order: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The v1 tool set, in a **fixed** order.
    ///
    /// Order is not cosmetic: tool schemas are serialized into the system
    /// prompt, so a different order changes the prompt bytes and invalidates
    /// the llama.cpp prefix cache for every turn in every session. If a tool is
    /// added later, append it; never reorder.
    pub fn with_v1_tools() -> Self {
        Self::with_v1_tools_search(web::SearchBackend::DuckDuckGo)
    }

    /// Same fixed order as `with_v1_tools`; only the search backend differs.
    /// There is exactly one place that decides the order, because order changes
    /// the prompt bytes and invalidates the prefix cache for every later turn.
    ///
    /// Adding a tool also changes those bytes, so every existing session takes
    /// one cold-prefill turn after an upgrade. That is unavoidable and cheaper
    /// than versioning the tool set.
    pub fn with_v1_tools_search(search: web::SearchBackend) -> Self {
        let mut r = Self::new();
        r.register(Box::new(exec::ExecTool::default()));
        r.register(Box::new(files::ReadFileTool));
        r.register(Box::new(files::WriteFileTool));
        r.register(Box::new(xlsx::WriteXlsxTool));
        r.register(Box::new(web::WebFetchTool::default()));
        r.register(Box::new(web::WebSearchTool::from_backend(search)));
        r
    }

    pub fn register(&mut self, t: Box<dyn Tool>) {
        let n = t.name().to_string();
        self.tools.insert(n.clone(), t);
        self.order.push(n);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Schemas in a **stable order**. Tool order changes the prompt bytes, which
    /// invalidates the llama.cpp prefix cache for every subsequent turn.
    pub fn defs(&self) -> Vec<crate::provider::ToolDef> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n))
            .map(|t| crate::provider::ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.schema(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Clamp output to the context budget, marking that we did.
pub fn clamp(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    // Cut on a char boundary, keep the head (most command output of interest
    // is at the start) and tell the model how much was dropped.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    let mut out = s[..end].to_string();
    out.push_str(&format!("\n… [{dropped} bytes truncated]"));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_defs_order_is_stable() {
        let mut r = Registry::new();
        r.register(stub("zeta"));
        r.register(stub("alpha"));
        let a: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
        for _ in 0..16 {
            let b: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
            assert_eq!(a, b, "tool order drifted; prompt bytes would change");
        }
    }

    #[test]
    fn clamp_keeps_char_boundaries() {
        let (out, trunc) = clamp("héllo 世界".repeat(50), 20);
        assert!(trunc);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.contains("truncated"));
    }

    #[test]
    fn clamp_noop_when_short() {
        let (out, trunc) = clamp("short".into(), 100);
        assert_eq!(out, "short");
        assert!(!trunc);
    }

    fn stub(name: &str) -> Box<dyn Tool> {
        struct S(String);
        #[async_trait]
        impl Tool for S {
            fn name(&self) -> &str { &self.0 }
            fn description(&self) -> &str { "stub" }
            fn schema(&self) -> Value { serde_json::json!({"type":"object"}) }
            fn risk(&self) -> Risk { Risk::ReadOnly }
            async fn run(&self, _a: Value, _c: &ToolCtx, _s: &ExecSink) -> ToolResult {
                ToolResult::ok("stub")
            }
        }
        Box::new(S(name.into()))
    }
}
