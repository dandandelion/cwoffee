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
        Self { timeout_secs: 60, allow_unsandboxed: false }
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
                self.timeout_secs.to_string(),
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
            std::time::Duration::from_secs(self.timeout_secs + 10),
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
                error: Some(format!("exec: killed after {}s", self.timeout_secs)),
            };
        }

        // 124 is coreutils `timeout`'s exit code.
        if code == Some(124) {
            return ToolResult {
                ok: false,
                output: combine(&out, &err),
                exit_code: code,
                truncated: false,
                error: Some(format!("exec: killed after {}s", self.timeout_secs)),
            };
        }

        let combined = clamp(combine(&out, &err), ctx.max_output_bytes);
        ToolResult {
            ok: code == Some(0),
            output: combined.0,
            exit_code: code,
            truncated: combined.1,
            error: None,
        }
    }
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
fn lexical_resolve(base: &std::path::Path, rel: &str) -> std::path::PathBuf {
    use std::path::{Component, Path, PathBuf};
    let rel_path = Path::new(rel);
    let mut out = if rel_path.is_absolute() {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    for comp in rel_path.components() {
        match comp {
            Component::RootDir => out = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::Prefix(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;

    fn sink() -> ExecSink {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
    }

    fn ws() -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("cowry_exec_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ToolCtx { workspace: dir, max_output_bytes: 4096 }
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t.run(json!({"cmd": "echo hello"}), &ctx, &sink()).await;
        assert!(r.ok, "err={:?}", r.error);
        assert_eq!(r.output.trim(), "hello");
    }

    #[tokio::test]
    async fn captures_stderr_and_nonzero_exit() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t.run(json!({"cmd": "echo oops >&2; exit 3"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("oops"));
    }

    #[tokio::test]
    async fn network_is_blocked() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t
            .run(json!({"cmd": "curl -s -m 3 -o /dev/null http://example.com || echo NET-BLOCKED"}), &ctx, &sink())
            .await;
        assert!(r.output.contains("NET-BLOCKED"), "network was reachable: {:?}", r.output);
    }

    #[tokio::test]
    async fn host_filesystem_is_read_only() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t
            .run(json!({"cmd": "touch /cowry_escape_test 2>/dev/null && echo WROTE || echo BLOCKED"}), &ctx, &sink())
            .await;
        assert!(r.output.contains("BLOCKED"), "escaped the sandbox: {:?}", r.output);
        let _ = std::fs::remove_file("/cowry_escape_test");
    }

    #[tokio::test]
    async fn workspace_is_writable_and_persists_to_host() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t
            .run(json!({"cmd": "echo hi > persisted.txt && cat persisted.txt"}), &ctx, &sink())
            .await;
        assert!(r.ok, "err={:?}", r.error);
        let on_host = ctx.workspace.join("persisted.txt");
        assert_eq!(std::fs::read_to_string(&on_host).ok().as_deref(), Some("hi\n"));
        let _ = std::fs::remove_dir_all(&ctx.workspace);
    }

    #[tokio::test]
    async fn timeout_kills_a_hang() {
        let t = ExecTool { timeout_secs: 2, ..Default::default() };
        let ctx = ws();
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
        let ctx = ws();
        let r = t.run(json!({"cmd": "pwd", "cwd": "../../etc"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("escapes the workspace"));
    }

    #[tokio::test]
    async fn absolute_cwd_outside_workspace_is_refused() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t.run(json!({"cmd": "pwd", "cwd": "/etc"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("escapes the workspace"));
    }

    #[test]
    fn lexical_resolve_collapses_dotdot() {
        let base = std::path::Path::new("/home/u/ws");
        assert_eq!(lexical_resolve(base, "a/b").to_str().unwrap(), "/home/u/ws/a/b");
        assert_eq!(lexical_resolve(base, "a/../b").to_str().unwrap(), "/home/u/ws/b");
        assert_eq!(lexical_resolve(base, "../../etc").to_str().unwrap(), "/home/etc");
        assert_eq!(lexical_resolve(base, "../../../etc").to_str().unwrap(), "/etc");
        assert_eq!(lexical_resolve(base, "/etc").to_str().unwrap(), "/etc");
        assert_eq!(lexical_resolve(base, "./x").to_str().unwrap(), "/home/u/ws/x");
        // the point: these must not look contained
        assert!(!lexical_resolve(base, "../../etc").starts_with(base));
        assert!(lexical_resolve(base, "sub/dir").starts_with(base));
    }

    #[tokio::test]
    async fn missing_cmd_is_a_clear_error() {
        let t = ExecTool::default();
        let ctx = ws();
        let r = t.run(json!({"command": "echo hi"}), &ctx, &sink()).await;
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap_or("").contains("cmd"));
    }

    #[tokio::test]
    async fn output_is_clamped() {
        let t = ExecTool::default();
        let mut ctx = ws();
        ctx.max_output_bytes = 200;
        let r = t.run(json!({"cmd": "seq 1 2000"}), &ctx, &sink()).await;
        assert!(r.truncated);
        assert!(r.output.len() < 500);
    }
}
