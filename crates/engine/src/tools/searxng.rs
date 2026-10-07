//! A SearXNG instance that belongs to the app, not to the person using it.
//!
//! DuckDuckGo's keyless HTML endpoint answers the first query and then starts
//! replying with a bot challenge - which is not an error the model can act on,
//! just a search that quietly stops working. A private SearXNG answers every
//! query, and because it is ours it can have the JSON API on and the bot limiter
//! off.
//!
//! The cost is a container, so `backend = "auto"` keeps the shape this machine's
//! other tooling already uses: start it on the first search, stop it when it goes
//! idle, and if the machine has no container runtime, say so *in the search
//! result* and keep answering with DuckDuckGo. Enabling the feature configures it;
//! disabling it only disables it - nothing is uninstalled, and the container is
//! left alone if someone else owns it.
//!
//! Port 8899 rather than the usual 8888 on purpose: an instance started by another
//! program keeps its port, and `backend = "searxng"` with a URL is still how you
//! point at one.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_AUTO_PORT: u16 = 8899;
pub const DEFAULT_IDLE_SECS: u64 = 120;
pub const IMAGE: &str = "docker.io/searxng/searxng:latest";
pub const CONTAINER: &str = "cwoffee-searxng";

/// What `auto` needs: which port it owns, how long an idle container is worth
/// keeping, and where its own config lives.
#[derive(Debug, Clone)]
pub struct AutoSearXNG {
    pub port: u16,
    pub idle_secs: u64,
    /// Written on first use: `settings.yml` (JSON format on, limiter off) and
    /// `limiter.toml`. Ours, so a packaged SearXNG's config is never touched.
    pub dir: PathBuf,
    /// Epoch seconds of the last search. Atomic because the tool touches it from
    /// sync code and the idle reaper reads it from a task.
    last_used: Arc<AtomicU64>,
    reaper_armed: Arc<AtomicBool>,
}

impl Default for AutoSearXNG {
    fn default() -> Self {
        Self::new(DEFAULT_AUTO_PORT, DEFAULT_IDLE_SECS, PathBuf::from("/tmp/cwoffee-searxng"))
    }
}

impl AutoSearXNG {
    pub fn new(port: u16, idle_secs: u64, dir: PathBuf) -> Self {
        Self {
            port,
            idle_secs,
            dir,
            last_used: Arc::new(AtomicU64::new(0)),
            reaper_armed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn now_secs() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    /// The reaper's whole decision, kept pure so it can be tested without a
    /// container: an empty clock means "never used", which is not a reason to stop
    /// something that has not started.
    pub fn should_reap(last_used: u64, now: u64, idle_secs: u64) -> bool {
        last_used > 0 && now.saturating_sub(last_used) >= idle_secs
    }

    pub fn touch(&self) {
        self.last_used.store(Self::now_secs(), Ordering::Relaxed);
    }

    pub fn settings_written(&self) -> bool {
        self.dir.join("settings.yml").is_file() && self.dir.join("limiter.toml").is_file()
    }

    /// Write the two files the container mounts. `Ok(true)` means created now, so
    /// the log says "configured" once rather than on every search.
    ///
    /// `use_default_settings` keeps this short: the only two things we change are
    /// the response formats (JSON is what the tool parses) and the limiter, which
    /// otherwise blocks an agent that runs several searches in a row - the exact
    /// failure the DuckDuckGo fallback has.
    pub fn write_settings(&self) -> Result<bool, String> {
        if self.settings_written() {
            return Ok(false);
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("could not create {}: {e}", self.dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        }
        std::fs::write(
            self.dir.join("settings.yml"),
            format!(
                "use_default_settings: true\n\nserver:\n  secret_key: \"{}\"\n  \
                 bind_address: \"0.0.0.0\"\n  port: 8080\n  limiter: false\n  \
                 public_instance: false\n\nsearch:\n  formats:\n    - html\n    - json\n\n\
                 general:\n  debug: false\n  instance_name: \"cwoffee-ephemeral-search\"\n",
                secret_hex()
            ),
        )
        .map_err(|e| format!("could not write settings.yml: {e}"))?;
        std::fs::write(
            self.dir.join("limiter.toml"),
            "[botdetection.ip_limit]\nlink_token = false\n\n[botdetection.ip_lists]\n\
             pass_searxng_org = true\n",
        )
        .map_err(|e| format!("could not write limiter.toml: {e}"))?;
        Ok(true)
    }

    /// The exact `<runtime> run` argument list. Pinned by a test, because the two
    /// flags that are easy to drop are the ones that fail mysteriously: `:z` on the
    /// mounts (SELinux: without it the container cannot read its own settings and
    /// exits immediately), and the loopback-only publish, which decides whether a
    /// search box is reachable from the LAN.
    pub fn run_args(&self, secret: &str) -> Vec<String> {
        vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            CONTAINER.into(),
            "--rm".into(),
            "-v".into(),
            format!("{}/settings.yml:/etc/searxng/settings.yml:ro,z", self.dir.display()),
            "-v".into(),
            format!("{}/limiter.toml:/etc/searxng/limiter.toml:ro,z", self.dir.display()),
            "-e".into(),
            format!("SEARXNG_SECRET={secret}"),
            "-p".into(),
            format!("127.0.0.1:{}:8080", self.port),
            // Bounded on purpose: SearXNG is a Python app that will happily grow,
            // and it is a search box, not a service worth unbounded memory.
            "--memory".into(),
            "256m".into(),
            "--cpus".into(),
            "1.0".into(),
            IMAGE.into(),
        ]
    }
}

/// Which container runtime to drive, podman first: it is the default on
/// Fedora-family systems, needs no daemon, and rootless podman already gives the
/// container its own network namespace. `probe` is injected so the order can be
/// tested without installing anything.
pub fn pick_runtime(probe: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    if probe("podman") {
        Some("podman")
    } else if probe("docker") {
        Some("docker")
    } else {
        None
    }
}

pub fn runtime() -> Option<&'static str> {
    pick_runtime(&is_on_path)
}

/// A `which` without a dependency: PATH plus the executable bit. Rootless podman is
/// found through PATH exactly as a shell finds it.
#[cfg(unix)]
fn is_on_path(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        std::fs::metadata(dir.join(bin))
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    })
}

#[cfg(not(unix))]
fn is_on_path(_bin: &str) -> bool {
    false
}

/// Not a security boundary - the service listens on loopback - but SearXNG refuses
/// to boot without a secret, and a fixed value on disk is worse than a fresh one.
///
/// Exactly 16 bytes, with `read_exact`. A first version used `fs::read("/dev/urandom")`
/// and that is an infinite stream, not a file: the test binary grew until the machine
/// OOM-killed the inference process next to it. Never read a character device to EOF.
fn secret_hex() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let got = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if got.is_err() {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u128)
            .unwrap_or(0)
            ^ (u128::from(std::process::id()) << 32);
        for (i, b) in t.to_le_bytes().iter().take(16).enumerate() {
            bytes[i] = *b;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AutoStatus {
    /// Answering on the expected port.
    pub up: bool,
    /// A container with our name exists, answering or not.
    pub container_exists: bool,
    pub runtime: Option<String>,
    pub base: String,
    /// One English sentence, enough on a phone where there is no log to read.
    pub note: String,
}

impl AutoSearXNG {
    async fn http_ok(&self, path: &str, budget: Duration) -> bool {
        let client = match reqwest::Client::builder().timeout(budget).build() {
            Ok(c) => c,
            Err(_) => return false,
        };
        match client.get(format!("{}{}", self.base(), path)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    pub async fn health(&self) -> bool {
        self.http_ok("/", Duration::from_millis(1500)).await
    }

    async fn container_running(&self, rt: &str) -> bool {
        let out = tokio::process::Command::new(rt)
            .args(["ps", "-q", "--filter", &format!("name=^{CONTAINER}$")])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;
        matches!(&out, Ok(o) if o.status.success()
            && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
    }

    /// Bring the instance up. Blocking by design: the caller is about to run a
    /// search and has nothing else to do meanwhile. On a fresh machine this also
    /// pulls the image, which is why the budget is long and the log line says so -
    /// ninety seconds of "starting" otherwise reads as a hang.
    pub async fn ensure_up(&self) -> Result<(), String> {
        if self.health().await {
            self.touch();
            return Ok(());
        }
        let rt = runtime().ok_or_else(|| {
            "no container runtime on this machine (looked for podman, then docker)".to_string()
        })?;
        if self.write_settings()? {
            eprintln!(
                "[cwoffee] configured an ephemeral SearXNG in {} (JSON API on, limiter off)",
                self.dir.display()
            );
        }
        if self.container_running(rt).await {
            return self.wait_for_health(rt, Duration::from_secs(30)).await;
        }
        // A stopped container still holding our name: clear it, or `run` refuses
        // the name and the failure looks like a broken image.
        let _ = tokio::process::Command::new(rt)
            .args(["rm", CONTAINER])
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await;

        eprintln!(
            "[cwoffee] starting {} on {} (the first run pulls the image, which can take a minute)",
            CONTAINER,
            self.base()
        );
        let secret = secret_hex();
        let out = tokio::process::Command::new(rt)
            .args(self.run_args(&secret))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("{rt} run failed: {e}"))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!(
                "{rt} could not start {IMAGE}: {}",
                if why.is_empty() { "no reason given".to_string() } else { why }
            ));
        }
        self.wait_for_health(rt, Duration::from_secs(90)).await?;
        self.touch();
        self.arm_reaper();
        Ok(())
    }

    async fn wait_for_health(&self, rt: &str, budget: Duration) -> Result<(), String> {
        let until = Instant::now() + budget;
        while Instant::now() < until {
            if self.health().await {
                return Ok(());
            }
            if !self.container_running(rt).await {
                return Err(format!(
                    "the container exited without answering on {}",
                    self.base()
                ));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Err(format!("{IMAGE} did not answer on {} in time", self.base()))
    }

    /// One reaper per instance. It exits when it has stopped the container, so a
    /// long-running app does not accumulate timers.
    fn arm_reaper(&self) {
        if self.reaper_armed.swap(true, Ordering::SeqCst) {
            return;
        }
        let cfg = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let last = cfg.last_used.load(Ordering::Relaxed);
                let now = Self::now_secs();
                if !AutoSearXNG::should_reap(last, now, cfg.idle_secs) {
                    continue;
                }
                if !cfg.health().await {
                    // Someone else stopped it, or it died. Nothing to reap.
                    cfg.reaper_armed.store(false, Ordering::SeqCst);
                    return;
                }
                eprintln!(
                    "[cwoffee] stopped {} after {}s without a search; it starts again with the next one",
                    CONTAINER,
                    now.saturating_sub(last)
                );
                cfg.stop().await;
                cfg.reaper_armed.store(false, Ordering::SeqCst);
                return;
            }
        });
    }

    pub async fn stop(&self) {
        if let Some(rt) = runtime() {
            let _ = tokio::process::Command::new(rt)
                .args(["stop", CONTAINER])
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
        }
    }

    pub async fn status(&self) -> AutoStatus {
        let rt = runtime();
        let up = self.health().await;
        let exists = match rt {
            Some(rt) => self.container_running(rt).await,
            None => false,
        };
        let note = if up {
            format!(
                "running on {} — stops by itself after {}s without a search",
                self.base(),
                self.idle_secs
            )
        } else if rt.is_none() {
            "no container runtime here (looked for podman, then docker): search uses DuckDuckGo".into()
        } else if exists {
            format!("a container exists but is not answering on {}", self.base())
        } else {
            "not running yet — it starts with the first search".into()
        };
        AutoStatus {
            up,
            container_exists: exists,
            runtime: rt.map(String::from),
            base: self.base(),
            note,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cwoffee-searx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_run_line_is_the_one_that_actually_boots() {
        let a = AutoSearXNG::new(8899, 120, PathBuf::from("/home/me/.config/cwoffee/searxng"));
        let args = a.run_args("deadbeef");
        let joined = args.join(" ");
        // Loopback publish: a search box must not become a LAN service.
        assert!(joined.contains("-p 127.0.0.1:8899:8080"), "{joined}");
        // SELinux relabelling, or the container exits on boot with no useful error.
        assert!(
            joined.contains("/etc/searxng/settings.yml:ro,z")
                && joined.contains("/etc/searxng/limiter.toml:ro,z"),
            "{joined}"
        );
        assert!(joined.contains("--rm") && joined.contains("--name cwoffee-searxng"));
        assert!(joined.contains("--memory 256m") && joined.contains("--cpus 1.0"));
        assert!(joined.contains("SEARXNG_SECRET=deadbeef"));
        assert_eq!(args.last().unwrap(), IMAGE);
    }

    #[test]
    fn podman_before_docker_and_neither_is_not_a_panic() {
        assert_eq!(pick_runtime(&|b| b == "podman"), Some("podman"));
        assert_eq!(pick_runtime(&|b| b == "docker"), Some("docker"));
        // podman wins when both are installed: it needs no daemon and runs rootless.
        assert_eq!(pick_runtime(&|_| true), Some("podman"));
        assert_eq!(pick_runtime(&|_| false), None);
    }

    #[test]
    fn the_settings_we_depend_on_are_the_ones_written() {
        let dir = tmpdir("settings");
        let a = AutoSearXNG::new(8899, 120, dir.clone());
        assert!(!a.settings_written());
        assert!(a.write_settings().unwrap(), "first call should create them");
        let yml = std::fs::read_to_string(dir.join("settings.yml")).unwrap();
        // The JSON format is what web_search parses, and the limiter is what blocks
        // an agent that runs several searches in a row.
        assert!(yml.contains("use_default_settings: true"));
        assert!(yml.contains("limiter: false"));
        assert!(yml.contains("- json"), "{yml}");
        assert!(yml.contains("port: 8080"), "the container port the publish maps from");
        assert!(!a.write_settings().unwrap(), "second call must not rewrite them");
        assert!(a.settings_written());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_secret_is_random_and_shaped_like_one() {
        let a = secret_hex();
        let b = secret_hex();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "the same secret twice is not a secret");
    }

    #[test]
    fn idle_means_idle_not_unused() {
        // Never used: the container may not even exist. Never reap on that.
        assert!(!AutoSearXNG::should_reap(0, 10_000, 120));
        assert!(!AutoSearXNG::should_reap(9_900, 10_000, 120));
        assert!(AutoSearXNG::should_reap(9_880, 10_000, 120));
        // A clock in the future (NTP jump) must not reap either.
        assert!(!AutoSearXNG::should_reap(10_500, 10_000, 120));
    }

    #[test]
    fn the_port_is_ours_so_another_programs_instance_keeps_its_own() {
        let a = AutoSearXNG::default();
        assert_eq!(a.base(), "http://127.0.0.1:8899");
        assert_ne!(a.port, 8888, "8888 belongs to whatever else is already running");
    }
}
