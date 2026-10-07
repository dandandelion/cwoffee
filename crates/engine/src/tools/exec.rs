//! `exec` — command execution inside a bubblewrap sandbox.
//!
//! The sandbox shape below was verified on this machine before being written
//! down (see scripts/sandbox-probe.sh): network blocked, host filesystem
//! read-only, workspace bind writable and visible to the host, PID namespace
//! isolated, uid preserved so files the agent writes stay owned by the user.
//!
//! Two ordering details that are easy to get wrong and silently break things:
//!   * `--tmpfs /tmp` must come **before** `--bind <workspace>` when the
//!     workspace lives under /tmp, otherwise the tmpfs masks the bind and the
//!     agent's writes vanish into a private filesystem.
//!   * the deadline is enforced *inside* the sandbox with coreutils `timeout`,
//!     not only by killing bwrap from outside. Killing bwrap alone can leave the
//!     sandboxed process tree running.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::path::lexical_resolve;
use super::{ExecChunk, ExecSink, Risk, Tool, ToolCtx, ToolResult, clamp};
use crate::event::ExecStream;

pub struct ExecTool {
    /// Seconds before the command is killed.
    pub timeout_secs: u64,
    /// When false, refuse to run anything unless bwrap is available.
    pub allow_unsandboxed: bool,
}

impl Default for ExecTool {
    fn default() -> Self {
        // Matches the documented config default. Two different "defaults" for
        // the same knob is how a config key ends up looking ignored.
        Self { timeout_secs: crate::config::default_exec_timeout(), allow_unsandboxed: false }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Run a shell command in a sandbox (no network, host filesystem read-only, \
         only the workspace is writable). Returns stdout and stderr combined. \
         Do not author file contents here: no here-documents and no long echo/printf \
         redirections. Write files with write_file, which takes the content as an \
         argument and shows the approver a diff instead of a wall of text."
    }

    fn schema(&self) -> Value {
        // The ceiling is config-driven, so the model is told the real number
        // rather than guessing and repeatedly dying at the default.
        let d = self.timeout_secs;
        let c = self.timeout_secs;
        json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Shell command to run (POSIX sh syntax)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional directory relative to the workspace."
                },
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "Wall-clock seconds to allow. Default {d}s; anything above the \
                         hard ceiling of {c}s is clamped down. Set it deliberately for \
                         slow commands (builds, large searches) instead of letting them die."
                    )
                }
            },
            "required": ["cmd"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Sensitive
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, sink: &ExecSink) -> ToolResult {
        let Some(cmd) = super::repair::field_as_string(&args, "cmd") else {
            return ToolResult::err("exec: missing required string field \"cmd\"");
        };

        // A here-document that writes a two-thousand-character text file is the
        // one case where the sandbox is not the problem: the *approval prompt*
        // has to show the whole command to mean anything, and a card of prose the
        // approver cannot scroll is a card they cannot read. `write_file` takes
        // the same bytes in a body that scrolls and diffs, so refuse by name.
        if let Some((target, bytes)) = oversized_inline_write(&cmd) {
            return ToolResult::err(format!(
                "exec: this command carries {bytes} characters of content to write into {target:?}. \
                 Use write_file with path {target} and that content as its argument instead: an \
                 approval prompt has to show the entire command, and a wall of text nobody can \
                 scroll is not an approval. (Under {MAX_INLINE_WRITE} characters, carry on.)"
            ));
        }

        // Resolve cwd inside the workspace and refuse escapes. The sandbox bind
        // already confines writes, but a clear error beats a confusing one.
        let cwd = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(c) => {
                let resolved = lexical_resolve(&ctx.workspace, c);
                if !resolved.starts_with(&ctx.workspace) {
                    return ToolResult::err(format!(
                        "exec: cwd {c:?} escapes the workspace {:?}",
                        ctx.workspace
                    ));
                }
                resolved
            }
            None => ctx.workspace.clone(),
        };

        // The model budgets the deadline per call, but only downwards: the
        // configured ceiling is the user's, not the model's to raise.
        let (timeout_secs, timeout_note) = self.effective_timeout(&args);

        let bwrap = which("bwrap");
        let use_sandbox = bwrap.is_some();
        if !use_sandbox && !self.allow_unsandboxed {
            return ToolResult::err(
                "exec: bwrap (bubblewrap) is not installed, and unsandboxed execution \
                 is disabled. Install bubblewrap, or set tools.exec.allow_unsandboxed=true.",
            );
        }

        let (program, pre_args) = if use_sandbox {
            let b = bwrap.unwrap();
            let mut a = sandbox_args(&ctx.workspace);
            // Deadline enforced inside the namespace; -k 2 forces SIGKILL if the
            // command ignores SIGTERM.
            a.extend([
                "--".into(),
                "/usr/bin/timeout".into(),
                "-k".into(),
                "2".into(),
                timeout_secs.to_string(),
                "/bin/sh".into(),
                "-c".into(),
                cmd.clone(),
            ]);
            (b, a)
        } else {
            (
                "/bin/sh".to_string(),
                vec!["-c".into(), cmd.clone()],
            )
        };

        let _ = sink.send(ExecChunk {
            stream: ExecStream::Stderr,
            chunk: if use_sandbox {
                "[sandboxed: bwrap] ".into()
            } else {
                "[WARNING: running UNSANDBOXED] ".into()
            },
        });
        // A silently clamped deadline is the worst version of this: the model
        // would keep asking for longer and never learn why runs keep dying.
        if let Some(note) = &timeout_note {
            let _ = sink.send(ExecChunk {
                stream: ExecStream::Stderr,
                chunk: format!("{note}\n"),
            });
        }

        let mut c = Command::new(&program);
        c.args(&pre_args)
            .current_dir(&cwd)
            .env("HOME", &ctx.workspace)
            .env("PS1", "")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return ToolResult::err(format!("exec: failed to spawn: {e}")),
        };

        // Drain both pipes concurrently, or a chatty command deadlocks on a
        // full stderr pipe while we wait on stdout.
        // Both pipes are piped above, so both readers exist.
        let out_task = drain(child.stdout.take(), sink, false).expect("stdout piped");
        let err_task = drain(child.stderr.take(), sink, true).expect("stderr piped");

        // Outer safety net: inner `timeout` should already have fired.
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs + 10),
            child.wait(),
        )
        .await;

        // Await the readers only after the child is gone: while it runs, a
        // long-running command keeps the pipes open and these would block.
        let out = out_task.await.unwrap_or_default();
        let err = err_task.await.unwrap_or_default();

        let (code, timed_out) = match status {
            Ok(Ok(st)) => (st.code(), false),
            Ok(Err(e)) => return ToolResult::err(format!("exec: wait failed: {e}")),
            Err(_) => {
                let _ = child.kill().await;
                (None, true)
            }
        };

        if timed_out {
            return ToolResult {
                ok: false,
                output: combine(&out, &err),
                exit_code: None,
                truncated: false,
                changes: Vec::new(),
                error: Some(timeout_error(timeout_secs)),
                files: Vec::new(),
            };
        }

        // 124 is coreutils `timeout`'s exit code.
        if code == Some(124) {
            return ToolResult {
                ok: false,
                output: combine(&out, &err),
                exit_code: code,
                truncated: false,
                changes: Vec::new(),
                error: Some(timeout_error(timeout_secs)),
                files: Vec::new(),
            };
        }

        let combined = clamp(combine(&out, &err), ctx.max_output_bytes);
        ToolResult {
            ok: code == Some(0),
            output: combined.0,
            exit_code: code,
            truncated: combined.1,
            error: None,
            files: Vec::new(),
            // exec can of course change files, but not through a path this layer
            // knows about, so there is no honest +N -M to show. The card shows the
            // command and its output instead.
            changes: Vec::new(),
        }
    }
}

impl ExecTool {
    /// Resolve the per-call deadline. Accepts a JSON number or a numeric string,
    /// because quantized models emit both. Never exceeds the configured ceiling.
    fn effective_timeout(&self, args: &Value) -> (u64, Option<String>) {
        let requested = match args.get("timeout") {
            Some(Value::Null) | None => None,
            Some(v) => v.as_u64().or_else(|| {
                v.as_str()
                    .map(|s| s.trim().parse::<u64>().ok())
                    .flatten()
            }),
        };
        let Some(r) = requested else {
            return (self.timeout_secs, None)
        };
        if r == 0 {
            return (
                self.timeout_secs,
                Some(format!(
                    "note: timeout 0 is not allowed; using the default {n}s",
                    n = self.timeout_secs
                )),
            )
        }
        if r > self.timeout_secs {
            return (
                self.timeout_secs,
                Some(format!(
                    "note: requested timeout {r}s exceeds the configured ceiling; clamped to {c}s",
                    c = self.timeout_secs
                )),
            )
        }
        (r, None)
    }
}

fn timeout_error(secs: u64) -> String {
    format!(
        "exec: killed after {secs}s. Re-run with a larger \'timeout\' if the command \
         legitimately needs longer (ceiling applies)."
    )
}

fn combine(out: &str, err: &str) -> String {
    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (_, true) => out.to_string(),
        (true, _) => err.to_string(),
        _ => format!("{out}{err}"),
    }
}

/// The verified sandbox shape.
fn sandbox_args(workspace: &std::path::Path) -> Vec<String> {
    let ws = workspace.to_string_lossy().to_string();
    vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        // tmpfs BEFORE any bind under /tmp — ordering is load-bearing.
        "--tmpfs".into(),
        "/tmp".into(),
        "--bind".into(),
        ws.clone(),
        ws,
        "--unshare-user".into(),
        "--unshare-ipc".into(),
        "--unshare-pid".into(),
        "--unshare-net".into(),
        "--die-with-parent".into(),
    ]
}

fn drain<R>(
    stream: Option<R>,
    sink: &ExecSink,
    is_err: bool,
) -> Option<tokio::task::JoinHandle<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let reader = stream?;
    let sink = sink.clone();
    Some(tokio::spawn(async move {
        let mut acc = String::new();
        let mut br = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match br.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    acc.push_str(&line);
                    let _ = sink.send(ExecChunk {
                        stream: if is_err { ExecStream::Stderr } else { ExecStream::Stdout },
                        chunk: line.clone(),
                    });
                }
                Err(_) => break,
            }
        }
        acc
    }))
}

/// How much file content a shell command may carry inline before the call is
/// bounced back with the tool to use named instead.
///
/// Not a security limit: `write_file` accepts far more. It is a legibility limit
/// on the approval card, which quotes the command verbatim.
pub const MAX_INLINE_WRITE: usize = 1200;

/// Find a here-document (or a long redirect) whose payload is being written to a
/// file, and report `(path, payload_bytes)`.
///
/// Deliberately narrow, because a false positive stops a legitimate command:
/// only a redirection that appears in the command *head* - before any
/// here-document body - counts as the target. So `python3 - <<'PY'` with a
/// script that happens to contain `>` is not mistaken for a file write, and a
/// body full of `>` characters does not create a second phantom write.
pub fn oversized_inline_write(cmd: &str) -> Option<(String, usize)> {
    let (head, payload) = match heredoc_span(cmd) {
        Some((head, body)) => (head, body.len()),
        None => (cmd, cmd.len()),
    };
    if payload < MAX_INLINE_WRITE {
        return None;
    }
    let target = redirect_target(head)?;
    if target == "/dev/null" {
        return None;
    }
    Some((target, payload))
}

/// Split at the first here-document: everything before its body, and the body.
fn heredoc_span(cmd: &str) -> Option<(&str, &str)> {
    let b = cmd.as_bytes();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] == b'<' && b[i + 1] == b'<' && !(i + 2 < b.len() && b[i + 2] == b'<') {
            let mut j = i + 2;
            if j < b.len() && b[j] == b'-' {
                j += 1;
            }
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let quote = if j < b.len() && (b[j] == b'\'' || b[j] == b'"') {
                Some(b[j])
            } else {
                None
            };
            if quote.is_some() {
                j += 1;
            }
            let delim_start = j;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j == delim_start {
                // `<<` inside something that is not a here-document (a bit shift
                // in an arithmetic expansion, a stray token): keep looking.
                i += 1;
                continue;
            }
            if let Some(q) = quote {
                // The delimiter has to be closed by the quote it opened with,
                // otherwise this is not a here-document header.
                if j >= b.len() || b[j] != q {
                    i += 1;
                    continue;
                }
                j += 1;
            }
            let delim = &cmd[delim_start..j];
            let body_start = match cmd[j..].find('\n') {
                Some(k) => j + k + 1,
                None => return None,
            };
            let mut body_end = cmd.len();
            for (off, line) in cmd[body_start..].split('\n').enumerate() {
                if line.trim() == delim {
                    body_end = body_start + off;
                    break;
                }
            }
            // `body_end` is the start of the terminator line; the newline before
            // it belongs to the payload, and counting it is fine either way.
            return Some((&cmd[..i], &cmd[body_start..body_end]));
        }
        i += 1;
    }
    None
}

/// The file a redirection in `head` writes, ignoring `2>`-style fd redirects,
/// herestrings and `>|`/`>>` details.
fn redirect_target(head: &str) -> Option<String> {
    let b = head.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'>' && (i + 1 >= b.len() || (b[i + 1] != b'>' && b[i + 1] != b'&')) {
            let prev = if i > 0 { b[i - 1] } else { b' ' };
            // `2>` or `1>&2`: a file descriptor, not a file.
            if prev.is_ascii_digit() {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t' || b[j] == b'\n') {
                j += 1;
            }
            if j >= b.len() || b[j] == b'|' || b[j] == b'&' || b[j] == b'<' {
                i += 1;
                continue;
            }
            let quote = if b[j] == b'\'' || b[j] == b'"' { Some(b[j]) } else { None };
            if let Some(q) = quote {
                let start = j + 1;
                let rest = &head[start..];
                let end = rest.find(q as char)?;
                return Some(rest[..end].to_string());
            }
            let start = j;
            while j < b.len() && b[j] != b' ' && b[j] != b'\t' && b[j] != b'\n' && b[j] != b';' && b[j] != b'|' && b[j] != b'&' {
                j += 1;
            }
            let tok = &head[start..j];
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
        i += 1;
    }
    None
}

fn which(name: &str) -> Option<String> {
    if let Ok(p) = std::env::var("PATH") {
        for dir in p.split(':') {
            let cand = std::path::Path::new(dir).join(name);
            if cand.is_file() {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Collapse `.` and `..` lexically.
///
/// `Path::starts_with` compares components without resolving `..`, so a naive
/// containment check happily accepts `<workspace>/../../etc`. We cannot use
/// `canonicalize` here: the directory may not exist yet, and canonicalize would
/// also follow symlinks out of the workspace in a way the sandbox does not.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;

    /// Removes the test workspace when the test ends.
    struct WsGuard(std::path::PathBuf);
    impl Drop for WsGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sink() -> ExecSink {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
    }

    /// Each test needs its **own** workspace. Keying on the pid is not enough:
    /// all tests in one binary share a pid, and one test deleting the shared
    /// directory makes unrelated tests fail intermittently (this produced two
    /// phantom failures before it was caught).
    fn ws() -> (ToolCtx, WsGuard) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cwoffee_exec_test_{}_{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (ToolCtx { workspace: dir, max_output_bytes: 4096, ..Default::default() }, WsGuard(std::env::temp_dir().join(format!("cwoffee_exec_test_{}_{n}", std::process::id()))))
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t.run(json!({"cmd": "echo hello"}), &ctx, &sink()).await;
        assert!(r.ok, "err={:?}", r.error);
        assert_eq!(r.output.trim(), "hello");
    }

    #[tokio::test]
    async fn captures_stderr_and_nonzero_exit() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t.run(json!({"cmd": "echo oops >&2; exit 3"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("oops"));
    }

    #[tokio::test]
    async fn network_is_blocked() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t
            .run(json!({"cmd": "curl -s -m 3 -o /dev/null http://example.com || echo NET-BLOCKED"}), &ctx, &sink())
            .await;
        assert!(r.output.contains("NET-BLOCKED"), "network was reachable: {:?}", r.output);
    }

    #[tokio::test]
    async fn host_filesystem_is_read_only() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t
            .run(json!({"cmd": "touch /cwoffee_escape_test 2>/dev/null && echo WROTE || echo BLOCKED"}), &ctx, &sink())
            .await;
        assert!(r.output.contains("BLOCKED"), "escaped the sandbox: {:?}", r.output);
        let _ = std::fs::remove_file("/cwoffee_escape_test");
    }

    #[tokio::test]
    async fn workspace_is_writable_and_persists_to_host() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t
            .run(json!({"cmd": "echo hi > persisted.txt && cat persisted.txt"}), &ctx, &sink())
            .await;
        assert!(r.ok, "err={:?}", r.error);
        let on_host = ctx.workspace.join("persisted.txt");
        assert_eq!(std::fs::read_to_string(&on_host).ok().as_deref(), Some("hi\n"));
    }

    #[tokio::test]
    async fn timeout_kills_a_hang() {
        let t = ExecTool { timeout_secs: 2, ..Default::default() };
        let (ctx, _g) = ws();
        let started = std::time::Instant::now();
        let r = t.run(json!({"cmd": "sleep 30"}), &ctx, &sink()).await;
        let elapsed = started.elapsed();
        assert!(!r.ok);
        assert!(elapsed.as_secs() < 8, "took too long: {elapsed:?}");
        assert!(r.error.as_deref().unwrap_or("").contains("killed"));
    }

    #[tokio::test]
    async fn cwd_escape_is_refused() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t.run(json!({"cmd": "pwd", "cwd": "../../etc"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("escapes the workspace"));
    }

    #[tokio::test]
    async fn absolute_cwd_outside_workspace_is_refused() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t.run(json!({"cmd": "pwd", "cwd": "/etc"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("escapes the workspace"));
    }


    #[tokio::test]
    async fn missing_cmd_is_a_clear_error() {
        let t = ExecTool::default();
        let (ctx, _g) = ws();
        let r = t.run(json!({"command": "echo hi"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("cmd"));
    }

    #[test]
    fn a_long_here_document_write_is_bounced_toward_write_file() {
        // The approval card quotes the command verbatim, so a here-document that
        // carries a document makes a prompt nobody can read - and the tool that
        // takes the same bytes as an argument already exists.
        let body = "tea shares coffee's origin in the same highland fog\n".repeat(60);
        let cmd = format!("cat > notes.md <<'EOF'\n{body}EOF\n");
        let hit = oversized_inline_write(&cmd).expect("should be caught");
        assert_eq!(hit.0, "notes.md");
        assert!(hit.1 > MAX_INLINE_WRITE, "{}", hit.1);
    }

    #[test]
    fn a_short_here_document_is_left_alone() {
        let cmd = "cat > notes.md <<'EOF'\nthree lines\nonly three\nEOF\n";
        assert_eq!(oversized_inline_write(cmd), None);
    }

    #[test]
    fn a_script_here_document_is_not_mistaken_for_a_file_write() {
        // A python body full of `>` must not create a phantom redirect: only the
        // command head names a target, and `python3 -` has none.
        let body = "if x > 0 and y > 1: print('greater')\n".repeat(80);
        let cmd = format!("python3 - <<'PY'\n{body}PY\n");
        assert_eq!(oversized_inline_write(&cmd), None);
    }

    #[test]
    fn a_redirect_into_dev_null_is_not_a_file_write() {
        let body = "noise\n".repeat(400);
        let cmd = format!("some-noisy-thing > /dev/null\n{body}");
        assert_eq!(oversized_inline_write(&cmd), None);
    }

    #[test]
    fn a_long_echo_redirect_is_caught_too() {
        let cmd = format!("echo '{}' > story.md", "a long wind-up about a photocopier ".repeat(60));
        let hit = oversized_inline_write(&cmd).expect("should be caught");
        assert_eq!(hit.0, "story.md");
    }

    #[test]
    fn an_stderr_redirect_is_not_read_as_a_file_write() {
        let cmd = format!("make all 2>&1 | tail -5; printf '%s' '{}' > /dev/null", "x".repeat(2000));
        // /dev/null target -> not a write. The point is that a long *output*
        // pipeline is not held to the file-authoring rule.
        assert_eq!(oversized_inline_write(&cmd), None);
    }

    #[tokio::test]
    async fn a_long_here_document_write_is_refused_at_run_time() {
        let (ctx, _g) = ws();
        let body = "line of prose about the harrowing branch library\n".repeat(60);
        let args = json!({ "cmd": format!("cat > big.md <<'EOF'\n{body}EOF\n") });
        let r = ExecTool::default().run(args, &ctx, &sink()).await;
        assert!(!r.ok, "a wall-of-text write should not run");
        let msg = format!("{} {}", r.output, r.error.unwrap_or_default());
        assert!(msg.contains("write_file"), "{}", msg);
        assert!(msg.contains("big.md"), "{}", msg);
    }

    #[tokio::test]
    async fn a_small_here_document_still_runs() {
        // The guard is about legibility of the prompt, not about sandbox policy:
        // a two-line here-document must work exactly as before.
        let (ctx, _g) = ws();
        let args = json!({ "cmd": "cat > small.md <<'EOF'\nte\nEOF\ncat small.md" });
        let r = ExecTool::default().run(args, &ctx, &sink()).await;
        assert!(r.ok, "{}", r.output);
        assert!(r.output.contains("te"), "{}", r.output);
    }

    #[tokio::test]
    async fn output_is_clamped() {
        let t = ExecTool::default();
        let (mut ctx, _g) = ws();
        ctx.max_output_bytes = 200;
        let r = t.run(json!({"cmd": "seq 1 2000"}), &ctx, &sink()).await;
        assert!(r.truncated);
        assert!(r.output.len() < 500);
    }

    // ---- per-call deadline -------------------------------------------------

    #[test]
    fn per_call_timeout_clamps_to_the_configured_ceiling() {
        let t = ExecTool { timeout_secs: 30, ..Default::default() };
        assert_eq!(t.effective_timeout(&json!({})), (30, None));
        assert_eq!(t.effective_timeout(&json!({"timeout": 5})), (5, None));
        // Quantized models emit numbers and numeric strings interchangeably.
        assert_eq!(t.effective_timeout(&json!({"timeout": "8"})), (8, None));
        let (secs, note) = t.effective_timeout(&json!({"timeout": 3600}));
        assert_eq!(secs, 30, "the ceiling belongs to the user, not the model");
        assert!(note.unwrap().contains("clamped to 30s"));
        let (secs, note) = t.effective_timeout(&json!({"timeout": 0}));
        assert_eq!(secs, 30, "0 would mean an instant kill");
        assert!(note.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn a_short_per_call_timeout_kills_a_long_command() {
        // Ceiling is generous; the call asks for one second. If the per-call
        // value were ignored this would take 5s and succeed.
        let t = ExecTool { timeout_secs: 60, ..Default::default() };
        let (ctx, _g) = ws();
        let started = std::time::Instant::now();
        let r = t.run(json!({"cmd": "sleep 5; echo done", "timeout": 1}), &ctx, &sink()).await;
        let took = started.elapsed();
        assert!(!r.ok, "a 1s budget must not complete a 5s command");
        assert!(took < std::time::Duration::from_secs(4), "took {took:?}");
        let err = r.error.unwrap_or_default();
        assert!(err.contains("killed after 1s"), "err={err}");
        assert!(err.contains("timeout"), "the model has to be told how to fix it: {err}");
    }

    #[tokio::test]
    async fn a_clamped_deadline_is_reported_to_the_model() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = ExecTool { timeout_secs: 5, ..Default::default() };
        let (ctx, _g) = ws();
        let r = t.run(json!({"cmd": "echo hi", "timeout": 600}), &ctx, &tx).await;
        assert!(r.ok);
        let mut seen = String::new();
        while let Ok(c) = rx.try_recv() {
            seen.push_str(&c.chunk);
        }
        assert!(seen.contains("clamped to 5s"), "chunks={seen:?}");
    }
}
