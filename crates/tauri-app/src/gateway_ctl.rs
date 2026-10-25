//! Start / stop / report the LAN gateway from inside the running app.
//!
//! Until now the gateway was boot-only: `CWOFFEE_GATEWAY_PORT` set or not, and
//! that was the whole story. To show an honest "gateway: on/off" in the footer
//! and let the user toggle it, something has to own the listener task and be
//! able to name the address it actually bound. That is what this is.
//!
//! Two things are deliberately NOT done here:
//!   * The token is never invented silently for a non-loopback bind. The gateway
//!     itself refuses that bind, and so do we, because an unauthenticated agent
//!     with a sandboxed shell on a LAN address is not a thing to ship by
//!     accident because someone clicked a toggle.
//!   * The token value is not in the status struct. It goes out as a URL the
//!     user can act on, because a raw field invites the next caller to log it.

use cwoffee_gateway::{EngineHandle, GatewayConfig};
use serde::Serialize;
use std::net::IpAddr;
use std::process::Stdio;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct GatewayCtl {
    /// Shared with the gateway task: the same handle the listener uses, so the
    /// client gauge we read is the one it writes.
    handle: Arc<EngineHandle>,
    state: Mutex<State>,
}

struct State {
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    host: IpAddr,
    port: u16,
    drive: Option<String>,
    watch: Option<String>,
    /// Where the drive token came from, for the status readout.
    token_source: String,
    /// Set between "clicked" and "bound", so a double click cannot spawn two
    /// accept loops on the same port.
    starting: bool,
    /// Why the last start attempt failed. The footer's "off" is true but useless
    /// when the reason is that another Cwoffee already holds the port - which is
    /// exactly when a phone can reach a gateway this window says nothing about.
    last_error: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct GatewayStatus {
    pub running: bool,
    pub host: String,
    pub port: u16,
    /// Devices attached, not connections: two tabs on one phone is one device.
    pub clients: usize,
    pub connections: usize,
    /// Addresses behind that count, so the number can be checked by eye.
    pub peers: Vec<String>,
    /// True when the bind is anything but loopback.
    pub lan: bool,
    pub has_token: bool,
    pub watch_only: Option<bool>,
    pub token_source: String,
    /// Ready-to-open URLs, token included. Local-window only: this command is
    /// not reachable over the gateway itself.
    pub urls: Vec<String>,
    pub tls: bool,
    pub note: String,
}

impl GatewayCtl {
    pub fn new(handle: Arc<EngineHandle>) -> Self {
        Self {
            handle,
            state: Mutex::new(State {
                task: None,
                host: IpAddr::from([127, 0, 0, 1]),
                port: 8099,
                drive: None,
                watch: None,
                token_source: "none".into(),
                starting: false,
                last_error: None,
            }),
        }
    }

    /// Bind and serve. Returns an error naming the reason when the gateway
    /// refuses (bad address, no built UI, no token on a non-loopback bind).
    ///
    /// The state lock is never held across an await. That is not stylistic: the
    /// command that calls this runs on Tauri's multi-thread runtime, and a
    /// future carrying a `std::sync::MutexGuard` past a suspension point is not
    /// `Send`, which fails to compile.
    pub async fn start(
        &self,
        host: IpAddr,
        port: u16,
        drive: Option<String>,
        watch: Option<String>,
        token_source: impl Into<String>,
    ) -> Result<GatewayStatus, String> {
        {
            let mut st = self.state.lock().unwrap();
            if st.task.is_some() {
                return Err(format!(
                    "the gateway is already running on {}:{}",
                    st.host, st.port
                ));
            }
            if st.starting {
                return Err("the gateway is already starting".into());
            }
            st.starting = true;
        }

        let bound = self.bind(host, port, drive.clone(), watch.clone()).await;

        let mut st = self.state.lock().unwrap();
        st.starting = false;
        match bound {
            Ok(task) => {
                st.task = Some(task);
                st.host = host;
                st.port = port;
                st.drive = drive;
                st.watch = watch;
                st.token_source = token_source.into();
                st.last_error = None;
            }
            Err(e) => {
                st.task = None;
                st.last_error = Some(e.clone());
                drop(st);
                return Err(e);
            }
        }
        drop(st);
        eprintln!("[cwoffee] gateway on http://{host}:{port}");
        Ok(self.status())
    }

    /// Spawn the accept loop and tell the difference between "listening" and
    /// "refused", which is what the footer needs to show instead of quietly
    /// flipping the toggle back off.
    async fn bind(
        &self,
        host: IpAddr,
        port: u16,
        drive: Option<String>,
        watch: Option<String>,
    ) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
        let cfg = GatewayConfig {
            host,
            port,
            dist: crate::ui_dist_dir(),
            drive_token: drive,
            watch_token: watch,
        };
        // The gateway refuses this bind as well. Duplicated on purpose: the
        // toggle wants the reason in its own terms, before anything is spawned.
        if !cfg.is_loopback() && cfg.drive_token.is_none() {
            return Err(
                "refusing to expose this agent on a LAN address without a token; set CWOFFEE_GATEWAY_TOKEN or create ~/.config/cwoffee/gateway.token".into(),
            );
        }
        let handle = Arc::clone(&self.handle);
        let (tx, mut notify) = tokio::sync::oneshot::channel();
        let task = tauri::async_runtime::spawn(async move {
            let r = cwoffee_gateway::serve(cfg, handle).await;
            if let Err(e) = &r {
                eprintln!("[cwoffee] gateway not running: {e}");
            }
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });

        // `serve` returns at once when the bind fails, so a short wait is what
        // separates "still running" from "refused, and here is why".
        match tokio::time::timeout(std::time::Duration::from_millis(600), &mut notify).await {
            Ok(Ok(Err(e))) => Err(e),
            Ok(Ok(Ok(()))) => Err("the gateway stopped right after starting".into()),
            Ok(Err(_)) => Err("the gateway task vanished".into()),
            Err(_) => Ok(task),
        }
    }

    /// What closing this window would do to the gateway, worked out before anyone presses the
    /// close button. The settings panel shows this beside the toggle, so a hand-off that cannot
    /// happen is visible while there is still a window to explain it in.
    pub fn plan(&self) -> HandoffPlan {
        let (host, port, running) = {
            let st = self.state.lock().unwrap();
            (st.host.to_string(), st.port, st.task.is_some())
        };
        let launch = supervisor_prefix(port, &handoff_log_path());
        HandoffPlan {
            binary: find_gateway_binary().map(|p| p.display().to_string()),
            running,
            host,
            port,
            via: launch.via.to_string(),
            stop: launch.stop,
            log: handoff_log_path().display().to_string(),
        }
    }

    /// Hand the port to a standalone gateway, then let the window go.
    ///
    /// The gateway in this window is a task *inside this process*: it shares the engine, the
    /// session store and the approval table, and it cannot outlive the process it is in. So the
    /// only honest form of "keep serving devices" is a different process that owns its own engine
    /// and reads the same config, store and token. The UI says so rather than implying the same
    /// gateway carried over — a phone that reconnects to a fresh host mid-conversation should not
    /// learn that from the conversation.
    ///
    /// Order is the whole feature: release the listener, then start the child, then exit. In the
    /// other order the child refuses the port this process still holds.
    pub fn handoff(&self, ctx: &HandoffCtx) -> Result<HandoffReport, String> {
        let (host, port, running) = {
            let st = self.state.lock().unwrap();
            (st.host, st.port, st.task.is_some())
        };
        if !running {
            return Err(
                "remote access is off in this window, so there is nothing to keep. Turn it on \
                 first."
                    .into(),
            );
        }
        let Some(binary) = find_gateway_binary() else {
            return Err(format!(
                "no cwoffee-gateway found. The gateway in this window is a task inside the app, so \
                 keeping it means starting the standalone one: build it \
                 (cargo build --release -p cwoffee-gateway) or install it \
                 (scripts/install-desktop.sh). Searched: {}",
                binary_candidates()
                    .iter()
                    .map(|p| p.display().to_string())
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let args = handoff_args(ctx, &host.to_string(), port);
        let log = handoff_log_path();
        let launch = supervisor_prefix(port, &log);
        // An earlier hand-off's unit still owns the port and the name. systemd-run has no
        // --replace on this box, so the unit is stopped by name first; when there is none, the
        // stop command fails quietly and the spawn below is the real test of whether it worked.
        if let Some(pre) = launch.pre_stop.as_ref() {
            let _ = std::process::Command::new(&pre[0]).args(&pre[1..]).status();
        }
        if let Some(dir) = log.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| format!("cannot open {}: {e}", log.display()))?;

        let mut cmd = if launch.argv.is_empty() {
            let mut c = std::process::Command::new(&binary);
            c.args(&args);
            c
        } else {
            // The wrapper goes first, and `--` keeps the gateway's own argv on the gateway, so a
            // path with a space in it stays one argument.
            let mut c = std::process::Command::new(&launch.argv[0]);
            c.args(launch.argv[1..].iter());
            c.arg("--");
            c.arg(&binary);
            c.args(&args);
            c
        };
        cmd.stdin(Stdio::null());
        // Not null: a process with no terminal still has a reason to say why it failed, and a
        // file is the only place left for it once this window is gone.
        match file.try_clone() {
            Ok(err) => {
                cmd.stdout(Stdio::from(file));
                cmd.stderr(Stdio::from(err));
            }
            Err(_) => {
                cmd.stdout(Stdio::from(file));
                cmd.stderr(Stdio::null());
            }
        }
        // The listener goes first. The token is deliberately not in argv: the child inherits this
        // environment and reads the same token file, which is the only reason `ps` cannot read a
        // credential that drives a sandboxed shell.
        if let Err(e) = self.stop() {
            return Err(format!("the gateway would not release the port: {e}"));
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", binary.display()))?;
        let pid = child.id();

        // A spawned process is not the promise, so check for a listener. The budget is short
        // because this window is on its way out, and it is printed instead of assumed: on the
        // first run of this code the child "started" fine and turned out to be systemd-run
        // complaining about an option it does not have.
        let bound = wait_for_bind(port, BIND_WAIT);
        let host_s = host.to_string();
        let probe_host = if host.is_loopback() { "127.0.0.1".to_string() } else { host_s.clone() };
        // Did it die on the way? A wrapper that rejects an option exits in milliseconds and leaves
        // a listener-shaped hole, so saying "it exited with status N" is the difference between a
        // ten-second diagnosis and an afternoon.
        let died = child.try_wait().ok().flatten();
        let secs = BIND_WAIT.as_millis() as f64 / 1000.0;
        let note = if bound {
            format!(
                "serving http://{probe_host}:{port} again as {}/{}; its own lines are in {}",
                launch.via, pid,
                log.display()
            )
        } else {
            match died {
                Some(status) => format!(
                    "did NOT take port {port}: pid {pid} (started via {}) exited with {} inside \
                     {secs:.1}s. {} says why, and `{}` is how you would have stopped it.",
                    launch.via,
                    status,
                    log.display(),
                    launch.stop
                ),
                None => format!(
                    "did NOT take port {port} within {secs:.1}s. It is still running as pid {pid} \
                     via {}; {} says why, and `{}` stops it if it does come up.",
                    launch.via,
                    log.display(),
                    launch.stop
                ),
            }
        };
        Ok(HandoffReport {
            binary: binary.display().to_string(),
            host: host_s,
            port,
            via: launch.via.to_string(),
            pid,
            stop: launch.stop,
            bound,
            note,
        })
    }

    pub fn stop(&self) -> Result<GatewayStatus, String> {
        let mut st = self.state.lock().unwrap();
        match st.task.take() {
            Some(t) => {
                t.abort();
                eprintln!("[cwoffee] gateway stopped ({}:{} was live)", st.host, st.port);
                Ok(self.status_inner(&st))
            }
            None => Err("the gateway was not running".into()),
        }
    }

    pub fn status(&self) -> GatewayStatus {
        let st = self.state.lock().unwrap();
        self.status_inner(&st)
    }

    fn status_inner(&self, st: &State) -> GatewayStatus {
        let running = st.task.is_some();
        let lan = !st.host.is_loopback();
        let token = st.drive.clone().unwrap_or_default();
        let mut urls = Vec::new();
        if running {
            let q = if token.is_empty() {
                String::new()
            } else {
                format!("?token={token}")
            };
            urls.push(format!("http://127.0.0.1:{}/{}", st.port, q));
            for ip in lan_addresses() {
                urls.push(format!("http://{ip}:{}/{}", st.port, q));
            }
        }
        GatewayStatus {
            running,
            host: st.host.to_string(),
            port: st.port,
            clients: self.handle.devices(),
            connections: self.handle.connections(),
            peers: self.handle.peers(),
            lan,
            has_token: st.drive.is_some(),
            watch_only: st.watch.as_ref().map(|_| st.drive.is_none()),
            token_source: st.token_source.clone(),
            urls,
            tls: false,
            note: note_for(running, lan, st.last_error.as_deref()),
        }
    }
}

// ---- hand-off: keeping the gateway when the window goes -------------------------------

/// What the standalone gateway needs that this window's own flags carry.
///
/// Tokens are deliberately absent. `--token` on a command line is readable by any process on
/// the machine through `ps`, so the hand-off passes none: the child inherits this environment
/// and reads the same token file (`gateway_ctl::drive_token` wrote it). Same for the pi paths —
/// `CWOFFEE_PI`, `CWOFFEE_PI_EXT`, `CWOFFEE_CWTOOL` are inherited, and the gateway now falls
/// back to them, which is why this list is short.
pub struct HandoffCtx {
    pub core: String,
    pub url: String,
    pub model: String,
    pub workspace: String,
    pub mode: String,
    pub persona: String,
    pub dist: Option<PathBuf>,
    pub config_path: PathBuf,
}

/// What the next close would do, for the settings panel to show *before* anyone closes a window.
#[derive(Serialize, Clone)]
pub struct HandoffPlan {
    /// The gateway binary that would be started, or None when there is none — and then the
    /// setting cannot be honoured, which the panel says out loud.
    pub binary: Option<String>,
    /// Whether there is a gateway in this window to hand over at all.
    pub running: bool,
    pub host: String,
    pub port: u16,
    /// How the child would be started: `systemd-run` (its own unit, so it is not inside this
    /// app's scope) or `detached` for a plain child.
    pub via: String,
    /// How to stop it afterwards, in the same terms it was started in.
    pub stop: String,
    /// Where the started process's own lines go. A process with no terminal still has a reason
    /// to say why it failed, and after this window closes a file is the only place left.
    pub log: String,
}

/// What was actually started, and whether it took the port.
#[derive(Serialize, Clone)]
pub struct HandoffReport {
    pub binary: String,
    pub host: String,
    pub port: u16,
    pub via: String,
    pub pid: u32,
    pub stop: String,
    /// True when something answered on the port within the short wait this window can afford.
    pub bound: bool,
    pub note: String,
}

/// The command line for the standalone gateway.
///
/// Split out from the spawn because this list *is* the claim worth testing: a hand-off that
/// forgets `--core pi` quietly replaces a tool-using agent with a bare engine, and nothing in
/// the window would say so. The endpoint, model, folder, mode and persona are passed because
/// they came from this window's own flags or config, and a gateway started without them reads
/// whichever config file happens to be there instead.
pub fn handoff_args(ctx: &HandoffCtx, host: &str, port: u16) -> Vec<String> {
    let mut a = vec![
        "--host".to_string(),
        host.to_string(),
        "--port".to_string(),
        port.to_string(),
        "--core".to_string(),
        ctx.core.clone(),
        "--url".to_string(),
        ctx.url.clone(),
        "--model".to_string(),
        ctx.model.clone(),
        "--workspace".to_string(),
        ctx.workspace.clone(),
        "--mode".to_string(),
        ctx.mode.clone(),
        "--persona".to_string(),
        ctx.persona.clone(),
        "--config".to_string(),
        ctx.config_path.display().to_string(),
    ];
    if let Some(d) = &ctx.dist {
        a.push("--dist".to_string());
        a.push(d.display().to_string());
    }
    a
}

/// Where a standalone gateway binary can be, most-local first.
///
/// Order is not cosmetic: the copy next to this executable is the copy that was installed with
/// it, so it is the same version. Finding a `/usr/bin/cwoffee-gateway` from an old package while
/// this window is a fresh build would hand the phone a different program than the one the user
/// was talking to.
pub fn binary_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // target/release/ and the installed .../cwoffee-desktop/bin/ both keep them together.
            out.push(dir.join("cwoffee-gateway"));
            if let Some(parent) = dir.parent() {
                // An installed launcher runs .../cwoffee-desktop/cwoffee; its bin/ is a sibling.
                out.push(parent.join("bin").join("cwoffee-gateway"));
                out.push(parent.join("cwoffee-gateway"));
            }
        }
    }
    for d in ["/usr/local/bin", "/usr/bin"] {
        out.push(PathBuf::from(d).join("cwoffee-gateway"));
    }
    if let Some(p) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&p) {
            out.push(d.join("cwoffee-gateway"));
        }
    }
    out
}

pub fn find_gateway_binary() -> Option<PathBuf> {
    binary_candidates().into_iter().find(|p| is_executable(p))
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file() && std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(name))
            .find(|f| f.is_file())
    })
}

/// How to start the child so that it outlives this app.
///
/// A plain fork+exec child of a GUI app on systemd stays inside the app's cgroup scope. When the
/// app exits, that scope stops and systemd kills what is left in it — the child would be gone in
/// under a second and the setting would read as broken. `systemd-run --user` asks the user
/// manager for its own transient unit, which is not under the app's scope.
///
/// Two things that page is worth knowing, both learned the hard way on this box (systemd 259):
///
/// * **`--replace` is not an option.** It is not in `systemd-run --help`, and passing it makes
///   systemd-run exit with `unrecognized option` — which is a hand-off that "started" a process
///   that never was. A gateway left over by a previous window is therefore stopped by name first,
///   and `systemctl --user stop` on a unit that does not exist is an error we ignore.
/// * **The unit does not inherit this process's environment.** It gets the user manager's. So the
///   variables the child needs are named one by one with `--setenv=`, or a pi-core hand-off comes
///   up with no extension, no tool host and the wrong store — the quiet failure where the phone
///   reconnects and the model has no tools.
///
/// Without a session bus (a plain shell, a container, CI) a detached child is exactly right, so
/// the fallback is a bare spawn — with `setsid` when it exists, so it does not die with the
/// terminal either. There is no scope to kill it in that case, which is why the fallback is not a
/// compromise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// argv that goes *before* the gateway binary. Empty means spawn it directly.
    pub argv: Vec<String>,
    /// How to stop what this starts, in words the user can paste.
    pub stop: String,
    /// Run before spawning: an earlier hand-off's unit would still be holding the port.
    pub pre_stop: Option<Vec<String>>,
    /// Named for the settings panel's readout, so "via systemd-run" is a fact and not a guess.
    pub via: &'static str,
}

/// The environment a hand-off child has to be handed explicitly. Only used for the systemd-run
/// route, because a bare child inherits this process's environment and needs none of it.
///
/// The XDG trio is here because a hand-off is supposed to serve *the same* session: the config,
/// the session store and the token file all live under those paths, and a user who moved one of
/// them would otherwise get a gateway reading a different — possibly empty — store.
pub const HANDOFF_ENV: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "HOME",
    "PATH",
    "LANG",
    "LC_ALL",
    "CWOFFEE_PI",
    "CWOFFEE_PI_EXT",
    "CWOFFEE_PI_TOOLS",
    "CWOFFEE_PI_AGENT_DIR",
    "CWOFFEE_PI_SESSION_DIR",
    "CWOFFEE_CWTOOL",
    "CWOFFEE_MODE",
    "CWOFFEE_WORKSPACE",
];

/// Pick the `K=V` pairs to forward. Split from reading the environment so the rule is testable.
///
/// A name carrying a credential is never forwarded, even though `--setenv=` is not itself the
/// problem: the *option* sits in systemd-run's argv, and any local process can read another
/// user's... cannot, but can read a same-user process's argv, and `ps` is how people debug. The
/// token already travels as a file the child reads by path; nothing is gained by also putting it
/// on a command line.
fn forward_env<I, T, U>(src: I) -> Vec<String>
where
    I: IntoIterator<Item = (T, U)>,
    T: AsRef<std::ffi::OsStr>,
    U: AsRef<std::ffi::OsStr>,
{
    let mut out = Vec::new();
    for (k, v) in src {
        let name = k.as_ref().to_string_lossy().to_string();
        if !HANDOFF_ENV.iter().any(|want| *want == name) {
            continue;
        }
        if name.contains("TOKEN") || name.contains("KEY") || name.contains("SECRET") {
            continue;
        }
        let val = v.as_ref().to_string_lossy().to_string();
        if val.is_empty() {
            continue;
        }
        out.push(format!("{name}={val}"));
    }
    out.sort();
    out
}

/// The same, read from this process's environment.
fn forwardable_env() -> Vec<String> {
    forward_env(std::env::vars_os())
}

pub fn supervisor_prefix(port: u16, log: &std::path::Path) -> Launch {
    let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
    if bus && executable_on_path("systemd-run").is_some() {
        let unit = format!("cwoffee-gateway-{port}");
        let mut argv = vec![
            "systemd-run".to_string(),
            "--user".to_string(),
            "--quiet".to_string(),
            "--collect".to_string(),
            format!("--unit={unit}"),
        ];
        // The file descriptors this process opens are inherited by systemd-run, not by the unit it
        // asks the manager to start: without these two properties the gateway's own lines — a
        // refused bind, a missing token, "no extension mounted" — go to the journal and the
        // hand-off log stays empty. `StandardOutput=append:` is what makes the note's
        // "its lines are in ..." true; systemd-run has no `--output=` option (checked on 259,
        // which rejects it the way it rejected `--replace`).
        let sink = format!("append:{}", log.display());
        argv.push(format!("--property=StandardOutput={sink}"));
        argv.push(format!("--property=StandardError={sink}"));
        for kv in forwardable_env() {
            argv.push(format!("--setenv={kv}"));
        }
        return Launch {
            argv,
            stop: format!("systemctl --user stop {unit}"),
            pre_stop: Some(vec!["systemctl".into(), "--user".into(), "stop".into(), unit]),
            via: "systemd-run",
        };
    }
    if executable_on_path("setsid").is_some() {
        return Launch {
            argv: vec!["setsid".to_string()],
            stop: format!("pkill -f 'cwoffee-gateway.*--port {port}'"),
            pre_stop: None,
            via: "setsid",
        };
    }
    Launch {
        argv: Vec::new(),
        stop: format!("kill $(pgrep -f 'cwoffee-gateway.*--port {port}')"),
        pre_stop: None,
        via: "detached",
    }
}

/// How long a hand-off waits for a listener before it says so out loud. Long enough for a release
/// gateway to open its store and bind, short enough that a window that is closing does not appear
/// to have frozen.
const BIND_WAIT: std::time::Duration = std::time::Duration::from_millis(4000);

/// Where a handed-off gateway's own lines go. A process with no terminal still has things to
/// say — a refusal to bind, a missing token — and once the window is gone a file is the only
/// place left for them.
fn handoff_log_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/state")
        });
    base.join("cwoffee").join("gateway-handoff.log")
}

/// Did something take the port? A spawned process is not the promise; a listener is.
///
/// Probes loopback regardless of the bind address, because a gateway bound to 0.0.0.0 answers
/// there too and a LAN address may be unreachable from this process for unrelated reasons.
fn wait_for_bind(port: u16, budget: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + budget;
    while std::time::Instant::now() < until {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    false
}

/// One line that tells the user what the gateway state *means*, including why a
/// start failed. Split out so the wording is testable without a live server.
fn note_for(running: bool, lan: bool, last_error: Option<&str>) -> String {
    if running && lan {
        "plain http: anyone who can read the link can drive this agent".into()
    } else if running {
        "loopback only".into()
    } else if let Some(e) = last_error {
        format!("off - {e}")
    } else {
        "off".into()
    }
}

/// Guess the machine's non-loopback IPv4 addresses.
///
/// Reads the interface list rather than the "connect a UDP socket somewhere"
/// trick, because that trick reports only the default-route interface and this
/// box has Tailscale, LAN and loopback all live at once - and the Tailscale
/// address is usually the one that matters on a phone.
fn lan_addresses() -> Vec<IpAddr> {
    let Ok(out) = std::process::Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _idx = it.next()?;
            let _name = it.next()?;
            let _inet = it.next()?;
            let cidr = it.next()?;
            cidr.split('/').next().and_then(|s| s.parse().ok())
        })
        .filter(|ip: &IpAddr| !ip.is_loopback())
        .take(6)
        .collect()
}

/// Read the drive token the app should use, or create one.
///
/// Creating it is the point: a user who clicks "gateway on" on a laptop they
/// intend to reach from a phone should not have to know that a 32-byte file has
/// to exist first. The file is written 0600 and the path is reported in the
/// status so it is never a mystery where the secret lives.
pub fn drive_token() -> (Option<String>, String) {
    if let Some(t) = std::env::var("CWOFFEE_GATEWAY_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return (Some(t), "CWOFFEE_GATEWAY_TOKEN".into());
    }
    let path = token_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return (Some(s), path.display().to_string());
        }
    }
    match path.parent() {
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            let t = random_token();
            if std::fs::write(&path, &t).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                eprintln!("[cwoffee] wrote a gateway token to {}", path.display());
                return (Some(t), path.display().to_string());
            }
            (None, "none".into())
        }
        None => (None, "none".into()),
    }
}

fn token_path() -> PathBuf {
    // Reuse the engine's own config-dir resolution instead of reimplementing
    // XDG here: two definitions of "where the config lives" is how a token ends
    // up written somewhere the app never looks.
    cwoffee_engine::config::Config::default_path()
        .parent()
        .map(|p| p.join("gateway.token"))
        .unwrap_or_else(|| PathBuf::from("cwoffee-gateway.token"))
}

fn random_token() -> String {
    // No new dependency for one token: /dev/urandom is the same source the
    // gateway's own docs tell the user to pipe through tr. Read exactly N bytes
    // - `fs::read` on urandom never ends.
    use std::io::Read;
    let mut buf = [0u8; 24];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    buf.iter()
        .map(|b| ALPHA[(*b as usize) % ALPHA.len()] as char)
        .collect()
}

/// Shared client gauge, so the ctl and the gateway task see one number.
/// The shared "who is attached" table. The gateway task fills it in as
/// connections authenticate and leave; the desktop reads it for the panel.
pub fn client_gauge() -> Arc<Mutex<std::collections::BTreeMap<String, usize>>> {
    Arc::new(Mutex::new(std::collections::BTreeMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_path_is_under_cwoffee() {
        let p = token_path();
        assert!(p.ends_with("cwoffee/gateway.token"), "{}", p.display());
    }
    #[test]
    fn tokens_are_alphanumeric_and_stable_length() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 24);
        assert_ne!(a, b, "two draws were identical");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn a_failed_start_says_why_and_a_working_one_says_where() {
        // "off" alone sent a user to me with a phone connected to a gateway this
        // window insisted was off: another instance already held the port.
        assert_eq!(note_for(false, false, None), "off");
        assert_eq!(
            note_for(false, false, Some("Address already in use (os error 98)")),
            "off - Address already in use (os error 98)"
        );
        assert_eq!(note_for(true, false, None), "loopback only");
        assert!(note_for(true, true, None).contains("plain http"));
        // A running gateway outranks an older failure.
        assert_eq!(note_for(true, false, Some("earlier")), "loopback only");
    }

    /// The argument list *is* the hand-off. A hand-off that forgets `--core pi` replaces a
    /// tool-using agent with a bare engine and says nothing while doing it; one that forgets the
    /// folder points the tools at whatever the new process happens to start in.
    #[test]
    fn a_hand_off_carries_the_core_endpoint_folder_mode_and_persona() {
        let ctx = HandoffCtx {
            core: "pi".into(),
            url: "http://127.0.0.1:8090/v1".into(),
            model: "qwq-3.8".into(),
            workspace: "/home/me/cwoffee-workspace".into(),
            mode: "auto".into(),
            persona: "chat".into(),
            dist: Some(PathBuf::from("/opt/cwoffee/ui/dist")),
            config_path: PathBuf::from("/home/me/.config/cwoffee/config.toml"),
        };
        let a = handoff_args(&ctx, "0.0.0.0", 8123);
        let joined = a.join(" ");
        for want in [
            "--host 0.0.0.0",
            "--port 8123",
            "--core pi",
            "--url http://127.0.0.1:8090/v1",
            "--model qwq-3.8",
            "--workspace /home/me/cwoffee-workspace",
            "--mode auto",
            "--persona chat",
            "--dist /opt/cwoffee/ui/dist",
            "--config /home/me/.config/cwoffee/config.toml",
        ] {
            assert!(joined.contains(want), "missing `{want}` in: {joined}");
        }
    }

    /// The token is the credential that drives a sandboxed shell. It is never on a command line,
    /// where every process on the box can read it in `ps`; the child inherits the environment and
    /// reads the same token file, which the standalone gateway now does too.
    #[test]
    fn a_hand_off_never_puts_a_token_in_argv() {
        let ctx = HandoffCtx {
            core: "rust".into(),
            url: "http://x/v1".into(),
            model: "m".into(),
            workspace: "/tmp/ws".into(),
            mode: "ask".into(),
            persona: "cowork".into(),
            dist: None,
            config_path: PathBuf::from("/home/me/.config/cwoffee/config.toml"),
        };
        let a = handoff_args(&ctx, "127.0.0.1", 8099);
        assert!(
            !a.iter().any(|s| s.contains("token")),
            "a token flag appeared in the hand-off argv: {a:?}"
        );
        assert!(!a.iter().any(|s| s == "--dist"), "a --dist with no dist to name: {a:?}");
    }

    /// A child of a GUI app lives in the app's cgroup scope, and systemd stops that scope - with
    /// everything left inside it - when the app exits. So on a desktop session the hand-off has to
    /// ask the user manager for its own unit, and the sentence that stops it has to name that
    /// same unit.
    #[test]
    fn the_route_out_of_the_window_scope_names_the_unit_and_the_environment() {
        // A systemd-run unit is the only way this survives a desktop session, so the argv it gets
        // is the product. Two failure modes this test exists to catch, both from the first real
        // run of this code: an option this systemd does not have (`--replace`, gone in 259 — the
        // child "started" and was a usage error), and a unit that inherits the user manager's
        // environment instead of the app's, which loses the pi extension, the tool host and any
        // moved XDG directory.
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/0/bus");
        std::env::set_var("CWOFFEE_PI", "/opt/pi/bin/pi");
        std::env::set_var("CWOFFEE_PI_EXT", "/opt/cwoffee/pi-tools/cwoffee-tools.ts");
        let launch = supervisor_prefix(8099, std::path::Path::new("/tmp/state/cwoffee/gateway-handoff.log"));
        if executable_on_path("systemd-run").is_some() {
            assert_eq!(launch.via, "systemd-run");
            assert_eq!(launch.argv.first().map(String::as_str), Some("systemd-run"));
            assert!(launch.argv.iter().any(|a| a == "--user"), "not the user manager: {:?}", launch.argv);
            assert!(
                !launch.argv.iter().any(|a| a == "--replace"),
                "this systemd rejects --replace; a leftover unit is stopped by name instead: {:?}",
                launch.argv
            );
            let unit = launch
                .argv
                .iter()
                .find(|a| a.starts_with("--unit="))
                .cloned()
                .unwrap_or_default();
            assert!(unit.contains("8099"), "the unit does not name the port: {unit}");
            let name = &unit["--unit=".len()..];
            assert!(launch.stop.contains(name), "{} does not name {name}", launch.stop);
            let pre = launch.pre_stop.clone().unwrap_or_default();
            assert_eq!(
                pre,
                vec!["systemctl".to_string(), "--user".into(), "stop".into(), name.to_string()],
                "nothing releases a previous hand-off's unit, so the port would be refused"
            );
            for want in ["--setenv=CWOFFEE_PI=/opt/pi/bin/pi", "--setenv=CWOFFEE_PI_EXT=/opt/cwoffee/pi-tools/cwoffee-tools.ts"] {
                assert!(launch.argv.iter().any(|a| a == want), "{want} is not in {:?}", launch.argv);
            }
            // Without these the unit's lines go to the journal and the hand-off log we point the
            // user at is an empty file. systemd 259 has no `--output=`, so the property is the way.
            for stream in ["StandardOutput", "StandardError"] {
                let prop = launch
                    .argv
                    .iter()
                    .find(|a| a.starts_with(&format!("--property={stream}=append:")))
                    .cloned()
                    .unwrap_or_default();
                assert!(prop.ends_with("gateway-handoff.log"), "{stream} does not go to the hand-off log: {prop}");
            }
            assert!(
                launch.argv.iter().all(|a| !a.starts_with("--output=")),
                "systemd-run on this box rejects --output=; the property form is the one that works"
            );
        } else {
            // No systemd-run: the fallback still has to be able to name the process it started.
            assert!(launch.stop.contains("8099"), "{}", launch.stop);
            assert!(launch.pre_stop.is_none(), "no unit to stop without systemd");
        }
        std::env::remove_var("CWOFFEE_PI");
        std::env::remove_var("CWOFFEE_PI_EXT");
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    }

    #[test]
    fn the_forwarded_environment_is_the_paths_and_never_the_credential() {
        let got = forward_env([
            ("XDG_CONFIG_HOME", "/scratch/config"),
            ("XDG_DATA_HOME", "/scratch/data"),
            ("PATH", "/usr/bin:/bin"),
            ("CWOFFEE_PI", "/opt/pi/bin/pi"),
            ("CWOFFEE_PI_EXT", "/opt/ext.ts"),
            ("CWOFFEE_CWTOOL", "/opt/cwtool"),
            // Not on the list at all, and a credential either way.
            ("CWOFFEE_GATEWAY_TOKEN", "sekrit"),
            ("CWOFFEE_GATEWAY_WATCH_TOKEN", "sekrit2"),
            ("SOME_API_KEY", "sekrit3"),
            ("CWOFFEE_MODE", ""),
            ("SHELL", "/bin/sh"),
        ]);
        assert!(got.contains(&"XDG_CONFIG_HOME=/scratch/config".to_string()), "{got:?}");
        assert!(got.contains(&"XDG_DATA_HOME=/scratch/data".to_string()), "{got:?}");
        assert!(got.contains(&"CWOFFEE_PI=/opt/pi/bin/pi".to_string()), "{got:?}");
        assert!(got.contains(&"CWOFFEE_CWTOOL=/opt/cwtool".to_string()), "{got:?}");
        assert!(got.contains(&"PATH=/usr/bin:/bin".to_string()), "{got:?}");
        for line in &got {
            assert!(!line.contains("sekrit"), "a credential reached argv: {line}");
            assert!(!line.ends_with('='), "an empty value was forwarded: {line}");
            assert!(!line.starts_with("SHELL="), "an unlisted variable was forwarded: {line}");
            assert!(!line.starts_with("CWOFFEE_MODE="), "an empty value was forwarded: {line}");
        }
        // The systemd route puts these in argv, so the list itself is the boundary. Reading it
        // here rather than trusting the filter alone means adding a token variable to
        // HANDOFF_ENV fails a test instead of leaking a token into `ps`.
        for name in HANDOFF_ENV {
            assert!(
                !name.contains("TOKEN") && !name.contains("KEY") && !name.contains("SECRET"),
                "{name} is on the forward list and looks like a credential"
            );
        }
    }

    #[test]
    fn the_gateway_binary_search_starts_next_to_this_executable() {
        let c = binary_candidates();
        assert!(!c.is_empty(), "nowhere to look for cwoffee-gateway at all");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            c[0],
            exe.parent().unwrap().join("cwoffee-gateway"),
            "the copy next to this binary must be found first: {c:?}"
        );
        // Every candidate is a path to a file named for the program; none of them is a directory
        // someone happened to create by the same name.
        for p in &c {
            assert!(p.file_name().unwrap() == "cwoffee-gateway", "{p:?}");
        }
        // And on this checkout it is actually there, because the workspace builds both.
        assert!(
            find_gateway_binary().is_some(),
            "no cwoffee-gateway next to {:?}: the hand-off would be dead on arrival in tests",
            exe
        );
    }

    /// A spawned process is not the promise; a listener is. So the hand-off checks the port, and
    /// this checks the check.
    #[test]
    fn the_port_probe_answers_for_a_listener_and_not_for_its_absence() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(wait_for_bind(port, std::time::Duration::from_millis(400)), "a live listener read as dead");
        drop(l);
        // The negative half has a premise: an ephemeral port can be handed to an unrelated
        // socket between the drop and the probe, and then a closed port is not what is being
        // measured any more. So check it the same way the probe does, and say which case ran.
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                !wait_for_bind(port, std::time::Duration::from_millis(300)),
                "a closed port read as a gateway"
            );
        } else {
            eprintln!("[test] port {port} went to another socket; only the positive half ran");
        }
    }

    #[test]
    fn lan_addresses_skips_loopback() {
        // Whatever this box has, loopback must never be offered as a LAN URL:
        // a phone scanning a 127.0.0.1 QR is a support ticket.
        for ip in lan_addresses() {
            assert!(!ip.is_loopback(), "{ip}");
        }
    }
}
