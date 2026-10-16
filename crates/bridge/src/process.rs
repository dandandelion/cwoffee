//! One `pi --mode rpc` child process, spoken to over LF-delimited JSON.
//!
//! Three things here are deliberate and worth keeping if this file grows:
//!
//! 1. **Framing is bytes, split on `0x0A`, and nothing else.** pi frames JSONL
//!    with LF, and U+2028/U+2029 are legal characters inside a JSON string. A
//!    line reader that treats them as terminators (`readline`, and most
//!    text-mode "lines" iterators) will cut a record in half in the middle of
//!    the one payload most likely to contain them: text the model wrote.
//! 2. **Correlation is by the `id` we send**, echoed on the matching
//!    `{"type":"response"}` record. Everything that is not a response to one of
//!    our commands is a stream record and goes to the caller untouched.
//! 3. **The child cannot outlive us.** `kill_on_drop` plus an explicit `reap()`
//!    is the whole answer to "the app quit but a 74 MB node process is still
//!    running". pi has no sandbox and no reason to survive us.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum PiError {
    #[error("could not start {bin}: {source}")]
    Spawn { bin: String, source: std::io::Error },
    #[error("pi closed the pipe while waiting for {command}")]
    Closed { command: String },
    #[error("pi refused {command}: {error}")]
    Rpc { command: String, error: String },
    #[error("no answer from pi for {command} after {secs}s")]
    Timeout { command: String, secs: u64 },
    #[error("pi at {bin} reports version {found}; cwoffee is pinned to {want}")]
    Version { bin: String, found: String, want: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The pi release this bridge is built against (locked decision 13). The version
/// handshake compares `pi --version` to this at spawn, so an upgrade is a commit
/// that also re-runs the protocol gates, not a surprise at boot on a user's box.
pub const PINNED_PI: &str = "1.2.0";

/// How to start one pi process. One process per workspace: pi has no RPC verb
/// that changes the working directory of a live session, so moving the tools to
/// another folder is process lifecycle, not a command (PI-CORE-PROTOCOL.md).
#[derive(Debug, Clone)]
pub struct PiLaunch {
    pub bin: PathBuf,
    pub workdir: PathBuf,
    pub provider: String,
    pub model: String,
    /// pi's config home: models.json, extensions/, its own state. Isolated per
    /// bridge so a test run never edits the `pi` the user types at a terminal.
    pub agent_dir: PathBuf,
    /// Where pi writes its JSONL sessions. Kept separate from `agent_dir` so the
    /// transcripts of a cwoffee install have one obvious place to look.
    pub session_dir: PathBuf,
    /// Resume this conversation file. `None` starts a new one.
    pub session_file: Option<PathBuf>,
    pub extensions: Vec<PathBuf>,
    /// `--tools` entries. A coworker product names them all as `-`/`+` edits so
    /// pi's own `read`/`bash`/`edit`/`write` stay off: the tools that matter are
    /// ours, and the guardrails live with them.
    pub tools: Vec<String>,
    /// Chat mode: `--no-tools`, which covers built-in *and* extension tools, and no
    /// extension is mounted either. Asking pi to be a plain assistant means taking the
    /// tools out of the process, not telling a tool host to behave.
    pub no_tools: bool,
    /// Replaces pi's own prompt. In Chat mode cwoffee's prompt is one line, and it is
    /// the whole prompt: anything longer claims capabilities this mode does not have.
    pub system_prompt: Option<String>,
    pub extra_env: Vec<(String, String)>,
    /// Refuse to start unless `pi --version` says exactly this. A silent protocol
    /// drift between a shipped runtime and a pinned bridge is the worst possible
    /// failure mode to discover on a user's machine (locked decision 13).
    pub pinned_version: Option<String>,
}

impl PiLaunch {
    pub fn argv(&self) -> Vec<String> {
        let mut a = vec![
            "--mode".into(),
            "rpc".into(),
            "--provider".into(),
            self.provider.clone(),
            "--model".into(),
            self.model.clone(),
            "--session-dir".into(),
            self.session_dir.display().to_string(),
        ];
        if let Some(f) = &self.session_file {
            a.push("--session".into());
            a.push(f.display().to_string());
        }
        for e in &self.extensions {
            a.push("-e".into());
            a.push(e.display().to_string());
        }
        if self.no_tools {
            a.push("--no-tools".into());
        } else if !self.tools.is_empty() {
            // **Space form, deliberately.** pi 1.2.0's argument parser is
            // hand-rolled and takes the next token for `--tools` even when that
            // token starts with `-` (which is what a `-read,-bash` denylist looks
            // like). It does *not* accept `--tools=...`: that lands in its
            // unknown-flag path, so the switch that keeps pi's own read/bash/
            // edit/write off would be silently ignored. A guardrail that turns
            // itself off without saying so is the failure mode to avoid here.
            a.push("--tools".into());
            a.push(self.tools.join(","));
        }
        if let Some(sp) = &self.system_prompt {
            // Space form again: pi's hand-rolled parser does not take `--flag=value`.
            a.push("--system-prompt".into());
            a.push(sp.clone());
        }
        a
    }
}

/// A live pi process plus the two ends of its RPC pipe.
/// The bridge's own pid ledger, one pid per line. `kill_on_drop(true)` only fires on a
/// normal Rust drop: SIGKILL the app or let it panic and the pi it started keeps running,
/// holding whatever it had loaded — two orphans from killed runs were still sitting at
/// 205 MB with no idle clock anywhere. So the agent dir keeps a ledger of the processes
/// this bridge started, and the next bridge to start adopts them and ends them.
pub fn children_file(agent_dir: &Path) -> PathBuf {
    agent_dir.join("pi-children.txt")
}

pub fn note_children(agent_dir: &Path, pids: &[u32]) {
    let _ = std::fs::write(
        children_file(agent_dir),
        pids.iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// The parent of a process, from `/proc/<pid>/stat`. The comm field is parenthesised and
/// may hold spaces and parentheses, so the fields are counted after the *last* `)`.
fn ppid_of(pid: u32) -> Option<u32> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    raw[close + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// The three things that must hold before a signal is allowed, as a pure function so it can
/// be tested against process lines that are not on this machine:
///
///   * `ppid == 1` — an orphan in the kernel's sense. A live parent means somebody else
///     owns that process and its idle clock, including a second gateway sharing the same
///     agent dir. This is also what keeps the check from ever reaching a pi a user is
///     talking through: an interactive pi has a parent, and it does not run `--mode rpc`.
///     (An earlier version of this check matched on the process line alone, and a name-based
///     sweep of `pi` processes is how a session somebody is typing into gets ended.)
///   * the process line says `--mode rpc` — only a driven pi is a bridge's child.
///   * the executable is the launcher's own — pid reuse is real, and a recycled pid must not
///     inherit an old claim.
fn is_orphan_rpc_pi(pid: u32, ppid: Option<u32>, parts: &[String], bin: &Path) -> bool {
    if pid == 0 || pid == std::process::id() || ppid != Some(1) {
        return false;
    }
    let Some(head) = parts.first() else {
        return false;
    };
    let same_launcher = Path::new(head)
        .file_name()
        .and_then(|s| s.to_str())
        == Path::new(bin).file_name().and_then(|s| s.to_str());
    same_launcher && parts.iter().any(|p| p == "rpc") && parts.iter().any(|p| p == "--mode")
}

fn is_our_pi(pid: u32, bin: &Path) -> bool {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    is_orphan_rpc_pi(pid, ppid_of(pid), &parts, bin)
}

/// Kill pi processes a previous bridge started and never finished reaping.
pub fn adopt_orphans(agent_dir: &Path, bin: &Path, mut log: impl FnMut(String)) -> usize {
    let path = children_file(agent_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return 0;
    };
    let mut killed = 0;
    for line in raw.lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == std::process::id() {
            continue;
        }
        if is_our_pi(pid, bin) {
            // The owner is gone, and an idle clock needs an owner: end it rather than let
            // it serve a client that no longer exists.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            killed += 1;
        }
    }
    if killed > 0 {
        log(format!(
            "[cwoffee/pi] adopted and ended {killed} pi process(es) left over from an earlier bridge —              nothing was reaping them"
        ));
    }
    let _ = std::fs::remove_file(&path);
    killed
}

pub struct PiChild {
    to_stdin: mpsc::UnboundedSender<Vec<u8>>,
    /// Told to the writer task when we want stdin closed, which is pi's own
    /// "please shut down" signal (docs/rpc.md).
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    child: Arc<Mutex<Child>>,
    pub pid: u32,
    alive: Arc<AtomicBool>,
    /// The last lines pi wrote to stderr, newest last. A crashed pi is otherwise
    /// a silent exit code, and "pi exited" is not a message to give a user.
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl PiChild {
    /// Start pi, and refuse to run it if its version is not the pinned one.
    pub async fn spawn(launch: &PiLaunch) -> Result<(PiChild, mpsc::UnboundedReceiver<Value>), PiError> {
        let version = Self::check_version(launch).await?;
        let bin = launch.bin.display().to_string();
        let mut cmd = Command::new(&launch.bin);
        cmd.args(launch.argv())
            .current_dir(&launch.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // No shell: the arguments are paths the user typed in a settings field.
            .env("PI_CODING_AGENT_DIR", &launch.agent_dir)
            // A desktop app checks for updates on its own schedule; pi phoning
            // home from a sandboxed product is a privacy answer we should not
            // have to give twice.
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("NO_COLOR", "1")
            .kill_on_drop(true);
        for (k, v) in &launch.extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|source| PiError::Spawn { bin: bin.clone(), source })?;
        let pid = child.id().unwrap_or(0);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PiError::Spawn { bin: bin.clone(), source: std::io::Error::other("no stdin pipe") })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PiError::Spawn { bin, source: std::io::Error::other("no stdout pipe") })?;
        let stderr = child.stderr.take();

        let (to_stdin, mut from_us) = mpsc::unbounded_channel::<Vec<u8>>();
        let (sd_tx, sd_rx) = oneshot::channel::<()>();
        let shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(Some(sd_tx)));
        let version_for_log = version.clone();
        // The writer owns stdin so that closing it is a thing we can do on
        // purpose: pi treats EOF on stdin as "shut down" (docs/rpc.md), which is
        // a far cleaner goodnight than a signal.
        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut sd = sd_rx;
            loop {
                tokio::select! {
                    _ = &mut sd => break,
                    b = from_us.recv() => match b {
                        Some(bytes) => {
                            if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            let _ = stdin.shutdown().await;
        });

        if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
            eprintln!("[bridge] pi {version_for_log} started, pid {pid}");
        }
        let (records_tx, records_rx) = mpsc::unbounded_channel::<Value>();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let pending = Arc::clone(&pending);
            let alive = Arc::clone(&alive);
            let records_tx = records_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("[bridge] reading pi's stdout failed: {e}");
                            break;
                        }
                    }
                    // LF is the only terminator. CR is trimmed only because a
                    // stray CRLF is survivable; nothing else here is text.
                    while buf.last() == Some(&b'\n') {
                        buf.pop();
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    if buf.iter().all(|b| b.is_ascii_whitespace()) {
                        continue;
                    }
                    let line = match std::str::from_utf8(&buf) {
                        Ok(s) => s,
                        Err(_) => {
                            eprintln!("[bridge] pi sent {} non-UTF8 bytes on stdout; dropped", buf.len());
                            continue;
                        }
                    };
                    let v: Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(e) => {
                            // A torn record is the signature of a wrong framer,
                            // so this is worth printing even when nothing else is.
                            eprintln!("[bridge] unparseable record from pi ({e}): {}", &line[..line.len().min(120)]);
                            continue;
                        }
                    };
                    if v.get("type").and_then(|t| t.as_str()) == Some("response") {
                        if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                            if let Some(tx) = pending.lock().await.remove(id) {
                                let _ = tx.send(v);
                                continue;
                            }
                        }
                    }
                    if records_tx.send(v).is_err() {
                        break;
                    }
                }
                alive.store(false, Ordering::SeqCst);
            });
        }

        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let s = line.trim_end().to_string();
                    if s.is_empty() {
                        continue;
                    }
                    // Kept for the error path; only the noisy ones reach the log,
                    // because pi is chatty on stderr when an extension prints.
                    if std::env::var("CWOFFEE_PI_TRACE").is_ok() {
                        eprintln!("[pi stderr] {s}");
                    }
                    let mut t = tail.lock().await;
                    t.push_back(s);
                    while t.len() > 40 {
                        t.pop_front();
                    }
                }
            });
        }

        Ok((
            PiChild {
                to_stdin,
                shutdown,
                pending,
                next_id: AtomicU64::new(0),
                child: Arc::new(Mutex::new(child)),
                pid,
                alive,
                stderr_tail,
            },
            records_rx,
        ))
    }

    /// The version handshake: `pi --version` must say exactly what we shipped
    /// against. `None` disables the check for a developer pointing at a checkout.
    async fn check_version(launch: &PiLaunch) -> Result<String, PiError> {
        let want = launch.pinned_version.clone();
        let bin = launch.bin.display().to_string();
        let out = Command::new(&launch.bin)
            .arg("--version")
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .output()
            .await
            .map_err(|source| PiError::Spawn { bin: bin.clone(), source })?;
        let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some(w) = &want {
            if &found != w {
                return Err(PiError::Version { bin, found, want: w.clone() });
            }
        }
        Ok(found)
    }

    fn write_raw(&self, v: &Value) -> Result<(), PiError> {
        let mut bytes = serde_json::to_vec(v).unwrap_or_default();
        bytes.push(b'\n');
        self.to_stdin
            .send(bytes)
            .map_err(|_| PiError::Closed { command: "write".into() })
    }

    /// Send one command and await its response. The response record is returned
    /// whole; `data` is what most callers want.
    pub async fn request(&self, cmd: Value, timeout: Duration) -> Result<Value, PiError> {
        let command = cmd
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("command")
            .to_string();
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("c{n}");
        let mut msg = cmd;
        msg["id"] = Value::String(id.clone());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.write_raw(&msg)?;
        match tokio::time::timeout(timeout, rx).await {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(PiError::Timeout { command, secs: timeout.as_secs() })
            }
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(PiError::Closed { command })
            }
            Ok(Ok(v)) => {
                if v.get("success").and_then(|b| b.as_bool()) == Some(false) {
                    return Err(PiError::Rpc {
                        command,
                        error: v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("no reason given")
                            .to_string(),
                    });
                }
                Ok(v.get("data").cloned().unwrap_or(Value::Null))
            }
        }
    }

    /// A record pi does not answer: `extension_ui_response` mainly. Errors are
    /// ignored on purpose — a reply to a request pi already timed out is noise.
    pub fn notify(&self, v: Value) {
        let _ = self.write_raw(&v);
    }

    pub fn is_up(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.to_stdin.is_closed()
    }

    /// Close stdin (pi's own shutdown path), wait `grace`, then kill.
    pub async fn reap(&self, grace: Duration) {
        if let Some(tx) = self.shutdown.lock().await.take() {
            let _ = tx.send(());
        }
        let killed = {
            let mut child = self.child.lock().await;
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                true
            } else {
                false
            }
        };
        self.alive.store(false, Ordering::SeqCst);
        if killed && std::env::var("CWOFFEE_PI_TRACE").is_ok() {
            eprintln!("[bridge] pi pid {} needed SIGKILL to leave", self.pid);
        }
    }

    /// Quit-while-busy (locked decision 15): stop everything, bounded, then go.
    pub async fn kill_now(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        self.alive.store(false, Ordering::SeqCst);
    }

    pub async fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().await.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// `pi --version` on this machine, for the probe and for a version report.
pub async fn pi_version(bin: impl AsRef<Path>) -> Result<String, PiError> {
    let bin = bin.as_ref();
    let out = Command::new(bin).arg("--version").output().await.map_err(|source| PiError::Spawn {
        bin: bin.display().to_string(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
