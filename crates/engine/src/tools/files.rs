//! File tools: `read_file` and `write_file`.
//!
//! Both route every path through `super::path::confine`. There is no code path
//! in this module that opens a path the containment check has not seen.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::path::{confine, display_rel};
use super::{ExecSink, Risk, Tool, ToolCtx, ToolResult};

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
        Risk::ReadOnly
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("read_file: missing required string field \"path\""),
        };
        let path = match confine(&ctx.workspace, raw) {
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
            return ToolResult::err(format!(
                "{} looks like a binary file ({} bytes). read_file handles text; \
                 use exec with an appropriate converter.",
                display_rel(&ctx.workspace, &path),
                bytes.len()
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
         the file, \"append\" adds to the end. Parent directories are created."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Path relative to the workspace root" },
                "content": { "type": "string", "description": "Full text content to write" },
                "mode":    { "type": "string", "enum": ["overwrite", "append"], "description": "Default overwrite" }
            },
            "required": ["path", "content"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Mutating
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

        let path = match confine(&ctx.workspace, raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path == ctx.workspace || path.is_dir() {
            return ToolResult::err(format!("{} is a directory, not a file", path.display()));
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
            }
        }

        let existed = path.exists();
        let wrote = if append {
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).create(true).open(&path).and_then(|mut f| {
                f.write_all(content.as_bytes()).map(|_| content.len())
            })
        } else {
            std::fs::write(&path, content).map(|_| content.len())
        };

        match wrote {
            Ok(n) => ToolResult::ok(format!(
                "{} {} {} bytes / {} lines",
                display_rel(&ctx.workspace, &path),
                if append { "appended to" } else if existed { "overwrote" } else { "created" },
                n,
                content.lines().count()
            )),
            Err(e) => ToolResult::err(format!("cannot write {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn ctx_in(dir: &Path) -> ToolCtx {
        ToolCtx { workspace: dir.to_path_buf(), max_output_bytes: 4096 }
    }

    fn tmp(label: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "cowry_file_{}_{}_{}",
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
            "cowry_file_outer_{}",
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
}
