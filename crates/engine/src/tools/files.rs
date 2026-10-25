//! File tools: `read_file`, `write_file` and `edit_file`.
//!
//! Both route every path through `super::path::confine`. There is no code path
//! in this module that opens a path the containment check has not seen.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::changes::line_change;
use super::path::{confine_permitted, display_rel};
use super::{CallRisk, ExecSink, OutputFile, Risk, Tool, ToolCtx, ToolResult};

/// The size above which a file-tool call stops reporting a diff. Counting lines
/// in a 4 MB text file is fine; holding two copies of it to diff them is not.
const DIFF_READ_LIMIT: u64 = 1024 * 1024;

/// Where a path would land if the tool ran. Used by `risk_for` so the approval
/// prompt can name the file the user is being asked about, and so the gate can
/// hand back exactly the path the tool will later be permitted to open.
fn resolve_path(ctx: &ToolCtx, raw: &str) -> Option<std::path::PathBuf> {
    if raw.trim().is_empty() || raw.contains('\0') {
        return None;
    }
    Some(super::path::lexical_resolve(&ctx.workspace, raw))
}

fn escapes_workspace(ctx: &ToolCtx, raw: &str) -> Option<String> {
    let p = resolve_path(ctx, raw)?;
    let ws = super::path::lexical_resolve(&ctx.workspace, ".");
    if p == ws || p.starts_with(&ws) {
        None
    } else {
        Some(p.display().to_string())
    }
}

/// Where a write actually lands, given the call. One function, used by the risk
/// verdict and by the write itself: an approval card that names one file while the
/// tool writes another is the thing an approval card exists to prevent.
fn placed(args: &Value, raw: &str) -> (String, super::scratch::Place) {
    let place = super::scratch::Place::of_write(args);
    (super::scratch::destination_for(raw, place), place)
}

/// Snapshot of a file's text before a write, for the diff in the tool card.
/// `None` means "no useful snapshot": the file did not exist, was too big, or
/// was binary - in which case the card says what it can rather than guessing.
fn snapshot(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > DIFF_READ_LIMIT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Reading a whole large file into the prompt costs prefill at 30-70 tok/s on a
/// local model. Above this size the tool insists on a range instead.
const WHOLE_FILE_LIMIT: u64 = 256 * 1024;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file inside the workspace. Returns numbered lines. \
         For large files pass offset/limit to read a range instead of the whole file. \
         PDFs are converted to text automatically."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace root" },
                "offset": { "type": "integer", "description": "First line to read, 1-based" },
                "limit":  { "type": "integer", "description": "Max number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    /// Reading inside the workspace stays silent: the agent does it hundreds of
    /// times a session, and a prompt on each one teaches people to click through
    /// prompts without reading them. Reading **outside** it is a different
    /// question, and the prompt has to name the file.
    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match escapes_workspace(ctx, raw) {
            Some(out) => CallRisk {
                level: Risk::Sensitive,
                escapes: vec![out],
            },
            None => CallRisk::harmless(),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("read_file: missing required string field \"path\""),
        };
        let path = match confine_permitted(&ctx.workspace, raw, &ctx.permits) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        if path.is_dir() {
            return ToolResult::err(format!(
                "{} is a directory. Use exec with `ls` to list it.",
                display_rel(&ctx.workspace, &path)
            ));
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolResult::err(format!("cannot stat {}: {e}", path.display())),
        };

        let offset = args.get("offset").and_then(|v| v.as_i64()).filter(|n| *n > 0);
        let limit = args.get("limit").and_then(|v| v.as_i64()).filter(|n| *n > 0);

        // PDF: convert rather than dumping compressed bytes into the prompt.
        if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("pdf")) == Some(true)
        {
            return read_pdf(&path, ctx, offset, limit).await;
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("cannot read {}: {e}", path.display())),
        };

        if looks_binary(&bytes) {
            // The pointer has to name the tool that can actually do it. A .xlsx is a
            // zip of xml parts, and "use a converter" would send the model off to
            // build a pipeline for something the engine can read directly.
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            let pointer = match ext.as_str() {
                "xlsx" | "xlsm" | "xlsb" => "sheet_read opens a .xlsx: it lists the sheets \
                 and returns cells by range; sheet_query totals, groups and filters the \
                 whole file without loading it into the chat.",
                _ => "use exec with an appropriate converter.",
            };
            return ToolResult::err(format!(
                "{} looks like a binary file ({} bytes). read_file handles text; {}",
                display_rel(&ctx.workspace, &path),
                bytes.len(),
                pointer
            ));
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        let total_lines = text.lines().count();

        // Refuse to silently swallow a huge file into the context window.
        if meta.len() > WHOLE_FILE_LIMIT && offset.is_none() && limit.is_none() {
            return ToolResult::err(format!(
                "{} is {} bytes / {} lines, too large to read at once. \
                 Call read_file again with offset and limit, or use exec with \
                 head/tail/grep to find the part you need.",
                display_rel(&ctx.workspace, &path),
                meta.len(),
                total_lines
            ));
        }

        let start = offset.unwrap_or(1) as usize;
        let take = limit.unwrap_or(i64::MAX) as usize;

        let mut out = String::new();
        let mut shown = 0usize;
        let mut truncated = false;
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if lineno < start {
                continue;
            }
            if shown >= take {
                truncated = true;
                break;
            }
            let rendered = format!("{lineno}\t{line}\n");
            if out.len() + rendered.len() > ctx.max_output_bytes {
                truncated = true;
                break;
            }
            out.push_str(&rendered);
            shown += 1;
        }

        let header = format!(
            "{} ({} lines, showing {}{})\n",
            display_rel(&ctx.workspace, &path),
            total_lines,
            shown,
            if start > 1 { format!(", from line {start}") } else { String::new() }
        );
        let mut res = ToolResult::ok(format!("{header}{out}"));
        res.truncated = truncated;
        if truncated {
            res.output.push_str(&format!(
                "\n[truncated: call read_file with a larger offset to continue]"
            ));
        }
        res
    }
}

/// Extract text from a PDF with poppler's `pdftotext`, if present.
///
/// Fixed argv, no shell, input confined to the workspace: this is not a
/// general-purpose exec path.
async fn read_pdf(path: &Path, ctx: &ToolCtx, offset: Option<i64>, limit: Option<i64>) -> ToolResult {
    if !command_exists("pdftotext") {
        return ToolResult::err(format!(
            "{} is a PDF but `pdftotext` (poppler-utils) is not installed, so it \
             cannot be read as text.",
            display_rel(&ctx.workspace, path)
        ));
    }
    let out = tokio::process::Command::new("pdftotext")
        .arg("-q")
        .arg("-layout")
        .arg(path)
        .arg("-")
        .output()
        .await;
    let out = match out {
        Ok(o) => o,
        Err(e) => return ToolResult::err(format!("pdftotext failed to start: {e}")),
    };
    if !out.status.success() {
        return ToolResult::err(format!(
            "pdftotext failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let total_lines = text.lines().count();
    let start = offset.unwrap_or(1) as usize;
    let take = limit.unwrap_or(i64::MAX) as usize;

    let mut body = String::new();
    let mut shown = 0;
    let mut truncated = false;
    for (i, line) in text.lines().enumerate() {
        if i + 1 < start {
            continue;
        }
        if shown >= take || body.len() + line.len() + 1 > ctx.max_output_bytes {
            truncated = true;
            break;
        }
        body.push_str(line);
        body.push('\n');
        shown += 1;
    }
    let mut res = ToolResult::ok(format!(
        "{} (PDF, {} lines extracted)\n{body}",
        display_rel(&ctx.workspace, path),
        total_lines
    ));
    res.truncated = truncated;
    res
}

fn command_exists(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| d.join(bin).is_file())
    })
}

/// NUL in the first 8 KiB is the same heuristic git uses. Text files do not
/// contain NUL; most binary formats do within the first few bytes.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a text file inside the workspace. mode \"overwrite\" (default) replaces \
         the file, \"append\" adds to the end. Parent directories are created. \
         Scaffolding the person did not ask for — a script to run, a scratch dump — goes \
         under `.cws/`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Path relative to the workspace root" },
                "content": { "type": "string", "description": "Full text content. For .docx and .pdf this is markdown and gets converted." },
                "mode":    { "type": "string", "enum": ["overwrite", "append"], "description": "Default overwrite" },
                "place":   { "type": "string", "enum": ["workspace", "attach"], "description": "\"workspace\" (default): the path as written. \"attach\": write under `.cws/assistant_attachments/` instead — the only folder the transcript attaches to the turn. Use it for a file you are handing over." }
            },
            "required": ["path", "content"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Workspace
    }

    /// Writing a file that is not there yet is the agent doing its job. Writing
    /// over something that already exists changes the user's own work, and
    /// outside the workspace it needs a yes with the path spelled out.
    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let (dest, _) = placed(args, raw);
        if let Some(out) = escapes_workspace(ctx, &dest) {
            return CallRisk {
                level: Risk::Sensitive,
                escapes: vec![out],
            };
        }
        match resolve_path(ctx, &dest) {
            Some(p) if p.exists() => CallRisk::at(Risk::Workspace),
            _ => CallRisk::harmless(),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("write_file: missing required string field \"path\""),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolResult::err("write_file: missing required string field \"content\""),
        };
        let append = args.get("mode").and_then(|v| v.as_str()) == Some("append");
        let (dest, place) = placed(&args, raw);

        let path = match confine_permitted(&ctx.workspace, &dest, &ctx.permits) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path == ctx.workspace || path.is_dir() {
            return ToolResult::err(format!("{} is a directory, not a file", path.display()));
        }
        // An attachment goes in a folder that may not exist yet in this workspace, and
        // the document writers below (`docx`, `pdf`) write straight to `path` rather than
        // creating parents. Create it here, with the scratch folder's self-ignoring
        // `.gitignore`, so the first attachment in a fresh conversation does not fail on
        // a missing directory.
        if matches!(place, super::scratch::Place::Attach) {
            if let Err(e) = super::scratch::ensure_scratch(&ctx.workspace) {
                return ToolResult::err(e);
            }
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
                }
            }
        }

        // Document formats: the content is markdown and the extension decides
        // the writer. Keeping this inside write_file rather than adding a
        // second "write_document" tool matters for a small local model - every
        // extra tool is another schema it has to choose between correctly.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "xlsx" => {
                return ToolResult::err(
                    "write_file cannot produce .xlsx (cells would be text). Use write_xlsx \
                     to make one, sheet_read to read one.",
                )
            }
            "docx" | "pdf" => {
                if append {
                    return ToolResult::err("cannot append to a .docx/.pdf; rewrite the whole document");
                }
                let mut res = write_document(&path, content, &ext, ctx).await;
                if res.ok && matches!(place, super::scratch::Place::Attach) {
                    res.output.push('\n');
                    res.output.push_str(&super::scratch::place_note(
                        place,
                        &display_rel(&ctx.workspace, &path),
                    ));
                }
                return res;
            }
            _ => {}
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
            }
        }

        let existed = path.exists();
        // The card shows +N -M against what was really on disk a moment ago, not
        // against what the model said it was replacing.
        let before = if existed { snapshot(&path) } else { None };
        let wrote = if append {
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).create(true).open(&path).and_then(|mut f| {
                f.write_all(content.as_bytes()).map(|_| content.len())
            })
        } else {
            std::fs::write(&path, content).map(|_| content.len())
        };

        match wrote {
            Ok(n) => {
                let after = snapshot(&path);
                let mut res = ToolResult::ok(format!(
                    "{} {} {} bytes / {} lines",
                    display_rel(&ctx.workspace, &path),
                    if append { "appended to" } else if existed { "overwrote" } else { "created" },
                    n,
                    content.lines().count()
                ))
                .with_file(OutputFile::from_path(&ctx.workspace, &path));
                if let Some(after) = after {
                    res.changes
                        .push(line_change(&ctx.workspace, &path, before.as_deref(), &after));
                }
                if matches!(place, super::scratch::Place::Attach) {
                    // Stated, so the rule is learned rather than guessed: the next turn
                    // should not have to rediscover which folder the transcript attaches.
                    res.output.push('\n');
                    res.output.push_str(&super::scratch::place_note(place, &display_rel(&ctx.workspace, &path)));
                }
                res
            }
            Err(e) => ToolResult::err(format!("cannot write {}: {e}", path.display())),
        }
    }
}

// ---------------------------------------------------------------------------

/// Largest file `edit_file` will load and rewrite. Above this the model is
/// almost certainly editing the wrong thing, and the honest answer is exec.
const EDIT_LIMIT: u64 = 4 * 1024 * 1024;

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Change part of an existing text file in place. old_text must match the file \
         exactly, including indentation and spacing. Prefer this over write_file for \
         small changes - write_file replaces the whole file. Fails if old_text matches \
         zero times or more than once; add surrounding lines to make it unique, or set \
         replace_all."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Existing file, relative to the workspace root" },
                "old_text":{ "type": "string", "description": "Exact text to replace, copied from the file" },
                "new_text":{ "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Workspace
    }

    /// An edit always changes a file the user can see, so it asks in ask mode.
    /// Outside the workspace it is Sensitive, with the path named in the prompt.
    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match escapes_workspace(ctx, raw) {
            Some(out) => CallRisk {
                level: Risk::Sensitive,
                escapes: vec![out],
            },
            None => CallRisk::at(Risk::Workspace),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("edit_file: missing required string field \"path\""),
        };
        let old = match args.get("old_text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return ToolResult::err("edit_file: missing required string field \"old_text\""),
        };
        let new = match args.get("new_text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return ToolResult::err("edit_file: missing required string field \"new_text\""),
        };
        let all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

        if old.is_empty() {
            return ToolResult::err(
                "edit_file: old_text is empty. Give the exact text to replace.",
            );
        }

        let path = match confine_permitted(&ctx.workspace, raw, &ctx.permits) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path == ctx.workspace || path.is_dir() {
            return ToolResult::err(format!("{} is a directory, not a file", path.display()));
        }
        if !path.exists() {
            return ToolResult::err(format!(
                "{} does not exist. edit_file changes an existing file; use write_file \
                 to create one.",
                display_rel(&ctx.workspace, &path)
            ));
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolResult::err(format!("cannot stat {}: {e}", path.display())),
        };
        if meta.len() > EDIT_LIMIT {
            return ToolResult::err(format!(
                "{} is {} bytes, too large to rewrite here. Use exec with sed/patch, or \
                 work on a smaller file.",
                display_rel(&ctx.workspace, &path),
                meta.len()
            ));
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("cannot read {}: {e}", path.display())),
        };
        if looks_binary(&bytes) {
            return ToolResult::err(format!(
                "{} looks binary. edit_file works on text; for a binary format write the \
                 whole file with write_file/write_xlsx.",
                display_rel(&ctx.workspace, &path)
            ));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();

        let hits = text.matches(old).count();
        if hits == 0 {
            let hint = near_miss(&text, old);
            let mut msg = format!(
                "old_text was not found in {} ({} lines).",
                display_rel(&ctx.workspace, &path),
                text.lines().count()
            );
            match hint {
                // The file is already in hand, so the correction is to quote the bytes
                // rather than to spend another call reading them. Session 1306 paid a
                // read_file for every failed edit because the old message said
                // "use read_file first" and gave nothing else.
                Some(hint) => msg.push_str(&format!("\n{hint}")),
                None => msg.push_str(
                    " Copy it from the file exactly - indentation and spacing are part of \
                     the match. Use read_file first.",
                ),
            }
            return ToolResult::err(msg);
        }
        if hits > 1 && !all {
            return ToolResult::err(format!(
                "old_text matches {hits} places in {}. Include more surrounding lines so \
                 the edit is unambiguous, or set replace_all to change every one.",
                display_rel(&ctx.workspace, &path)
            ));
        }

        let edited = text.replace(old, new);
        // Write beside the original and move it into place, so an interrupted
        // write cannot leave a half-written file where a whole one used to be.
        let tmp = sibling_tmp(&path);
        if let Err(e) = std::fs::write(&tmp, edited.as_bytes()) {
            let _ = std::fs::remove_file(&tmp);
            return ToolResult::err(format!("cannot write {}: {e}", path.display()));
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return ToolResult::err(format!("cannot replace {}: {e}", path.display()));
        }

        let before = text.lines().count();
        let after = edited.lines().count();
        let mut res = ToolResult::ok(format!(
            "{} edited: {hits} replacement(s), {before} -> {after} lines, {} -> {} bytes",
            display_rel(&ctx.workspace, &path),
            bytes.len(),
            edited.len()
        ))
        .with_file(OutputFile::from_path(&ctx.workspace, &path));
        // Both sides are already in hand here, so the diff costs nothing extra and
        // the card can show exactly which lines the edit replaced.
        res.changes
            .push(line_change(&ctx.workspace, &path, Some(&text), &edited));
        res
    }
}

/// Point at the line the model probably meant, because "not found" alone sends
/// it back through the same paraphrase. Wrong leading whitespace is the common
/// case: read_file renders "12\ttext", and the tab is not part of the file.
fn near_miss(file: &str, old: &str) -> Option<String> {
    let want = old.trim();
    if want.is_empty() {
        return None;
    }
    for (i, line) in file.lines().enumerate() {
        if line.trim() == want {
            return Some(format!(
                "near miss: line {} holds the same text with different leading whitespace: {}",
                i + 1,
                clip_line(line, 200)
            ));
        }
    }
    let first = old.lines().next().unwrap_or("").trim();
    if !first.is_empty() {
        // Where the snippet starts: quote the file's own lines from there, so the
        // retry can be written from the text in front of the model. Line numbers and
        // exact leading spaces are part of the quote, because those are the two
        // things that were wrong.
        for (i, line) in file.lines().enumerate() {
            if line.trim() == first {
                let want = old.lines().count().clamp(1, 12);
                let excerpt: Vec<String> = file
                    .lines()
                    .skip(i)
                    .take(want)
                    .enumerate()
                    .map(|(k, l)| format!("  {}: {}", i + k + 1, clip_line(l, 200)))
                    .collect();
                return Some(format!(
                    "near miss: the snippet's first line is line {} of the file. From there \
                     the file holds:\n{}\nQuote those lines exactly - the leading spaces and \
                     the line breaks are part of the match.",
                    i + 1,
                    excerpt.join("\n")
                ));
            }
        }
        // The first line exists only *inside* a longer line: the snippet cut a line in
        // half, or the file wrapped it.
        for (i, line) in file.lines().enumerate() {
            if line.contains(first) {
                return Some(format!(
                    "near miss: line {} holds the snippet's first line inside a longer line: {}",
                    i + 1,
                    clip_line(line, 200)
                ));
            }
        }
    }
    None
}

/// One line, bounded. An excerpt of a minified file would otherwise be a wall.
fn clip_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let kept: String = line.chars().take(max).collect();
    format!("{kept}\u{2026}")
}

fn sibling_tmp(path: &std::path::Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    path.with_file_name(format!(".{name}.cwoffee-tmp"))
}

/// Convert markdown into a binary document and place it at `path`.
async fn write_document(path: &Path, markdown: &str, ext: &str, ctx: &ToolCtx) -> ToolResult {
    match ext {
        "docx" => match super::docs::markdown_to_docx(markdown) {
            Ok(bytes) => match std::fs::write(path, &bytes) {
                Ok(_) => ToolResult::ok(format!(
                    "{} created (docx, {} bytes from markdown)",
                    display_rel(&ctx.workspace, path),
                    bytes.len()
                ))
                .with_file(OutputFile::from_path(&ctx.workspace, path)),
                Err(e) => ToolResult::err(format!("cannot write {}: {e}", path.display())),
            },
            Err(e) => ToolResult::err(e),
        },
        "pdf" => match super::docs::markdown_to_pdf(markdown, path, 120).await {
            Ok(p) => {
                let n = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                ToolResult::ok(format!(
                    "{} created (pdf, {} bytes; rendered via LibreOffice)",
                    display_rel(&ctx.workspace, &p),
                    n
                ))
                .with_file(OutputFile::from_path(&ctx.workspace, &p))
            }
            Err(e) => ToolResult::err(e),
        },
        other => ToolResult::err(format!("unsupported document format {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn ctx_in(dir: &Path) -> ToolCtx {
        ToolCtx { workspace: dir.to_path_buf(), max_output_bytes: 4096, ..Default::default() }
    }

    use crate::tools::FileKind;

    fn tmp(label: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "cwoffee_file_{}_{}_{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sink() -> ExecSink {
        mpsc::unbounded_channel().0
    }

    #[tokio::test]
    async fn read_returns_numbered_lines() {
        let d = tmp("read");
        std::fs::write(d.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let r = ReadFileTool.run(json!({"path": "a.txt"}), &ctx_in(&d), &sink()).await;
        assert!(r.ok, "{:?}", r.error);
        assert!(r.output.contains("1\talpha"));
        assert!(r.output.contains("3\tgamma"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_offset_and_limit_window_the_file() {
        let d = tmp("window");
        let body: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        std::fs::write(d.join("big.txt"), &body).unwrap();
        let r = ReadFileTool
            .run(json!({"path": "big.txt", "offset": 50, "limit": 5}), &ctx_in(&d), &sink())
            .await;
        assert!(r.output.contains("50\tline50"));
        assert!(r.output.contains("54\tline54"));
        assert!(!r.output.contains("55\tline55"), "limit not respected");
        assert!(!r.output.contains("49\tline49"), "offset not respected");
        assert!(r.truncated, "should report there is more");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_refuses_escape_from_workspace() {
        let d = tmp("escape");
        std::fs::write(d.join("secret"), "top secret").unwrap();
        let outer = d.parent().unwrap().join(format!(
            "cwoffee_file_outer_{}",
            std::process::id()
        ));
        std::fs::write(&outer, "outside").unwrap();

        let r = ReadFileTool.run(json!({"path": "secret"}), &ctx_in(&d), &sink()).await;
        assert!(r.ok);
        let r = ReadFileTool.run(json!({"path": "../outside"}), &ctx_in(&d), &sink()).await;
        assert!(!r.ok, "escape was allowed");
        assert!(r.error.as_deref().unwrap().contains("outside the workspace"));
        let _ = std::fs::remove_file(&outer);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_refuses_huge_file_without_a_range() {
        let d = tmp("huge");
        std::fs::write(d.join("huge.txt"), vec![b'a'; 300 * 1024]).unwrap();
        let r = ReadFileTool.run(json!({"path": "huge.txt"}), &ctx_in(&d), &sink()).await;
        assert!(!r.ok, "a 300 KB file should not be dumped into the prompt");
        assert!(r.error.as_deref().unwrap().contains("offset"));
        // ...but an explicit range works
        let r = ReadFileTool.run(json!({"path": "huge.txt", "limit": 3}), &ctx_in(&d), &sink()).await;
        assert!(r.ok);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_detects_binary_instead_of_emitting_garbage() {
        let d = tmp("bin");
        std::fs::write(d.join("x.bin"), [0u8, 1, 2, 3, 255, 254]).unwrap();
        let r = ReadFileTool.run(json!({"path": "x.bin"}), &ctx_in(&d), &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("binary"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_output_is_clamped_to_the_context_budget() {
        let d = tmp("clamp");
        // Under WHOLE_FILE_LIMIT so the range guard does not fire first; this
        // test is about the per-call byte clamp.
        let body: String = (1..=1000).map(|_| "x".repeat(200) + "\n").collect();
        std::fs::write(d.join("long.txt"), &body).unwrap();
        let mut c = ctx_in(&d);
        c.max_output_bytes = 2000;
        let r = ReadFileTool.run(json!({"path": "long.txt"}), &c, &sink()).await;
        assert!(r.ok);
        assert!(r.output.len() < 3000, "output {} exceeded clamp", r.output.len());
        assert!(r.truncated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn write_creates_parents_and_reports() {
        let d = tmp("write");
        let r = WriteFileTool
            .run(json!({"path": "deep/nested/file.md", "content": "# hi\nworld\n"}), &ctx_in(&d), &sink())
            .await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(d.join("deep/nested/file.md")).unwrap(), "# hi\nworld\n");
        assert!(r.output.contains("created"));
        assert!(r.output.contains("2 lines"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn write_overwrite_replaces_and_append_extends() {
        let d = tmp("modes");
        let c = ctx_in(&d);
        WriteFileTool.run(json!({"path": "f.txt", "content": "one"}), &c, &sink()).await;
        WriteFileTool.run(json!({"path": "f.txt", "content": "two"}), &c, &sink()).await;
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "two");
        WriteFileTool
            .run(json!({"path": "f.txt", "content": "three\n", "mode": "append"}), &c, &sink())
            .await;
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "twothree\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn write_refuses_escape_from_workspace() {
        let d = tmp("wescape");
        let r = WriteFileTool
            .run(json!({"path": "../evil.txt", "content": "nope"}), &ctx_in(&d), &sink())
            .await;
        assert!(!r.ok);
        assert!(!d.parent().unwrap().join("evil.txt").exists(), "file escaped the workspace");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn missing_arguments_produce_model_correctable_errors() {
        let d = tmp("args");
        let c = ctx_in(&d);
        let r = ReadFileTool.run(json!({}), &c, &sink()).await;
        assert!(!r.ok && r.error.as_deref().unwrap().contains("path"));
        let r = WriteFileTool.run(json!({"path": "a"}), &c, &sink()).await;
        assert!(!r.ok && r.error.as_deref().unwrap().contains("content"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_detection_matches_git_heuristic() {
        assert!(looks_binary(b"\x89PNG\0\x1a\n"));
        assert!(!looks_binary(b"fn main() {}\n"));
        assert!(!looks_binary(b""));
        // NUL past the probe window is not treated as binary
        let mut s = vec![b'a'; 9000];
        s[8500] = 0;
        assert!(!looks_binary(&s));
    }

    /// The attachments folder is a placement the tool implements, not a convention the
    /// prompt hopes for. The report this ends: a turn's file strip filled with probe files
    /// and scaffolding, because nothing distinguished "written while working" from "handed
    /// to the person". One word in the call does, and the transcript then has a fact to
    /// attach from.
    #[tokio::test]
    async fn a_write_can_be_placed_as_an_attachment_and_says_so() {
        let d = tmp("attach");
        let r = WriteFileTool
            .run(
                json!({ "path": "page.html", "content": "<html>hi</html>", "place": "attach" }),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(r.ok, "{:?}", r.error);
        let landed = d.join(".cws/assistant_attachments/page.html");
        assert!(landed.is_file(), "the attachment did not land in the attachments folder: {}", r.output);
        assert!(!d.join("page.html").exists(), "an attach must not also write into the person's folder");
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].kind, FileKind::Attachment, "the transcript attaches from this kind alone");
        assert_eq!(r.files[0].path, ".cws/assistant_attachments/page.html");
        // The rule is stated back to the model, so the next turn does not have to
        // rediscover which folder the transcript reads.
        assert!(r.output.contains("assistant_attachments"), "placement not stated: {}", r.output);
        // The scratch folder keeps its self-ignoring .gitignore even when the very first
        // write in a fresh workspace is an attachment.
        assert_eq!(
            std::fs::read_to_string(d.join(".cws/.gitignore")).unwrap(),
            "*\n",
            "an attachment folder created from nothing left the scratch folder unignored"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The other direction, which is the one that would silently move people's files:
    /// a write that says nothing about placement lands exactly where its path says.
    #[tokio::test]
    async fn a_write_without_place_still_lands_where_the_path_says() {
        let d = tmp("keep");
        let r = WriteFileTool
            .run(json!({ "path": "notes/page.html", "content": "<html>hi</html>" }), &ctx_in(&d), &sink())
            .await;
        assert!(r.ok, "{:?}", r.error);
        assert!(d.join("notes/page.html").is_file(), "a named write was moved: {}", r.output);
        assert!(!d.join(".cws").exists(), "a plain write created the scratch folder");
        assert_ne!(r.files[0].kind, FileKind::Attachment, "a plain write is not an attachment");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The approval card and the write must agree on the destination, or a person said yes
    /// to one file and got another.
    #[tokio::test]
    async fn an_attachment_call_is_risked_at_the_folder_it_will_actually_write() {
        let d = tmp("risk");
        let ctx = ctx_in(&d);
        let r = WriteFileTool.risk_for(&json!({ "path": "x.html", "place": "attach" }), &ctx);
        assert_eq!(r.level, Risk::Harmless, "creating an attachment is creating a file: {r:?}");
        // Same call, asked about with the destination it will use: also inside, also
        // harmless. The point is that placement cannot turn a workspace write into an
        // escape, in either direction.
        let r2 = WriteFileTool.risk_for(&json!({ "path": ".cws/assistant_attachments/x.html" }), &ctx);
        assert_eq!(r2.level, Risk::Harmless, "{r2:?}");
        // A relative `..` is absorbed by the prefix: `.cws/assistant_attachments/../../x`
        // resolves to `<workspace>/x`, which is a workspace write, not an escape. Saying so
        // here matters because the same resolution is what the card shows: the path a
        // person approves is the path the tool resolves, in both directions.
        let absorbed =
            WriteFileTool.risk_for(&json!({ "path": "../../outside.html", "place": "attach" }), &ctx);
        assert_eq!(absorbed.level, Risk::Harmless, "the prefix absorbs it: {absorbed:?}");
        assert!(
            ctx.workspace.join("outside.html").starts_with(&ctx.workspace),
            "the absorbed destination left the workspace"
        );
        // One level more than the prefix can absorb is an escape, and attach does not
        // paper over it.
        let esc = WriteFileTool.risk_for(&json!({ "path": "../../../outside.html", "place": "attach" }), &ctx);
        assert_eq!(esc.level, Risk::Sensitive, "an attach is not a licence to escape: {esc:?}");
        let abs = WriteFileTool.risk_for(&json!({ "path": "/etc/outside.html", "place": "attach" }), &ctx);
        assert_eq!(abs.level, Risk::Sensitive, "an absolute path stays an escape: {abs:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn edit_replaces_one_occurrence_and_reports_the_file() {
        let d = tmp("edit");
        std::fs::write(d.join("notes.md"), "# Title\n\nfirst line\nlast line\n").unwrap();
        let r = EditFileTool
            .run(
                json!({"path": "notes.md", "old_text": "first line", "new_text": "opened line"}),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            std::fs::read_to_string(d.join("notes.md")).unwrap(),
            "# Title\n\nopened line\nlast line\n"
        );
        assert!(r.output.contains("1 replacement(s)"), "{}", r.output);
        assert!(r.output.contains("4 -> 4 lines"), "{}", r.output);
        // The card header and the attachment both come from this; the path is
        // workspace-relative so the UI can never be talked into opening /etc.
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].path, "notes.md");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn edit_refuses_an_ambiguous_match_and_says_what_to_do() {
        let d = tmp("edit-ambig");
        std::fs::write(d.join("a.txt"), "dup\ndup\ndup\n").unwrap();
        let r = EditFileTool
            .run(json!({"path": "a.txt", "old_text": "dup", "new_text": "x"}), &ctx_in(&d), &sink())
            .await;
        assert!(!r.ok, "an ambiguous match must not be guessed at");
        assert!(r.text().contains("3 places"), "not actionable: {}", r.text());
        assert!(r.text().contains("replace_all"), "no way out offered: {}", r.text());
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "dup\ndup\ndup\n",
            "a refused edit must not touch the file"
        );
        let r = EditFileTool
            .run(
                json!({"path": "a.txt", "old_text": "dup", "new_text": "x", "replace_all": true}),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "x\nx\nx\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn edit_that_matches_nothing_points_at_the_real_mistake() {
        let d = tmp("edit-miss");
        std::fs::write(d.join("p.py"), "def f():\n    return 1\n").unwrap();
        // The model read the file with read_file, which renders "2\treturn 1",
        // and quoted it without the indentation. "not found" alone would send it
        // around the same loop; the hint names the line and shows the spacing.
        // The model quoted the snippet with the indentation dropped. A bare
        // substring like "return 1" would still match; a multi-line snippet
        // without the leading spaces will not, and that is the common mistake.
        let r = EditFileTool
            .run(
                json!({"path": "p.py", "old_text": "def f():\nreturn 1", "new_text": "def f():\nreturn 2"}),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(!r.ok, "a snippet with the wrong indentation must not match");
        let msg = r.text();
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("near miss"), "no hint: {msg}");
        assert!(msg.contains("line 1"), "hint does not name the line: {msg}");

        // The same snippet quoted correctly does match.
        let r = EditFileTool
            .run(
                json!({"path": "p.py", "old_text": "def f():\n    return 1", "new_text": "def f():\n    return 2"}),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(r.ok, "{:?}", r.error);
        assert!(std::fs::read_to_string(d.join("p.py")).unwrap().contains("return 2"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The old message ended with "Use read_file first", which is a second call for
    /// information the tool already had. The near miss has to carry the file's own
    /// bytes - line numbers, leading spaces and all - and the retry written from that
    /// quote has to work.
    #[tokio::test]
    async fn a_failed_edit_quotes_the_lines_it_wanted_instead_of_sending_a_read() {
        let d = tmp("nearquote");
        std::fs::write(
            d.join("a.py"),
            "def f():\n    if x:\n        return 1\n    return 2\n",
        )
        .unwrap();
        let r = EditFileTool
            .run(
                json!({"path": "a.py", "old_text": "def f():\nif x:\n    return 1", "new_text": "nope"}),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(!r.ok);
        let msg = r.text();
        assert!(msg.contains("near miss"), "{msg}");
        // The file's own spacing, not a description of it.
        assert!(msg.contains("        return 1"), "no exact spacing: {msg}");
        assert!(msg.contains("2:     if x:"), "no line numbering: {msg}");
        assert!(!msg.contains("Use read_file first"), "asked for a read: {msg}");

        // The retry, written straight from that quote, applies.
        let ok = EditFileTool
            .run(
                json!({
                    "path": "a.py",
                    "old_text": "def f():\n    if x:\n        return 1",
                    "new_text": "def f():\n    if x:\n        return 3"
                }),
                &ctx_in(&d),
                &sink(),
            )
            .await;
        assert!(ok.ok, "a retry written from the quote must work: {}", ok.text());
    }

    /// A quote of a 40-line snippet in a minified file is a dump unless it is
    /// bounded, and a tool result is not a file viewer.
    #[tokio::test]
    async fn a_near_miss_quote_stays_a_quote() {
        let d = tmp("nearbound");
        let long_line = "x".repeat(5000);
        let mut body = String::new();
        body.push_str(&format!("{long_line}\n"));
        for i in 0..40 {
            body.push_str(&format!("    line {i}\n"));
        }
        std::fs::write(d.join("big.txt"), &body).unwrap();
        // The snippet starts on the unreadable line, which is the case that would
        // otherwise paste 5000 characters back at the model.
        let old: String = std::iter::once(long_line.clone())
            .chain((0..40).map(|i| format!("line {i}")))
            .collect::<Vec<_>>()
            .join("\n");
        let r = EditFileTool
            .run(json!({"path": "big.txt", "old_text": old, "new_text": "nope"}), &ctx_in(&d), &sink())
            .await;
        assert!(!r.ok);
        let msg = r.text();
        assert!(
            msg.len() < 3000,
            "a near miss leaked {} bytes of file: {}",
            msg.len(),
            &msg[..200.min(msg.len())]
        );
        assert!(
            msg.lines().count() <= 15,
            "the quote kept more than its share of the file:\n{msg}"
        );
        assert!(msg.contains('…'), "a 5000-char line was not clipped: {msg}");
    }

    /// And when there is genuinely nothing nearby, the advice to read the file is
    /// still the right advice.
    #[tokio::test]
    async fn a_miss_with_nothing_nearby_still_says_read_the_file() {
        let d = tmp("nearnone");
        std::fs::write(d.join("q.txt"), "alpha\nbeta\n").unwrap();
        let r = EditFileTool
            .run(json!({"path": "q.txt", "old_text": "gamma delta", "new_text": "x"}), &ctx_in(&d), &sink())
            .await;
        let msg = r.text();
        assert!(!msg.contains("near miss"), "{msg}");
        assert!(msg.contains("Use read_file first"), "{msg}");
    }

    #[tokio::test]
    async fn edit_refuses_missing_files_escapes_and_binaries() {
        let d = tmp("edit-bad");
        let c = ctx_in(&d);
        let r = EditFileTool
            .run(json!({"path": "nope.txt", "old_text": "a", "new_text": "b"}), &c, &sink())
            .await;
        assert!(!r.ok && r.text().contains("write_file"), "unhelpful: {}", r.text());

        let r = EditFileTool
            .run(json!({"path": "../evil", "old_text": "a", "new_text": "b"}), &c, &sink())
            .await;
        assert!(!r.ok, "escape allowed");
        assert!(!d.parent().unwrap().join("evil").exists());

        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let r = EditFileTool
            .run(json!({"path": "b.bin", "old_text": "a", "new_text": "b"}), &c, &sink())
            .await;
        assert!(!r.ok && r.text().contains("binary"), "unhelpful: {}", r.text());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn edit_leaves_no_temp_file_behind() {
        let d = tmp("edit-tmp");
        std::fs::write(d.join("t.txt"), "one\n").unwrap();
        EditFileTool
            .run(json!({"path": "t.txt", "old_text": "one", "new_text": "two"}), &ctx_in(&d), &sink())
            .await;
        let left = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(left, vec!["t.txt".to_string()], "stray entries: {left:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn missing_arguments_are_correctable_by_the_model() {
        let d = tmp("edit-args");
        let c = ctx_in(&d);
        std::fs::write(d.join("f.txt"), "x\n").unwrap();
        for args in [
            json!({"old_text": "x", "new_text": "y"}),
            json!({"path": "f.txt", "new_text": "y"}),
            json!({"path": "f.txt", "old_text": "x"}),
            json!({"path": "f.txt", "old_text": "", "new_text": "y"}),
        ] {
            let r = EditFileTool.run(args.clone(), &c, &sink()).await;
            assert!(!r.ok, "accepted {args}");
            assert!(!r.text().is_empty(), "silent refusal for {args}");
        }
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "x\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn docx_extension_produces_a_real_package() {
        let dir = tmp("docx");
        let (tx, _rx) = mpsc::unbounded_channel();
        let r = WriteFileTool
            .run(
                json!({"path": "note.docx", "content": "# Title\n\nTom & Jerry **bold**"}),
                &ctx_in(&dir),
                &tx,
            )
            .await;
        assert!(r.ok, "{}", r.text());
        let bytes = std::fs::read(dir.join("note.docx")).unwrap();
        assert_eq!(&bytes[0..2], b"PK", "not a zip");
        let cur = std::io::Cursor::new(bytes);
        let zip = zip::ZipArchive::new(cur).expect("docx opens as a zip");
        assert!(zip.file_names().any(|n| n == "word/document.xml"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn xlsx_through_write_file_is_refused_and_points_at_the_right_tool() {
        let dir = tmp("xlsx-refuse");
        let (tx, _rx) = mpsc::unbounded_channel();
        let r = WriteFileTool
            .run(json!({"path": "a.xlsx", "content": "x,y\n1,2"}), &ctx_in(&dir), &tx)
            .await;
        assert!(!r.ok, "xlsx accepted by write_file");
        assert!(r.text().contains("write_xlsx"), "not actionable: {}", r.text());
        assert!(!dir.join("a.xlsx").exists(), "refusal must not leave a file behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- what each call is allowed to do, decided per call ------------------
    //
    // The gate's whole value rests on this classification being right. A prompt
    // on harmless calls trains people to click through prompts; no prompt on an
    // escape hands anything outside the workspace to the model.

    #[test]
    fn reading_inside_the_workspace_is_harmless() {
        let d = tmp("risk-read-in");
        let c = ctx_in(&d);
        for p in ["a.txt", "deep/nested/a.txt", "./a.txt", "a/../b.txt"] {
            let r = ReadFileTool.risk_for(&json!({"path": p}), &c);
            assert_eq!(r.level, Risk::Harmless, "{p} was not harmless: {r:?}");
            assert!(r.escapes.is_empty());
        }
    }

    #[test]
    fn reading_outside_asks_and_names_the_exact_file() {
        let d = tmp("risk-read-out");
        let c = ctx_in(&d);
        let r = ReadFileTool.risk_for(&json!({"path": "../secrets/notes.txt"}), &c);
        assert_eq!(r.level, Risk::Sensitive);
        assert_eq!(
            r.escapes,
            vec!["/tmp/secrets/notes.txt".to_string()],
            "the prompt must name the resolved path (.. collapsed), not the string the model typed: {} vs {}",
            d.display(),
            r.escapes[0]
        );
    }

    #[test]
    fn creating_a_file_is_harmless_and_replacing_one_is_not() {
        let d = tmp("risk-write");
        let c = ctx_in(&d);
        let fresh = WriteFileTool.risk_for(&json!({"path": "new.md", "content": "x"}), &c);
        assert_eq!(fresh.level, Risk::Harmless, "creating output is the agent's job");
        std::fs::write(d.join("yours.md"), "the user's own notes\n").unwrap();
        let over = WriteFileTool.risk_for(&json!({"path": "yours.md", "content": "x"}), &c);
        assert_eq!(over.level, Risk::Workspace, "overwriting the user's file must ask");
    }

    #[test]
    fn editing_always_asks_because_it_changes_a_real_file() {
        let d = tmp("risk-edit");
        std::fs::write(d.join("a.md"), "one\n").unwrap();
        let r = EditFileTool.risk_for(&json!({"path": "a.md", "old_text": "one", "new_text": "two"}), &ctx_in(&d));
        assert_eq!(r.level, Risk::Workspace);
        assert!(r.escapes.is_empty());
    }

    #[tokio::test]
    async fn an_escape_is_refused_until_the_user_permits_that_exact_path() {
        let d = tmp("permit");
        let outside = d.parent().unwrap().join(format!(
            "cwoffee_outside_{}",
            file_name_of(&d)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "do not read\n").unwrap();

        let rel = format!(
            "../{}/secret.txt",
            outside.file_name().unwrap().to_string_lossy()
        );
        let ctx = ctx_in(&d);
        let denied = ReadFileTool
            .run(json!({"path": rel}), &ctx, &sink())
            .await;
        assert!(!denied.ok, "an unpermitted escape was allowed");
        assert!(denied.text().contains("outside the workspace"), "{}", denied.text());

        let mut permitted = ctx.clone();
        permitted.permits = vec![outside.join("secret.txt")];
        let granted = ReadFileTool
            .run(json!({"path": rel}), &permitted, &sink())
            .await;
        assert!(granted.ok, "the permitted read failed: {}", granted.text());
        assert!(granted.text().contains("do not read"));
    }

    /// The grant is for the path that was shown, not for the directory it happens
    /// to sit in. Otherwise one yes quietly becomes "read anything in here".
    #[tokio::test]
    async fn a_permit_does_not_cover_its_neighbours() {
        let d = tmp("permit-sibling");
        let outside = d.parent().unwrap().join(format!("cwoffee_outside_{}", file_name_of(&d)));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.txt"), "yes\n").unwrap();
        std::fs::write(outside.join("b.txt"), "no\n").unwrap();

        let mut ctx = ctx_in(&d);
        ctx.permits = vec![outside.join("a.txt")];
        let ok = ReadFileTool
            .run(json!({"path": format!("../{}/a.txt", outside.file_name().unwrap().to_string_lossy())}), &ctx, &sink())
            .await;
        assert!(ok.ok, "the permitted file failed: {}", ok.text());
        let sibling = ReadFileTool
            .run(json!({"path": format!("../{}/b.txt", outside.file_name().unwrap().to_string_lossy())}), &ctx, &sink())
            .await;
        assert!(!sibling.ok, "a permit leaked to a sibling path");
    }

    #[tokio::test]
    async fn a_trick_path_cannot_borrow_a_permit() {
        let d = tmp("permit-trick");
        let outside = d.parent().unwrap().join(format!("cwoffee_outside_{}", file_name_of(&d)));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.txt"), "x\n").unwrap();
        let name = outside.file_name().unwrap().to_string_lossy().into_owned();

        let mut ctx = ctx_in(&d);
        // Permit is written in normalised form; the call arrives dressed up.
        ctx.permits = vec![outside.join("a.txt")];
        let r = ReadFileTool
            .run(json!({"path": format!("../{}/sub/../a.txt", name)}), &ctx, &sink())
            .await;
        assert!(r.ok, "a normalising path should still match its permit: {}", r.text());

        // And a permit that was *not* normalised cannot be spoofed into matching
        // something else.
        let mut ctx2 = ctx_in(&d);
        ctx2.permits = vec![outside.join("b.txt")];
        let r2 = ReadFileTool
            .run(json!({"path": format!("../{}/../{}/a.txt", name, name)}), &ctx2, &sink())
            .await;
        assert!(!r2.ok, "an unrelated permit was reused: {}", r2.text());
    }

    fn file_name_of(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// The badge in the header is only trustworthy if it describes the file that
    /// is actually on disk afterwards, not what the model claimed to write.
    #[tokio::test]
    async fn a_created_file_reports_its_lines_and_no_removals() {
        let d = tmp("change-create");
        let r = WriteFileTool
            .run(json!({"path": "note.md", "content": "one\ntwo\nthree\n"}), &ctx_in(&d), &sink())
            .await;
        assert!(r.ok, "{}", r.text());
        assert_eq!(r.changes.len(), 1);
        let c = &r.changes[0];
        assert_eq!((c.added, c.removed, c.created), (3, 0, true), "{c:?}");
        assert_eq!(c.path, "note.md");
    }

    #[tokio::test]
    async fn overwriting_reports_both_sides_of_the_change() {
        let d = tmp("change-overwrite");
        std::fs::write(d.join("note.md"), "keep\nold line\ntail\n").unwrap();
        let r = WriteFileTool
            .run(json!({"path": "note.md", "content": "keep\nnew line\ntail\n"}), &ctx_in(&d), &sink())
            .await;
        assert!(r.ok, "{}", r.text());
        let c = &r.changes[0];
        assert_eq!((c.added, c.removed, c.created), (1, 1, false), "{c:?}");
        let diff = c.diff.clone().expect("a diff for a small edit");
        assert!(diff.contains("-old line") && diff.contains("+new line"), "{diff}");
    }

    #[tokio::test]
    async fn an_edit_reports_the_lines_it_replaced() {
        let d = tmp("change-edit");
        std::fs::write(d.join("a.md"), "alpha\nbeta\ngamma\n").unwrap();
        let r = EditFileTool
            .run(json!({"path": "a.md", "old_text": "beta", "new_text": "BETA\nand more"}), &ctx_in(&d), &sink())
            .await;
        assert!(r.ok, "{}", r.text());
        let c = &r.changes[0];
        assert_eq!((c.added, c.removed), (2, 1), "{c:?}");
        assert!(c.diff.clone().unwrap_or_default().contains("+BETA"));
    }

    #[tokio::test]
    async fn appending_to_a_binary_document_is_refused() {
        let dir = tmp("docx-append");
        let (tx, _rx) = mpsc::unbounded_channel();
        let r = WriteFileTool
            .run(
                json!({"path": "a.docx", "content": "more", "mode": "append"}),
                &ctx_in(&dir),
                &tx,
            )
            .await;
        assert!(!r.ok, "append to docx accepted");
        std::fs::remove_dir_all(&dir).ok();
    }
}
