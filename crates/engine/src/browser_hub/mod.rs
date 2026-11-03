//! A browser the app **keeps**, so a person can watch one and an agent can drive one.
//!
//! The `browse` tool (`crate::tools::browser`) is deliberately one-shot: open, walk, report,
//! close, one card naming one origin. That shape is wrong for two things a person asked for.
//! A page you are looking at does not exist if the browser that rendered it exited with the
//! call. And an agent that wants to test something usually needs more than one look: click,
//! read, change a file, reload, click again — which across separate one-shot walks means a
//! fresh browser each time, so the login, the scrolled row, and the half-filled form are all
//! gone between steps.
//!
//! So this module owns a *session*: one browser, held by the app, answering commands from
//! either side. Two decisions carry the rest of the design:
//!
//! * **The app owns it, not the tool process.** On the pi core the tool host is one process
//!   per call — the same wall [`crate::mcp`] ran into — so a browser held there would be a
//!   new browser per call. The app holds it and hands the tool process a socket.
//! * **The sidebar is a window onto that same browser.** The kept browser is a page **in this
//!   app's own window** (`crate::page_view` in the app): the person is looking at the document the
//!   engine is driving, and a click either end is the same click. The alternative — a *second*
//!   webview rendering its own copy — was rejected for the reason it still gets rejected: a second
//!   browser has its own profile, its own cookies and clicks nobody made, and the agreement between
//!   the two screens cannot be checked. What the panel shows is a rectangle, not a picture.
//!   A kept browser may also still run as a child process (`browser/session.mjs`), which is what
//!   the one-shot `browse` walk uses and the only path that can hand a vision model a screenshot.
//!
//! And because a browser sitting on someone's desktop with nobody accountable for it is the
//! objection the one-shot design was answering, a session is **ephemeral by construction**:
//! an idle clock (five minutes by default, `[browser] idle_secs`) that closes the browser and
//! *says that it did*, a close when the app exits, and a backstop inside the driver for the
//! case where this process dies uncleanly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

use crate::tools::browser::BrowserCfg;

/// Five minutes of nobody caring, and the browser goes away.
///
/// Long enough that stepping away to read a page you just opened does not lose it, short
/// enough that "did the app leave a browser running?" is not a question with a day-long
/// answer. A person can change it (`[browser] idle_secs`); the panel always says what the
/// current number is and how much of it is left.
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// The driver's own backstop, which is deliberately *longer* than the hub's clock: the
/// sentence a person reads should come from the hub, which knows who last touched the
/// browser. The driver's timer exists for the case where this process hangs or dies and
/// cannot report anything.
pub const DRIVER_IDLE_MARGIN_SECS: u64 = 180;

/// What one command's answer costs in wall clock, and why they differ.
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);
const ACT_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Who last put the browser in a new state.
///
/// Not a permission — nobody needs permission to click a button in their own window. It is
/// the answer to "why did this page just change?", which a person watching a frame stream
/// needs within one glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Who {
    #[default]
    Nobody,
    Person,
    Model,
}

impl Who {
    pub fn words(self) -> &'static str {
        match self {
            Who::Nobody => "nobody",
            Who::Person => "you",
            Who::Model => "the model",
        }
    }
    pub fn is_actor(self) -> bool {
        !matches!(self, Who::Nobody)
    }
}

/// What the session is, as far as a screen, a status line, and a risk check need to know.
///
/// This is a *mirror*, kept up to date by both the hub and the stdout reader task, so that
/// something synchronous can read it. `ToolCtx` risk checks are not async: the one question
/// they ask — "is this the browser the person is looking at?" — has to be answerable without
/// awaiting a lock, and a risk level that silently degrades to "no session" because a lock
/// was busy would be a risk level that lies.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// A browser process is up and answering.
    pub alive: bool,
    /// A page has been opened in it.
    pub opened: bool,
    pub url: String,
    pub title: String,
    /// Who started this session, and who acted last.
    pub opened_by: Who,
    pub last_actor: Who,
    /// The last act, in the words the hub used: `click "More"`, `typed into "note"`.
    pub last_act: String,
    /// Why it is not alive, when it is not. Set the moment it dies, so the panel can say
    /// "closed itself after 5 minutes idle" instead of showing a dead frame.
    pub why: String,
    pub idle_secs: u64,
    /// Seconds left on the idle clock; `None` while nothing is alive to time out. Kept
    /// derived from `idle_deadline` rather than stored, because a countdown copied into a
    /// payload starts lying the moment the payload is read.
    pub idle_left: Option<i64>,
    /// When the clock runs out, in whole unix seconds. `None` while nothing is alive.
    pub idle_deadline: Option<i64>,
    pub console: Vec<LogLine>,
    pub page_errors: Vec<String>,
    pub requests_failed: Vec<String>,
    pub downloads_blocked: Vec<String>,
    /// Everything the page said, saw or failed, as records rather than lines of text.
    ///
    /// The four bags above are the panel's columns, and they stay for that. What a model needs is
    /// the distinction between "the page printed something", "the page threw", "a request 404'd"
    /// and "a request never got there" — which is exactly what a list of strings loses.
    /// The box the panel last asked for. Kept beside the box the widget got so that "the page is
    /// where it was told to be" is answerable from the snapshot alone — by whoever asks, at any
    /// moment, rather than only by the window that asked.
    pub want_x: u32,
    pub want_y: u32,
    pub want_w: u32,
    pub want_h: u32,
    pub diagnostics: Vec<Diag>,
    /// The newest diagnostic id, so a reader can ask "anything since 41?" without re-reading the
    /// buffer.
    pub diag_seq: u64,
    /// How many documents this session has had, whoever moved it there.
    pub nav: u64,
    pub frames_taken: u64,
    /// The id of the newest picture this session has. A viewer compares what it is drawing against
    /// this one; `seq` counts page state and cannot tell a watcher anything about pictures, because
    /// a live stream draws several frames of the same state.
    pub frame_pic: u64,
    pub viewport_w: u32,
    pub viewport_h: u32,
    /// Where the page widget **actually is** in the window, as the widget tree reports it. A page in
    /// this window is a native widget, and nothing else can say whether it was given a box at all:
    /// the panel knows what it asked for, the page knows what it was rendered into, and the space
    /// between those two is where "the page is here" turns out to be a lie.
    pub placed_x: u32,
    pub placed_y: u32,
    pub placed_w: u32,
    pub placed_h: u32,
    /// Whether that widget is shown. A widget with a box and no visibility paints nothing, which
    /// from the outside looks exactly like a page that failed to load.
    pub placed_shown: bool,
    /// Whether the browser is pushing frames on its own (its screencast is on) rather than being
    /// asked for one at a time. The panel says which kind of view it is drawing, because "a live
    /// view" and "a picture someone took a moment ago" are different promises.
    pub live: bool,
    /// Why the live view is not available here, when it is not: an empty string means nothing went
    /// wrong, only that nobody asked.
    pub live_why: String,
    /// Whether the kept browser is a page **in the app's own window**. A page on a screen needs no
    /// picture of itself, and the panel draws the rectangle it occupies instead of a view of one.
    pub native: bool,
    /// Monotonic, so a poller can ask "anything newer than 17?" and skip the bytes if not.
    pub seq: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    pub kind: String,
    pub text: String,
}

/// How many diagnostics one session keeps.
///
/// Bounded because the alternative is a page with `while (true) console.log(1)` growing the app
/// until it dies. The oldest records go; the counts stay, so "37 errors, here are the last 12" is
/// still true after the buffer turns over.
pub const DIAG_CAP: usize = 240;

/// Keep a panel column bounded without losing the count of what went past.
fn trim_bag<T>(bag: &mut Vec<T>) {
    if bag.len() > 60 {
        bag.drain(0..bag.len() - 60);
    }
}

/// One thing the page said, saw or failed.
///
/// `kind` is the load-bearing field. It is the difference between "the page printed something",
/// "the page threw", "a request came back 404", "a script or image did not load", "the document
/// never arrived" and "a file was downloaded" — and a model handed a flat list of strings has to
/// guess, which is how a favicon 404 becomes "the page is broken".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Diag {
    /// The page's own id for this record, monotonic within one document.
    #[serde(default)]
    pub seq: i64,
    /// Milliseconds since the epoch, on the page's clock.
    #[serde(default)]
    pub t: i64,
    /// `console` | `error` | `network` | `resource` | `navigation` | `download`.
    #[serde(default)]
    pub kind: String,
    /// `debug` | `info` | `warning` | `error`. A console.log is `info`, not a failure.
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Where in the page it happened — `file:line:col`, or a trimmed stack. Named `at` because
    /// `where` is a Rust keyword; the JSON key stays `where`.
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Which document this belongs to. The hub stamps it on arrival, so "everything on *this*
    /// page load" is a question a reader can actually answer.
    #[serde(default)]
    pub nav: i64,
    /// Who said it. Empty means the page said it about itself; `hub` means the app noticed a thing
    /// the page never reports — a document arriving, an address moving within the page. Worth a
    /// field because "the page logged nothing" and "nothing happened" are different answers, and a
    /// reader who cannot tell them apart stops trusting the log.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

impl Diag {
    /// Read one off the wire, tolerating a driver that sends fewer fields than this one does.
    pub fn from_value(v: &Value) -> Diag {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
        Diag {
            seq: v.get("seq").and_then(|x| x.as_i64()).unwrap_or(0),
            source: v.get("source").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            t: v.get("t").and_then(|x| x.as_i64()).unwrap_or_else(now_secs),
            kind: s("kind").unwrap_or_else(|| "console".into()),
            level: s("level").unwrap_or_else(|| "info".into()),
            text: s("text").unwrap_or_default(),
            url: s("url"),
            status: v.get("status").and_then(|x| x.as_u64()).map(|x| x as u32),
            method: s("method"),
            at: s("where"),
            nav: 0,
        }
    }

    /// Whether this is a thing that went wrong, as opposed to a thing the page said.
    pub fn is_failure(&self) -> bool {
        matches!(self.kind.as_str(), "error" | "network" | "resource" | "navigation")
            && self.level != "info"
    }
}

impl Snapshot {
    /// Counts by kind, and how many of them are things that went wrong. Split out of [`health`] so a
    /// payload polled every second can carry the numbers without carrying the records; both are read
    /// from the same buffer, so a badge and a drawer cannot disagree about what "3 errors" means.
    pub fn diag_counts(&self) -> (Value, usize) {
        let mut counts = serde_json::Map::new();
        for d in &self.diagnostics {
            let slot = counts.entry(d.kind.clone()).or_insert(json!(0));
            let n = slot.as_u64().unwrap_or(0) + 1;
            *slot = json!(n);
        }
        let failed = self.diagnostics.iter().filter(|d| d.is_failure()).count();
        (Value::Object(counts), failed)
    }

    /// The consolidated page-health report: counts by kind, the failures, and which document they
    /// belong to. Built from the same buffer the panel's drawer shows, so a person looking at the
    /// console and a model asking about the page cannot be given different numbers.
    pub fn health(&self) -> Value {
        let (counts, failed_total) = self.diag_counts();
        let failures: Vec<&Diag> = self.diagnostics.iter().filter(|d| d.is_failure()).collect();
        json!({
            "url": self.url,
            "title": self.title,
            "nav": self.nav,
            "counts": counts,
            "failed_total": failed_total,
            "failed": failures.iter().rev().take(12).map(|d| serde_json::to_value(d).unwrap_or_default()).collect::<Vec<_>>(),
            "console_total": self.diagnostics.iter().filter(|d| d.kind == "console").count(),
            "native": self.native,
        })
    }

    /// The buffer, filtered and trimmed, newest first — which is what both the panel's drawer and
    /// the model's tool ask for. `since` is a `diag_seq` the reader already has.
    pub fn diagnostics_since(
        &self,
        since: i64,
        kind: Option<&str>,
        level: Option<&str>,
        limit: usize,
    ) -> Vec<Diag> {
        self.diagnostics
            .iter()
            .rev()
            .filter(|d| d.seq > since)
            .filter(|d| kind.map(|k| d.kind == k).unwrap_or(true))
            .filter(|d| level.map(|l| d.level == l).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect()
    }
    /// One payload, for the socket, the in-process link and the model's tool alike. Written as a
    /// free function rather than a method because the page-in-the-window driver and a driver
    /// process both answer with it, and a reader must not be able to tell them apart.
    pub fn diagnostics_value(
        &self,
        since: i64,
        kind: Option<&str>,
        level: Option<&str>,
        limit: usize,
    ) -> Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let got = self.diagnostics_since(since, kind, level, limit);
        json!({
            "ok": true,
            // The page may be closed while the records of its last page are kept: a reader needs
            // to know which, or it will describe a page that is no longer there.
            "alive": self.alive && self.opened,
            "url": self.url,
            "nav": self.nav,
            // What the newest record the page has ever sent is, whether or not it was asked for.
            // A reader who has seen `newest` has seen everything; that is what makes "tell me if
            // the page printed something" answerable without re-reading the whole log.
            "newest": self.diag_seq,
            "held": self.diagnostics.len(),
            "returned": got.len(),
            "health": self.health(),
            "records": got.iter().map(|d| json!({
                "seq": d.seq,
                "kind": d.kind,
                "level": d.level,
                "text": d.text,
                "url": d.url,
                "status": d.status,
                "method": d.method,
                "at": d.at,
                // Which page this happened on. `nav` is the page counter, so a record from before
                // a navigation can be told from one after it — which is how "the error was there
                // when you arrived" and "your click caused it" stop being the same sentence.
                "nav": d.nav,
                "on_this_page": d.nav as u64 == self.nav,
                // Who said it: empty for the page speaking about itself, `hub` for a thing only the
                // app could know (a document arriving, an address moving inside one page).
                "source": d.source,
                "ago_secs": (now - d.t) / 1000,
            })).collect::<Vec<_>>(),
        })
    }

    /// The sentence for a screen or a transcript note: what is open, and who is driving it.
    /// Seconds of idle left, read now rather than at the last act.
    pub fn idle_left_now(&self) -> Option<i64> {
        let deadline = self.idle_deadline?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Some(deadline - now)
    }

    pub fn says(&self) -> String {
        if self.alive && self.opened {
            let who = match self.last_actor {
                Who::Nobody => "nobody has touched it yet".to_string(),
                w => format!("last touched by {}", w.words()),
            };
            let left = self
                .idle_left_now()
                .map(|s| format!(", and it closes itself in {}s of idle", s.max(0)))
                .unwrap_or_default();
            // The em dash is not decoration. The url is the last thing in the sentence that a
            // person reads as data, and it regularly ends in a fragment or a query; without a
            // separator the address ran into the next clause and the panel read
            // "…/page#firstlast touched by you", which is neither a url nor a sentence.
            format!("a browser is open at {} — {}{}.", self.url, who, left)
        } else if self.alive {
            "a browser is running with no page open.".to_string()
        } else if self.why.is_empty() {
            "no browser is open.".to_string()
        } else {
            format!("no browser is open: {}.", self.why)
        }
    }
}

/// One picture of the page, for whoever is watching. UI bytes only: these never go near a
/// model, whose own picture comes from `browse`'s screenshot path.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The **state** sequence this picture pictures — not its own id. See [`needs_pic`].
    pub seq: u64,
    /// This picture's own id, monotonic across the session. A live stream draws several pictures
    /// of one state (the page animated with no act in front of it), so a viewer that compared
    /// `seq` would be told there was nothing new to draw and keep the frame from before the move.
    pub pic: u64,
    pub b64: String,
    pub mime: String,
    pub bytes: u64,
    pub ms: u64,
    /// A frame the browser offered up itself (a repaint) rather than one someone asked for.
    pub live: bool,
}

/// Where the kept browser's driver is, and how long it may sit untouched.
#[derive(Clone)]
pub struct SessionCfg {
    /// `session.mjs`, next to `driver.mjs`. `None` means there is no kept browser here, and
    /// the panel says so with the same install commands `browse` prints.
    pub session_driver: Option<PathBuf>,
    pub node: String,
    pub idle_secs: u64,
    /// Why keeping a browser is refused here, in the words to say, when a person turned it
    /// off. Kept separate from "the driver is missing" because the two need different
    /// sentences: one is "you switched it off", the other is "install it".
    pub off_reason: Option<String>,
    /// Whether a visible window is wanted. `Some(true)` asks the driver for a real window;
    /// `Some(false)` asks for none; `None` lets the driver decide, which today means "a
    /// window, because a kept browser exists because somebody is looking at it" — and the
    /// driver reports when the display refuses and it falls back.
    pub headed: Option<bool>,
    /// Set when the kept browser is a page view inside the app's own window rather than a browser
    /// a driver process launches. The app mounts this; nothing in the engine can create it.
    pub native: Option<Arc<dyn NativeDriver>>,
}

impl Default for SessionCfg {
    fn default() -> Self {
        Self {
            session_driver: None,
            node: "node".to_string(),
            idle_secs: DEFAULT_IDLE_SECS,
            off_reason: None,
            headed: None,
            native: None,
        }
    }
}

impl std::fmt::Debug for SessionCfg {
    /// `native` cannot be debugged (it is a trait object), and pretending otherwise would print a
    /// lie about what backs this browser.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCfg")
            .field("session_driver", &self.session_driver)
            .field("node", &self.node)
            .field("idle_secs", &self.idle_secs)
            .field("off_reason", &self.off_reason)
            .field("headed", &self.headed)
            .field("native", &self.native.is_some())
            .finish()
    }
}

impl SessionCfg {
    /// The kept-browser driver beside the one-shot driver this start already found.
    ///
    /// Derived rather than configured separately: a machine with `driver.mjs` next to
    /// cwoffee has both files from the same install, and two config keys that have to agree
    /// is two keys that eventually don't.
    pub fn from_browser_cfg(cfg: &BrowserCfg) -> Self {
        // `CWOFFEE_BROWSER_SESSION_DRIVER` names it outright. The path is otherwise derived
        // from the one-shot driver's folder, which is right for an install and wrong for a
        // test board that runs its drivers out of `scripts/`. A named path that does not
        // exist is *not* replaced by the derived one: an override that quietly falls back is
        // how a broken `driver=` line ends up "working" against a file nobody meant to run.
        let named = std::env::var("CWOFFEE_BROWSER_SESSION_DRIVER")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let found = resolve_session_driver(named.as_deref(), cfg.driver.as_deref());
        Self {
            // A person's `keep = false` makes the feature absent, not broken: the panel says
            // they turned it off, and nothing about it is left to fail later.
            session_driver: if cfg.keep { found } else { None },
            off_reason: if cfg.keep {
                None
            } else {
                Some("kept browsers are off: [browser] keep = false in cwoffee's config".to_string())
            },
            node: cfg.node.clone(),
            idle_secs: cfg.idle_secs.unwrap_or(DEFAULT_IDLE_SECS),
            headed: cfg.headed_override,
            // The app decides this, after it knows whether it has a window to put a page in.
            native: None,
        }
    }

    pub fn usable(&self) -> bool {
        if self.native.is_some() {
            return true;
        }
        self.session_driver.as_ref().map(|p| p.is_file()).unwrap_or(false)
    }
}

impl Drop for Hub {
    /// The browser belongs to the process that started it, so it leaves with it. A held browser
    /// that outlives the app is the objection every part of this design has to answer, and a Drop
    /// is the only place that can promise it for *every* exit path — the window closing, the host
    /// stopping, a panic.
    ///
    /// It cannot await (`hard_stop` does, and the shutdown path calls it), so this is the blunt
    /// half: close the pipe and signal the child. The driver treats a closed stdin as "the app
    /// stopped talking to me" and shuts the browser itself, which is also what happens when this
    /// process is killed outright and no destructor runs at all.
    fn drop(&mut self) {
        self.driver_in = None;
        if let Some(d) = self.native.take() {
            d.quit();
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
        }
    }
}

/// Which session driver this start uses. Split out from [`SessionCfg::from_browser_cfg`] so
/// the ordering rule can be tested without mutating a process-wide environment variable —
/// which every other test in this binary also reads.
fn resolve_session_driver(named: Option<&Path>, one_shot: Option<&Path>) -> Option<PathBuf> {
    match named {
        Some(p) => p.is_file().then(|| p.to_path_buf()),
        None => one_shot?.parent().map(|d| d.join("session.mjs")).filter(|sib| sib.is_file()),
    }
}

/// How a command reaches the kept browser's driver.
enum Sink {
    /// `browser/session.mjs` running as a child process: the browser is outside this one.
    Child(ChildStdin),
    /// The app's own page view, in this process. Same line protocol, no pipes.
    Native(Arc<dyn NativeDriver>),
}

/// A driver that lives inside the app instead of in a child process: the page shown in the
/// window *is* the kept browser, so the person's real clicks and the model's acts land in the
/// same document.
///
/// It answers the same line protocol as `browser/session.mjs` — one JSON value per line in each
/// direction, `id` on an answer, `ev` on something volunteered — which is what lets the idle clock,
/// the panel's sentences, the who-did-what record and the pending-command map be one set of code
/// rather than two that drift.
pub trait NativeDriver: Send + Sync + 'static {
    /// Start talking to the hub. Every answer and event the driver produces goes out here.
    fn attach(&self, out: tokio::sync::mpsc::UnboundedSender<String>);
    /// One command, already serialized, no trailing newline.
    fn send(&self, line: &str);
    /// The hub is done with this browser: put the page away.
    fn quit(&self);
}

/// The parts the driver's answers have to reach, whichever transport they arrived on.
#[derive(Clone)]
struct ReaderCtx {
    shared: Arc<RwLock<Snapshot>>,
    frame_slot: Arc<Mutex<Option<Arc<Frame>>>>,
    frame_seq: Arc<AtomicU64>,
    frame_pic: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>,
    activity: Arc<AtomicBool>,
}

/// One line off the driver: an answer to a command we sent, or something the browser volunteered.
///
/// This is free rather than a closure inside the spawn so that a driver living *in this process*
/// (the app's own page view) can answer through exactly the same bookkeeping as a driver living in
/// a child. Two transports, one set of rules about what a `frame`, a `nav` or a `closed` means.
fn dispatch_line(raw: &str, ctx: &ReaderCtx) {
    let line = raw.trim();
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        eprintln!("[cwoffee/browser] the driver said something that is not JSON: {line}");
        return;
    };
    let ReaderCtx { shared, frame_slot, frame_seq, frame_pic, pending, activity } = ctx;
        if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
            let tx = pending.lock().map(|mut m| m.remove(&id)).unwrap_or(None);
            if let Some(tx) = tx {
                let _ = tx.send(v);
            }
            return;
        }
        match v.get("ev").and_then(|e| e.as_str()).unwrap_or("") {
            "placed" => {
                // The widget tree's own answer about the page's box. Stored rather than forwarded:
                // whoever is looking reads it off the snapshot, and a mismatch is something the panel
                // has to be able to say out loud instead of leaving as an empty rectangle.
                if let Some(pl) = v.get("placed") {
                    let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                    g.placed_x = pl.get("x").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                    g.placed_y = pl.get("y").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                    g.placed_w = pl.get("width").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                    g.placed_h = pl.get("height").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                    g.placed_shown = pl.get("shown").and_then(|w| w.as_bool()).unwrap_or(false);
                    g.seq += 1;
                }
            }
            "ready" | "viewport" => {
                // The size the page really has, from the side that owns it. For a browser in another
                // process that is the viewport a driver was asked for; for a page in this window it is
                // the box the panel reported, and the panel cannot know it by having asked — the
                // window manager, a scrollbar and a hi-hi-scale factor all get a say.
                if let Some(vp) = v.get("viewport") {
                    let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                    g.viewport_w = vp.get("width").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                    g.viewport_h = vp.get("height").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
                    g.seq += 1;
                }
            }
            "nav" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
                g.url = url.clone();
                g.opened = !g.url.is_empty();
                g.why = String::new();
                // A document change, whoever caused it. This is the number that tells a reader the
                // page they were told about is not the page on screen now.
                g.nav += 1;
                g.seq += 1;
                note_move(&mut g, "a new document", &url, &title);
            }
            // The address moved without a new document: a fragment, `history.pushState`, a Back
            // through either. The page counter must not move — the document is the same document, and
            // "how many pages has this been?" is a question about documents — but the url a panel or
            // a model holds has to follow, or Back looks like it did nothing.
            "url" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let url = v.get("url").and_then(|u| u.as_str()).unwrap_or(&g.url.clone()).to_string();
                let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
                g.url = url.clone();
                if !title.is_empty() {
                    g.title = title.clone();
                }
                g.opened = !g.url.is_empty();
                g.seq += 1;
                note_move(&mut g, "the address moved within the page", &url, &title);
            }
            "console" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("log").to_string();
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                g.console.push(LogLine { kind: kind.clone(), text: text.clone() });
                if g.console.len() > 60 {
                    g.console.remove(0);
                }
                note_flat(&mut g, "console", console_level(&kind), &text, None, None, None);
                g.seq += 1;
            }
            "pageerror" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                let at = v.get("where").and_then(|t| t.as_str()).map(|x| x.to_string());
                g.page_errors.push(text.clone());
                if g.page_errors.len() > 20 {
                    g.page_errors.remove(0);
                }
                note_flat(&mut g, "error", "error", &text, None, None, at.as_deref());
                g.seq += 1;
            }
            "requestfailed" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                let why = v.get("why").and_then(|w| w.as_str()).unwrap_or("failed").to_string();
                let status = v.get("status").and_then(|x| x.as_u64()).map(|x| x as u32);
                let method = v.get("method").and_then(|m| m.as_str()).map(|m| m.to_string());
                g.requests_failed.push(format!("{url} ({why})"));
                if g.requests_failed.len() > 20 {
                    g.requests_failed.remove(0);
                }
                note_flat(
                    &mut g,
                    "network",
                    "error",
                    &why,
                    Some(&url),
                    status,
                    method.as_deref(),
                );
                g.seq += 1;
            }
            "download" => {
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("a file").to_string();
                let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                g.downloads_blocked.push(name.clone());
                note_flat(&mut g, "download", "info", &name, Some(&url), None, None);
                g.seq += 1;
            }
            // A frame the browser volunteered: the page repainted and nobody asked. This is
            // the live view, and it is the only way a page that moved without an act — an
            // animation, a video, the model's own scroll — reaches a person watching.
            "frame" => {
                let b64 = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
                if b64.is_empty() {
                    return;
                }
                let pic = frame_pic.fetch_add(1, Ordering::Relaxed) + 1;
                let mime = v.get("mime").and_then(|m| m.as_str()).unwrap_or("image/jpeg");
                let bytes = v.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
                let (seq, vp) = {
                    let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                    g.frames_taken += 1;
                    g.frame_pic = pic;
                    g.live = true;
                    g.live_why = v.get("why").and_then(|w| w.as_str()).unwrap_or("").to_string();
                    (g.seq, (g.viewport_w, g.viewport_h))
                };
                // The viewport travels with the frame, and the panel scales its clicks by it.
                // A page that was resized while a frame was in flight would otherwise get a
                // picture and a viewport number that disagree, which is a wrong click.
                if let (Some(w), Some(h)) = (
                    v.get("viewport").and_then(|p| p.get("width")).and_then(|w| w.as_u64()),
                    v.get("viewport").and_then(|p| p.get("height")).and_then(|h| h.as_u64()),
                ) {
                    if (w as u32, h as u32) != vp {
                        let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                        g.viewport_w = w as u32;
                        g.viewport_h = h as u32;
                    }
                }
                frame_seq.store(seq, Ordering::Relaxed);
                *frame_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(Frame {
                    seq,
                    pic,
                    b64: b64.to_string(),
                    mime: mime.to_string(),
                    bytes,
                    ms: 0,
                    live: true,
                }));
            }
            "activity" => {
                // Somebody's hand, on a page in this window. It is activity, not a picture, and not
                // a reason to stop the clock of a browser nobody is *using* — the two are the same
                // fact here, told by whoever can see the page.
                activity.store(true, Ordering::Relaxed);
            }
            "closed" => {
                let why = v.get("why").and_then(|w| w.as_str()).unwrap_or("it closed").to_string();
                // No picture of a browser that is gone, and no picture count for the next
                // browser to be mistaken against.
                *frame_slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
                frame_pic.store(0, Ordering::Relaxed);
                frame_seq.store(0, Ordering::Relaxed);
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                g.alive = false;
                g.opened = false;
                g.live = false;
                g.frame_pic = 0;
                g.why = why.clone();
                g.url.clear();
                g.title.clear();
                g.seq += 1;
                drop(g);
                // Anything still waiting gets an answer naming the reason, rather
                // than a caller sitting out its whole timeout.
                if let Ok(mut m) = pending.lock() {
                    for (_, tx) in m.drain() {
                        let _ = tx.send(json!({"ok": false, "why": format!("that browser closed ({why})"), "dead": true}));
                    }
                }
            }
            "diag" => {
                // One structured record from a driver. The four panel bags are filled from it as
                // well, so the person's drawer and the model's tool are reading one truth and not
                // two that can disagree.
                let Some(d) = v.get("diag") else { return };
                let item = Diag::from_value(d);
                let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
                // The app's own sequence number, not the page's. A page counts from one every time a
                // document loads, so trusting its number gives two records the same id a few seconds
                // apart — and an id is what a drawer keys its rows by and what a reader says "newer
                // than this" against. The page's counter is for the page's bookkeeping.
                g.diag_seq += 1;
                let item = Diag { seq: g.diag_seq as i64, nav: g.nav as i64, ..item };
                g.diagnostics.push(item.clone());
                let over = g.diagnostics.len().saturating_sub(DIAG_CAP);
                if over > 0 {
                    g.diagnostics.drain(0..over);
                }
                match item.kind.as_str() {
                    // The panel's column speaks one vocabulary whether the line arrived flat or as a
                    // record, so the level is translated into the kinds the flat path already used
                    // rather than passed through: "info" and "log" are the same column entry.
                    "console" => g.console.push(LogLine {
                        kind: match item.level.as_str() {
                            "warning" => "warning".into(),
                            "error" => "error".into(),
                            _ => "log".into(),
                        },
                        text: item.text.clone(),
                    }),
                    "error" => g.page_errors.push(item.text.clone()),
                    "network" | "resource" => g.requests_failed.push(format!(
                        "{} ({})",
                        item.url.clone().unwrap_or_else(|| "a request".into()),
                        item.status.map(|s| s.to_string()).unwrap_or_else(|| item.text.clone())
                    )),
                    "download" => g.downloads_blocked.push(item.text.clone()),
                    _ => {}
                }
                trim_bag(&mut g.console);
                trim_bag(&mut g.page_errors);
                trim_bag(&mut g.requests_failed);
                trim_bag(&mut g.downloads_blocked);
                g.seq += 1;
            }
            other => {
                eprintln!("[cwoffee/browser] driver event I do not handle: {other}");
            }
        }

}

/// The driver is gone — exited, or its transport dropped. Anything still waiting gets told why.
fn driver_gone(ctx: &ReaderCtx, why: &str) {
    let ReaderCtx { shared, pending, .. } = ctx;
    {
        let mut g = shared.write().unwrap_or_else(|p| p.into_inner());
        if g.alive {
            g.alive = false;
            g.opened = false;
            g.why = why.to_string();
            g.seq += 1;
        }
    }
    if let Ok(mut m) = pending.lock() {
        for (_, tx) in m.drain() {
            let _ = tx.send(json!({"ok": false, "why": why, "dead": true}));
        }
    }
}

/// The app's one kept browser.
pub struct Hub {
    cfg: SessionCfg,
    /// Read without awaiting: see [`Snapshot`].
    shared: Arc<RwLock<Snapshot>>,
    child: Option<Child>,
    /// How commands reach the driver: a child process's stdin, or a page view in this process.
    driver_in: Option<Sink>,
    /// Set when the driver is the app's own page view rather than `session.mjs`.
    native: Option<Arc<dyn NativeDriver>>,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>,
    seq: u64,
    /// The newest picture, shared with the stdout reader so a frame the browser volunteered can
    /// land in it as easily as one someone asked for.
    frame_slot: Arc<Mutex<Option<Arc<Frame>>>>,
    /// The **state** the newest picture pictures, and the newest picture's own id. Two numbers,
    /// two questions: "does the newest frame still describe the newest page?" and "has this viewer
    /// seen the newest frame?".
    frame_seq: Arc<AtomicU64>,
    frame_pic: Arc<AtomicU64>,
    /// Whether somebody is looking. The driver's screencast costs it real work, so it runs while a
    /// panel is open and stops when the panel closes.
    watching: bool,
    /// How wide the watching panel's box is, in CSS pixels. The stream is asked for a picture of that
    /// width: a 1280-wide page shown in a 400px column does not need 1280-wide pictures. 0 means the
    /// watcher gave no hint and gets the page's own width.
    watch_w: u32,
    last_act: Instant,
    /// A page in the window can be touched by a real hand. The bridge counts those touches and the
    /// driver repeats them as `activity` lines; this flag is what `reap_if_idle` reads, because
    /// "nobody touched it" has to include somebody touching it with a mouse.
    activity: Arc<AtomicBool>,
}

impl Hub {
    /// Mount a hub. **Nothing is started here** — mounting is cheap and always done, so that
    /// opening a browser after installing a driver does not need a restart, and so the panel
    /// can exist before there is anything in it.
    pub fn mount(cfg: SessionCfg) -> Self {
        let why = if cfg.usable() {
            String::new()
        } else {
            cfg.off_reason.clone().unwrap_or_else(|| {
                "no kept-browser driver here (browser/session.mjs is missing)".to_string()
            })
        };
        let shared = Snapshot {
            idle_secs: cfg.idle_secs,
            native: cfg.native.is_some(),
            why,
            ..Default::default()
        };
        Self {
            cfg,
            shared: Arc::new(RwLock::new(shared)),
            child: None,
            driver_in: None,
            native: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            seq: 0,
            frame_slot: Arc::new(Mutex::new(None)),
            frame_seq: Arc::new(AtomicU64::new(0)),
            frame_pic: Arc::new(AtomicU64::new(0)),
            watching: false,
            watch_w: 0,
            last_act: Instant::now(),
            activity: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A synchronous read of the current state. Safe to call from a risk check.
    pub fn peek_shared(shared: &Arc<RwLock<Snapshot>>) -> Snapshot {
        shared
            .read()
            .map(|g| g.clone())
            // A poisoned lock means a writer panicked mid-update; the least dishonest answer
            // is the state we can still recover from the guard.
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Give this hub a driver that lives in the app's own window. The rules do not change with it:
    /// the clock, the panel's words and the who-opened-it record are the hub's, not the driver's.
    pub fn set_native(&mut self, drv: Arc<dyn NativeDriver>) {
        self.cfg.native = Some(drv);
        self.set_note(|g| {
            g.native = true;
            // A page in the window is always available: there is no driver file to be missing and
            // no Playwright install to fail.
            if g.why.contains("driver") {
                g.why = String::new();
            }
        });
    }

    pub fn shared(&self) -> Arc<RwLock<Snapshot>> {
        Arc::clone(&self.shared)
    }

    /// This hub's current state, read synchronously.
    pub fn peek(&self) -> Snapshot {
        Self::peek_shared(&self.shared)
    }

    pub fn cfg(&self) -> &SessionCfg {
        &self.cfg
    }

    pub fn idle_left(&self) -> Option<i64> {
        if !self.peek().alive {
            return None;
        }
        let spent = self.last_act.elapsed().as_secs() as i64;
        Some(self.cfg.idle_secs as i64 - spent)
    }

    fn set_note(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut g = match self.shared.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        f(&mut g);
        g.seq += 1;
    }

    /// The seq of the newest frame anyone has taken. A poller compares this against what it
    /// is already holding and skips the bytes when nothing new has been drawn — which is how
    /// a panel can poll often without shipping a picture every time.
    pub fn frame_seq(&self) -> u64 {
        self.frame_seq.load(Ordering::Relaxed)
    }

    /// The id of the newest picture this session has, live or asked-for.
    pub fn frame_pic(&self) -> u64 {
        self.frame_pic.load(Ordering::Relaxed)
    }

    /// Whether the newest picture still pictures the newest state of the page.
    pub fn frame_pictures_state(&self, newest_state: u64) -> bool {
        self.frame_seq() >= newest_state
    }

    /// Put a picture up for whoever is looking, and hand it back. Shared by the still path and by
    /// the frames the driver volunteers, so both mean the same thing downstream.
    fn publish_frame(&self, b64: &str, mime: &str, bytes: u64, live: bool) -> Arc<Frame> {
        let pic = self.frame_pic.fetch_add(1, Ordering::Relaxed) + 1;
        let seq = {
            let mut g = self.shared.write().unwrap_or_else(|p| p.into_inner());
            g.frames_taken += 1;
            g.frame_pic = pic;
            if live {
                g.live = true;
            }
            // A frame the browser offered describes the page as it is *now*, so it pictures the
            // newest state; that is what stops a poll taking a second picture of the same pixels.
            g.seq
        };
        self.frame_seq.store(seq, Ordering::Relaxed);
        let f = Arc::new(Frame {
            seq,
            pic,
            b64: b64.to_string(),
            mime: mime.to_string(),
            bytes,
            ms: 0,
            live,
        });
        *self.frame_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&f));
        f
    }

    /// Start the driver process if there isn't one. The browser itself is only launched by
    /// an `open`, so a hub that is mounted and never used spawns nothing at all.
    async fn ensure_child(&mut self) -> Result<(), String> {
        if self.driver_in.is_some() {
            return Ok(());
        }
        if let Some(drv) = self.cfg.native.clone() {
            return self.attach_native(drv).await;
        }
        let driver = self
            .cfg
            .session_driver
            .as_ref()
            .filter(|p| p.is_file())
            .cloned()
            .ok_or_else(|| {
                "there is no kept-browser driver here: `browser/session.mjs` is missing. \
                 The one-shot browser may still work; put the driver back with the rest of \
                 cwoffee and install its dependencies (`npm --prefix browser install`)."
                    .to_string()
            })?;
        let shown = driver.display().to_string();
        let mut cmd = Command::new(&self.cfg.node);
        cmd.arg(driver)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env(
                "CWOFFEE_BROWSER_DRIVER_IDLE_MS",
                ((self.cfg.idle_secs + DRIVER_IDLE_MARGIN_SECS) * 1000).to_string(),
            );
        match self.cfg.headed {
            Some(false) => {
                cmd.env("CWOFFEE_BROWSER_SESSION_HEADLESS", "1");
            }
            Some(true) => {
                cmd.env_remove("CWOFFEE_BROWSER_SESSION_HEADLESS");
            }
            None => {}
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("the kept-browser driver would not start ({} {shown}): {e}", self.cfg.node))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the kept-browser driver has no stdin to send commands to".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the kept-browser driver has no stdout to read answers from".to_string())?;
        let stderr = child.stderr.take();

        let ctx = self.reader_ctx();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch_line(&line, &ctx);
            }
            driver_gone(&ctx, "the browser driver exited");
        });

        // The driver's stderr is a note, not a stream: it is where a Playwright complaint
        // about *this machine* shows up, and the panel's line about a dead browser is where
        // a person would look for it. Keep the last few lines.
        if let Some(err) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let l = l.trim();
                    if !l.is_empty() {
                        eprintln!("[cwoffee/browser] {l}");
                    }
                }
            });
        }

        self.child = Some(child);
        self.driver_in = Some(Sink::Child(stdin));
        Ok(())
    }

    async fn request(&mut self, mut req: Value, timeout: Duration) -> Result<Value, String> {
        // A browser that is already known dead is answered at once, with the reason. Writing
        // into a closed pipe can *look* like it worked — the bytes go into a buffer — and a
        // caller that waits out its timeout for an answer that was already true is a panel
        // that sits there frozen for 20 seconds.
        let snap = self.peek();
        let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("").to_string();
        if !snap.alive && !matches!(cmd.as_str(), "open" | "state" | "ping") && !snap.why.is_empty() {
            return Err(format!("that browser is closed ({})", snap.why));
        }
        self.ensure_child().await?;
        let id = self.seq;
        self.seq += 1;
        req["id"] = json!(id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().map_err(|_| "the pending map is poisoned".to_string())?.insert(id, tx);
        let line = serde_json::to_string(&req).unwrap_or_default();
        match self.driver_in.as_mut().ok_or_else(|| "the browser driver is not reachable".to_string())? {
            Sink::Child(w) => {
                use tokio::io::AsyncWriteExt;
                // The newline is what makes it a line: the driver reads commands one line at a
                // time, so a command without one sits in its buffer unanswered until timeout.
                w.write_all(format!("{line}\n").as_bytes())
                    .await
                    .map_err(|e| format!("the command never reached the browser driver: {e}"))?;
                w.flush().await.map_err(|e| format!("could not flush to the driver: {e}"))?;
            }
            Sink::Native(drv) => drv.send(&line),
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => {
                self.pending.lock().map(|mut m| m.remove(&id)).ok();
                Err("the browser driver stopped answering".to_string())
            }
            Err(_) => {
                self.pending.lock().map(|mut m| m.remove(&id)).ok();
                Err(format!(
                    "the browser did not answer that command within {}s",
                    timeout.as_secs()
                ))
            }
        }
    }

    /// Open a page — in the current session if one is alive, otherwise by starting one.
    ///
    /// `who` is not a permission check. It is the record of who caused a window to appear,
    /// which is the thing a person watching their own screen asks about.
    pub async fn open(&mut self, url: &str, who: Who, allow_file: bool) -> Result<Value, String> {
        let url = url.trim();
        if url.is_empty() {
            return Err("no url to open".to_string());
        }
        self.last_act = Instant::now();
        // A dead session is not reused. "Open again" after a crash, or after the idle clock
        // closed it, starts a fresh browser — and the sentence that explains the old one is
        // kept in `why` until this open succeeds, so the panel can say both.
        if !self.peek().alive && self.child.is_some() {
            self.hard_stop().await;
        }
        let out = self
            .request(
                json!({"cmd": "open", "url": url, "allow_file": allow_file, "timeout_ms": OPEN_TIMEOUT.as_millis()}),
                OPEN_TIMEOUT,
            )
            .await?;
        let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if ok {
            let url_after = out
                .get("url_after")
                .and_then(|u| u.as_str())
                .unwrap_or(url)
                .to_string();
            let title = out.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
            self.set_note(|g| {
                g.alive = true;
                g.opened = true;
                g.idle_left = Some(self.cfg.idle_secs as i64);
                g.idle_deadline = Some(now_secs() + self.cfg.idle_secs as i64);
                g.why = String::new();
                g.url = url_after.clone();
                g.title = title.clone();
                if !g.opened_by.is_actor() {
                    g.opened_by = who;
                }
                g.last_actor = who;
                g.last_act = format!("opened {url_after}");
            });
            // A browser that had to be started (or restarted) has no screencast attached to it, so
            // a panel that is watching asks again rather than sitting on a frame from the old one.
            if self.watching {
                let _ = self.watch(true, None).await;
            }
        } else {
            let why = out.get("why").and_then(|w| w.as_str()).unwrap_or("the page did not open").to_string();
            self.set_note(|g| {
                g.last_actor = who;
                g.last_act = format!("tried to open {url}");
                g.why = why.clone();
            });
            return Err(why);
        }
        Ok(out)
    }

    /// One act on the open page: click, fill, press, scroll, eval, back, reload…
    pub async fn act(&mut self, act: Value, who: Who) -> Result<Value, String> {
        self.last_act = Instant::now();
        let mut req = json!({"cmd": "act", "timeout_ms": ACT_TIMEOUT.as_millis()});
        if let (Some(obj), Some(a)) = (req.as_object_mut(), act.as_object()) {
            for (k, v) in a {
                obj.insert(k.clone(), v.clone());
            }
        }
        let out = self.request(req, ACT_TIMEOUT).await;
        match out {
            Ok(out) => {
                let ok = out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                let desc = describe_act(&act);
                self.set_note(|g| {
                    g.last_actor = who;
                    g.last_act = if ok { desc.clone() } else { format!("{desc} — which failed") };
                    // Touching it rewinds the clock; the panel shows the countdown, so the
                    // number has to be current rather than whatever it was at open.
                    g.idle_left = Some(self.cfg.idle_secs as i64);
                    g.idle_deadline = Some(now_secs() + self.cfg.idle_secs as i64);
                });
                Ok(out)
            }
            Err(e) => {
                let desc = describe_act(&act);
                self.set_note(|g| {
                    g.last_actor = who;
                    g.last_act = format!("{desc} — {e}");
                });
                Err(e)
            }
        }
    }

    /// A picture of the page, for whoever is looking. Asked for by a client that is displaying one;
    /// when the live view is running the newest frame is already in the slot and this takes no
    /// picture at all, so a fast poll does not cost a screenshot.
    pub async fn frame(&mut self, png: bool) -> Result<Arc<Frame>, String> {
        let out = self
            .request(json!({"cmd": "frame", "type": if png { "png" } else { "jpeg" }}), FRAME_TIMEOUT)
            .await?;
        if !out.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(out
                .get("why")
                .and_then(|w| w.as_str())
                .unwrap_or("no picture of the page")
                .to_string());
        }
        Ok(self.publish_frame(
            out.get("b64").and_then(|b| b.as_str()).unwrap_or(""),
            out.get("mime").and_then(|m| m.as_str()).unwrap_or("image/jpeg"),
            out.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0),
            false,
        ))
    }

    /// Ask the browser to push frames of its own — its screencast, its repaints — or stop it.
    ///
    /// This is not a permission and it is not the idle clock: it is the load note. A screencast is
    /// work for the browser process, so it runs while a panel is looking and not otherwise; a
    /// session nobody watches pays nothing for pixels it will never show.
    ///
    /// A driver with no screencast (an older Chromium) answers `live: false` with the reason, and
    /// the panel goes back to taking stills rather than showing nothing.
    /// Ask the browser for (or stop) its own frames. `max_width` is the watching panel's box in CSS
    /// pixels: the stream is sized to it, because a page-width picture sent to a narrow column is
    /// bytes nobody can see. `None` keeps the width already stated; `Some(0)` asks for the page's own
    /// width. Watching is **not** activity — it never touches the idle clock, because the five-minute
    /// promise is a promise about nobody *using* the browser.
    pub async fn watch(&mut self, on: bool, max_width: Option<u32>) -> Result<Value, String> {
        self.watching = on;
        // A watcher's box is re-stated every time it asks, and a panel that resizes (full window,
        // or a window being dragged) re-asks with the new width. `None` means "as before".
        let hint = max_width.unwrap_or(self.watch_w).min(4096);
        self.watch_w = hint;
        if !self.peek().alive {
            return Ok(json!({"ok": false, "why": "no browser is open to watch", "live": false}));
        }
        let out = self
            .request(json!({"cmd": if on { "watch" } else { "unwatch" }, "max_width": hint }), ACT_TIMEOUT)
            .await?;
        let live = on && out.get("live").and_then(|l| l.as_bool()).unwrap_or(false);
        let why = out.get("why").and_then(|w| w.as_str()).unwrap_or("").to_string();
        self.set_note(|g| {
            g.live = live;
            if !live && !why.is_empty() {
                g.live_why = why.clone();
            }
            if on && !live && g.why.is_empty() {
                // The live view failing must not read as the *browser* failing: the page is open
                // and the panel can still show it, one still at a time.
                g.live_why = why.clone();
            }
        });
        if live {
            eprintln!("[cwoffee/browser] live view on: the browser is pushing its own frames");
        } else if on {
            eprintln!("[cwoffee/browser] live view unavailable ({why}); falling back to stills");
        }
        Ok(out)
    }

    /// Reshape the page to the box it is being looked at. A panel that goes full-height asks for a
    /// page that fills it; a page that stays 1280x800 inside a tall panel is a picture with a
    /// letterbox, which is the empty space arriving by a different door.
    pub async fn set_viewport(&mut self, width: u32, height: u32) -> Result<Value, String> {
        if width < 320 || height < 240 {
            return Err("that is too small a page to resize to".to_string());
        }
        let out = self
            .request(json!({"cmd": "viewport", "width": width, "height": height}), ACT_TIMEOUT)
            .await?;
        if out.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
            if let (Some(w), Some(h)) = (
                out.get("viewport").and_then(|p| p.get("width")).and_then(|w| w.as_u64()),
                out.get("viewport").and_then(|p| p.get("height")).and_then(|h| h.as_u64()),
            ) {
                self.set_note(|g| {
                    g.viewport_w = w as u32;
                    g.viewport_h = h as u32;
                });
            }
        }
        Ok(out)
    }

    /// Tell the page where the panel's box is, in CSS pixels from the window's top left. Only a
    /// page view inside the window uses this; a browser in a child process has no idea where the
    /// panel is and never sees the command.
    pub async fn view_rect(&mut self, x: f64, y: f64, width: f64, height: f64) -> Result<Value, String> {
        if self.cfg.native.is_none() {
            return Ok(json!({"ok": false, "why": "this browser is not a page in the window; it has no box to fill"}));
        }
        // A box with no size is not a mistake: it is the panel saying "there is nowhere to put it",
        // which is how a page gets taken out of the way when the pane closes. Refusing it would leave
        // a page floating over the conversation with nothing able to hide it.
        let (width, height) = (width.max(0.0), height.max(0.0));
        {
            let mut g = self.shared.write().unwrap_or_else(|p| p.into_inner());
            g.want_x = x.max(0.0) as u32;
            g.want_y = y.max(0.0) as u32;
            g.want_w = width.max(0.0) as u32;
            g.want_h = height.max(0.0) as u32;
        }
        self.request(
            json!({"cmd": "view_rect", "x": x, "y": y, "width": width, "height": height}),
            ACT_TIMEOUT,
        )
        .await
    }

    /// The newest frame, or `None` when the caller already holds it (`since`) or there is no
    /// viewer-driven reason to take one.
    pub fn frame_now(&self) -> Option<Arc<Frame>> {
        self.frame_slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub async fn state(&mut self) -> Result<Snapshot, String> {
        if self.driver_in.is_none() {
            return Ok(self.peek());
        }
        let out = self.request(json!({"cmd": "state"}), ACT_TIMEOUT).await?;
        let alive = out.get("alive").and_then(|a| a.as_bool()).unwrap_or(false);
        let url = out.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let title = out.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let live = out.get("live").and_then(|l| l.as_bool()).unwrap_or(false);
        let live_why = out.get("live_why").and_then(|w| w.as_str()).unwrap_or("").to_string();
        let vp = (out.get("viewport").and_then(|p| p.get("width")).and_then(|w| w.as_u64()),
            out.get("viewport").and_then(|p| p.get("height")).and_then(|h| h.as_u64()));
        self.set_note(|g| {
            g.alive = alive;
            g.opened = alive && !url.is_empty();
            if !url.is_empty() {
                g.url = url.clone();
            }
            if !title.is_empty() {
                g.title = title;
            }
            // What the driver says it is doing, not what this hub last asked it to do: a driver
            // that fell back to stills on its own should be reported as falling back to stills.
            g.live = live;
            g.live_why = live_why;
            if let (Some(w), Some(h)) = vp {
                g.viewport_w = w as u32;
                g.viewport_h = h as u32;
            }
        });
        Ok(self.peek())
    }

    /// Close this browser, on purpose, and say why.
    pub async fn close(&mut self, why: &str) {
        if self.driver_in.is_some() {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.request(json!({"cmd": "close"}), CLOSE_TIMEOUT)).await;
        }
        self.hard_stop().await;
        self.frame_seq.store(0, Ordering::Relaxed);
        self.frame_pic.store(0, Ordering::Relaxed);
        *self.frame_slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.watching = false;
        self.set_note(|g| {
            g.alive = false;
            g.opened = false;
            g.opened_by = Who::Nobody;
            g.last_actor = Who::Nobody;
            g.url.clear();
            g.title.clear();
            g.why = why.to_string();
            // The mirror carries the same counters as the hub, so they go back together. A panel
            // that read `frame_pic: 3` off a closed session would treat the first picture of the
            // next browser as one it already had.
            g.frame_pic = 0;
            g.live = false;
        });
    }

    async fn hard_stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Drop stdin first: the driver treats a closed stdin as "the app stopped talking
            // to me" and closes the browser itself. Then make sure it is gone.
            self.driver_in = None;
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(drv) = self.native.take() {
            // The page view is not a process to kill: it is told to put the page away, and it
            // answers with the same `closed` line a driver does.
            drv.quit();
        }
        self.driver_in = None;
    }

    /// Attach a driver that lives in this process: the app's own page view. From here the hub
    /// cannot tell the two apart, which is the point — the idle clock, the panel's words, the
    /// who-did-what record and the pending-command map are the same code for both.
    ///
    /// Commands go *to* the driver by call (`drv.send`); answers and volunteered events come back
    /// on a channel, which is the mirror of a child process: stdin is a call we make, stdout is a
    /// queue we read.
    async fn attach_native(&mut self, drv: Arc<dyn NativeDriver>) -> Result<(), String> {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ctx = self.reader_ctx();
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                dispatch_line(&line, &ctx);
            }
            driver_gone(&ctx, "the page view is gone");
        });
        drv.attach(out_tx);
        self.driver_in = Some(Sink::Native(Arc::clone(&drv)));
        self.native = Some(drv);
        Ok(())
    }

    fn reader_ctx(&self) -> ReaderCtx {
        ReaderCtx {
            shared: Arc::clone(&self.shared),
            frame_slot: Arc::clone(&self.frame_slot),
            frame_seq: Arc::clone(&self.frame_seq),
            frame_pic: Arc::clone(&self.frame_pic),
            pending: Arc::clone(&self.pending),
            activity: Arc::clone(&self.activity),
        }
    }

    /// The idle clock, callable by a reaper task *and* by a test with a clock it controls.
    /// Returns the sentence it produced, if it closed one.
    pub async fn reap_if_idle(&mut self) -> Option<String> {
        if self.activity.swap(false, Ordering::Relaxed) {
            self.last_act = Instant::now();
        }
        let snap = self.peek();
        if !snap.alive {
            return None;
        }
        let left = self.cfg.idle_secs as i64 - self.last_act.elapsed().as_secs() as i64;
        if left > 0 {
            self.set_note(|g| {
                g.idle_left = Some(left);
                g.idle_deadline = Some(now_secs() + left);
            });
            return None;
        }
        // The engine's own duration words, so a person who set `[browser] idle_secs = 45`
        // reads "45 seconds" rather than "0 minutes".
        let why = format!(
            "nobody touched it for {}, so it closed itself",
            idle_words(self.cfg.idle_secs)
        );
        self.close(&why).await;
        eprintln!("[cwoffee/browser] {why}");
        Some(why)
    }

    /// What the whole app's shutdown owes: no browser left behind.
    pub async fn shutdown(&mut self) {
        if self.child.is_some() || self.native.is_some() {
            self.close("the app ended").await;
        }
    }

    /// Refresh what this hub can do from the config this app just read.
    ///
    /// A person who installs Playwright while the app is open should be able to open a
    /// browser, not restart the window to get one.
    pub fn adopt(&mut self, cfg: SessionCfg) {
        let changed_idle = cfg.idle_secs != self.cfg.idle_secs;
        self.cfg = cfg;
        self.set_note(|g| {
            g.idle_secs = self.cfg.idle_secs;
            if !self.cfg.usable() && g.why.is_empty() {
                g.why = "no kept-browser driver here".to_string();
            }
            if changed_idle && g.alive {
                g.idle_left = Some(self.cfg.idle_secs as i64);
                g.idle_deadline = Some(now_secs() + self.cfg.idle_secs as i64);
            }
        });
    }
}

impl std::fmt::Debug for Hub {
    /// Describes the session, not the process handles inside it: a `Debug` line that printed
    /// a child handle and a lock would be noise, and the state is the part worth reading.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.peek();
        write!(f, "browser_hub::Hub({})", if s.alive { format!("alive at {}", s.url) } else { format!("closed ({})", s.why) })
    }
}

/// Short, human words for what an act did, so the panel can say `clicked "More"` rather
/// than echoing a JSON object.
fn describe_act(act: &Value) -> String {
    let what = act.get("do").and_then(|d| d.as_str()).unwrap_or("?");
    let target = act.get("target").and_then(|t| t.as_str()).unwrap_or("");
    match what {
        "click" if !target.is_empty() => format!("clicked {target:?}"),
        "fill" if !target.is_empty() => format!("typed into {target:?}"),
        "press" => format!("pressed {}", act.get("key").and_then(|k| k.as_str()).unwrap_or("Enter")),
        "scroll" => format!("scrolled {}px", act.get("px").and_then(|p| p.as_u64()).unwrap_or(600)),
        // The two the person's panel uses. They are said here because the panel's own activity line
        // reads this function: "you clicked at (120, 200)" is the sentence that makes a picture
        // click traceable, and a description invented in the UI is one the engine cannot check.
        "click_xy" => format!(
            "clicked at ({}, {})",
            act.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0).round(),
            act.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0).round()
        ),
        "type_keys" => format!(
            "typed {} character(s) into the page",
            act.get("text").and_then(|t| t.as_str()).unwrap_or("").chars().count()
        ),
        "reload" => "reloaded the page".to_string(),
        "back" => "went back".to_string(),
        "forward" => "went forward".to_string(),
        "stop" => "stopped loading".to_string(),
        // A step of a walk, not a button: the page this browser is showing changed to another address.
        // Named here so the panel's line and the engine's record say the same thing about it.
        "navigate" => format!(
            "opened {}",
            act.get("url").and_then(|u| u.as_str()).unwrap_or("another page")
        ),
        "eval" => "read something out of the page".to_string(),
        "text" | "snapshot" => "read the page".to_string(),
        other => format!("{other}"),
    }
}

/// One way to drive the kept browser, whichever process is asking.
///
/// The tool path is written against this rather than against [`Hub`] so that the Rust core
/// (which owns the browser) and a pi-core tool host (which reaches it over a socket) run the
/// *same* sequence and get the same answers. Two sequences is how one of them ends up
/// clicking something the other never did.
#[derive(Clone)]
pub enum Link {
    InProcess(Arc<tokio::sync::Mutex<Hub>>),
    Socket(std::path::PathBuf),
}

impl Link {
    pub async fn open(&self, url: &str, who: Who, allow_file: bool) -> Result<Value, String> {
        match self {
            Link::InProcess(h) => h.lock().await.open(url, who, allow_file).await,
            Link::Socket(p) => socket::ask(p, &json!({"cmd": "open", "url": url, "who": who_key(who), "allow_file": allow_file}), socket::CALL_TIMEOUT).await,
        }
    }

    pub async fn act(&self, act: Value, who: Who) -> Result<Value, String> {
        match self {
            Link::InProcess(h) => h.lock().await.act(act, who).await,
            Link::Socket(p) => {
                let mut req = json!({"cmd": "act", "who": who_key(who)});
                req["act"] = act;
                socket::ask(p, &req, socket::CALL_TIMEOUT).await
            }
        }
    }

    /// What the page printed, threw, and failed to load — read from the hub's own record, so this
    /// costs nothing at the page and does not count as touching it. A model that has to poll the
    /// page to find out whether it broke is a model that stops checking; this is the reason it does
    /// not have to.
    pub async fn diagnostics(&self, since: i64, kind: &str, level: &str, limit: usize) -> Result<Value, String> {
        let kind = if kind.is_empty() || kind == "all" { None } else { Some(kind) };
        let level = if level.is_empty() || level == "all" { None } else { Some(level) };
        match self {
            // A plain memory read of the mirror the hub keeps. No lock on the page, no act, and no
            // reset of the idle clock — reading about a browser is not using it.
            Link::InProcess(h) => Ok(h.lock().await.peek().diagnostics_value(since, kind, level, limit)),
            Link::Socket(p) => socket::ask(p, &json!({"cmd": "diagnostics", "since": since, "kind": kind, "level": level, "limit": limit}), socket::CALL_TIMEOUT).await,
        }
    }

    pub async fn state(&self) -> Result<Value, String> {
        match self {
            Link::InProcess(h) => match h.lock().await.state().await {
                Ok(s) => Ok(socket::state_value(&s)),
                Err(e) => Err(e),
            },
            Link::Socket(p) => socket::ask(p, &json!({"cmd": "state"}), socket::CALL_TIMEOUT).await,
        }
    }

    pub async fn frame(&self, png: bool) -> Result<Value, String> {
        match self {
            Link::InProcess(h) => match h.lock().await.frame(png).await {
                Ok(f) => Ok(json!({"ok": true, "b64": f.b64, "mime": f.mime, "bytes": f.bytes, "seq": f.seq})),
                Err(e) => Err(e),
            },
            Link::Socket(p) => socket::ask(p, &json!({"cmd": "frame", "type": if png { "png" } else { "jpeg" }}), socket::CALL_TIMEOUT).await,
        }
    }
}

fn who_key(who: Who) -> &'static str {
    match who {
        Who::Person => "person",
        _ => "model",
    }
}

/// The payload every screen reads, built in one place.
///
/// `writable` is a parameter rather than a constant because the two transports differ in
/// one fact only: a phone tab cannot drive a browser that lives on the machine running the
/// tools, and a page that shows controls which will be refused is worse than one that says
/// which machine the browser belongs to.
pub fn status_payload(cfg: &SessionCfg, extra: &Snapshot, mounted: bool, writable: bool, route: &str) -> Value {
    json!({
        "mounted": mounted,
        "driver": cfg.session_driver.as_ref().map(|p| p.display().to_string()),
        "node": cfg.node,
        "keep_off": cfg.off_reason.is_some(),
        "off_reason": cfg.off_reason.clone(),
        "idle_secs": cfg.idle_secs,
        // The clock, read now: a panel that shows "4:12 left" off a value cached at the last
        // act is a panel that is wrong about the one thing the person is asking about.
        "idle_left": extra.idle_left_now(),
        "alive": extra.alive,
        "opened": extra.opened,
        "url": extra.url,
        "title": extra.title,
        "opened_by": format!("{:?}", extra.opened_by).to_lowercase(),
        "last_actor": format!("{:?}", extra.last_actor).to_lowercase(),
        "last_act": extra.last_act,
        "why": extra.why,
        "console": extra.console.iter().map(|l| json!({"kind": l.kind, "text": l.text})).collect::<Vec<_>>(),
        "page_errors": extra.page_errors,
        "requests_failed": extra.requests_failed,
        "downloads_blocked": extra.downloads_blocked,
        "frames_taken": extra.frames_taken,
        // The page's own record, as numbers. The records themselves are one command away
        // (`browser_log`, and the model's `page_log`) rather than in every poll: a shut drawer has
        // no reason to carry 80 of them over the wire once a second, and a badge that counted
        // something other than what the drawer shows would be a badge nobody trusts.
        "nav": extra.nav,
        "diag_seq": extra.diag_seq,
        "diag_counts": extra.diag_counts().0,
        "diag_failed": extra.diag_counts().1,
        // What kind of view this is: the browser pushing its own frames, or stills somebody asked
        // for. A panel that says "live" when nothing is streaming is making a promise it is not
        // keeping, which is why the flag comes from the driver's answer rather than from the panel
        // having asked.
        "live": extra.live,
        "live_why": extra.live_why,
        "native": extra.native,
        "frame_pic": extra.frame_pic,
        "viewport": {"width": extra.viewport_w, "height": extra.viewport_h},
        // The page widget's real box, from the widget tree. Zero in both directions means the page
        // was never given a place, which the panel says rather than drawing an empty frame.
        "want": { "x": extra.want_x, "y": extra.want_y, "width": extra.want_w, "height": extra.want_h },
        "placed": {
            "x": extra.placed_x,
            "y": extra.placed_y,
            "width": extra.placed_w,
            "height": extra.placed_h,
            "shown": extra.placed_shown,
        },
        "seq": extra.seq,
        "writable": writable,
        "route": route,
        "says": panel_says(cfg, extra),
    })
}

/// What the panel says about this browser, in one sentence.
///
/// Split from [`status_line`] because the two are asked different questions: a settings row asks
/// what this start *would* do, the panel asks what is true *now*. They only have to agree when
/// nothing is open — and when nothing is open, the panel is where a person looks for the reason,
/// so it gets the config sentence ("switched off with `keep = false`", "no session driver here,
/// and this is how one goes in") rather than a bare "no browser is open."
pub fn panel_says(cfg: &SessionCfg, snap: &Snapshot) -> String {
    // A page that is on the screen needs no description of a picture of it.
    if snap.native && snap.alive && snap.opened {
        return format!(
            "the page is open in this window at {}{} — you can click it, and the model can drive the same page",
            snap.url,
            if snap.title.is_empty() { String::new() } else { format!(" (\"{}\")", snap.title) }
        );
    }
    if snap.alive {
        return snap.says();
    }
    // A reason for *this* moment — it closed itself, the driver died, the page refused — is the
    // headline, because that is the question the panel is being asked. What this start is
    // allowed to do comes after it, and only if it adds something.
    let head = snap.says();
    if !snap.why.is_empty() {
        let mut out = head;
        if let Some(r) = cfg.off_reason.as_ref() {
            out.push_str(&format!(" Kept browsers are switched off here ({r})."));
        } else if !cfg.usable() {
            out.push(' ');
            out.push_str(&status_line(cfg, snap));
        }
        return out;
    }
    if let Some(r) = cfg.off_reason.as_ref() {
        return format!("no browser is open, and none can be: kept browsers are switched off ({r}).");
    }
    if !cfg.usable() {
        return status_line(cfg, snap);
    }
    head
}

/// The one-line answer for a settings row, in the engine's words.
pub fn status_line(cfg: &SessionCfg, snap: &Snapshot) -> String {
    if let Some(r) = cfg.off_reason.as_ref() {
        return format!(
            "Kept browsers are switched off here: {r}. The one-shot `browse` tool still works; \
             the sidebar has nothing to show because there is no browser being kept."
        );
    }
    if !cfg.usable() {
        // The install commands are the point of this sentence: an agent handed a bare "no
        // browser" tries harder in ways that are worse, and a person handed one can fix it.
        return [
            "The kept browser is not mounted: `browser/session.mjs` is not here, so the sidebar",
            "has no browser to show. It comes with cwoffee; its dependencies come from",
            "`npm --prefix browser install` and the browser itself from",
            "`npx playwright install chromium`.",
        ]
        .join(" ");
    }
    let mut s = format!(
        "The kept browser is mounted (driver {}, node {}). It closes itself after {} of \
         nobody touching it; a person's [browser] idle_secs changes that.",
        cfg.session_driver.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        cfg.node,
        idle_words(cfg.idle_secs),
    );
    if snap.alive || !snap.why.is_empty() {
        s.push(' ');
        s.push_str(&snap.says());
    }
    s
}

/// Does a viewer holding the picture with id `since` need bytes?
///
/// Two conditions, and they are not the same question:
///
/// * **a newer picture exists** — with the live view on, the browser draws a frame every time the
///   page repaints, so an animation, a video, or the model's own scroll produces pictures with no
///   act in front of them. The picture ids count those; the state sequence does not.
/// * **the newest picture still pictures the newest state** — with no live view, an act (a reload,
///   a click) changes the page without drawing anything, so the newest frame on hand predates what
///   the page now looks like and somebody has to take one. Compare the frame against the *state*
///   and a viewer still holding the picture from before a reload gets told it already has the
///   newest: that is the bug this rule exists to prevent.
///
/// Both shells call this rather than writing the comparison themselves, because the two numbers
/// look alike on the wire and mean different things.
pub fn needs_pic(since: u64, newest_pic: u64, newest_pictures_state: bool) -> bool {
    newest_pic > since || !newest_pictures_state
}

/// The engine's words for an idle span, so a person who configured `45` reads "45 seconds"
/// and one who configured `300` reads "5 minutes" — in both the tool's report and the panel.
pub fn idle_words(s: u64) -> String {
    if s >= 60 && s % 60 == 0 {
        format!("{} minute{}", s / 60, if s == 60 { "" } else { "s" })
    } else if s >= 60 {
        format!("{:.1} minutes", s as f64 / 60.0)
    } else {
        format!("{s} second{}", if s == 1 { "" } else { "s" })
    }
}

/// A page that moved is something the app knows and the page never said, so nobody puts it in the
/// log. It goes in here anyway: "where has this walk been, and did that click take me somewhere?" is
/// the first question a browsing model asks, and a `page_log` that answers `kind: navigation` with
/// nothing is a log missing the interesting half.
/// A driver that speaks the flat vocabulary — `console`, `pageerror`, `requestfailed` — still lands
/// a structured record. Without this, the same page's complaint is visible in a panel column and
/// invisible to the model's `page_log`, which is two readers of one fact disagreeing: the person
/// sees the error, the model insists the page said nothing.
fn note_flat(
    g: &mut Snapshot,
    kind: &str,
    level: &str,
    text: &str,
    url: Option<&str>,
    status: Option<u32>,
    at: Option<&str>,
) {
    if text.is_empty() && url.map(|u| u.is_empty()).unwrap_or(true) {
        return;
    }
    g.diag_seq += 1;
    g.diagnostics.push(Diag {
        seq: g.diag_seq as i64,
        t: now_secs() * 1_000,
        kind: kind.to_string(),
        level: level.to_string(),
        text: text.to_string(),
        url: url.filter(|u| !u.is_empty()).map(|u| u.to_string()),
        status,
        method: None,
        at: at.map(|a| a.to_string()),
        nav: g.nav as i64,
        // Empty: the page said this about itself, relayed by its driver. `hub` marks the records
        // only the app could know.
        source: String::new(),
    });
    let over = g.diagnostics.len().saturating_sub(DIAG_CAP);
    if over > 0 {
        g.diagnostics.drain(0..over);
    }
}

/// The flat path's console line type translated to a record level. `warn` and `warning` are the same
/// word said two ways, and neither is a failure.
fn console_level(kind: &str) -> &'static str {
    match kind {
        "error" => "error",
        "warn" | "warning" => "warning",
        _ => "info",
    }
}

fn note_move(g: &mut Snapshot, how: &str, url: &str, title: &str) {
    if url.is_empty() {
        return;
    }
    g.diag_seq += 1;
    g.diagnostics.push(Diag {
        seq: g.diag_seq as i64,
        t: now_secs() * 1_000,
        kind: "navigation".into(),
        level: "info".into(),
        text: format!(
            "{how} to {url}{}",
            if title.is_empty() { String::new() } else { format!(" — \"{title}\"") }
        ),
        url: Some(url.to_string()),
        nav: g.nav as i64,
        source: "hub".into(),
        ..Default::default()
    });
    let over = g.diagnostics.len().saturating_sub(DIAG_CAP);
    if over > 0 {
        g.diagnostics.drain(0..over);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where a session's driver lives beside an installed one-shot driver. Exposed for tests and
/// for the two mains, which look for both files in the same places.
pub fn session_driver_beside(driver: &Path) -> Option<PathBuf> {
    let sib = driver.parent()?.join("session.mjs");
    sib.is_file().then_some(sib)
}

pub mod downloads;
pub mod socket;

#[cfg(test)]
mod tests;
