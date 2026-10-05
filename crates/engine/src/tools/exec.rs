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
         only the workspace is writable). Returns stdout and stderr combined."
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
        Risk::Exec
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, sink: &ExecSink) -> ToolResult {
        let Some(cmd) = super::repair::field_as_string(&args, "cmd") else {
            return ToolResult::err("exec: missing required string field \"cmd\"");
        };

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
