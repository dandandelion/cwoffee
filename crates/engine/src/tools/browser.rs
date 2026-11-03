//! The browser: so a model can run the thing it just wrote before handing it over.
//!
//! `exec` can start a dev server and `read_file` can read the source, but neither can say
//! whether the page renders, whether the button works, or whether the console is full of
//! errors. That gap is where "I tested it" means "it compiled". This tool opens the URL in
//! a real browser, walks a short scripted sequence, and reports the page's own account of
//! itself: what it rendered, what its scripts logged, which requests failed, and a
//! screenshot when the model can actually look at one.
//!
//! Why the browser is a **one-shot child** and not something the app keeps open: cwoffee's
//! tool host on the pi core is one process per tool call (`docs/MCP-SERVERS.md` runs into
//! the same wall with MCP servers), so a browser held by the tool host would be a new
//! browser on every call. A browser held by the app would work, and would also leave a
//! window on the user's screen between turns with nobody accountable for what it is
//! showing. The unit of work is therefore one walk - open, act, report, close - which also
//! keeps the approval decision in one piece: one card, naming one origin.
//!
//! Why headless versus headed is the **host's** decision and not the model's: the honest
//! rule is that a window is only worth painting for someone who can see it. A model with no
//! image input gets headless (text, console, structure - and no window flashing on the
//! user's desktop); a model with image input gets a real browser window and the pixels back
//! in its result. The model cannot ask for headed. That would let it decide, from inside
//! the prompt, that a window should appear on somebody's screen.

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{CallRisk, ExecSink, OutputFile, Risk, Tool, ToolCtx, ToolResult};

/// What the host says about the model it is running, as far as this tool cares.
///
/// Three states because an endpoint's silence is a real state: llama.cpp publishes
/// `input_modalities` and a hand-written endpoint may not. Guessing "yes" puts a window on
/// the user's screen for a model that cannot see it; guessing "no" withholds pixels from a
/// model that can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vision {
    #[default]
    Unknown,
    Yes,
    No,
}

/// Where the browser machinery is, and what the host knows about seeing.
///
/// `driver` is `None` when the driver is not installed here. The tool then says so together
/// with the command that would install it: an agent that gets back a bare "no browser"
/// tries harder in ways that are worse than being told.
#[derive(Debug, Clone)]
pub struct BrowserCfg {
    pub driver: Option<PathBuf>,
    /// The node runtime that runs the driver - the same lookup the pi tool host uses, so
    /// one install of node serves both.
    pub node: String,
    pub vision: Vision,
    /// A person's own override from config (`[browser] headed = true|false`). `None` means
    /// "decide from the model", which is the default and the honest one.
    pub headed_override: Option<bool>,
    /// Ceiling on screenshot bytes before they stop being worth their prompt cost.
    pub max_image_bytes: usize,
    /// Whether a browser may be **kept** — held open between calls so a person can watch it
    /// and a walk can continue where it left off. `true` unless a person said otherwise
    /// (`[browser] keep = false`), and never the model's to set: the schema has no such
    /// argument, and this struct is built from the config and the environment only.
    pub keep: bool,
    /// Minutes-of-nobody-touching for a kept browser, as configured. `None` means the
    /// engine's own five minutes.
    pub idle_secs: Option<u64>,
}

impl BrowserCfg {
    /// The same decision, read from the environment.
    ///
    /// This exists because the pi core's tools do not run inside cwoffee: `cwtool` is a fresh
    /// process per call, so the app hands it the decision it already made through the child's
    /// environment. Exactly one place builds a BrowserCfg from those variables, so the two
    /// cores cannot grow two rules for finding a driver or reading a model's eyes.
    pub fn from_env() -> Self {
        use std::env::var;
        Self {
            driver: var("CWOFFEE_BROWSER_DRIVER")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_file()),
            node: var("CWOFFEE_NODE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "node".to_string()),
            vision: match var("CWOFFEE_MODEL_VISION").ok().as_deref() {
                Some("yes") => Vision::Yes,
                Some("no") => Vision::No,
                _ => Vision::Unknown,
            },
            headed_override: match var("CWOFFEE_BROWSER_HEADED").ok().as_deref() {
                Some("yes") => Some(true),
                Some("no") => Some(false),
                _ => None,
            },
            max_image_bytes: var("CWOFFEE_BROWSER_MAX_IMAGE_KB")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .map(|kb| kb.saturating_mul(1024))
                .unwrap_or(2 * 1024 * 1024),
            // The app's decision, carried down the same way the vision decision is. A tool
            // process that read `keep` for itself would be a second rule for the same knob, and
            // `keep = no` spelled `0` would be a switch that appears not to work.
            keep: crate::config::Config::keep_decision(None, var("CWOFFEE_BROWSER_KEEP").ok().as_deref()),
            idle_secs: var("CWOFFEE_BROWSER_IDLE_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok()),
        }
    }

    /// Whether this start can open a browser at all.
    pub fn usable(&self) -> bool {
        self.driver.as_ref().map(|p| p.is_file()).unwrap_or(false)
    }
}

impl Default for BrowserCfg {
    fn default() -> Self {
        Self {
            driver: None,
            node: "node".to_string(),
            vision: Vision::Unknown,
            headed_override: None,
            max_image_bytes: 2 * 1024 * 1024,
            keep: true,
            idle_secs: None,
        }
    }
}

/// How this tool process reaches the **kept** browser — the one behind the window's
/// sidebar that a person can see, open, and act in themselves.
///
/// Two handles, deliberately:
///
/// * `hub` drives it, and only the process that owns the browser has it. On the pi core the
///   tool host is a separate process per call, so the route there is a socket (`sock`) and
///   `hub` is `None` — the same wall [`crate::mcp`] hit, for the same reason.
/// * `shared` is a *synchronous* read of the state. [`Tool::risk_for`] is not async: the
///   question it asks — "is this the browser somebody is looking at?" — has to be answerable
///   while the approval card is being built, before any await. A risk level that fell back to
///   "no session" because a lock was busy would be a level that lies.
#[derive(Clone)]
pub struct BrowserRoute {
    pub hub: Option<std::sync::Arc<tokio::sync::Mutex<crate::browser_hub::Hub>>>,
    pub shared: std::sync::Arc<std::sync::RwLock<crate::browser_hub::Snapshot>>,
    pub sock: Option<std::path::PathBuf>,
}

impl Default for BrowserRoute {
    fn default() -> Self {
        Self {
            hub: None,
            shared: std::sync::Arc::new(std::sync::RwLock::new(crate::browser_hub::Snapshot::default())),
            sock: None,
        }
    }
}

impl std::fmt::Debug for BrowserRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.peek();
        write!(
            f,
            "BrowserRoute({})",
            if s.alive {
                format!("alive at {}", s.url)
            } else {
                format!("closed ({})", s.why)
            }
        )
    }
}

impl BrowserRoute {
    /// A route for the process that **owns** the browser.
    ///
    /// The state mirror is handed over rather than fetched from inside the lock, and that is
    /// not laziness: reading it must never wait on whoever is driving the browser, and taking
    /// a tokio lock to get it would mean a blocking read inside a runtime — which panics in
    /// the middle of a risk check. Own the hub, keep the mirror beside it.
    pub fn in_process(hub: std::sync::Arc<tokio::sync::Mutex<crate::browser_hub::Hub>>, shared: std::sync::Arc<std::sync::RwLock<crate::browser_hub::Snapshot>>) -> Self {
        Self { hub: Some(hub), shared, sock: None }
    }

    /// A route for a tool host in another process: the state mirror is local and updated by
    /// the answers themselves, since the hub is not reachable in-process.
    pub fn over_socket(sock: std::path::PathBuf, shared: std::sync::Arc<std::sync::RwLock<crate::browser_hub::Snapshot>>) -> Self {
        Self { hub: None, shared, sock: Some(sock) }
    }

    pub fn peek(&self) -> crate::browser_hub::Snapshot {
        crate::browser_hub::Hub::peek_shared(&self.shared)
    }

    /// How to reach it, if at all.
    pub fn link(&self) -> Option<crate::browser_hub::Link> {
        if let Some(h) = self.hub.as_ref() {
            return Some(crate::browser_hub::Link::InProcess(std::sync::Arc::clone(h)))
        }
        self.sock.as_ref().map(|p| crate::browser_hub::Link::Socket(p.clone()))
    }

    /// Pull the app's current state into the local mirror, blocking briefly.
    ///
    /// A tool process is not on a runtime when it answers the risk question, so this is
    /// called once, on purpose, at startup — *not* from [`peek`](Self::peek), which a risk
    /// check calls and which must stay a plain memory read.
    pub fn refresh_blocking(&self) {
        let Some(path) = self.sock.as_ref() else { return };
        if let Ok(v) = crate::browser_hub::socket::ask_blocking(path, &serde_json::json!({"cmd": "state"}), std::time::Duration::from_millis(600)) {
            let snap = crate::browser_hub::socket::snapshot_from(&v);
            if let Ok(mut g) = self.shared.write() {
                *g = snap;
            }
        }
    }
}

/// The outer bound on one walk. The driver keeps its own internal deadline for the steps;
/// this is the one that turns a hung browser into a sentence.
const WALK_TIMEOUT_MS: u64 = 60_000;

pub struct BrowseTool;

fn base() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "The http(s) URL to open. A file:// URL inside the workspace is also accepted. Required unless a page is already open in the browser cwoffee is keeping."
            },
            "session": {
                "type": ["boolean", "string"],
                "description": "Act in the browser cwoffee is **keeping open** — the one shown in the person's sidebar. In a start that has that sidebar browser this is what `browse` does anyway, so you do not need to pass it. Pass the boolean `false` to ask deliberately for a throwaway walk that does not touch the page the person is looking at; it runs without painting a window. Pass the boolean `true`; the word `\"true\"` counts too, because models type it.",
            },
            "steps": {
                "type": "array",
                "maxItems": 40,
                "description": "Run after the page loads, in order. A step that fails does not stop the rest; every step's outcome is reported, with the milliseconds the ones that wait spent waiting — measured, not asked for. `text` reads the page's words, `snapshot` its controls, `a11y` its elements by role and name, `health` how many messages/errors/failed requests *this page* has made, `back`/`forward`/`reload`/`stop`/`navigate` move the page the way a person's buttons do. They all work in every browser; in a throwaway walk the only history `back` has is the pages that walk visited.",
                "items": {
                    "type": "object",
                    "properties": {
                        "do": { "type": "string", "enum": ["click","fill","press","wait","wait_for","scroll","eval","text","snapshot","a11y","health","back","forward","reload","stop","navigate"] },
                        "target": { "type": "string", "description": "click/fill/wait_for: how a person would name it - a label, a placeholder, visible text, or a CSS selector." },
                        "text": { "type": "string", "description": "fill: what to type. For a <select>, name one of its options (by its text or its value) rather than typing into it." },
                        "key": { "type": "string", "description": "press: one key, e.g. Enter. Tab moves focus the way the page's tab order does; Enter in a field submits its form." },
                        "ms": { "type": "integer", "description": "wait: how long to wait. wait_for: how long to keep looking (default: the time this step has left). The answer says how many milliseconds the step actually took." },
                        "px": { "type": "integer", "description": "scroll: pixels." },
                        "code": { "type": "string", "description": "eval: a JavaScript expression evaluated in the page; its value comes back as text. If it hands back a promise, the settled value comes back — a rejection is reported as a failure." },
                        "max": { "type": "integer", "description": "a11y: how many elements to list." },
                        "url": { "type": "string", "description": "navigate: the http(s) or workspace file:// page to point this browser at. Only with `session`: a one-off walk already names its page." }
                    },
                    "required": ["do"]
                }
            },
            "screenshot": { "type": "string", "enum": ["none","viewport","full"], "description": "A picture of the page after the walk. Default: viewport when this model can see images, none when it cannot. A page in cwoffee's own window has no picture of itself — it is the page — so asking for one there only gets you told that; read such a page with text, snapshot, a11y or eval. In a kept browser the person watching the panel sees the page either way; this only decides whether *you* get pixels too." },
            "wait_until": { "type": "string", "enum": ["load","domcontentloaded","networkidle"] },
            "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 120 }
        },
        // `url` is not declared required, because `session: true` against an already-open page
        // is a legitimate call with no url in it. The rule is checked in the tool, which can
        // say which of the two it wanted rather than a schema validator failing on one.
        "required": []
    })
}

/// The one-line answer to "what will this tool do, and why", for a settings screen.
///
/// Written here rather than in the UI because it is the same sentence the tool prints in the
/// result, and a screen that paraphrases a policy is a screen that eventually contradicts it.
/// It has to say three things: whether the tool exists at all in this start, which way the
/// browser will run, and what decided that — because "headed" is a claim about a window
/// appearing on the person's desktop, and an unexplained claim about that is not a setting.
pub fn status_line(cfg: &BrowserCfg, window: bool) -> String {
    if !cfg.usable() {
        return "The browser tool is not mounted: no driver was found, so the model has no way to open a page. Put `browser/driver.mjs` next to cwoffee with its dependencies installed (`npm --prefix browser install`), and a browser build with                 `npx playwright install chromium`."
            .to_string();
    }
    // A settings screen describes the plain walk, which is the only thing a start without a page
    // of its own can do; the sidebar's own browser is reported by the browser panel instead. When
    // this start *does* have that page, `window` is what makes the sentence true of the start.
    let (headed, why, shot) = decide(cfg, "", window);
    format!(
        "The browser tool is mounted (driver {}, node {}). It will run {} because {}; a page picture {} back to the model by default.",
        cfg.driver.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        cfg.node,
        if headed { "with a visible window" } else { "headless" },
        why,
        if shot == "none" { "is not taken or sent" } else { "is taken and sent" },
    )
}

/// Headless or headed, and why — one place, because this is the tool's whole promise.
///
/// The order is deliberate. A person's `[browser] headed = false` outranks the model's
/// modality: whoever wrote that has said they do not want windows painted on their desktop,
/// whichever model is answering. With no override, the model's eyes decide, because a window
/// is only worth a person's attention if the run can also hand the picture to something that
/// will use it. `Unknown` is treated as "no": an endpoint that never answered `/models` has
/// not earned the right to put a window on someone's screen, and the sentence says so rather
/// than pretending the run was headless by design.
///
/// The model is not in this decision and cannot be: there is no `headed` field in the schema,
/// so nothing in the prompt can ask for a window. That is not caution theatre — a tool call
/// is the one place a model gets to act, and "make a window appear on a human's desktop" is
/// not a thing an agent should be able to request.
pub fn decide(cfg: &BrowserCfg, asked_shot: &str, window: bool) -> (bool, String, &'static str) {
    let headed = match cfg.headed_override {
        Some(h) => h,
        // A start that shows pages in cwoffee's own window has a browser already. Painting a second
        // one on the person's desktop for a page they cannot look at, while the one they can look at
        // sits idle, is the failure this feature exists to avoid — so a walk that does happen here
        // happens out of sight.
        None if window => false,
        None => cfg.vision == Vision::Yes,
    };
    let why = match (cfg.headed_override, window, cfg.vision) {
        (Some(true), _, _) => "you set [browser] headed = true".to_string(),
        (Some(false), _, _) => "you set [browser] headed = false".to_string(),
        (None, true, _) => {
            "cwoffee's own window is the browser in this start, so a walk that runs elsewhere paints no window".to_string()
        }
        (None, false, Vision::Yes) => "this model takes image input".to_string(),
        (None, false, Vision::No) => "this model takes text only".to_string(),
        (None, false, Vision::Unknown) => {
            "this endpoint never said whether the model sees images, so nothing was painted on your desktop"
                .to_string()
        }
    };
    // A picture the model cannot see is nothing; one it can see is the most informative thing
    // this tool returns. Either way the person still gets the file, so "I looked at it" stays
    // true for a text-only model — it just has to read the description of the page instead.
    let shot = match asked_shot {
        "none" => "none",
        "full" => "full",
        "viewport" => "viewport",
        _ => {
            if cfg.vision == Vision::Yes {
                "viewport"
            } else {
                "none"
            }
        }
    };
    (headed, why, shot)
}

#[async_trait::async_trait]
impl Tool for BrowseTool {
    fn name(&self) -> &str {
        "browse"
    }

    fn description(&self) -> &str {
        "Open a page in a real browser and report what it says and what its own scripts do: the \
         rendered text, the controls on it, every console message and uncaught error, failed \
         requests, and the outcome of a short scripted walk (click, fill, press, wait, read a \
         value). Use it to check something you built or changed - a page that compiles is not a \
         page that works. By default one call is one browser: put the whole sequence in `steps`, \
         because the browser closes when the call ends. **When cwoffee is showing pages in its own \
         window — the sidebar browser — that is the browser this call gets**, with no argument \
         needed: it is the page the person can see, it carries their logins, and it outlives the \
         call. cwoffee does not open a second browser beside the window; `session` says the same \
         thing by name, and `session: false` is the deliberate exception — a one-off walk that \
         leaves the page the person is looking at alone, which runs without painting a window \
         either. Whether pixels come back to you depends on whether this conversation's model can \
         see images, and a page in the window has no screenshot of itself: read it with `text`, \
         `snapshot`, `a11y` or `eval`. The page fetches for itself, so its requests are not routed \
         through the sandbox's package-manager proxy. Whether a browser window is painted on the \
         person's desktop is the host's decision from config and the model's recorded inputs, not \
         an argument here."
    }

    fn schema(&self) -> Value {
        base()
    }

    fn risk(&self) -> Risk {
        // Not `Harmless`, even though `web_fetch` is. A fetch reads a page; a browser *runs* it —
        // somebody else's scripts, in a real renderer, over a network stack that is not the
        // sandboxed one `exec` commands go through. Ask mode therefore gets one card naming the
        // origin, and a session grant covers the rest of the walk: a model checking a page it just
        // wrote opens it several times, and a card each time is a card nobody reads.
        Risk::Sensitive
    }

    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        let raw = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let Some(path) = file_url_path(raw) else {
            // An http(s) origin. The level says "a person decides this once per conversation" and
            // `per_call` stays false so that grant means what it says; the card names the origin,
            // which is the one part of the call a person can actually judge.
            //
            // **Except** in a kept session, where `per_call` is forced on. "Yes to browse for this
            // conversation" was said about a browser that opened and closed inside one call. The
            // same words must not become standing permission to click around inside a window a
            // person left open, on pages they may be logged into, while they watch. That is a
            // different decision and it gets asked each time — the same reason an MCP call is
            // never covered by a remembered grant.
            //
            // And the same applies to a call that did not say `session` at all in a start whose
            // browser *is* the sidebar: routing it there is what makes it a decision about a window
            // the person is watching, so it cannot inherit a grant written for a browser that had
            // already closed.
            if wants_session(args) || (window_browser(ctx) && ctx.browser.keep && !wants_throwaway(args)) {
                return CallRisk {
                    level: Risk::Sensitive,
                    escapes: Vec::new(),
                    per_call: true,
                };
            }
            return CallRisk {
                level: Risk::Sensitive,
                escapes: Vec::new(),
                per_call: false,
            };
        };
        // A file:// URL is the local filesystem wearing a page's costume. Inside the
        // workspace it is the same reach the file tools already have; outside it, it is a
        // read the person has to say yes to by name - the same discipline as `read_file`
        // on a path outside the folder.
        let real = PathBuf::from(&path).canonicalize().unwrap_or_else(|_| PathBuf::from(&path));
        if real.starts_with(&ctx.workspace) {
            CallRisk::at(Risk::Workspace)
        } else {
            CallRisk {
                level: Risk::Sensitive,
                escapes: vec![real.display().to_string()],
                per_call: false,
            }
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        // A kept browser is a different animal — it may belong to the person, it outlives the
        // call, and somebody may be watching it — so it is a different path through here, and
        // it is taken only when the call asks for it by name. Checked **before** the one-shot
        // driver: a kept browser has its own driver and its own sentence for being missing,
        // and answering a `session: true` call with install advice about the other program
        // would send a person off to fix the wrong thing.
        if wants_session(&args) {
            return session_walk(&args, ctx).await;
        }

        let cfg = &ctx.browser;
        // **The browser in cwoffee's window is the browser.** A start that has one does not get to
        // also open a second one somewhere else because the call forgot to say `session`: that is a
        // Chromium window on the person's desktop, running a page they cannot see, while the page
        // they *are* looking at — with their logins in it — does nothing. The argument is not how
        // this gets decided; the hub knowing which browser it is holding is.
        //
        // Two ways out, both spelled by the caller: `session: false` asks for a walk that deliberately
        // does not touch the sidebar page (and it paints no window either), and `keep = false` means
        // the person refused a standing browser, so the one-off walk is what they asked for.
        if window_browser(ctx) && cfg.keep && !wants_throwaway(&args) {
            return session_walk(&args, ctx).await;
        }
        let Some(driver) = cfg.driver.as_ref().filter(|p| p.is_file()) else {
            return ToolResult::err(
                "browse: this start has no browser driver, so nothing was opened. The driver is \
                 `browser/driver.mjs` next to cwoffee's source with its dependencies installed \
                 (`npm --prefix browser install`); a browser build comes from \
                 `npx playwright install chromium`.",
            );
        };

        let raw_url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => return ToolResult::err("browse: missing required string field \"url\""),
        };

        let (allow_file, opened_url) = match file_open_rule(&raw_url, ctx) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let (headed, why_headed, shot_mode) = decide(cfg, args.get("screenshot").and_then(|v| v.as_str()).unwrap_or(""), window_browser(ctx));

        let timeout_ms = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|s| (s.min(120) * 1000).min(WALK_TIMEOUT_MS))
            .unwrap_or(WALK_TIMEOUT_MS);

        // The picture goes into the workspace's own scratch folder: it is the model's
        // working material, not something the person asked for, and it must not be a
        // deliverable chip just because a browser happened to render something.
        let shot_name = format!(
            "browser/{}-{}.png",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            std::process::id()
        );
        let shot_path = scratch_dir(&ctx.workspace).join(&shot_name);
        if shot_mode != "none" {
            if let Some(parent) = shot_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
        }

        // `opened_url` came out of `file_open_rule` already resolved: for a file URL that is
        // the canonical path the check decided about, rebuilt as a URL, because passing the
        // caller's string through would let a URL whose spelling the check misread reach the
        // browser unchanged.
        let mut request = json!({
            "url": opened_url,
            "headless": !headed,
            "allow_file": allow_file,
            "screenshot": if shot_mode == "none" { Value::String("none".into()) } else { Value::String(shot_mode.to_string()) },
            "screenshot_path": shot_path.display().to_string(),
            "timeout_ms": timeout_ms,
            "text": true,
        });
        if let Some(steps) = args.get("steps") {
            request["steps"] = steps.clone();
        }
        if let Some(wu) = args.get("wait_until").and_then(|v| v.as_str()) {
            request["wait_until"] = json!(wu);
        }

        let out = match walk(&cfg.node, driver, &request, timeout_ms).await {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        let mut output = render(&out, &args, ctx, cfg, shot_mode, headed, &why_headed, &shot_path, &shot_name);
        // If a browser is sitting open in the window and this walk used its own throwaway one,
        // say so. Not to nag: two browsers looking at the same site is how an agent ends up
        // "fixed" a page the person never saw change. The fix is one word in the next call, and
        // the note gives the url and who opened it.
        if let Some(n) = other_session_note(ctx) {
            output = format!("{output}\n\n{n}");
        }
        ToolResult {
            ok: out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            output,
            exit_code: None,
            truncated: false,
            error: None,
            files: if out
                .get("screenshot")
                .and_then(|s| s.get("path"))
                .and_then(|p| p.as_str())
                .is_some()
            {
                vec![OutputFile {
                    path: format!(".cws/{shot_name}"),
                    name: shot_name.rsplit('/').next().unwrap_or("screenshot.png").to_string(),
                    mime: "image/png".to_string(),
                    bytes: out
                        .get("screenshot")
                        .and_then(|s| s.get("bytes"))
                        .and_then(|b| b.as_u64())
                        .unwrap_or(0),
                    kind: super::FileKind::Scratch,
                    created: false,
                }]
            } else {
                Vec::new()
            },
            changes: Vec::new(),
            checkpoints: Vec::new(),
            activates: Vec::new(),
            images: model_image(&out, cfg, &shot_path),
        }
    }
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Ask the driver for one page walk. The request goes in on stdin rather than in argv:
/// a step list is arbitrary text from the model, and an argument list made of it is an
/// quoting bug waiting to be found in somebody's `eval` string.
async fn walk(node: &str, driver: &std::path::Path, request: &Value, timeout_ms: u64) -> Result<Value, String> {
    let mut child = Command::new(node)
        .arg(driver)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            format!(
                "browse: could not start `{node}` for the browser driver: {e}. Install node, or \
                 point cwoffee at it (CWOFFEE_NODE). Nothing was opened."
            )
        })?;

    if let Some(mut si) = child.stdin.take() {
        let body = serde_json::to_vec(request).map_err(|e| format!("browse: could not write the walk request: {e}"))?;
        si.write_all(&body).await.map_err(|e| format!("browse: could not hand the request to the driver: {e}"))?;
        let _ = si.shutdown().await;
    }

    // Read the pipes before waiting: a driver that writes more than the pipe buffer would
    // block forever against a parent that is waiting for it to exit.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let budget = std::time::Duration::from_millis(timeout_ms + 5_000);
    let read = tokio::time::timeout(budget, async {
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut out).await?;
        let mut err = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut err).await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((out, err, status))
    })
    .await;
    let (stdout_bytes, stderr_bytes) = match read {
        Ok(Ok((o, e, _))) => (o, e),
        Ok(Err(e)) => return Err(format!("browse: the browser driver could not run: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            // No claim about files: the child may have written a screenshot before it was
            // stopped, and "nothing was written" would be a promise this parent cannot make.
            //
            // Name the clock, and say what a timeout costs. A model that reads "within {}s" learns
            // nothing about how long it had, which is the one fact that tells it whether the page
            // was slow or the tool was broken; and "no page report" is a different thing from "the
            // page said nothing", which is what the kept browser can still answer after a walk dies.
            return Err(format!(
                "browse: the browser did not answer within {}s of walk time ({}s of waiting), so the walk was stopped and no page report came back. A one-shot browser closes with the walk, so what it had seen is gone; a screenshot it took before stopping may be on disk under .cws/browser/. A page that survives a timeout — and keeps its console, errors and failed requests to be read afterwards — is the kept browser: the same call with \"session\": true.",
                timeout_ms as f64 / 1000.0,
                budget.as_secs_f64()
            ));
        }
    };

    if stdout_bytes.is_empty() {
        let why = String::from_utf8_lossy(&stderr_bytes);
        return Err(format!(
            "browse: the browser driver returned nothing. {}",
            why.trim().lines().next().unwrap_or("(no message from it)")
        ));
    }
    serde_json::from_slice(&stdout_bytes).map_err(|e| {
        format!(
            "browse: the browser driver's answer is not the shape expected ({e}). Treating it as \
             no result rather than guessing at one."
        )
    })
}

/// `file:///home/x/y.html` -> Some("/home/x/y.html"). Percent escapes are decoded by hand:
/// the set that appears in a path is small, and pulling the `url` crate's parser in here
/// would accept things a browser would not (a `file:` URL with a host, for one).
/// The absolute path a `file://` URL names, or None if it is not one.
///
/// Absolute on purpose. The first version of this stripped the leading slashes and joined
/// what was left onto the workspace, which made every `file:///etc/passwd` look like a page
/// inside the folder — and the check then approved a URL that the browser was handed
/// *verbatim*, so the browser really did read /etc/passwd. A confinement test built out of
/// relative strings would not have caught that; this shape cannot.
///
/// A URL with a host in it (`file://othermachine/x`) is refused here rather than guessed at:
/// the answer would be "some other machine's filesystem", which this tool cannot judge.
fn file_url_path(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    if rest.starts_with("localhost/") {
        return Some(decode_pct(&format!("/{}", rest.strip_prefix("localhost/")?)));
    }
    if let Some(slash) = rest.find('/') {
        if !rest[..slash].is_empty() {
            return None;
        }
    }
    let path = decode_pct(rest);
    if path.len() >= 2 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic() {
        // Windows drive letter written as file:///C:/x (the host's own form).
        return Some(path);
    }
    if let Some(stripped) = path.strip_prefix('/') {
        // file:// followed by *nothing* is the filesystem root, which is the widest possible
        // read; name it as itself rather than turning it into "".
        Some(format!("/{stripped}"))
    } else {
        None
    }
}

fn decode_pct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn scratch_dir(ws: &std::path::Path) -> PathBuf {
    super::scratch::scratch_root(ws).join("browser")
}

/// Base64, written here rather than taken as a dependency: the engine carries no base64
/// crate today, and this is the only place that needs one. Standard alphabet, `=` padding.
fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16) | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8) | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// The inverse, for the same reason: a kept browser's picture comes back over a pipe as
/// base64, and the only way to hand it to a model that takes images — or to save it for the
/// person — is to put the bytes back. Ignores whitespace and missing padding rather than
/// failing a whole tool call over a newline in a pipe.
fn b64d(text: &str) -> Vec<u8> {
    const T: &[u8; 256] = &{
        let mut t = [0xFFu8; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            t[alphabet[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    for b in text.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' {
            continue;
        }
        let v = T[b as usize];
        if v == 0xFF {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}


/// The picture, for the model, only when the model can look at one.
///
/// The size ceiling is not about memory: an image is the most expensive thing a tool result
/// can carry into the next prompt, and a 4 MB full-page capture would cost more than the
/// conversation's whole history. Over the ceiling the file stays on disk for the person and
/// the result says so.
/// The sentence appended to a one-shot walk when a kept browser is sitting open.
///
/// Two browsers looking at one site is how an agent ends up having "fixed" a page the person
/// never saw change, so the walk says which browser it used and how to use the other one.
/// It does not silently switch: the card approved a throwaway browser, and a call that ran
/// somewhere else would be a call the card did not describe.
fn other_session_note(ctx: &ToolCtx) -> Option<String> {
    let r = ctx.browser_route.as_ref()?;
    let s = r.peek();
    if !(s.alive && s.opened) {
        return None;
    }
    Some(format!(
        "note: a browser is already open at {} ({} opened it, and the person can see it in \
         cwoffee's sidebar). This walk ran in its own throwaway browser and closed it. If your \
         task is about *that* page, pass \"session\": true.",
        s.url,
        s.opened_by.words()
    ))
}

/// Whether a `file://` page may be opened, and the *resolved* url to point a browser at.
///
/// One function because it is one rule: a local page reads its own folder's siblings, so a
/// path outside the workspace is a read outside the workspace and needs the person's yes by
/// name — the same discipline as `read_file` on such a path. The kept-browser walk and the
/// one-shot walk call this same function; two copies is how one of them ends up opening a
/// folder the other refused.
///
/// Returns `(allow_file, url_to_open)`. The url is rebuilt from the canonicalised path on
/// purpose: passing the caller's string through would let a URL whose spelling this check
/// misread reach the browser unchanged.
fn file_open_rule(raw: &str, ctx: &ToolCtx) -> Result<(bool, String), String> {
    let mut allow_file = false;
    let mut opened_url = raw.to_string();
    if let Some(p) = file_url_path(raw) {
        let real = PathBuf::from(&p).canonicalize().unwrap_or_else(|_| PathBuf::from(&p));
        if real.starts_with(&ctx.workspace) {
            allow_file = true;
        } else if ctx.permits.iter().any(|g| real.starts_with(g)) {
            allow_file = true;
        } else {
            return Err(format!(
                "browse: {} is outside the workspace, and a file:// page can read its own \
                 folder's siblings. Approve the call and name the folder, or serve the page \
                 over http.",
                real.display()
            ));
        }
        opened_url = format!("file://{}", real.display());
    }
    Ok((allow_file, opened_url))
}

/// The kept-browser walk: act inside the browser cwoffee is holding open, and tell the model
/// what the page said *now*, since the point of the session is that the page is still there.
///
/// Two things this must not do:
///
/// * It must not pretend to be the person's click. Every step is recorded as coming from the
///   model, and the panel shows that, because "why did this page just change?" has an answer
///   a watching person is entitled to.
/// * It must not report a page it did not see. The state comes from the session's own
///   bookkeeping and two reads of the live page (`text`, `snapshot`); a step that failed is
///   said as failed, in order.
/// `session` as a call actually writes it. A local model types `"True"` for a boolean more often
/// than a schema expects, and one reader is not enough: the risk gate and the walk both ask "is this
/// about the kept page?", and if they disagree then a request to click inside a window a person left
/// open — on pages they may be logged into — gets graded as a throwaway browser that closes itself.
/// So the word is read one way, here.
fn wants_session(args: &Value) -> bool {
    match args.get("session") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "keep" | "session"
        ),
        _ => false,
    }
}

/// Whether this start shows pages **in cwoffee's own window** — the sidebar browser a person can
/// see, scroll, and log into. The hub is the only thing that knows: it has either got a native
/// driver attached or it has not, and the answer is in the same mirror the panel reads.
///
/// This decides which browser a `browse` call gets, and it is not the model's to choose. A start
/// with a window browser has one browser; sending a call somewhere else is what put a second,
/// invisible-to-the-model Chromium on the desktop while the page the person was looking at stayed
/// untouched.
fn window_browser(ctx: &ToolCtx) -> bool {
    ctx.browser_route.as_ref().map(|r| r.peek().native).unwrap_or(false)
}

/// Every step this tool offers, in one list. The browsers are Node files and a JavaScript string
/// sitting *beside the app binary*, and an install that swapped the binary without them leaves a
/// tool advertising `text` to a browser that has never heard of it — which a model reads as "this
/// page has no words" and stops trying. The list is the one place the two halves of that promise
/// can be compared, and it is compared twice: here against what a browser answered, and in a test
/// against the driver sources in this tree.
const VERBS: &[&str] = &[
    "click", "fill", "press", "wait", "wait_for", "scroll", "eval", "text", "snapshot", "a11y",
    "health", "back", "forward", "reload", "stop", "navigate",
];

/// Whether a browser's own words say it has never heard of the step it was sent.
fn does_not_know(why: &str) -> bool {
    why.contains("do not know the ")
}

/// Name the old half. "I do not know the step" from a browser is a fact about the *install*, and a
/// model holding only that sentence goes on to conclude the page is broken.
fn stale_driver_note(verbs: &[String]) -> Option<String> {
    if verbs.is_empty() {
        return None;
    }
    Some(format!(
        "the browser this call ran has never heard of {} — the tool documents them, so the browser \
         driver beside the running app is older than the app itself. They are installed together on \
         purpose: re-run scripts/install-desktop.sh, then say so rather than retrying the page.",
        verbs
            .iter()
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// `session: false` spelled as an actual boolean: the one way a call asks for the *other* browser —
/// a page the window will not show, or a walk that must not touch what the person is looking at.
/// It gets no window either, which is the point of doing it behind the sidebar's back.
fn wants_throwaway(args: &Value) -> bool {
    matches!(args.get("session"), Some(Value::Bool(false)))
}

async fn session_walk(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let routed = !wants_session(args);
    let cfg = &ctx.browser;
    let Some(route) = ctx.browser_route.as_ref() else {
        return ToolResult::err(
            "browse: no kept browser is mounted in this start, so `session: true` opened \
             nothing. The browser behind cwoffee's sidebar is held by the app; this tool host \
             was not given a way to reach it. Use the browser button in the window, or leave \
             `session` out and take a one-off look at a page.",
        );
    };
    let Some(link) = route.link() else {
        return ToolResult::err(
            "browse: the kept browser is switched off here, so `session: true` did nothing. \
             That is a person's setting (`[browser] keep = false`), not a failure to retry \
             around: a one-shot walk (`session` left out) still works.",
        );
    };
    let snap = route.peek();
    let raw_url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if raw_url.is_empty() && !(snap.alive && snap.opened) {
        // A call that never mentioned `session` gets told what it left out, not what it did not.
        let what = if routed {
            "a call with no url"
        } else {
            "`session: true` with no url"
        };
        return ToolResult::err(format!(
            "browse: {what} acts on the page that is already open, and \
             there is none. {} The person can open one from the browser panel, or you can pass \
             a url.",
            if snap.alive {
                "The kept browser is running with no page in it.".to_string()
            } else {
                format!("No kept browser is open: {}", if snap.why.is_empty() { "none was started".to_string() } else { snap.why.clone() })
            }
        ));
    }
    if !cfg.keep {
        // Checked again, after the route: a route that exists while `keep` is off means the
        // config changed while this call was in flight, and the honest answer is the refusal.
        return ToolResult::err(
            "browse: kept browsers are switched off in this start ([browser] keep = false), so \
             `session: true` did nothing and no browser was opened.",
        );
    }

    let mut lines: Vec<String> = Vec::new();
    if !raw_url.is_empty() {
        let (allow_file, url) = match file_open_rule(&raw_url, ctx) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        match link.open(&url, crate::browser_hub::Who::Model, allow_file).await {
            Ok(out) => {
                let where_to = out.get("url_after").and_then(|u| u.as_str()).unwrap_or(&url).to_string();
                let title = out.get("title").and_then(|t| t.as_str()).unwrap_or("");
                lines.push(format!(
                    "opened {where_to}{} in the browser cwoffee is keeping open, and the person \
                     can see it happen in the sidebar.",
                    if title.is_empty() { String::new() } else { format!(" — \"{title}\"") }
                ));
            }
            Err(e) => return ToolResult::err(format!("browse: the kept browser would not open {url}: {e}")),
        }
    } else {
        lines.push(format!(
            "acted in the browser that was already open at {} ({} opened it). Nothing was \
             reloaded: the page is where it was left, including any log in.",
            snap.url,
            snap.opened_by.words()
        ));
    }

    // The page is in the window the person is looking at — unless the panel is closed, which the
    // window itself reports as a zero box. Saying so is the difference between "the person can see
    // this" and a model believing somebody is watching a page nobody opened.
    let now = route.peek();
    if now.native && !now.placed_shown {
        lines.push(
            "the browser panel is closed right now, so this page is open without anyone watching it; \
             the person can open the panel to see it.".to_string(),
        );
    }

    // The steps, in order, each answered for itself. A failed step does not stop the rest —
    // the same rule as the one-shot walk, because a walk that stops at the first missing
    // button tells the model nothing about the page beyond that button.
    let mut failed = 0usize;
    // A browser that has never heard of a step the tool documents is an old browser, not a broken
    // page. Collected rather than printed at once, because a walk of five such steps says one thing.
    let mut unknown: Vec<String> = Vec::new();
    if let Some(steps) = args.get("steps").and_then(|v| v.as_array()) {
        for st in steps {
            let what = st.get("do").and_then(|d| d.as_str()).unwrap_or("?").to_string();
            let target = st.get("target").and_then(|t| t.as_str()).unwrap_or("");
            let mut act = st.clone();
            act["timeout_ms"] = serde_json::json!(20_000);
            match link.act(act, crate::browser_hub::Who::Model).await {
                Ok(out) => {
                    let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if !ok {
                        failed += 1;
                    }
                    let why = out.get("why").and_then(|w| w.as_str()).unwrap_or("");
                    if !ok && does_not_know(why) && VERBS.contains(&what.as_str()) && !unknown.contains(&what) {
                        unknown.push(what.clone());
                    }
                    // How long the step says it actually took. The page measures this — a driver that
                    // answers "ok" for a wait it never performed is otherwise indistinguishable from
                    // one that waited, and that is exactly the bug this line caught.
                    let mut timing = String::new();
                    if let Some(ms) = out.get("waited").and_then(|v| v.as_u64()) {
                        timing.push_str(&format!(", waited {ms}ms"));
                    }
                    if let Some(ms) = out.get("awaited").and_then(|v| v.as_u64()) {
                        timing.push_str(&format!(", awaited the page's promise for {ms}ms"));
                    }
                    lines.push(format!(
                        "step {what}{} {}{timing}",
                        if target.is_empty() { String::new() } else { format!(" \"{target}\"") },
                        if ok { "ok".to_string() } else { format!("did not work: {why}") }
                    ));
                    // A step's answer is text when the driver has text for it. An answer in another
                    // shape — an array of elements, an object of counts — used to be dropped, which
                    // made a step with plenty to say report `ok` and then go silent.
                    if let Some(v) = out.get("value") {
                        let said = match v {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        let said = said.trim();
                        if !said.is_empty() {
                            lines.push(format!("  it gave back: {}", said.replace('\n', " ")));
                        }
                    }
                    if let Some(total) = out.get("total").and_then(|v| v.as_u64()) {
                        let shown = out.get("value").and_then(|v| v.as_str()).map(|s| s.split('\n').count()).unwrap_or(0);
                        if out.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false) {
                            lines.push(format!("  ({total} match this page makes; the first {shown} are listed)"));
                        }
                    }
                    // The driver's own aside about a step: what it matched when several things did, why
                    // it stopped early, that the page did not have the keyboard. Dropping it leaves the
                    // model to infer the caveat from the absence of one.
                    if let Some(n) = out.get("note").and_then(|v| v.as_str()) {
                        lines.push(format!("  {n}"));
                    }
                    // A step that moved the page answered about the page it started in — the old
                    // document is still scriptable while the new one arrives. Say which page the
                    // words above describe, or the model reads a correct `location.href` off a page
                    // that no longer exists and concludes the click failed.
                    if out.get("navigated").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let to = out.get("url_after").and_then(|u| u.as_str()).unwrap_or("");
                        lines.push(format!(
                            "  that step moved the page{}; the answer above came from the page it \
                             started in, and the steps after this one read the new page.",
                            if to.is_empty() { String::new() } else { format!(" to {to}") }
                        ));
                    }
                }
                Err(e) => {
                    failed += 1;
                    lines.push(format!("step {what} never reached the browser: {e}"));
                }
            }
        }
    }
    if let Some(n) = stale_driver_note(&unknown) {
        lines.push(format!("note: {n}"));
    }

    // What the page says now, read from the live page rather than from what the steps imply. But a
    // walk that already asked for the text and the controls gets them once: re-reading both after it
    // doubles a page the model is already being handed, and a long page doubles fast.
    let asked_for: Vec<&str> = args
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|st| st.get("do").and_then(|d| d.as_str())).collect())
        .unwrap_or_default();
    let mut page_text = String::new();
    let mut controls = String::new();
    if !asked_for.contains(&"text") {
        if let Ok(out) = link.act(serde_json::json!({"do": "text"}), crate::browser_hub::Who::Model).await {
            page_text = out.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        }
    }
    if !asked_for.contains(&"snapshot") {
        if let Ok(out) = link.act(serde_json::json!({"do": "snapshot"}), crate::browser_hub::Who::Model).await {
            controls = out.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        }
    }

    let state = link.state().await.unwrap_or_else(|e| serde_json::json!({"ok": false, "why": e}));
    let where_now = state.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    if !where_now.is_empty() {
        lines.push(format!("the page is at {where_now}"));
    }
    if !controls.is_empty() {
        lines.push(format!("controls on the page: {}", controls.replace('\n', "; ")));
    }
    // What the page's own code has said, kept to the document that is up now.
    //
    // Read from the hub's records rather than its flat bags, because the bags are per-session: a walk
    // that clicked through three pages and reloaded one of them was handed fourteen console lines from
    // five documents, in which a working reload appeared to double every error, and nothing in the
    // report said which of them belonged to the page being described. The records from the documents
    // this session has left are not hidden — `page_log` reads them — they are just not part of a
    // report about *this* page.
    let nav_now = state.get("nav").and_then(|v| v.as_i64());
    let diags = link.diagnostics(0, "", "", 400).await.unwrap_or(Value::Null);
    let records: Vec<Value> = diags.get("records").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let here: Vec<&Value> = records
        .iter()
        .filter(|d| match (nav_now, d.get("nav").and_then(|v| v.as_i64())) {
            // A record with no page counter came from a driver that does not stamp one. Keep it:
            // dropping a record nobody labelled is worse than possibly showing a stale one.
            (Some(n), Some(nav)) => nav == n,
            _ => true,
        })
        .collect();
    let older = records.len().saturating_sub(here.len());
    let of_kind = |kind: &str| -> Vec<String> {
        here.iter()
            .filter(|d| d.get("kind").and_then(|k| k.as_str()).unwrap_or("") == kind)
            .map(|d| {
                let text = d.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let at = d.get("at").and_then(|a| a.as_str()).unwrap_or("");
                format!("{text}{}", if at.is_empty() { String::new() } else { format!(" at {at}") })
            })
            .collect()
    };
    let console: Vec<String> = here
        .iter()
        .filter(|d| d.get("kind").and_then(|k| k.as_str()).unwrap_or("") == "console")
        .map(|d| {
            let level = match d.get("level").and_then(|l| l.as_str()).unwrap_or("info") {
                "warning" => "warning",
                "error" => "error",
                "debug" => "debug",
                _ => "log",
            };
            format!("{}: {}", level, d.get("text").and_then(|t| t.as_str()).unwrap_or(""))
        })
        .collect();
    let errs: Vec<String> = of_kind("error");
    let failed_reqs: Vec<String> = here
        .iter()
        .filter(|d| matches!(d.get("kind").and_then(|k| k.as_str()).unwrap_or(""), "network" | "resource"))
        .map(|d| {
            let url = d.get("url").and_then(|u| u.as_str()).unwrap_or("a request");
            let why = d.get("status").and_then(|s| s.as_u64()).map(|s| s.to_string()).unwrap_or_else(|| d.get("text").and_then(|t| t.as_str()).unwrap_or("failed").to_string());
            format!("{url} ({why})")
        })
        .collect();
    let downloads: Vec<String> = of_kind("download");
    if !console.is_empty() {
        lines.push(format!("what the page's own code said: {}", console.join(" | ")));
    }
    if !errs.is_empty() {
        lines.push(format!("uncaught errors in the page's own code: {}", errs.join(" | ")));
    }
    if !failed_reqs.is_empty() {
        lines.push(format!("requests that failed: {}", failed_reqs.join(" | ")));
    }
    if !downloads.is_empty() {
        lines.push(format!(
            "the page downloaded {} on this machine: {}",
            if downloads.len() == 1 { "a file" } else { "files" },
            downloads.join(", ")
        ));
    }
    if older > 0 {
        lines.push(format!(
            "{older} record(s) the page made on the documents this session has already left are not \
             in this report: they belong to earlier pages, not the one above. page_log reads them."
        ));
    }
    let idle_secs = state.get("idle_secs").and_then(|s| s.as_u64()).unwrap_or(0);
    lines.push(format!(
        "the browser stays open at {where_now} for the person to look at; it closes itself \
         after {} of nobody touching it, and it is not a page you can assume will still be \
         there in a later turn.",
        crate::browser_hub::idle_words(idle_secs)
    ));
    if failed > 0 {
        lines.push(format!("{failed} of the steps did not work; the rest of the report is what the page actually did."));
    }
    if !page_text.is_empty() {
        lines.push(format!("the page's own words:\n{page_text}"));
    }

    // Pixels for the model are optional here in a way they are not for the one-shot walk: the
    // person watching the sidebar is already looking at the page. But **asked for** is not the same
    // as **silently skipped**: a call that passed `screenshot: "viewport"` and got neither an image
    // nor a sentence went on asking four more times, which is what this block used to do whenever
    // the conversation's model turned out to have no eyes.
    let mut files = Vec::new();
    let mut images: Vec<crate::provider::Attachment> = Vec::new();
    let asked = args.get("screenshot").and_then(|v| v.as_str()).unwrap_or("");
    let wants_picture = asked != "none" && (cfg.vision == Vision::Yes || !asked.is_empty());
    if wants_picture {
        match link.frame(true).await {
            Ok(f) => {
                let bytes = b64d(f.get("b64").and_then(|b| b.as_str()).unwrap_or(""));
                if bytes.is_empty() {
                    // The browser said yes and sent nothing. When the call *asked* for pixels that
                    // sentence is the answer it needs — without it the model cannot tell "this shape
                    // has no pictures" from "try again", and it tried again. When nobody asked, the
                    // tool description already says a page in the window has no picture of itself,
                    // and repeating it on every call is noise that buries the page's own words.
                    if !asked.is_empty() {
                        lines.push(no_pixels_words(f.get("why").and_then(|v| v.as_str())));
                    }
                } else if bytes.len() > cfg.max_image_bytes {
                    lines.push(format!(
                        "the picture of the page was {} B, more than the {} B a result here carries. The person can still see the page; read it with text, snapshot, a11y or eval.",
                        bytes.len(),
                        cfg.max_image_bytes
                    ));
                } else {
                    let shot_name = format!(
                        "browser/session-{}-{}.png",
                        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
                        std::process::id()
                    );
                    let path = scratch_dir(&ctx.workspace).join(&shot_name);
                    if let Some(parent) = path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    if tokio::fs::write(&path, &bytes).await.is_ok() {
                        files.push(OutputFile::from_path(&ctx.workspace, &path).with_created(true));
                        if cfg.vision == Vision::Yes {
                            images = vec![crate::provider::Attachment {
                                mime: "image/png".to_string(),
                                data_b64: f.get("b64").and_then(|b| b.as_str()).unwrap_or("").to_string(),
                                name: Some("kept browser screenshot".to_string()),
                            }];
                            lines.push(format!("a picture of the page is attached to this result, and saved at .cws/{shot_name} for the person."));
                        } else {
                            // The call asked for pixels this conversation cannot see. The file still
                            // goes to the person, and the sentence says which of the two happened —
                            // otherwise "I asked for a screenshot" reads to the model as "a screenshot
                            // came back".
                            lines.push(format!(
                                "a picture of the page was saved at .cws/{shot_name} for the person; this conversation's model takes text only, so nothing is attached to this result.",
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                if !asked.is_empty() {
                    lines.push(no_pixels_words(Some(&e)));
                }
            }
        }
    }

    let mut body = lines.join("\n");
    let cap = ctx.max_output_bytes.max(512);
    let mut truncated = false;
    if body.len() > cap {
        let mut cut = cap;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        let lost = body.len() - cut;
        body = format!("{}\n… {} B more were cut off this result (the page's words are the part that goes first).", &body[..cut], lost);
        truncated = true;
    }
    ToolResult {
        ok: state.get("alive").and_then(|a| a.as_bool()).unwrap_or(true),
        output: body,
        exit_code: None,
        truncated,
        error: None,
        files,
        // A kept browser's walk writes nothing in the workspace and copies nothing before it
        // does: there is nothing for the turn's Undo card to undo, and saying so with empty
        // lists is what stops the card appearing with a phantom file in it.
        changes: Vec::new(),
        checkpoints: Vec::new(),
        activates: Vec::new(),
        images,
    }
}

/// One sentence for one missing picture.
///
/// A kept page that is already on screen has no picture of itself, and a browser that answers with
/// no bytes is not an error — but silence is. A model given neither pixels nor a reason asks for
/// pixels again; three calls in one transcript did exactly that. The sentence names the reason the
/// browser gave and the way that does work in this shape.
fn no_pixels_words(why: Option<&str>) -> String {
    format!(
        "no picture of the page: {}. The page is on screen for the person and there are no pixels for you here: read it with text, snapshot, a11y or eval instead of asking again.",
        why.unwrap_or("the browser answered with no pixels")
    )
}

fn model_image(out: &Value, cfg: &BrowserCfg, fallback: &std::path::Path) -> Vec<crate::provider::Attachment> {
    if cfg.vision != Vision::Yes {
        return Vec::new();
    }
    let named = out.get("screenshot").and_then(|s| s.get("path")).and_then(|p| p.as_str()).map(PathBuf::from);
    let path = named.as_deref().unwrap_or(fallback);
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes.len() > cfg.max_image_bytes {
        return Vec::new();
    }
    vec![crate::provider::Attachment {
        mime: "image/png".to_string(),
        data_b64: b64(&bytes),
        name: Some("browse screenshot".to_string()),
    }]
}

/// What the model reads. The order is deliberate: the verdict line first (so a truncated
/// result still tells the truth about what happened), then what the page's own code said -
/// which is the reason this tool exists - then the page's words, then the picture's fate.
fn asked_none(args: &Value) -> bool {
    args.get("screenshot").and_then(|v| v.as_str()) == Some("none")
}

fn render(
    out: &Value,
    args: &Value,
    ctx: &ToolCtx,
    cfg: &BrowserCfg,
    shot_mode: &str,
    headed: bool,
    why: &str,
    shot_path: &std::path::Path,
    shot_rel: &str,
) -> String {
    let s = |k: &str| out.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |k: &str| out.get(k).and_then(|v| v.as_u64());
    let mut lines: Vec<String> = Vec::new();

    let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let status = out.get("status").and_then(|v| v.as_u64());
    let title = s("title");
    lines.push(format!(
        "{} {}{}",
        if ok { "opened" } else { "could not open" },
        s("url"),
        if ok {
            match status {
                Some(c) => format!(" ({c}{})", if title.is_empty() { String::new() } else { format!(", {:?}", title) }),
                None => String::new(),
            }
        } else {
            format!(": {}", s("why"))
        }
    ));
    let after = s("url_after");
    if ok && !after.is_empty() && after != s("url") {
        lines.push(format!("the page moved to {after}"));
    }
    lines.push(format!(
        "browser: {} — {why}",
        if headed { "headed (a real window)" } else { "headless" }
    ));
    if let Some(note) = out.get("note").and_then(|v| v.as_str()) {
        lines.push(format!("note: {note}"));
    }

    let steps = out.get("steps").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !steps.is_empty() {
        let said: Vec<String> = steps
            .iter()
            .map(|st| {
                let do_what = st.get("do").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let target = st.get("target").and_then(|v| v.as_str());
                let status = st.get("status").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let value = st.get("value").and_then(|v| v.as_str());
                let note = st.get("note").and_then(|v| v.as_str());
                // The time a step says it took, measured by the driver. It is in the report because a
                // step that reports `ok` for a wait it never performed is otherwise indistinguishable
                // from one that waited, and the model has no clock of its own to notice.
                let waited = st.get("waited").and_then(|v| v.as_u64());
                format!(
                    "{do_what}{} {status}{}{}{}",
                    target.map(|t| format!(" {t:?}")).unwrap_or_default(),
                    waited.map(|ms| format!(" (waited {ms}ms)")).unwrap_or_default(),
                    note.map(|n| format!(" ({n})")).unwrap_or_default(),
                    value.map(|v| format!(" -> {v:?}")).unwrap_or_default()
                )
            })
            .collect();
        lines.push(format!("steps: {}", said.join("; ")));
        // The same old-install guard the kept-browser path gives: a browser beside this binary that
        // has never heard of a documented step is an install that moved the app and left the driver.
        let mut unknown: Vec<String> = Vec::new();
        for st in &steps {
            let do_what = st.get("do").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let status = st.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if does_not_know(status) && VERBS.contains(&do_what.as_str()) && !unknown.contains(&do_what) {
                unknown.push(do_what);
            }
        }
        if let Some(n) = stale_driver_note(&unknown) {
            lines.push(format!("note: {n}"));
        }
    }

    let errs = out.get("page_errors").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let con = out.get("console").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let failed = out.get("requests_failed").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let blocked = out.get("downloads_blocked").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !errs.is_empty() {
        lines.push(format!(
            "uncaught errors in the page's own code ({}): {}",
            errs.len(),
            errs.iter().map(|e| e.as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" | ")
        ));
    }
    let noisy: Vec<String> = con
        .iter()
        .filter_map(|c| {
            let t = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let text = c.get("text").and_then(|v| v.as_str()).unwrap_or("");
            matches!(t, "error" | "warning" | "assert").then(|| format!("{t}: {text}"))
        })
        .collect();
    if !noisy.is_empty() {
        lines.push(format!("the page logged problems ({}): {}", noisy.len(), noisy.join(" | ")));
    }
    if !failed.is_empty() {
        lines.push(format!(
            "requests that failed: {}",
            failed
                .iter()
                .map(|f| format!("{} ({})", f.get("url").and_then(|v| v.as_str()).unwrap_or("?"), f.get("why").and_then(|v| v.as_str()).unwrap_or("failed")))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    if !blocked.is_empty() {
        lines.push(format!(
            "the page tried to download {}, which the browser tool declines: {}",
            if blocked.len() == 1 { "a file" } else { "files" },
            blocked.iter().map(|b| b.as_str().unwrap_or("?").to_string()).collect::<Vec<_>>().join(", ")
        ));
    }

    let snap = out.get("snapshot").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !snap.is_empty() {
        lines.push(format!(
            "controls on the page: {}",
            snap.iter().map(|x| x.as_str().unwrap_or("")).collect::<Vec<_>>().join("; ")
        ));
    }

    match shot_mode {
        "none" => lines.push(format!(
            "no screenshot: {} (the browser ran {})",
            // Three states, three sentences. Saying "this model has no image input" when the
            // endpoint never spoke would state a fact nobody established, and the reason the
            // picture is missing is exactly the thing the model might act on next.
            match (asked_none(args), cfg.vision) {
                (true, _) => "you asked for none".to_string(),
                (false, Vision::Yes) => "the walk asked for none".to_string(),
                (false, Vision::No) => "this model has no image input".to_string(),
                (false, Vision::Unknown) => {
                    "the endpoint never stated the model's inputs, so no pixels were sent"
                        .to_string()
                }
            },
            if headed { "headed" } else { "headless" }
        )),
        other => {
            let got = out.get("screenshot").and_then(|v| v.get("path")).and_then(|p| p.as_str());
            match got {
                Some(_) => {
                    let bytes = out.get("screenshot").and_then(|v| v.get("bytes")).and_then(|b| b.as_u64()).unwrap_or(0);
                    if cfg.vision == Vision::Yes {
                        lines.push(format!("the {other} screenshot ({bytes} B) is attached to this result as an image."));
                    } else {
                        lines.push(format!(
                            "the {other} screenshot ({bytes} B) is saved at .cws/{shot_rel} for the person to open; \
                             it is not in this result because this model has no image input."
                        ));
                    }
                }
                None => lines.push(format!(
                    "the screenshot did not happen: {}",
                    out.get("screenshot").and_then(|v| v.get("why")).and_then(|v| v.as_str()).unwrap_or("no reason given")
                )),
            }
            let _ = shot_path;
        }
    }

    let text = s("text");
    if !text.is_empty() {
        lines.push(if out.get("text_truncated").and_then(|v| v.as_bool()).unwrap_or(false) {
            format!("the page's own words (its own report, capped by the driver):\n{text}")
        } else {
            format!("the page's own words:\n{text}")
        });
    }
    let timing = n("total_ms");
    if let Some(ms) = out.get("timings").and_then(|t| t.get("total_ms")).and_then(|v| v.as_u64()) {
        lines.push(format!("the walk took {ms} ms"));
    }
    let _ = timing;

    // Truncated here, by the tool, with the amount lost named. A model that reads half a
    // page and is not told it got half will answer as if it read the page.
    let mut body = lines.join("\n");
    let cap = ctx.max_output_bytes.max(512);
    if body.len() > cap {
        let mut cut = cap;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        let lost = body.len() - cut;
        body = format!("{}\n… {} B more were cut off this result (the page's words are the part that goes first).", &body[..cut], lost);
    }
    body
}

/// The page's own record of what it printed, threw and failed to load.
///
/// A separate tool rather than a field on `browse`, because the thing it is for is *not* being
/// inside the page: a model that has to re-open or re-read a page to find out whether it broke
/// stops checking as soon as the page is big. The hub collects these as they happen, so this call
/// costs the page nothing and does not count as touching it.
pub struct PageLogTool;

#[async_trait::async_trait]
impl Tool for PageLogTool {
    fn name(&self) -> &str {
        "page_log"
    }

    fn description(&self) -> &str {
        "Read what the kept page has been saying while it sat there: its console messages, its own \
         uncaught exceptions (with where in the page they happened), requests that came back an \
         error or never finished, resources that failed to load, navigations and downloads. The \
         page is watched the whole time it is open, so nothing has to be switched on first and \
         reading this does not touch the page, move it, or keep it alive. Pass `since` with the \
         `newest` id from the last reply to get only what happened after you last looked; `kind` \
         console|error|network|resource|navigation|download and `level` info|warning|error narrow \
         it. The reply also carries a `health` summary - counts per kind and how many are failures - \
         which is usually enough to decide whether the records are worth reading. A page that has \
         nothing to report and a session that was never open are two different answers."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "since": { "type": "integer", "description": "The newest record id you have already seen. Only records after it come back." },
                "kind": { "type": "string", "enum": ["console", "error", "network", "resource", "navigation", "download"], "description": "One kind only. Omit for all of them." },
                "level": { "type": "string", "enum": ["info", "warning", "error"], "description": "One level only. Omit for all of them." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 240, "description": "Newest records to return. Default 60." }
            },
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        // Harmless, like `recall`. It reaches nothing the model is not already shown: the same
        // lines come back in the `browse` report automatically. Gating a read of a log that is
        // already in the app's memory would buy the person nothing and cost them the only
        // diagnostics path the model will actually use.
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &super::ExecSink) -> ToolResult {
        let Some(link) = ctx.browser_route.as_ref().and_then(|r| r.link()) else {
            // "There is no log" and "there is nothing that could have made one" are different
            // facts, and a model told the first will go looking for a page that never existed.
            return ToolResult::err(
                "there is no kept browser in this start, so there is nothing to read: no page has \
                 been open to make a record. `browse` without `session` opens a page for one call \
                 and reports what it said in the result of that call.",
            );
        };
        let since = args.get("since").and_then(Value::as_i64).unwrap_or(0);
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let level = args.get("level").and_then(Value::as_str).unwrap_or("");
        let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(60).clamp(1, 240) as usize;
        let got = match link.diagnostics(since, kind, level, limit).await {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("the page's log could not be read: {e}")),
        };
        ToolResult::ok(diagnostics_said(&got, since, &kind, &level))
    }
}

/// The records as a model should see them: one line each, newest last, with the numbers that make
/// a follow-up read possible. JSON straight at a model invites it to quote the payload back;
/// sentences it can read in order, and the ids it needs are still there.
fn diagnostics_said(v: &Value, since: i64, kind: &str, level: &str) -> String {
    let alive = v.get("alive").and_then(Value::as_bool).unwrap_or(false);
    let held = v.get("held").and_then(Value::as_u64).unwrap_or(0);
    let newest = v.get("newest").and_then(Value::as_u64).unwrap_or(0);
    let nav = v.get("nav").and_then(Value::as_u64).unwrap_or(0);
    let url = v.get("url").and_then(Value::as_str).unwrap_or("");
    let health = v.get("health").cloned().unwrap_or_else(|| json!({}));
    let mut lines = Vec::new();
    lines.push(format!(
        "{}{}",
        if alive {
            format!("the page at {url} (page {nav} of this session)")
        } else {
            "no page is open now".to_string()
        },
        if held == 0 && newest == 0 {
            " — it has said nothing at all since it opened".to_string()
        } else {
            String::new()
        }
    ));
    let mut counts: Vec<String> = health
        .get("counts")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, n)| format!("{k} {n}")).collect())
        .unwrap_or_default();
    counts.sort();
    if !counts.is_empty() {
        lines.push(format!(
            "held: {} ({}). newest id {newest}; pass it as `since` to read only what comes after.",
            counts.join(", "),
            health
                .get("failed_total")
                .and_then(Value::as_u64)
                .map(|f| format!("{f} of them are things that went wrong"))
                .unwrap_or_else(|| "none of them a failure".to_string())
        ));
    }
    if let Some(recs) = v.get("records").and_then(Value::as_array) {
        for r in recs.iter().rev() {
            let seq = r.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("?");
            let level = r.get("level").and_then(Value::as_str).unwrap_or("?");
            let text = r.get("text").and_then(Value::as_str).unwrap_or("");
            let mut extra = Vec::new();
            if let Some(u) = r.get("url").and_then(Value::as_str) {
                extra.push(u.to_string());
            }
            if let Some(m) = r.get("method").and_then(Value::as_str) {
                extra.push(m.to_string());
            }
            if let Some(s) = r.get("status").and_then(Value::as_i64) {
                if s > 0 {
                    extra.push(format!("status {s}"));
                }
            }
            if let Some(a) = r.get("at").and_then(Value::as_str) {
                extra.push(format!("at {a}"));
            }
            // Which page it belongs to, and who noticed it. The app records a page arriving even
            // when the page itself said nothing, and a model that cannot tell the two apart reads
            // "no records" as "nothing happened".
            let who = if r.get("source").and_then(Value::as_str).unwrap_or("") == "hub" {
                ", the app noticed this, not the page"
            } else {
                ""
            };
            let where_now = if r.get("on_this_page").and_then(Value::as_bool).unwrap_or(false) {
                format!("on this page{who}")
            } else {
                format!("before the last navigation{who}")
            };
            lines.push(format!(
                "#{seq} {level} {kind}: {text}{} [{}s ago, {where_now}]",
                if extra.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", extra.join(" "))
                },
                r.get("ago_secs").and_then(Value::as_i64).unwrap_or(0)
            ));
        }
        let returned = recs.len();
        if returned == 0 {
            // Three different noes, and a model has to be able to tell them apart: "nothing of that
            // kind" otherwise reads as "the page is quiet", when the page is shouting about something
            // else entirely.
            lines.push(if !kind.is_empty() || !level.is_empty() {
                let what = match (kind.is_empty(), level.is_empty()) {
                    (false, false) => format!("kind `{kind}` at level `{level}`"),
                    (false, true) => format!("kind `{kind}`"),
                    _ => format!("level `{level}`"),
                };
                format!(
                    "nothing of {what} among the {held} record(s) held (newest id {newest}) — ask \
                     again without the filter to see what the page did say."
                )
            } else if since > 0 && since >= newest as i64 {
                format!("nothing since id {since} — that is the newest one, so nothing has happened since")
            } else {
                "this page has said nothing at all since it opened".to_string()
            });
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Risk, Tool, ToolCtx};
    use serde_json::json;

    fn sink() -> crate::tools::ExecSink {
        tokio::sync::mpsc::unbounded_channel().0
    }

    

    // --- the kept browser (`session: true`) -------------------------------------------

    /// What a refusal actually said. `ToolResult::err` puts the sentence in `error`, not
    /// `output`, and a test that reads the wrong field passes on an empty string.
    fn refusal(out: &ToolResult) -> String {
        format!("{} {}", out.output, out.error.clone().unwrap_or_default())
    }

    fn fake_session_driver() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../../scripts/fake-browser-session.mjs");
        p.canonicalize().expect("scripts/fake-browser-session.mjs exists")
    }

    /// A mounted hub plus its state mirror, taken before the hub goes behind a lock — which
    /// is the same order the app's own startup uses.
    fn session_pair() -> (
        std::sync::Arc<tokio::sync::Mutex<crate::browser_hub::Hub>>,
        std::sync::Arc<std::sync::RwLock<crate::browser_hub::Snapshot>>,
    ) {
        let hub = crate::browser_hub::Hub::mount(crate::browser_hub::SessionCfg {
            session_driver: Some(fake_session_driver()),
            node: std::env::var("CWOFFEE_NODE").unwrap_or_else(|_| "node".to_string()),
            idle_secs: 300,
            headed: Some(false),
        native: None,
            ..Default::default()
        });
        let shared = hub.shared();
        (std::sync::Arc::new(tokio::sync::Mutex::new(hub)), shared)
    }

    fn session_ctx() -> (
        ToolCtx,
        std::sync::Arc<tokio::sync::Mutex<crate::browser_hub::Hub>>,
        std::sync::Arc<std::sync::RwLock<crate::browser_hub::Snapshot>>,
    ) {
        let (hub, shared) = session_pair();
        let mut ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 8192,
            ..Default::default()
        };
        ctx.browser.driver = None;
        ctx.browser.keep = true;
        ctx.browser_route = Some(BrowserRoute::in_process(std::sync::Arc::clone(&hub), std::sync::Arc::clone(&shared)));
        (ctx, hub, shared)
    }

    /// A page living in cwoffee's **own window**: the same three methods the app's page view
    /// answers, no child process and no pipes. Attaching one is what makes the start a start with a
    /// browser of its own, which is the only thing `window_browser` can read.
    #[derive(Default)]
    struct WindowPage {
        out: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    }

    impl WindowPage {
        fn emit(&self, v: Value) {
            if let Some(tx) = self.out.lock().unwrap().as_ref() {
                let _ = tx.send(v.to_string());
            }
        }
    }

    impl crate::browser_hub::NativeDriver for WindowPage {
        fn attach(&self, out: tokio::sync::mpsc::UnboundedSender<String>) {
            *self.out.lock().unwrap() = Some(out);
        }
        fn send(&self, line: &str) {
            let Ok(cmd) = serde_json::from_str::<Value>(line) else { return };
            let id = cmd.get("id").cloned().unwrap_or(Value::Null);
            let url = cmd
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("http://127.0.0.1:9/page")
                .to_string();
            match cmd.get("cmd").and_then(|c| c.as_str()).unwrap_or("") {
                "open" => {
                    self.emit(json!({"ev": "nav", "url": url, "title": "A page"}));
                    self.emit(json!({"id": id, "ok": true, "url_after": url, "title": "A page"}));
                }
                "act" => self.emit(json!({"id": id, "ok": true, "url_after": url, "value": "what the page said"})),
                "state" => self.emit(json!({"id": id, "ok": true, "alive": true, "url": url, "title": "A page"})),
                _ => self.emit(json!({"id": id, "ok": false, "why": "the page view does not do that"})),
            }
        }
        fn quit(&self) {}
    }

    #[tokio::test]
    async fn a_start_with_a_page_of_its_own_opens_no_other_browser() {
        // The complaint this exists for: asked to "use the embedded browser", the model got a
        // Playwright window on the desktop instead of the page in the sidebar.
        //
        // `driver` is None in this ctx, so a walk of the old kind could not have run at all — the
        // report is the proof of which browser the call reached.
        let (ctx, hub, _shared) = session_ctx();
        hub.lock().await.set_native(std::sync::Arc::new(WindowPage::default()));
        let out = BrowseTool.run(json!({"url": "http://127.0.0.1:9/page"}), &ctx, &sink()).await;
        let said = out.output.clone();
        assert!(
            out.ok && said.contains("keeping open"),
            "a call that never mentioned `session` reached the page in the window: {said}{}",
            out.error.clone().unwrap_or_default()
        );
        assert!(hub.lock().await.peek().opened, "and it is the window's page that moved");
    }

    #[tokio::test]
    async fn only_a_call_that_says_so_behind_the_sidebar_s_back_walks_elsewhere() {
        let (ctx, hub, _shared) = session_ctx();
        hub.lock().await.set_native(std::sync::Arc::new(WindowPage::default()));
        // `session: false` is the named exception, and it is not quietly redirected: it goes looking
        // for a browser of its own, which here means the missing-driver answer rather than the page
        // somebody else is looking at.
        let out = BrowseTool.run(json!({"url": "http://127.0.0.1:9/page", "session": false}), &ctx, &sink()).await;
        let why = out.error.clone().unwrap_or_default();
        assert!(!out.ok, "a throwaway walk did not borrow the person's page: {out:?}");
        assert!(why.contains("driver"), "it says the browser it needed is missing: {why}");
        assert!(!hub.lock().await.peek().opened, "and the page in the window was never touched");
    }

    #[tokio::test]
    async fn a_call_asked_of_a_window_page_is_a_decision_about_that_page() {
        // A yes given to "browse for this conversation" was said about a browser that opened and
        // closed inside one call. It cannot cover a call that clicks around inside the page a person
        // is watching, so the routing has to make the call ask again rather than inherit the grant.
        let (ctx, hub, _shared) = session_ctx();
        hub.lock().await.set_native(std::sync::Arc::new(WindowPage::default()));
        let call = json!({"url": "http://127.0.0.1:9/page", "steps": [{"do": "click", "target": "go"}]});
        let walked = BrowseTool.risk_for(&call, &ctx);
        assert!(
            walked.per_call,
            "a walk inside the window's page is not covered by a yes given to a browser that has gone"
        );
        // The same call in a start with no page of its own is the ordinary case, and the ordinary
        // case stays what `once` was said to be.
        let (plain, _hub, _s) = session_ctx();
        let off = BrowseTool.risk_for(&call, &plain);
        assert!(!off.per_call, "a one-off walk in a start with no window is still a per-conversation yes");
        // And naming the exception does not make it ask twice: the throwaway walk is the case the
        // grant was written for.
        let aside = BrowseTool.risk_for(&json!({"url": "http://127.0.0.1:9/page", "session": false}), &ctx);
        assert!(!aside.per_call, "`session: false` is the walk the conversation's yes already covers");
    }

    #[tokio::test]
    async fn the_model_reads_what_the_page_said_without_touching_the_page_or_its_clock() {
        let (ctx, hub, _shared) = session_ctx();
        BrowseTool
            .run(json!({"url": "http://127.0.0.1:9/page", "session": true}), &ctx, &sink())
            .await;
        let out = PageLogTool.run(json!({}), &ctx, &sink()).await;
        let said = out.output;
        assert!(said.contains("the page warned about something"), "the page's own warning is there: {said}");
        assert!(said.contains("404"), "and so is the request that came back an error, with its status: {said}");
        assert!(said.contains("network"), "the kind is said, so a shout and a failure are not the same line: {said}");
        assert!(said.contains("newest id"), "and the reader is told what to pass next: {said}");

        // Reading a page's log is not using the page. If it were, a model that checks on a page
        // could keep a person's window open forever without anyone touching it.
        let before = hub.lock().await.peek();
        let (deadline, actor, newest) = (before.idle_deadline, before.last_actor, before.diag_seq);
        let again = PageLogTool.run(json!({"since": newest}), &ctx, &sink()).await;
        let after = hub.lock().await.peek();
        assert_eq!(after.idle_deadline, deadline, "the clock was not reset by the read");
        assert_eq!(after.last_actor, actor, "and the read did not become 'the last thing done to the page'");
        assert!(again.output.contains("nothing since id"), "so the reader is told which id it is caught up with: {}", again.output);
        // And a filter that matches nothing says that, rather than sounding like a quiet page.
        let none = PageLogTool.run(json!({"kind": "download"}), &ctx, &sink()).await;
        assert!(
            none.output.contains("nothing of kind `download`") && none.output.contains("ask again without the filter"),
            "an empty filter is not the same answer as a silent page: {}",
            none.output
        );
        assert!(
            said.contains("the app noticed this, not the page"),
            "the app's own record of the page arriving is labelled as the app's: {said}"
        );
    }

    #[tokio::test]
    async fn a_log_read_with_no_page_says_why_rather_than_that_nothing_happened() {
        let ctx = ToolCtx::default();
        let out = PageLogTool.run(json!({}), &ctx, &sink()).await;
        let said = refusal(&out);
        assert!(said.contains("no kept browser"), "{said}");
        assert!(said.contains("without `session`"), "and it points at the way a page does exist: {said}");
    }

    #[tokio::test]
    async fn a_log_read_narrows_to_the_one_kind_a_model_cares_about() {
        let (ctx, _hub, _shared) = session_ctx();
        BrowseTool
            .run(json!({"url": "http://127.0.0.1:9/page", "session": true}), &ctx, &sink())
            .await;
        let out = PageLogTool.run(json!({"kind": "network"}), &ctx, &sink()).await;
        assert!(out.output.contains("404"), "the failed request is there: {}", out.output);
        assert!(!out.output.contains("the page warned"), "the console line is not: {}", out.output);
    }

    #[tokio::test]
    async fn a_session_call_is_asked_for_every_time_rather_than_granted_once() {
        let ctx = ToolCtx::default();
        let plain = BrowseTool.risk_for(&json!({ "url": "http://127.0.0.1:9/x" }), &ctx);
        assert_eq!(plain.per_call, false, "a throwaway walk is what \"yes to browse for this conversation\" was about");
        let kept = BrowseTool.risk_for(&json!({ "url": "http://127.0.0.1:9/x", "session": true }), &ctx);
        assert_eq!(kept.per_call, true, "clicking inside somebody's open window is a fresh decision each time");
        assert_eq!(kept.level, Risk::Sensitive);
    }

    #[test]
    fn the_browse_card_says_which_browser_and_that_it_stays_open() {
        let plain = crate::approval::summarize_with("browse", &json!({"url": "http://x/y", "steps": [{"do": "click", "target": "More"}]}), &[]);
        assert!(plain.contains("throwaway") && plain.contains("closes when the call ends"), "{plain}");
        assert!(plain.contains("click \"More\""), "the card should say what it will click: {plain}");
        let kept = crate::approval::summarize_with("browse", &json!({"url": "http://x/y", "session": true}), &[]);
        assert!(kept.contains("KEPT browser"), "{kept}");
        assert!(kept.contains("stays open"), "{kept}");
        assert!(kept.contains("may be theirs"), "the fact that changes the answer: {kept}");
    }

    /// A step that waits has to say how long it spent waiting, and a step the page annotated has to
    /// pass the annotation through. Both come from the driver's side; the tool's job is not to lose
    /// them — because without them, "wait_for \"Run\" ok" for a page that took four seconds to produce
    /// the button and "wait_for \"Run\" ok" for one that never did are the same sentence.
    #[tokio::test]
    async fn a_waiting_step_says_what_it_spent_waiting() {
        let (ctx, _hub, _shared) = session_ctx();
        let out = BrowseTool
            .run(
                json!({
                    "url": "http://127.0.0.1:9/selftest-page.html",
                    "session": true,
                    "steps": [
                        { "do": "wait", "ms": 120 },
                        { "do": "wait_for", "target": "More" },
                        { "do": "wait_for", "target": "A button that never arrives", "ms": 400 },
                        { "do": "click", "target": "Download invoice" },
                        { "do": "health" }
                    ]
                }),
                &ctx,
                &sink(),
            )
            .await;
        let o = &out.output;
        assert!(out.ok, "{o}");
        assert!(o.contains("step wait ok, waited "), "a wait reports the time it took, not the time it asked for: {o}");
        assert!(o.contains("step wait_for \"More\" ok, waited "), "a wait that found its thing says how long it looked: {o}");
        assert!(
            o.contains("looked for") && o.contains("and it never appeared"),
            "a wait that gave up says for how long, in its own sentence: {o}"
        );
        assert!(
            o.contains("this is a download link (invoice.pdf)"),
            "the page's own explanation of a click it cannot report on survives the trip: {o}"
        );
        assert!(
            o.contains("complete at http://127.0.0.1:9/selftest-page.html"),
            "health arrives as something a model can read, not as a bag of fields: {o}"
        );
    }

    /// One verb list, written in four files: the schema a model reads, and the three browsers that
    /// answer it. A step advertised by the schema and unknown to a browser is a model being told the
    /// *page* failed when the real answer is that this tree is out of step with itself, so the
    /// comparison happens here — against the driver sources in this checkout.
    #[test]
    fn every_step_the_tool_offers_is_a_step_every_browser_knows() {
        let read = |rel: &str| {
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
                .unwrap_or_else(|e| panic!("{rel} is not readable from this test: {e}"))
        };
        let browsers = [
            ("the one-shot walk", read("../../browser/driver.mjs")),
            ("the kept browser", read("../../browser/session.mjs")),
            ("the page in the window", read("../../crates/tauri-app/src/page_bridge.js")),
        ];
        // The schema is what a model is told it may ask for. If that list and `VERBS` drift, this
        // check would be grading the wrong sentence, so it grades both.
        let schema = BrowseTool.schema();
        let listed = schema["properties"]["steps"]["items"]["properties"]["do"]["enum"]
            .as_array()
            .expect("the steps' `do` is an enum")
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            VERBS.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
            "the schema's step list and the list this file checks are one list"
        );
        for (which, src) in browsers {
            for verb in VERBS {
                // Each browser dispatches its steps on the quoted verb name, which is the one shape
                // a missing branch cannot accidentally satisfy.
                let quoted = format!("'{verb}'");
                assert!(src.contains(&quoted), "{which} has no branch for `{verb}`, which the schema advertises");
            }
        }
    }

    /// The same drift one install away from being silent: the app moved, the Node driver beside it
    /// did not. The browser's own words are `I do not know the step "text"`, which reads like a
    /// verdict on the page. Say which half is old.
    #[test]
    fn a_browser_that_has_never_heard_of_a_step_is_named_as_the_old_half() {
        let out = json!({
            "ok": true, "url": "http://x/y", "url_after": "http://x/y", "status": 200, "title": "t",
            "steps": [
                { "do": "text", "status": "I do not know the step \"text\"" },
                { "do": "a11y", "status": "I do not know the act \"a11y\"" },
                { "do": "click", "target": "go", "status": "ok", "note": "the walk is now at http://x/y" }
            ]
        });
        let said = render(
            &out,
            &json!({}),
            &ToolCtx::default(),
            &BrowserCfg::default(),
            "none",
            false,
            "this model takes text only",
            std::path::Path::new("/tmp/shot.png"),
            ".shot.png",
        );
        assert!(said.contains("`text`") && said.contains("`a11y`"), "both steps it has never heard of are named: {said}");
        assert!(said.contains("install-desktop.sh"), "and the sentence says the driver is the old half: {said}");
        assert!(said.contains("the walk is now at http://x/y"), "a step's own aside survives: {said}");
    }

    #[tokio::test]
    async fn a_session_walk_reports_the_page_the_person_is_looking_at() {
        let (ctx, hub, shared) = session_ctx();
        let out = BrowseTool
            .run(
                json!({
                    "url": "http://127.0.0.1:9/selftest-page.html",
                    "session": true,
                    "steps": [
                        {"do": "click", "target": "More"},
                        {"do": "click", "target": "Maybe"},
                        {"do": "eval", "code": "1+1"}
                    ]
                }),
                &ctx,
                &sink(),
            )
            .await;
        let o = &out.output;
        assert!(out.ok, "{o}");
        assert!(o.contains("opened http://127.0.0.1:9/selftest-page.html"), "{o}");
        assert!(o.contains("the person \n                     can see it happen") || o.contains("can see it happen"), "the model is told this is a visible browser: {o}");
        assert!(o.contains("step click \"More\" ok"), "{o}");
        assert!(o.contains("step click \"Maybe\" did not work"), "{o}");
        assert!(o.contains("it gave back: 5"), "an eval's value comes back: {o}");
        assert!(o.contains("controls on the page: button \"More\""), "{o}");
        assert!(o.contains("the page's own words"), "{o}");
        assert!(o.contains("Cwoffee session selftest"), "{o}");
        // The console line came from the session's own bookkeeping, which the driver pushed
        // as an event — not from a read the tool had to ask for.
        assert!(o.contains("undefined.x is not a function"), "the page's own complaint reached the walk: {o}");
        // And the walk ends by saying what happens to the browser, because a model that
        // assumes the page will still be there next turn will plan around something that is
        // gone.
        assert!(o.contains("closes itself after 5 minutes"), "{o}");
        assert!(o.contains("1 of the steps did not work"), "{o}");
        // The session itself recorded the model, not the person.
        let snap = crate::browser_hub::Hub::peek_shared(&shared);
        assert_eq!(snap.last_actor, crate::browser_hub::Who::Model);
        hub.lock().await.close("test over").await;
    }

    #[tokio::test]
    async fn a_session_walk_with_no_kept_browser_says_what_to_do_instead() {
        let mut ctx = ToolCtx::default();
        ctx.browser.keep = true;
        let out = BrowseTool.run(json!({"url": "http://x/y", "session": true}), &ctx, &sink()).await;
        assert!(!out.ok);
        let said = refusal(&out);
        assert!(said.contains("no kept browser"), "{said}");
        assert!(said.contains("session"), "it has to name the argument that failed: {said}");

        // Switched off by a person is a different answer from not mounted.
        let mut ctx = ToolCtx::default();
        ctx.browser.keep = false;
        ctx.browser_route = Some(BrowserRoute::default());
        let out = BrowseTool.run(json!({"url": "http://x/y", "session": true}), &ctx, &sink()).await;
        assert!(!out.ok);
        let said = refusal(&out);
        assert!(said.contains("switched off"), "{said}");
        assert!(said.contains("keep = false"), "{said}");
    }

    #[tokio::test]
    async fn a_walk_in_a_closed_session_names_the_state_rather_than_inventing_a_page() {
        let (mut ctx, hub, shared) = session_ctx();
        // A route whose mirror says nothing is open: the hub is mounted, the browser is not.
        ctx.browser_route = Some(BrowserRoute::in_process(std::sync::Arc::clone(&hub), std::sync::Arc::clone(&shared)));
        let out = BrowseTool.run(json!({"session": true}), &ctx, &sink()).await;
        assert!(!out.ok);
        let said = refusal(&out);
        assert!(said.contains("no url") && said.contains("already open"), "{said}");
        assert!(said.contains("No kept browser is open"), "{said}");
        // The claim is about state, not about a failure it did not have.
        assert!(!said.contains("crashed"), "{said}");
    }

    #[test]
    fn a_throwaway_walk_says_someone_elses_browser_is_already_open() {
        let (hub, shared) = session_pair();
        let route = BrowserRoute::in_process(hub, shared);
        let ctx = ToolCtx { browser_route: Some(route.clone()), ..Default::default() };
        // Nothing open yet: no note, because there is nothing to confuse.
        assert!(other_session_note(&ctx).is_none());
        *route.shared.write().unwrap() = crate::browser_hub::Snapshot {
            alive: true,
            opened: true,
            url: "http://127.0.0.1:9/app".to_string(),
            opened_by: crate::browser_hub::Who::Person,
            ..Default::default()
        };
        let note = other_session_note(&ctx).expect("a note");
        assert!(note.contains("http://127.0.0.1:9/app"), "{note}");
        assert!(note.contains("you opened it"), "{note}");
        assert!(note.contains("throwaway browser"), "{note}");
        assert!(note.contains("\"session\": true"), "and it says how to use the right one: {note}");
    }

    #[test]
    fn the_picture_decoder_puts_the_bytes_back() {
        // The session's picture arrives as base64 over a pipe; the decoder is hand-written
        // beside the encoder, so it gets the round trip rather than a hope.
        for len in [0usize, 1, 2, 3, 4, 5, 17, 256, 1000] {
            let bytes: Vec<u8> = (0..len).map(|i| ((i * 7 + 3) % 256) as u8).collect();
            assert_eq!(b64d(&b64(&bytes)), bytes, "round trip at {len} bytes");
        }
        assert_eq!(b64d("AAEC\n"), vec![0, 1, 2], "a newline in a pipe is not a corrupt picture");
        assert!(b64d("!!!").is_empty(), "junk decodes to nothing rather than to something plausible");
    }

    #[test]
    fn the_model_cannot_ask_for_a_window() {
        // The whole safety claim about headed mode rests on this: there is no field in the
        // schema that lets a call ask for a visible browser. If one is ever added, the
        // sentence in `description` ("whether pixels come back depends on whether this
        // conversation's model can see images") stops being the whole rule, and a model gets
        // to decide that a window appears on a person's desktop.
        let schema = base();
        let props = schema["properties"].as_object().unwrap();
        for k in props.keys() {
            assert_ne!(k, "headed", "a `headed` argument would let the model choose to paint a window");
            assert_ne!(k, "headless", "same door, other spelling");
        }
        // And the picture is the only way it can ask about pixels.
        assert!(props.contains_key("screenshot"));
    }

    #[test]
    fn a_persons_headed_choice_outranks_the_models_eyes() {
        // The order is the point: `false` from the config means no window whatever the model
        // can do, and `true` means a window even for a model that will never see it — that is
        // a person watching over the model's shoulder, and it is allowed to be deliberate.
        let mut c = BrowserCfg { vision: Vision::Yes, ..Default::default() };
        c.headed_override = Some(false);
        let (headed, why, _) = decide(&c, "", false);
        assert!(!headed, "a person's no outranks a seeing model");
        assert!(why.contains("headed = false"), "{why}");

        let mut c = BrowserCfg { vision: Vision::No, ..Default::default() };
        c.headed_override = Some(true);
        let (headed, why, _) = decide(&c, "", false);
        assert!(headed, "a person's yes stands even for a blind model");
        assert!(why.contains("headed = true"), "{why}");
    }

    #[test]
    fn an_endpoint_that_never_spoke_gets_no_window_and_says_so() {
        // Unknown is not "probably yes". It is a door nobody opened, and the sentence handed
        // back has to admit that rather than let the run look like a deliberate headless one.
        let c = BrowserCfg { vision: Vision::Unknown, ..Default::default() };
        let (headed, why, shot) = decide(&c, "", false);
        assert!(!headed);
        assert!(why.contains("never said"), "{why}");
        assert_eq!(shot, "none", "no pixels for a model nobody confirmed can see them");
    }

    #[test]
    fn a_seeing_model_asks_for_a_picture_and_a_blind_one_does_not() {
        assert_eq!(decide(&BrowserCfg { vision: Vision::Yes, ..Default::default() }, "", false).2, "viewport");
        assert_eq!(decide(&BrowserCfg { vision: Vision::No, ..Default::default() }, "", false).2, "none");
        // A model that asks explicitly still gets what it asked for, in both directions —
        // except "headed", which is not in the schema at all.
        assert_eq!(decide(&BrowserCfg { vision: Vision::Yes, ..Default::default() }, "full", false).2, "full");
        assert_eq!(decide(&BrowserCfg { vision: Vision::Yes, ..Default::default() }, "none", false).2, "none");
    }

    #[test]
    fn a_window_of_its_own_outranks_the_model_s_eyes() {
        // The person is already looking at cwoffee's page. A walk that has to open a browser of its
        // own — a model told it may paint one, a person who set `headed = true`, a model with eyes
        // that would otherwise be shown a live page — still gets no second window, and the sentence
        // says why in terms of the window that already exists.
        let seeing = BrowserCfg { vision: Vision::Yes, ..Default::default() };
        let (headed, why, shot) = decide(&seeing, "", true);
        assert!(!headed, "the model's eyes do not buy a second window");
        assert_eq!(shot, "viewport", "a walk that did run can still show the model what it saw");
        assert!(why.contains("sidebar") || why.contains("cwoffee's own window"), "{why}");

        let mut c = seeing.clone();
        c.headed_override = Some(true);
        let (headed, _, _) = decide(&c, "", true);
        assert!(headed, "a person's own word still outranks the feature's default");
        // The window decides the *default*, not a written no-or-yes: it replaces "this model has
        // eyes, so paint one", and nothing else.
        let (headed, why, _) = decide(&seeing, "", true);
        assert!(!headed);
        assert!(why.contains("cwoffee's own window"), "{why}");

        // A walk asked for behind the sidebar's back falls back to the ordinary reasons.
        let (headed, why, _) = decide(&c, "", false);
        assert!(headed, "without a window of its own, a person's headed = true still paints one");
        assert!(why.contains("headed = true"), "{why}");
    }

    #[test]
    fn a_file_url_outside_the_folder_is_an_escape_named_by_path() {
        // Two real folders, not a made-up path: the rule is decided by what the path
        // *canonicalises to*, and a test built out of `..` segments would grade the string
        // handling instead of the confinement.
        let root = std::env::temp_dir().join(format!("cwoffee-browse-{}", std::process::id()));
        let ws = root.join("folder");
        let away = root.join("elsewhere");
        std::fs::create_dir_all(ws.join("page")).unwrap();
        std::fs::create_dir_all(&away).unwrap();
        let ctx = ToolCtx { workspace: ws.canonicalize().unwrap(), ..Default::default() };
        let t = BrowseTool;
        // A page on the network is not harmless: it runs, and it fetches for itself outside the
        // sandbox's proxy. Ask mode must see the origin once, and one yes must then cover the rest
        // of the walk — a model checking a page it wrote opens it several times.
        let web = t.risk_for(&json!({ "url": "http://127.0.0.1:8080/page.html" }), &ctx);
        assert_eq!(web.level, Risk::Sensitive, "a browser runs a page, it does not merely read it");
        assert!(!web.per_call, "one yes per conversation must mean one yes per conversation");


        // Inside: the reach the file tools already have, so no card. A file URL is always
        // absolute — there is no such thing as a relative one — so "inside the folder" means
        // the absolute path the URL names starts with the folder's own path.
        let inside = format!(
            "file://{}/page/index.html",
            ctx.workspace.display()
        );
        let r = t.risk_for(&json!({ "url": inside }), &ctx);
        assert_eq!(r.level, Risk::Workspace, "a page in the folder is a file read in the folder");

        // The hole the shape of this URL closed: a `file://` path that *looks* like a relative
        // path in the folder is still an absolute path on the machine, and the browser is
        // pointed at what the check resolved, not at the caller's spelling.
        let r = t.risk_for(&json!({ "url": "file:///etc/hostname" }), &ctx);
        assert_eq!(r.level, Risk::Sensitive, "/etc is not the folder, however the URL is dressed");

        // Outside: a page can read its own folder's siblings, so this is a read the person has
        // to say yes to — and the card has to name the folder it escapes to, because "a file
        // URL" is not a thing anyone can decide on.
        let outside = format!("file://{}/secret.html", away.canonicalize().unwrap().display());
        let r = t.risk_for(&json!({ "url": outside }), &ctx);
        assert_eq!(r.level, Risk::Sensitive);
        assert!(
            r.escapes.iter().any(|e| e.contains("elsewhere")),
            "the card must name where it goes: {:?}",
            r.escapes
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_start_with_no_driver_says_what_to_install_instead_of_failing_cryptically() {
        // An agent handed "no browser" tries harder in ways that are worse than being told.
        // The answer has to carry the fix, and it has to say nothing was opened.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let ctx = ToolCtx { browser: BrowserCfg { driver: None, ..Default::default() }, ..Default::default() };
            let res = BrowseTool.run(json!({ "url": "http://example.invalid" }), &ctx, &sink()).await;
            assert!(!res.ok);
            let said = res.error.unwrap_or_default();
            assert!(said.contains("nothing was opened"), "{said}");
            assert!(said.contains("driver.mjs"), "{said}");
            assert!(said.contains("npx playwright install"), "{said}");
        });
    }

    #[test]
    fn a_picture_is_only_handed_to_a_model_that_can_look_at_one() {
        let dir = std::env::temp_dir();
        let png = dir.join("cwoffee-b64-test.png");
        std::fs::write(&png, [0x89u8, b'P', b'N', b'G']).unwrap();
        let out = json!({ "screenshot": { "path": png.to_string_lossy(), "bytes": 4 } });

        let blind = model_image(&out, &BrowserCfg { vision: Vision::No, ..Default::default() }, &png);
        assert!(blind.is_empty(), "base64 for a model that cannot see it is pure prompt cost");
        let seeing = model_image(&out, &BrowserCfg { vision: Vision::Yes, ..Default::default() }, &png);
        assert_eq!(seeing.len(), 1);
        assert_eq!(seeing[0].mime, "image/png");
        // Over the ceiling it stops being worth it, and the tool says so in the text rather
        // than silently dropping the picture.
        let small = model_image(&out, &BrowserCfg { vision: Vision::Yes, max_image_bytes: 2, ..Default::default() }, &png);
        assert!(small.is_empty(), "the ceiling is a promise, not a suggestion");
        let _ = std::fs::remove_file(&png);
    }

    #[test]
    fn the_tool_is_mounted_only_where_a_driver_actually_exists() {
        // ~1.1 KB of every prompt, on both cores, for a door that is not there: a machine
        // without Playwright must not pay it.
        let no_driver = BrowserCfg { driver: None, ..Default::default() };
        let reg = crate::tools::Registry::with_v1_tools_browser(
            crate::tools::web::SearchBackend::DuckDuckGo,
            crate::tools::ExecCfg::default(),
            no_driver.usable().then(|| no_driver.clone()),
        );
        let names: Vec<String> = reg.defs().into_iter().map(|d| d.name).collect();
        assert!(!names.iter().any(|n| n == "browse"), "{names:?}");

        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../browser/driver.mjs");
        if here.is_file() {
            let c = BrowserCfg { driver: Some(here.clone()), ..Default::default() };
            let reg = crate::tools::Registry::with_v1_tools_browser(
                crate::tools::web::SearchBackend::DuckDuckGo,
                crate::tools::ExecCfg::default(),
                Some(c),
            );
            let names: Vec<String> = reg.defs().into_iter().map(|d| d.name).collect();
            assert!(names.iter().any(|n| n == "browse"), "a driver on disk is a tool mounted: {names:?}");
            // Prompt-order discipline: adding a tool must not reshuffle the ones before it,
            // or every existing session's prefix cache is thrown away for a feature the
            // conversation may never use.
            assert!(names.iter().position(|n| n == "browse") > names.iter().position(|n| n == "explore"));
        }
    }
}

#[cfg(test)]
mod pixels_tests {
    use super::no_pixels_words;

    /// A kept page that is already on screen has no picture of itself, and a browser that answers
    /// with no bytes is not a failure. Both used to leave the report with nothing at all about the
    /// picture, so a model with no pixels and no reason asked for pixels again — three calls in a
    /// row in one real transcript. The reason has to arrive as a sentence.
    #[test]
    fn a_model_asking_for_pixels_is_told_why_there_are_none() {
        let said = no_pixels_words(Some(
            "there is no picture of a page that is already on screen",
        ));
        assert!(said.contains("no picture of the page"), "{said}");
        assert!(said.contains("already on screen"), "{said}");
        assert!(
            said.contains("text, snapshot, a11y or eval"),
            "the sentence has to point at the way that does work: {said}"
        );
        let blank = no_pixels_words(None);
        assert!(blank.contains("the browser answered with no pixels"), "{blank}");
    }
}
