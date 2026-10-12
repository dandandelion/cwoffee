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

/// The prefix of the line a sandbox refusal appends to a command's output.
///
/// It is one constant because two facts have to agree: the tool description tells
/// the model to look for this exact string, and the execution path is what writes
/// it. Written separately, they drift, and a model told to distrust its own
/// instincts starts describing a refusal that never happened.
pub const EGRESS_BLOCKED_MARK: &str = "# egress blocked:";

pub struct ExecTool {
    /// Seconds before the command is killed.
    pub timeout_secs: u64,
    /// When false, refuse to run anything unless bwrap is available.
    pub allow_unsandboxed: bool,
    /// What the sandbox may reach over the network: nothing, an allowlisted proxy,
    /// or everything. See `tools::net` for what each one actually guarantees.
    pub net: crate::tools::net::NetCfg,
    /// The egress proxy, started on first use. Lazy because the registry is built
    /// before anyone knows whether a turn will ever need the network, and a
    /// listening socket is a thing to avoid when it is not needed.
    pub(crate) proxy: tokio::sync::OnceCell<crate::tools::net::EgressProxy>,
    /// The tool description, built once from the egress mode. It is a field rather
    /// than a literal because "no network" is a lie in the default mode, and a
    /// model that believes it spends the turn looking for an offline wheel.
    desc: String,
}

impl Default for ExecTool {
    fn default() -> Self {
        // Matches the documented config default. Two different "defaults" for
        // the same knob is how a config key ends up looking ignored.
        Self {
            timeout_secs: crate::config::default_exec_timeout(),
            allow_unsandboxed: false,
            net: crate::tools::net::NetCfg::default(),
            proxy: tokio::sync::OnceCell::new(),
            desc: blurb(crate::tools::net::NetCfg::default().mode),
        }
    }
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        &self.desc
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

        // What this call may reach over the network. `off` keeps --unshare-net and
        // nothing else changes; `proxy` shares the network namespace and points
        // every tool that reads a proxy variable at a proxy in this process;
        // `open` shares it with nothing in between.
        let net = self.net.mode;
        let egress: Option<&crate::tools::net::EgressProxy> = match net {
            crate::tools::net::NetMode::Proxy => match self
                .proxy
                .get_or_try_init(|| async {
                    crate::tools::net::start(
                        self.net.allow.clone(),
                        self.net.max_bytes_per_call,
                    )
                    .await
                })
                .await
            {
                Ok(p) => Some(p),
                // A proxy that will not start is not a licence to go out unfiltered.
                Err(e) => return ToolResult::err(format!("exec: {e}")),
            },
            _ => None,
        };
        if let Some(e) = egress {
            // Reset per call, so the egress lines attached to a result describe
            // that result. Two exec calls running in parallel share the counters;
            // the note says "this call and any other running alongside it", which
            // is what it actually is.
            e.state.begin_call(self.net.max_bytes_per_call);
        }

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
            let mut a = sandbox_args(&ctx.workspace, net);
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
        if let Some(e) = egress {
            // Six spellings because six programs read six different names; see
            // net::proxy_env. NO_PROXY keeps loopback direct, so a command in the
            // sandbox can still reach the app's own gateway or a local model
            // endpoint without detouring through an egress filter.
            for (k, v) in crate::tools::net::proxy_env(e.port) {
                c.env(k, v);
            }
        }

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
        // A failed command whose text says "the sandbox has no network" is a
        // diagnosis the model has to read and interpret; the follow-up call it
        // wastes is the cost of not saying it outright. Notes are only added on
        // failure, and only for signatures with exactly one right answer.
        let mut body = combined.0;
        // Not gated on the exit code. `curl ... | head -2` exits 0 - the pipeline's
        // status is head's - and the session that hit this saw a dead-network
        // message with no explanation because of exactly that pipe. The note keys on
        // phrases specific enough (an errno, an interpreter error, a shell quote
        // error) that a command printing one by accident is rarer than a command
        // hiding its status behind a pipe.
        if let Some(note) = failure_note(&body, net) {
            body.push_str(&format!("\n# note: {note}"));
        }
        // What the proxy decided, in the same place the model reads the answer.
        // A 403 from an allowlist is otherwise a mystery: npm calls it ECONNREFUSED
        // or a TLS error, and the model has no way to know a config line fixes it.
        if let Some(e) = egress {
            let hosts: Vec<String> = e
                .state
                .hosts_denied()
                .into_iter()
                .filter(|h| !h.starts_with("#bytes"))
                .collect();
            let bytes = e.state.bytes();
            if !hosts.is_empty() {
                body.push_str(&format!(
                    "\n{EGRESS_BLOCKED_MARK} {} - not in the sandbox allowlist. Add them to \
                     allow = [...] under [sandbox] in config.toml, or ask the user to run the \
                     command outside Cwoffee.",
                    hosts.join(", ")
                ));
            }
            if e.state.hosts_denied().iter().any(|h| h == "#bytes") {
                body.push_str(&format!(
                    "\n# egress budget spent for this call ({} MB through the proxy) - raise \
                     max_bytes_per_call under [sandbox] if a download genuinely needs more.",
                    bytes / 1_000_000
                ));
            } else if bytes > 2_000_000 {
                body.push_str(&format!("\n# egress: {} MB through the proxy", bytes / 1_000_000));
            }
        }
        ToolResult {
            ok: code == Some(0),
            output: body,
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

/// One line explaining a failure the **environment** caused, when the raw output
/// is a dead end for something that cannot see the machine.
///
/// Every entry here came out of a real session: `npm error code ENETUNREACH`
/// (the sandbox has no network, which is not visible from the text), and
/// `ModuleNotFoundError: No module named 'openpyxl'` (followed by a hand-written
/// xlsx parser). Adding the note costs one line; the wasted call it prevents is
/// a full round trip of prefill.
fn failure_note(text: &str, net: crate::tools::net::NetMode) -> Option<&'static str> {
    const NETWORK: [&str; 8] = [
        "enetunreach",
        "enetdown",
        "econnunreachable",
        "could not resolve host",
        "name or service not known",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
    ];
    let low = text.to_ascii_lowercase();
    // curl's own words when a route does not exist, which are different words in
    // every version and contain no errno. Only `off` reads them as "no network":
    // in `proxy` mode the same words mean the proxy itself is unreachable, and in
    // `open` mode they mean the machine has no route.
    let curl_no_route = matches!(net, crate::tools::net::NetMode::Off)
        && (low.contains("could not connect to server") || low.contains("failed to connect to"));
    if NETWORK.iter().any(|p| low.contains(p)) || curl_no_route {
        // The answer depends on the mode, and the model has to be told the right
        // one: in `off` there is nothing to configure per host, in `proxy` one
        // allowlist line fixes it, and in `open` the failure is the user's network.
        return match net {
            crate::tools::net::NetMode::Off => Some(
                "the exec sandbox has no network access, by design. Package installs (apt, pip, npm, \
             cargo) and any curl/wget to the internet will not work in here; web_fetch and \
             download_file are the sanctioned network tools, and installing something on the \
             machine is a step the user takes outside Cwoffee. To change this, set \
             [sandbox] network = \"proxy\" (allowlisted egress) or \"open\".",
            ),
            crate::tools::net::NetMode::Proxy => Some(
                "this looks like a network failure under the sandbox egress proxy. If the host \
                 you needed is not listed in the `# egress blocked` line above, add it to \
                 allow = [...] under [sandbox] in config.toml.",
            ),
            crate::tools::net::NetMode::Open => Some(
                "network failure with the sandbox's network open, so this is the machine's own \
                 connectivity, not the sandbox.",
            ),
        };
    }
    if low.contains("no module named") {
        // The note is the same either way; only the parenthetical is mode-dependent,
        // because under a proxy the package *could* be installed by the user and it
        // would be a lie to say the sandbox has no network.
        return Some(match net {
            crate::tools::net::NetMode::Off =>
                "that python package is not installed on the host, and the sandbox has no network \
                 to install it with. Do not write your own parser for the job: spreadsheets are \
                 sheet_read and sheet_query, documents are write_file.",
            _ =>
                "that python package is not installed on the host. Do not write your own parser \
                 for the job: spreadsheets are sheet_read and sheet_query, documents are \
                 write_file; a package the user can install, they install outside Cwoffee.",
        });
    }
    // The other half of the same misunderstanding as ENETUNREACH behind a proxy. A
    // git+ssh remote is not refused here, it hangs, and the words it dies with
    // ("Permission denied (publickey)", a banner-exchange timeout) read like a bad
    // key or a flaky network. They are neither: no route carries that transport, and
    // the fix is one `git remote set-url` the model can run itself. Without this it
    // spends several tool loops rediscovering https.
    if matches!(net, crate::tools::net::NetMode::Proxy)
        && (low.contains("permission denied (publickey)")
            || low.contains("banner exchange")
            || low.contains("ssh: connect to host")
            || low.contains("could not read username"))
    {
        return Some(
            "the sandbox egress route carries HTTP and HTTPS only, so an ssh remote hangs rather \
             than being refused. Rewrite the remote (git remote set-url origin https://...) instead \
             of debugging keys, or ask the user to set [sandbox] network = \"open\".",
        );
    }
    if low.contains("unexpected eof while looking for matching") {
        return Some(
            "the command's quotes do not balance, usually because the argument string was cut off \
             part-way. Keep shell commands to one level of quoting, and put file content in \
             write_file rather than a here-document.",
        );
    }
    None
}

/// The exec description for one egress mode. One shared body plus one sentence
/// about the network, so the mode the user configured is the mode the model is
/// told about - and the sentence is short because these bytes sit in every turn.
fn blurb(net: crate::tools::net::NetMode) -> String {
    let network: String = match net {
        crate::tools::net::NetMode::Off =>
            "There is no network in here: package installs and any request to the \
             internet cannot work, so do not try them.".into(),
        crate::tools::net::NetMode::Proxy =>
            format!(
                "Network access IS available here: the sandbox routes out through an allowlist \
                 proxy, and it already admits the hosts package managers dial - npm, Yarn, PyPI, \
                 crates.io, Go, RubyGems, GitHub (including git clone/pull/fetch), GitLab, \
                 jsDelivr, unpkg, deno, Hugging Face. So run the command: do not assume you are \
                 offline, do not tell the user you have no internet, and do not hand-write a \
                 substitute for `npm install`, `pip install`, `cargo add` or `git pull`. A real \
                 refusal is a line in the tool result that begins `{EGRESS_BLOCKED_MARK}` and names \
                 the host; any other failure is an ordinary network error, not the sandbox. A \
                 proxy filters where you go; it does not hide that you went. The route carries HTTP \
                 and HTTPS only, so an ssh:// or git@ remote hangs here instead of being refused: \
                 rewrite the remote to https:// rather than waiting on a transport that cannot exist."
            ),
        crate::tools::net::NetMode::Open =>
            "The network is open: failures there are the machine's connectivity, \
             not the sandbox.".into(),
    };
    format!(
        "Run a shell command in a sandbox (host filesystem read-only, only the \
         workspace is writable). {network} Each call gets a fresh filesystem: /tmp \
         and anything you create outside the workspace is gone before the next \
         call, so scratch files belong in the workspace. Returns stdout and stderr \
         combined. Do not author file contents here: no here-documents and no long \
         echo/printf redirections. Write files with write_file, which takes the \
         content as an argument and shows the approver a diff instead of a wall of \
         text."
    )
}

impl ExecTool {
    /// Build from the config-shaped knobs. A constructor rather than a struct
    /// literal at every call site, so adding a field cannot silently drop a
    /// configured value on the way in.
    pub fn new(cfg: &crate::tools::ExecCfg) -> Self {
        Self {
            timeout_secs: cfg.timeout_secs,
            // `enabled = false` is a debugging escape hatch; it flips the tool into
            // unsandboxed mode, which is why the receiving side spells it
            // allow_unsandboxed.
            allow_unsandboxed: !cfg.sandbox_enabled,
            net: cfg.net.clone(),
            proxy: tokio::sync::OnceCell::new(),
            desc: blurb(cfg.net.mode),
        }
    }

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

/// The verified sandbox shape. `mode` decides only the network namespace: every
/// other isolation (read-only root, own user, own PID, own IPC, workspace as the
/// only writable bind) holds in all three modes.
fn sandbox_args(workspace: &std::path::Path, net: crate::tools::net::NetMode) -> Vec<String> {
    let ws = workspace.to_string_lossy().to_string();
    let mut out = vec![
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
        "--die-with-parent".into(),
    ];
    // Adding it back is what takes the network away, and only `off` can afford
    // it: with no network there is no route to the proxy either, so `proxy` mode
    // without a namespace is the only shape where an allowlist decision can exist.
    if crate::tools::net::unshare_net(net) {
        out.push("--unshare-net".into());
    }
    out
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

    /// A tool pinned to one egress mode. `ExecTool::default()` is `proxy`, which
    /// starts a proxy - a no-network test would then be measuring the proxy rather
    /// than the network namespace.
    fn tool(net: crate::tools::net::NetMode) -> ExecTool {
        ExecTool {
            timeout_secs: 20,
            net: crate::tools::net::NetCfg { mode: net, ..Default::default() },
            ..Default::default()
        }
    }

    async fn run(t: &ExecTool, ctx: &ToolCtx, cmd: &str) -> ToolResult {
        t.run(json!({"cmd": cmd}), ctx, &sink()).await
    }

    /// The sandbox's no-network rule is invisible in npm's own words, and the
    /// session that hit it spent calls probing for a toolchain that cannot exist in
    /// there.
    #[tokio::test]
    async fn a_network_failure_says_the_sandbox_is_offline() {
        let (ctx, _g) = ws();
        let t = tool(crate::tools::net::NetMode::Off);
        let r = run(
            &t,
            &ctx,
            "{ echo 'npm error code ENETUNREACH'; echo 'npm error errno ENETUNREACH'; exit 1; }",
        )
        .await;
        assert!(!r.ok, "{}", r.output);
        assert!(
            r.output.contains("# note: the exec sandbox has no network access"),
            "{}",
            r.output
        );
        assert!(
            r.output.contains("[sandbox] network = \"proxy\""),
            "the note must name the knob that changes the answer: {}",
            r.output
        );
    }

    /// `network = "open"` has to be more than the absence of --unshare-net: a
    /// command inside the sandbox has to actually reach a socket. The listener is
    /// loopback so the test does not depend on the internet, and loopback is
    /// exactly what NO_PROXY keeps direct in proxy mode too.
    #[tokio::test]
    async fn open_mode_reaches_a_socket_outside_the_process() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut c, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = std::io::Read::read(&mut c, &mut buf);
                let body = "OPEN-MODE-OK";
                let _ = std::io::Write::write_all(
                    &mut c,
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
                        .as_bytes(),
                );
            }
        });
        let t = tool(crate::tools::net::NetMode::Open);
        let (ctx, _g) = ws();
        let r = t
            .run(json!({"cmd": format!("curl -s -m 6 http://127.0.0.1:{port}/")}), &ctx, &sink())
            .await;
        assert!(
            r.output.contains("OPEN-MODE-OK"),
            "open mode could not reach loopback: {:?}",
            r.output
        );
        let _ = server.join();
    }

    /// The other dead end from the same session: `import openpyxl` fails, and the
    /// next thing a model writes is a 111-line xlsx parser.
    #[tokio::test]
    async fn a_missing_python_module_points_at_the_tools() {
        let (ctx, _g) = ws();
        let t = tool(crate::tools::net::NetMode::Off);
        // Synthesise the interpreter's text: the note keys on the signature, and
        // depending on which packages this machine happens to have installed makes
        // a test pass or fail by host.
        let r = run(
            &t,
            &ctx,
            "printf 'ModuleNotFoundError: No module named %s\\n' openpyxl; exit 1",
        )
        .await;
        assert!(!r.ok, "{}", r.output);
        assert!(r.output.contains("# note: that python package is not installed"), "{}", r.output);
        assert!(r.output.contains("sheet_read"), "{}", r.output);
    }

    /// In `proxy` mode the same network words get a different answer, because the
    /// right fix is a line in the allowlist rather than a policy change.
    /// curl's text carries no errno, and the off-mode note has to fire on it: the
    /// probe that found this produced "Could not connect to server" for a registry
    /// URL with no route.
    #[test]
    fn curl_words_without_an_errno_still_read_as_no_route_in_off_mode() {
        use crate::tools::net::NetMode;
        let curl = "curl: (7) Failed to connect to registry.npmjs.org port 443 after 16 ms: Could not connect to server";
        assert!(failure_note(curl, NetMode::Off).unwrap().contains("no network access"));
        // In proxy mode these words mean the proxy itself was unreachable, and the
        // `# egress blocked` line the tool appends is the answer. What must not
        // happen is the note claiming the sandbox has no network.
        let proxy = failure_note(curl, NetMode::Proxy);
        assert!(proxy.is_none() || !proxy.unwrap().contains("no network access"));
    }

    #[test]
    fn the_network_note_depends_on_the_mode() {
        use crate::tools::net::NetMode;
        let en = "npm error code ENETUNREACH";
        assert!(failure_note(en, NetMode::Off).unwrap().contains("no network access"));
        let prox = failure_note(en, NetMode::Proxy).unwrap();
        assert!(prox.contains("egress proxy") && prox.contains("allow = ["), "{prox}");
        assert!(failure_note(en, NetMode::Open).unwrap().contains("machine's own"));
    }

    /// And no note where there is nothing to explain: an ordinary failure and a
    /// successful command both stay exactly as long as they were.
    #[tokio::test]
    async fn notes_are_not_attached_to_ordinary_failures_or_success() {
        let (ctx, _g) = ws();
        let t = tool(crate::tools::net::NetMode::Off);
        let plain = run(&t, &ctx, "{ echo 'boom'; exit 3; }").await;
        assert!(!plain.ok);
        assert!(!plain.output.contains("# note:"), "{}", plain.output);
        let good = run(&t, &ctx, "echo fine").await;
        assert!(good.ok);
        assert!(good.output.trim_end().ends_with("fine"), "{}", good.output);
        // A pipe hides the failing command's status, and the note must survive it.
        let piped = run(
            &t,
            &ctx,
            "curl -sS -m 10 -o /dev/null https://registry.npmjs.org/express 2>&1 | head -2",
        )
        .await;
        assert_eq!(piped.exit_code, Some(0), "the pipe should hide the status");
        assert!(
            piped.output.contains("# note: the exec sandbox has no network access"),
            "no note on a piped network failure: {:?}",
            piped.output
        );
    }

    #[test]
    fn unbalanced_quotes_are_named() {
        use crate::tools::net::NetMode;
        assert!(
            failure_note(r#"/bin/sh: -c: line 1: unexpected EOF while looking for matching `"'"#, NetMode::Off)
                .unwrap()
                .contains("quotes do not balance")
        );
        assert!(failure_note("Traceback: TypeError: NoneType is not callable", NetMode::Off).is_none());
        // Same network text, three different correct answers.
        let en = "npm error code ENETUNREACH";
        assert!(failure_note(en, NetMode::Off).unwrap().contains("no network access"));
        assert!(failure_note(en, NetMode::Proxy).unwrap().contains("egress blocked"));
        assert!(failure_note(en, NetMode::Open).unwrap().contains("machine's own"));
    }

    /// The description is prompt bytes on every turn, so it is one sentence per
    /// mode - but it must still be true, and the configured mode must survive the
    /// trip from config to tool.
    #[test]
    fn the_description_says_what_the_mode_actually_allows() {
        use crate::tools::net::NetMode;
        let off = blurb(NetMode::Off);
        assert!(off.contains("There is no network"));
        assert!(!off.contains("allowlist proxy"));
        let proxy = blurb(NetMode::Proxy);
        assert!(proxy.contains("allowlist proxy"));
        assert!(!proxy.contains("There is no network"));
        // The complaint this answers: a model that reads "allowlist proxy" as
        // "probably blocked" and spends two tool loops talking itself out of a
        // command it could have run on the first try.
        for needle in [
            "Network access IS available",
            "run the command",
            "do not assume you are offline",
            "npm install",
            EGRESS_BLOCKED_MARK,
        ] {
            assert!(proxy.contains(needle), "the proxy blurb lacked {needle:?}");
        }
        // The promise and the behaviour are one constant: if the execution path
        // stops writing the mark the description points at, this fails.
        assert!(
            crate::tools::exec::EGRESS_BLOCKED_MARK.starts_with("# egress blocked"),
            "the mark drifted from what the description quotes"
        );
        assert!(blurb(NetMode::Open).contains("The network is open"));

        let cfg = crate::tools::ExecCfg {
            timeout_secs: 33,
            sandbox_enabled: true,
            net: crate::tools::net::NetCfg {
                mode: NetMode::Open,
                allow: vec!["example.org".into()],
                max_bytes_per_call: 4096,
            },
        };
        let t = ExecTool::new(&cfg);
        assert_eq!(t.net, cfg.net, "ExecTool::new dropped a configured value");
        assert_eq!(t.timeout_secs, 33);
        assert!(t.description().contains("The network is open"));
    }

    /// The network namespace is the only part of the sandbox the knob moves. If a
    /// future edit drops the filesystem binds or the PID namespace while someone
    /// is "just turning the network on", this fails.
    #[test]
    fn only_the_network_namespace_moves_between_modes() {
        use crate::tools::net::NetMode;
        let ws = std::path::Path::new("/ws");
        let off = sandbox_args(ws, NetMode::Off);
        let proxy = sandbox_args(ws, NetMode::Proxy);
        let open = sandbox_args(ws, NetMode::Open);
        assert!(off.iter().any(|a| a == "--unshare-net"));
        assert!(!proxy.iter().any(|a| a == "--unshare-net"));
        assert!(!open.iter().any(|a| a == "--unshare-net"));
        for arg in [
            "--ro-bind",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--tmpfs",
            "--die-with-parent",
        ] {
            assert!(proxy.iter().any(|a| a == arg), "{arg} missing in proxy mode");
            assert!(open.iter().any(|a| a == arg), "{arg} missing in open mode");
        }
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

    /// `network = "off"` is the mode where the namespace really has no route.
    #[tokio::test]
    async fn off_mode_has_no_route_at_all() {
        let t = tool(crate::tools::net::NetMode::Off);
        let (ctx, _g) = ws();
        let r = t
            .run(
                json!({"cmd": "curl -s -m 3 -o /dev/null http://example.com || echo NET-BLOCKED"}),
                &ctx,
                &sink(),
            )
            .await;
        assert!(r.output.contains("NET-BLOCKED"), "network was reachable: {:?}", r.output);
    }

    /// The default mode: a real command inside the sandbox, a real request, and a
    /// refusal the model can act on. curl reports success on a 403 (no --fail), so
    /// the proof is the body it prints and the egress line the tool appends.
    #[tokio::test]
    async fn proxy_mode_refuses_an_unlisted_host_and_says_so_where_the_model_reads_it() {
        let t = tool(crate::tools::net::NetMode::Proxy);
        let (ctx, _g) = ws();
        let r = t
            .run(json!({"cmd": "curl -s -m 8 http://example.com/ 2>&1 | head -3"}), &ctx, &sink())
            .await;
        assert!(
            r.output.contains("not in the sandbox egress allowlist"),
            "no refusal body: {:?}",
            r.output
        );
        assert!(
            r.output.contains("# egress blocked: example.com"),
            "the tool did not report the refusal: {:?}",
            r.output
        );
        assert!(r.output.contains("allow = [\"example.com\"]"), "{}", r.output);
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
    fn an_ssh_failure_under_a_proxy_names_the_transport_not_the_key() {
        // What the model actually did: `git pull` on a git@ remote hung, it read
        // "Permission denied (publickey)", spent two more loops on ssh-agent and
        // known_hosts, and only then tried https. The sandbox cannot carry ssh at
        // all, so the note has to say that in the first failure.
        for probe in [
            "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.",
            "ssh: connect to host github.com port 22: Connection timed out during banner exchange",
        ] {
            let note = failure_note(probe, crate::tools::net::NetMode::Proxy).unwrap();
            assert!(
                note.contains("HTTP and HTTPS only") && note.contains("git remote set-url"),
                "the ssh note said: {note}"
            );
        }
        // The same words with no proxy in play are not the sandbox's business.
        assert!(
            failure_note("Permission denied (publickey).", crate::tools::net::NetMode::Open).is_none(),
            "an auth failure with the network open was blamed on the sandbox"
        );
    }

    #[test]
    fn the_proxy_blurb_rules_out_ssh_by_name() {
        let proxy = blurb(crate::tools::net::NetMode::Proxy);
        assert!(
            proxy.contains("ssh://") && proxy.contains("https://"),
            "the blurb advertised the proxy without saying which transports it carries"
        );
        assert!(
            !blurb(crate::tools::net::NetMode::Open).contains("ssh://"),
            "the open-network blurb carried a restriction that is not in place"
        );
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
