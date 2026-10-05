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
            }
            Err(e) => {
                st.task = None;
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
            note: if running && lan {
                "plain http: anyone who can read the link can drive this agent"
                    .into()
            } else if running {
                "loopback only".into()
            } else {
                "off".into()
            },
        }
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
    fn lan_addresses_skips_loopback() {
        // Whatever this box has, loopback must never be offered as a LAN URL:
        // a phone scanning a 127.0.0.1 QR is a support ticket.
        for ip in lan_addresses() {
            assert!(!ip.is_loopback(), "{ip}");
        }
    }
}
