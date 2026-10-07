//! Tools and the tool loop's view of them (PLAN.md §6).
//!
//! v1 ships `exec`, `read_file`, `write_file`, `edit_file`, `web_fetch` and
//! `web_search`. Deliberately few: every extra tool is more schema in the prompt
//! (which costs prefill on a slow local model) and more chances for a quantized
//! model to emit a call it shouldn't.

pub mod ask;
pub mod changes;
pub mod docs;
pub mod exec;
pub mod files;
pub mod path;
pub mod repair;
pub mod searxng;
pub mod web;
pub mod xlsx;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
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
/// What a call could do to the user. This drives the approval gate, and the
/// split is deliberately about **harm the user would notice**, not about which
/// syscall gets made:
///
///   Harmless  - nothing the user did not ask for changes. Reading a file inside
///               the workspace, searching or fetching the web, asking the user a
///               question. Never prompts, in any mode: an approval prompt on
///               `web_search` trains people to click through prompts without
///               reading them, which is worse than no prompt at all.
///   Workspace - changes a file inside the workspace that already exists. The
///               agent creating its own output is the point of the product;
///               overwriting something the user wrote is a different thing.
///   Sensitive - arbitrary code, or a path **outside** the workspace. Prompts in
///               ask and session modes.
///
/// `Tool::risk_for` decides this per call, because for file tools the answer
/// depends on the path and on what is already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Risk {
    /// The default because "assume nothing dangerous until a tool says so" is
    /// the wrong way round; it is the *gate* that defaults to Harmless for
    /// tools which never implement `risk_for`, and those are read-only helpers.
    #[default]
    Harmless,
    Workspace,
    Sensitive,
}

/// Risk of one specific call, plus the paths outside the workspace it wants.
///
/// Escapes are carried separately from the level because they need a different
/// grant: "yes to exec" may be remembered for the session, "yes to reading
/// ~/.ssh/id_ed25519" may not. The approved paths are handed to the tool for
/// this call only.
#[derive(Debug, Clone, Default)]
pub struct CallRisk {
    pub level: Risk,
    /// Absolute, already-resolved paths outside the workspace this call needs.
    pub escapes: Vec<String>,
}

impl CallRisk {
    pub fn harmless() -> Self {
        Self::default()
    }
    pub fn at(level: Risk) -> Self {
        Self { level, escapes: Vec::new() }
    }
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
    /// Files this call produced inside the workspace. The UI renders these as
    /// clickable attachments; the model never sees them, so they cost no
    /// prompt bytes and cannot invalidate the prefix cache.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<OutputFile>,
    /// What the call did to files, for the tool card: `+N -M` in the header and
    /// a diff when expanded. UI-only, like `files` - a diff of what the model
    /// just wrote is the most expensive possible thing to put back in a prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<changes::FileChange>,
}

/// A file the agent produced, described for the UI rather than the model.
/// `path` is always relative to the workspace: an absolute path would let the
/// front end be talked into opening anything on the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFile {
    pub path: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
}

impl OutputFile {
    /// Describe a file on disk. `path` is stored workspace-relative on purpose:
    /// an absolute path in this field would let a model-authored tool result
    /// point the UI at anything on the machine.
    pub fn from_path(workspace: &std::path::Path, path: &std::path::Path) -> OutputFile {
        let rel = path::display_rel(workspace, path);
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        OutputFile {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel.clone()),
            path: rel,
            mime: mime_for(path).to_string(),
            bytes,
        }
    }
}

/// Extension-based mime. Deliberately small: it exists so the UI can pick an
/// icon and decide whether to open in place, not to identify file contents.
pub fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "zip" => "application/zip",
        "toml" => "application/toml",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" | "ts" => "text/javascript",
        "css" => "text/css",
        _ => "application/octet-stream",
    }
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            exit_code: None,
            truncated: false,
            error: None,
            files: Vec::new(),
            changes: Vec::new(),
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let m = msg.into();
        Self {
            ok: false,
            output: String::new(),
            exit_code: None,
            truncated: false,
            error: Some(m),
            files: Vec::new(),
            changes: Vec::new(),
        }
    }

    /// Attach a produced file. Returns self so call sites read in one chain.
    pub fn with_file(mut self, f: OutputFile) -> Self {
        self.files.push(f);
        self
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
    /// Route a question to whoever is watching. `None` means this client cannot
    /// answer, and `ask_user` says so rather than waiting for a reply that can
    /// never come.
    pub ask: Option<ask::QuestionTx>,
    /// The engine event stream, for tools that have to tell every front end
    /// something on their own initiative. `ask_user` uses it to say a question
    /// closed without an answer: the reply channel it was waiting on is gone, so
    /// nothing else can report that to the window still showing the card.
    pub events: Option<tokio::sync::mpsc::UnboundedSender<crate::event::EngineEvent>>,
    /// Paths outside the workspace the **user approved for this one call**.
    /// Rebuilt per call by the agent loop; never inherited by the next call, and
    /// never widened by a session-level "allow for session" answer.
    pub permits: Vec<std::path::PathBuf>,
}

impl Default for ToolCtx {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            max_output_bytes: 32 * 1024,
            ask: None,
            events: None,
            permits: Vec::new(),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    /// Worst case for this tool, used when a call cannot be inspected (and as
    /// the default for `risk_for`).
    fn risk(&self) -> Risk;

    /// Risk of *this* call. File tools lower it for reading inside the workspace
    /// and raising it to Sensitive when the path lands outside; the gate needs
    /// the paths so the prompt can name them.
    ///
    /// The default is deliberately the tool's worst case: a tool that does not
    /// think about this gets the conservative answer rather than the free pass.
    fn risk_for(&self, _args: &Value, _ctx: &ToolCtx) -> CallRisk {
        CallRisk::at(self.risk())
    }

    /// `args` has already been through `repair::parse_or_repair`.
    async fn run(&self, args: Value, ctx: &ToolCtx, sink: &ExecSink) -> ToolResult;
}

/// Exec knobs that come from config. Kept separate from ExecTool so the registry
/// remains the single place that decides which tools exist and in what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecCfg {
    pub timeout_secs: u64,
    pub sandbox_enabled: bool,
}

impl Default for ExecCfg {
    fn default() -> Self {
        Self {
            timeout_secs: crate::config::default_exec_timeout(),
            sandbox_enabled: true,
        }
    }
}

/// Name-indexed tool set.
#[derive(Default)]
pub struct Registry {
    // Arc, not Box: a tool call is spawned so a panic inside one tool cannot
    // take the process with it (a web-fetch panic closed the app mid-session).
    // Spawning needs shared ownership of the tool across the task boundary.
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    search: Option<Arc<web::WebSearchTool>>,
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
        Self::with_v1_tools_exec(search, ExecCfg::default())
    }

    /// The config-driven variant. Without this the `[sandbox]` block parses,
    /// validates, and then goes nowhere: the registry would build ExecTool with
    /// its own defaults and the user's timeout and sandbox settings would be
    /// silently ignored.
    pub fn with_v1_tools_exec(search: web::SearchBackend, exec: ExecCfg) -> Self {
        let mut r = Self::new();
        r.register(Box::new(exec::ExecTool {
            timeout_secs: exec.timeout_secs,
            // `enabled = false` is a debugging escape hatch; it flips the tool
            // into unsandboxed mode, which is why it is spelled allow_unsandboxed
            // on the receiving side.
            allow_unsandboxed: !exec.sandbox_enabled,
        }));
        r.register(Box::new(files::ReadFileTool));
        r.register(Box::new(files::WriteFileTool));
        r.register(Box::new(xlsx::WriteXlsxTool));
        r.register(Box::new(web::WebFetchTool::default()));
        // Kept as a typed handle as well as a tool: the search provider can change
        // while the app runs (the settings card), and the only safe way to change
        // it is inside the tool. Its name, description and schema are prompt bytes,
        // so a rebuild of the tool list is not an option.
        let search_tool = std::sync::Arc::new(web::WebSearchTool::from_backend(search));
        r.search = Some(search_tool.clone());
        r.register_arc(search_tool);
        // Appended, never inserted: this list is the tool order in the system
        // prompt, and reordering it invalidates the llama.cpp prefix cache for
        // every existing session.
        r.register(Box::new(web::DownloadTool::default()));
        // Also appended. Adding a tool costs prompt bytes and gives a small
        // quantized model one more choice to get wrong, which is why edit_file
        // exists at all only because write_file's answer to a one-line change
        // is to re-emit the entire file.
        r.register(Box::new(files::EditFileTool));
        r.register(Box::new(ask::AskUserTool::default()));
        r
    }

    pub fn register(&mut self, t: Box<dyn Tool>) {
        let n = t.name().to_string();
        self.tools.insert(n.clone(), Arc::from(t));
        self.order.push(n);
    }

    /// Register an already-shared tool, for the cases where the caller needs the
    /// same handle back (see `search_tool`).
    pub fn register_arc(&mut self, t: Arc<dyn Tool>) {
        let n = t.name().to_string();
        self.tools.insert(n.clone(), t);
        self.order.push(n);
    }

    /// The search tool, typed. Switching providers at runtime goes through this
    /// rather than rebuilding the registry, because the registry order and the
    /// tool's schema are prompt bytes.
    pub fn search_tool(&self) -> Option<Arc<web::WebSearchTool>> {
        self.search.clone()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Shared handle for running a tool on its own task.
    pub fn get_shared(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
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
            fn risk(&self) -> Risk { Risk::Harmless }
            async fn run(&self, _a: Value, _c: &ToolCtx, _s: &ExecSink) -> ToolResult {
                ToolResult::ok("stub")
            }
        }
        Box::new(S(name.into()))
    }
}

#[cfg(test)]
mod cfg_tests {
    use super::*;

    /// The `[sandbox]` block has to reach the tool, not just parse. The schema
    /// description carries the ceiling, so this asserts the whole path:
    /// config value -> registry -> the number the model is actually told.
    #[test]
    fn sandbox_config_reaches_the_exec_tool() {
        let r = Registry::with_v1_tools_exec(
            web::SearchBackend::DuckDuckGo,
            ExecCfg { timeout_secs: 777, sandbox_enabled: true },
        );
        let schema = r.get("exec").unwrap().schema();
        let d = schema["properties"]["timeout"]["description"].as_str().unwrap();
        assert!(d.contains("777s"), "configured ceiling not surfaced: {d}");
        assert!(!d.contains("120s"), "default leaked through the config value: {d}");
    }

    /// Order is load-bearing for the prefix cache; assert it rather than trust it.
    ///
    /// `edit_file` is appended at the end rather than slotted next to
    /// `write_file`: inserting would re-render the tool list in the system prompt
    /// and invalidate every existing session's prefix. A new tool changes those
    /// bytes either way, but appending keeps the diff a pure suffix.
    #[test]
    fn tool_order_is_fixed() {
        let r = Registry::with_v1_tools();
        let names: Vec<String> = r.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            vec![
                "exec",
                "read_file",
                "write_file",
                "write_xlsx",
                "web_fetch",
                "web_search",
                "download_file",
                "edit_file",
                "ask_user",
            ]
        );
    }
}
