//! Tools and the tool loop's view of them (PLAN.md §6).
//!
//! v1 ships `exec`, `read_file`, `write_file`, `edit_file`, `web_fetch` and
//! `web_search`. Deliberately few: every extra tool is more schema in the prompt
//! (which costs prefill on a slow local model) and more chances for a quantized
//! model to emit a call it shouldn't.

pub mod ask;
pub mod changes;
pub mod discover;
pub mod docs;
pub mod exec;
pub mod files;
pub mod memory;
pub mod path;
pub mod repair;
pub mod sheet_sql;
pub mod net;
pub mod sheets;
pub mod scratch;
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
    /// Tools this call made available to the model, named by the registry that owns them.
    /// Only `explore` fills this, and it never reaches the model as text: the extension reads
    /// it to widen the declared tool set, and the transcript card can show what the call
    /// unlocked. A capability claim belongs to the tool that granted it, not to whatever
    /// process happens to be hosting the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activates: Vec<String>,
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
    /// Whether this is a deliverable to open from the chat or a file the model
    /// changed on its way through a codebase. The transcript treats them
    /// differently, so the distinction is carried rather than re-guessed by the
    /// renderer from a filename.
    #[serde(default)]
    pub kind: FileKind,
}

/// Which kind of file this is, from the reader's point of view.
///
/// The complaint this answers: implementing one feature in a real repository
/// touched 38 files, and the turn printed 38 chips for them - a strip of chips
/// in which the one thing worth clicking (the HTML page the model also wrote)
/// was invisible. Neither category is more important than the other; they want
/// different shapes. A deliverable wants a chip you can click. A codebase edit
/// wants a count and a list you can open when you want one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Something to open from the transcript: a downloaded file, a generated
    /// document or spreadsheet, a standalone HTML/SVG page, an image.
    Artifact,
    /// A file the model wrote or edited as part of working in a tree.
    Edit,
    /// A file the model wanted for itself: a reference download, a fetched dataset,
    /// a scratch dump. It lives under the workspace's `.cws/` folder so it stays out of
    /// the person's way, and the transcript keeps it out of the deliverable strip for
    /// the same reason. Opening it is still one click; it is just not presented as
    /// something the person asked for.
    Scratch,
    /// A file the model wrote into `.cws/assistant_attachments/` in order to hand it
    /// over. This is the only kind the transcript attaches: the folder *is* the decision,
    /// so a probe file or a scaffold cannot become a deliverable just by being written.
    Attachment,
    /// A row stored before the distinction existed. Rendered as it always was:
    /// guessing a category for data that never carried one would be inventing
    /// a fact about a turn we no longer have.
    #[default]
    Unknown,
}

/// Deliverable extensions: things a person opens, previews, or takes away.
/// Notably absent: `.md`, `.txt`, `.json`, `.toml`, and every source extension.
/// A repository feature that rewrites 20 READMEs and 15 JSON fixtures is a set
/// of *changes*, and the reader reviews those in a diff, not in a chip strip.
const ARTIFACT_EXTS: &[&str] = &[
    "html", "htm", "svg", "pdf", "docx", "doc", "xlsx", "xls", "xlsm", "pptx", "csv", "tsv",
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "zip", "tar", "gz", "ics", "ipynb", "ogg",
    "mp3", "mp4", "wav", "webm", "exe", "AppImage",
];

/// Classify one path. Deliberately by extension, not by tool: `write_file` is
/// used for both a single-file HTML app and a source file, and the tool cannot
/// tell them apart either. What the tool *can* say, and does, is that a
/// downloaded file is a deliverable whatever its name.
pub fn kind_for(path: &std::path::Path) -> FileKind {
    // Placement outranks extension, and the attachments folder outranks `.cws` itself:
    // `.cws/assistant_attachments/x.html` sits inside the scratch folder, but it is the
    // one thing in there that was meant to reach the person.
    //
    // The check is "`.cws/` immediately followed by `assistant_attachments/`", not "any
    // component named that": a repository folder someone else named
    // `src/assistant_attachments/` is not this app's attachments folder, and treating it
    // as one would let a directory of source files be presented as hand-over material.
    // Components are matched in pairs because `from_path` hands this function the
    // absolute path, so the prefix is not the start of the string.
    let mut previous: Option<std::ffi::OsString> = None;
    let attached = path.components().any(|c| {
        let here = c.as_os_str();
        let pair = here == scratch::ATTACH_DIR
            && previous.as_deref() == Some(std::ffi::OsStr::new(scratch::SCRATCH_DIR));
        previous = Some(here.to_os_string());
        pair
    });
    if attached {
        return FileKind::Attachment;
    }
    // Anything else under `.cws` is the model's own working material — a fetched PDF, a
    // scratch script, a dump it parsed — and presenting it as a deliverable would put the
    // model's leftovers in the same strip as the file the person asked for. This is the
    // path fact behind the placement decision the tools make, so a scaffolding file
    // written by `write_file` gets the same treatment as one placed by `download_file`.
    if path
        .components()
        .any(|c| c.as_os_str() == crate::tools::scratch::SCRATCH_DIR)
    {
        return FileKind::Scratch;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ARTIFACT_EXTS.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
        FileKind::Artifact
    } else {
        FileKind::Edit
    }
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
            kind: kind_for(path),
        }
    }

    /// A file that arrived from outside the workspace. Its name comes from a URL
    /// or the model's choice, so extension-based classification is a poor guide
    /// here; the act of downloading is the fact.
    pub fn artifact(path: String, name: String, mime: String, bytes: u64) -> OutputFile {
        OutputFile {
            path,
            name,
            mime,
            bytes,
            kind: FileKind::Artifact,
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
            activates: Vec::new(),
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
            activates: Vec::new(),
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
    /// How long an `ask_user` card may stay unanswered. The host fills it from
    /// `question_timeout_secs` in config.toml; `None` means the tool's own default,
    /// which is what examples and tests get. The knob exists because a question the
    /// user has to ask someone else cannot be answered in three minutes.
    pub question_timeout: Option<std::time::Duration>,
    /// The app's own conversation history, for the one tool that reads memory rather than
    /// the filesystem. `None` where the host has no store to offer — a probe, an example, a
    /// tool host that was never told where the database is — and then the tool says *that*,
    /// rather than reporting that it searched and found nothing.
    pub store: Option<Arc<crate::store::Store>>,
    /// Which conversation is asking, so a search of the others can leave its own text out.
    /// `None` when the runner cannot say; the tool then says which claim it cannot make.
    pub session: Option<i64>,
    /// How this host built the tool set: the search provider and the exec policy. Only
    /// `explore` reads these, and it reads them because a tool card that describes a
    /// different provider than the one the user mounted is a claim about the app that is
    /// simply wrong — the descriptions of `web_search` and `exec` are built from these.
    pub search_backend: web::SearchBackend,
    pub exec_cfg: ExecCfg,
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
            question_timeout: None,
            store: None,
            // The same default `with_v1_tools` builds with, so an example or a test that
            // never picks a provider sees the same card text as that registry.
            search_backend: web::SearchBackend::DuckDuckGo,
            exec_cfg: ExecCfg::default(),
            session: None,
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

    /// The questions this call wants a human to answer, in the order the host should
    /// ask them. `None` for every tool that does not ask.
    ///
    /// This exists for a host that cannot hold a question open *inside* the tool. On the
    /// pi core the tool runs in a separate process (`cwtool`) spawned by an extension,
    /// so the questions have to be read out before the call and the answers handed back
    /// as an argument. Listing them here — in the registry, from the same parse the tool
    /// itself uses — is what keeps the ~40 lines of TypeScript around it free of policy:
    /// it asks whatever the engine says to ask, and never decides for itself that some
    /// tool ought to ask.
    fn host_questions(&self, _args: &Value) -> Option<Result<Vec<ask::Question>, String>> {
        None
    }
}

/// Exec knobs that come from config. Kept separate from ExecTool so the registry
/// remains the single place that decides which tools exist and in what order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCfg {
    pub timeout_secs: u64,
    pub sandbox_enabled: bool,
    /// What the sandbox may reach over the network.
    pub net: net::NetCfg,
}

impl Default for ExecCfg {
    fn default() -> Self {
        Self {
            timeout_secs: crate::config::default_exec_timeout(),
            sandbox_enabled: true,
            net: net::NetCfg::default(),
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
        // Built from ExecCfg, so a `[sandbox]` key cannot be parsed, validated and
        // then silently dropped on the way into the tool.
        r.register(Box::new(exec::ExecTool::new(&exec)));
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
        // Appended, like everything else. `sheet_read` is read-only and gives the
        // model eyes on a workbook the user already has - until now the engine could
        // write .xlsx files and could not open one.
        r.register(Box::new(sheets::SheetReadTool));
        // Appended after sheet_read for the same reason edit_file was: a tool
        // schema is prompt bytes, so a new tool has to arrive as a suffix. The
        // tool_order test failing is the proof this is an addition, not a reshuffle.
        r.register(Box::new(sheet_sql::SheetQueryTool));
        // Appended, same discipline. `recall` reads the app's own history rather than the
        // filesystem, which is a new kind of reach: the risk level it picks is Harmless on
        // purpose (the user's own conversations, on their own disk, already listed in the
        // sidebar), and an approval card there only trains click-through.
        r.register(Box::new(memory::RecallTool::new()));
        // Appended last, and the two exist together: `recall` is how the coworker reads its
        // own past, `explore` is how it reaches the rest of this registry once the prompt
        // stops declaring all of it. Both are cheap enough to declare in every mode - see
        // `lean_tool_names`, where the price of always being declared is asserted.
        r.register(Box::new(discover::ExploreTool));
        r
    }

    /// The tools declared in **every** mode, lean included.
    ///
    /// Lean mode answers a measured fact: the declarations above cost ~12 KB of every prompt
    /// (`scripts/gateway-prompt-shape.mjs` reads the bytes that actually leave the process),
    /// which on a small local model with a small window is a fifth of the room spent
    /// describing capabilities the turn will not use. So the coworker starts with two: one to
    /// read its own past, one to ask for anything else. Everything else - including
    /// `ask_user`, the single most expensive declaration here - is one `explore` call away.
    ///
    /// A Rust decision, not an extension decision: the registry is the thing that knows what
    /// the tools are, and a list written in TypeScript would be a second policy answering the
    /// same question differently.
    pub fn lean_tool_names() -> Vec<String> {
        vec!["explore".to_string(), "recall".to_string()]
    }

    /// The tools that exist but are not declared until something asks for them.
    pub fn discoverable_tool_names(&self) -> Vec<String> {
        let lean = Self::lean_tool_names();
        self.defs()
            .into_iter()
            .map(|d| d.name)
            .filter(|n| !lean.contains(n))
            .collect()
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
            ExecCfg { timeout_secs: 777, sandbox_enabled: true, ..Default::default() },
        );
        let schema = r.get("exec").unwrap().schema();
        let d = schema["properties"]["timeout"]["description"].as_str().unwrap();
        assert!(d.contains("777s"), "configured ceiling not surfaced: {d}");
        assert!(!d.contains("120s"), "default leaked through the config value: {d}");
    }

    /// Order is load-bearing for the prefix cache; assert it rather than trust it.
    ///
    /// `sheet_read` is appended last for the same reason as `edit_file`, and the
    /// test failing on a new tool is the mechanism working: it says the addition is
    /// a pure suffix, not a reshuffle of bytes every existing session has cached.
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
                "sheet_read",
                "sheet_query",
                "recall",
                "explore",
            ]
        );
    }

    /// Tool schemas are prompt bytes: the block is re-sent on every turn of every
    /// session, and on a one-slot llama.cpp endpoint it is part of what gets
    /// re-prefilled whenever the prefix moves. So the cost of the surface is a
    /// number someone had to write down, and a description that sprawls fails a
    /// check instead of quietly charging the user prefill for the rest of the
    /// machine's life.
    #[test]
    fn the_tool_block_costs_what_it_says_it_costs() {
        let r = Registry::with_v1_tools();
        let defs = r.defs();
        let each: Vec<(String, usize)> = defs
            .iter()
            .map(|d| (d.name.clone(), serde_json::to_string(d).unwrap().len()))
            .collect();
        let total: usize = each.iter().map(|(_, b)| b).sum();
        let one = each
            .iter()
            .find(|(n, _)| n == "sheet_query")
            .map(|(_, b)| *b)
            .unwrap_or(0);
        eprintln!(
            "[cwoffee/test] tool block {total} bytes: {}",
            each.iter()
                .map(|(n, b)| format!("{n}={b}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(one > 0 && one < 1600, "sheet_query alone is {one} bytes");
        // The tools declared in *every* mode are the one thing lean mode cannot avoid, and the
        // lean design rests on them being small: if `explore` grows, the short menu stops being
        // short; if `recall` grows, the saving moves into the tool whose job is to make saving
        // possible. So the floor of the tax is asserted, not assumed.
        let lean = Registry::lean_tool_names();
        let lean_total: usize = each.iter().filter(|(n, _)| lean.contains(n)).map(|(_, b)| *b).sum();
        for n in &lean {
            let b = each.iter().find(|(m, _)| m == n).map(|(_, b)| *b).unwrap_or(0);
            assert!(b > 0 && b < 900, "`{n}` is declared in every mode, so it must stay cheap: {b} B");
        }
        assert!(
            lean_total < 1_700,
            "the always-declared set costs {lean_total} B ({lean:?}) - a short menu costing a fifth of the long one is not a short menu"
        );
        eprintln!(
            "[cwoffee/test] declared in every mode: {lean_total} B for {} tools; the other {} cost {} B and sit behind `explore`",
            lean.len(),
            each.len() - lean.len(),
            total - lean_total
        );
        // 13,200, up from 12,600 because `download_file` now carries the `.cws` placement
        // rule: its declaration went 671 B -> ~1,150 B, which is the whole increase. That is
        // a deliberate purchase — it is what stops a workspace filling with the model's
        // leftovers — and it is paid in full mode only: in lean mode `download_file` sits
        // behind `explore` and costs nothing until the model asks for it. If a later tool
        // pushes past this, the honest move is to say what it costs (as the message below
        // does) rather than grow the ceiling silently. The lean total printed below is what
        // discovery pays back: declaring two of these tools instead of thirteen.
        //
        // 13,700 now, up from 13,200: the attachments rule adds a `place` field to three
        // writers. Measured with the same metric as this test (`--nocapture`, this tree
        // with and without the change): the block went 13,081 -> 13,634 B, of which
        // `write_file` 645 -> 922, `write_xlsx` 727 -> 940 and `download_file` 1,189 ->
        // 1,252 — 553 B in total. What it buys is the fact the transcript depends on: an
        // attachment is a folder the model chose to write into, not a guess from an
        // extension, so scaffolding can no longer be presented as a deliverable. Full mode
        // only — none of the three is declared in lean mode, and the lean figure below is
        // unchanged at 1,462 B for two tools.
        assert!(
            total < 13_700,
            "the whole tool block is {total} bytes across {} tools: a description longer than this is a per-turn tax on every session",
            each.len()
        );
    }

    // ---- artifact or change: the shape of a turn's file list ---------------

    /// A feature in a real repository touches dozens of files. The turn used to
    /// print one chip per file, and the single HTML page the model also wrote -
    /// the one thing worth clicking - drowned in them. So the engine labels the
    /// two kinds, and the transcript gives them different shapes.
    #[test]
    fn a_deliverable_is_an_artifact_and_a_source_change_is_an_edit() {
        use std::path::Path;
        for ext in [
            "html", "htm", "svg", "pdf", "docx", "xlsx", "csv", "tsv", "png", "jpg", "zip",
            "ipynb",
        ] {
            let name = format!("out/build.{ext}");
            let p = Path::new(&name);
            assert_eq!(kind_for(p), FileKind::Artifact, "{ext} should be a deliverable");
        }
        for ext in ["rs", "py", "ts", "js", "json", "toml", "yaml", "md", "txt", "css"] {
            let name = format!("src/deep/mod.{ext}");
            let p = Path::new(&name);
            assert_eq!(kind_for(p), FileKind::Edit, "{ext} should read as a change");
        }
        // A file with no extension is not something you open from a chat.
        assert_eq!(kind_for(Path::new("Makefile")), FileKind::Edit);
        // The scratch folder outranks the extension: a PDF the model fetched for
        // itself is reference material, not a deliverable, whatever it is named.
        assert_eq!(kind_for(Path::new(".cws/fetch.py")), FileKind::Scratch);
        assert_eq!(kind_for(Path::new(".cws/tables/x.pdf")), FileKind::Scratch);
        // Case does not change what a file is.
        assert_eq!(kind_for(Path::new("a/b/report.PDF")), FileKind::Artifact);
        // from_path carries the label, so every tool that describes a file on
        // disk gets it without remembering to ask.
        let dir = std::env::temp_dir().join(format!("cwoffee_kind_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dash.html"), b"<html></html>").unwrap();
        std::fs::write(dir.join("handler.rs"), b"fn h(){}").unwrap();
        let f = OutputFile::from_path(&dir, &dir.join("dash.html"));
        assert_eq!(f.kind, FileKind::Artifact, "a written HTML page is a deliverable");
        let g = OutputFile::from_path(&dir, &dir.join("handler.rs"));
        assert_eq!(g.kind, FileKind::Edit, "a written source file is a change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The attachments folder is the whole rule for what the transcript may attach.
    ///
    /// The report: a turn showed chips for probe files and scaffolding — 150 kB of
    /// things nobody asked for — because the transcript classified by extension alone,
    /// and an extension cannot tell "handing this over" from "needed it on the way".
    /// This test is the fact the UI then relies on.
    #[test]
    fn only_the_attachments_folder_makes_a_file_an_attachment() {
        use std::path::Path;
        assert_eq!(
            kind_for(Path::new(".cws/assistant_attachments/report.html")),
            FileKind::Attachment,
            "a file written to hand over is an attachment whatever it is named"
        );
        // Including a name that would otherwise be scratch material: the folder is the
        // decision, not the extension.
        assert_eq!(
            kind_for(Path::new(".cws/assistant_attachments/helper.py")),
            FileKind::Attachment,
            "the attachments folder outranks the extension"
        );
        assert_eq!(
            kind_for(Path::new(".cws/report.pdf")),
            FileKind::Scratch,
            "a scratch PDF is not an attachment"
        );
        assert_eq!(
            kind_for(Path::new(".cws/assistant_attachments/tables/x.xlsx")),
            FileKind::Attachment,
            "subfolders of the attachments folder are still attachments"
        );
        // A folder that merely has the right name elsewhere in the tree is not it, and
        // neither is one whose `.cws/` prefix is missing.
        assert_eq!(
            kind_for(Path::new("src/assistant_attachments/x.rs")),
            FileKind::Edit,
            "the attachments rule is about the scratch folder, not any folder named that"
        );
        assert_eq!(
            kind_for(std::path::Path::new("/home/u/proj/.cws/assistant_attachments/x.rs")),
            FileKind::Attachment,
            "from_path passes an absolute path, so the pair must be found anywhere in it"
        );
        let wire = serde_json::to_string(&OutputFile {
            path: ".cws/assistant_attachments/x.html".into(),
            name: "x.html".into(),
            mime: "text/html".into(),
            bytes: 1,
            kind: FileKind::Attachment,
        })
        .unwrap();
        assert!(wire.contains("\"kind\":\"attachment\""), "wire form: {wire}");
    }

    /// Old rows have no kind. Defaulting them to Artifact or Edit would invent a
    /// fact about turns that were never labelled; `unknown` keeps the old
    /// rendering and says what is actually true.
    #[test]
    fn rows_written_before_the_kind_existed_still_decode_and_say_unknown() {
        let old = r#"{"path":"notes/x.md","name":"x.md","mime":"text/markdown","bytes":12}"#;
        let f: OutputFile = serde_json::from_str(old).unwrap();
        assert_eq!(f.kind, FileKind::Unknown, "an unlabelled row claimed a kind");
        let wire = serde_json::to_string(&OutputFile {
            path: "p.html".into(),
            name: "p.html".into(),
            mime: "text/html".into(),
            bytes: 1,
            kind: FileKind::Artifact,
        })
        .unwrap();
        assert!(wire.contains("\"kind\":\"artifact\""), "wire form: {wire}");
    }
}
