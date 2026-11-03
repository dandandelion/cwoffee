//! Cwoffee TUI — a thin skin over `cwoffee_engine::host::SessionHost`.
//!
//! Everything here is rendering and keybinding. There is no agent logic, no
//! provider logic, no persistence logic in this file: if a behaviour is wrong,
//! it is wrong in `crates/engine`, and the Tauri frontend will be wrong the same
//! way. That is the point of keeping the engine a library.
//!
//! Rendering note, because it is the difference between a smooth stream and a
//! janky one: we block for one event, then drain every event already queued and
//! draw once. A 50 tok/s stream produces ~50 events/s; drawing per event would
//! re-flatten and re-wrap the whole transcript 50 times a second for no gain.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use cwoffee_engine::approval::{ApprovalAnswer, ApprovalMode, ApprovalRequest};
use std::collections::VecDeque;

mod commands;

use cwoffee_engine::tools::ask::{Answer, Question};
use cwoffee_engine::event::{EngineEvent, ExecStream, FinishReason};
use cwoffee_engine::host::{HostCmd, HostConfig, HostOps, SessionHost};
use cwoffee_engine::provider::Reasoning;
use cwoffee_engine::store::Store;
use cwoffee_engine::telemetry::{Rolling, TurnTelemetry};
use cwoffee_engine::tools::Registry;
use cwoffee_engine::Config;

use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Item {
    User(String),
    Reasoning(String),
    Assistant(String),
    Tool {
        name: String,
        args: String,
        output: String,
        running: bool,
        ok: bool,
        exit: Option<i32>,
    },
    Note(String),
    Warn(String),
    Fail(String),
}

struct App {
    items: Vec<Item>,
    input: String,
    /// Scroll offset in wrapped lines from the bottom. None = follow the tail.
    scroll_up: Option<usize>,
    tel: TurnTelemetry,
    /// Rolling averages over recent turns, not lifetime: turn 1 of a session is
    /// cold-cache and would drag a lifetime mean somewhere it does not belong.
    avg: Rolling,
    ctx_tokens: usize,
    compactions: u32,
    mode: ApprovalMode,
    reasoning: Reasoning,
    busy: bool,
    session_id: i64,
    pending: Option<(ApprovalRequest, oneshot::Sender<ApprovalAnswer>)>,
    /// A question from `ask_user`. It owns the keyboard the same way an
    /// approval does: the turn is parked until someone answers.
    pending_q: Option<(Question, oneshot::Sender<Answer>)>,
    /// A model that asks several questions in one call sends them together, and the
    /// terminal can only put one on screen at a time. The rest wait here: overwriting
    /// `pending_q` would drop the earlier reply sender, and the engine would read that
    /// as the question being cancelled - the whole set would collapse because the
    /// terminal had nowhere to put the second question.
    q_tail: VecDeque<(Question, oneshot::Sender<Answer>)>,
    /// Indices picked in the question that is up. Kept apart from the question
    /// itself so a digit toggles a choice without touching the engine's copy.
    q_sel: Vec<usize>,
    quit: bool,
    model: String,
    /// What is waiting in the mid-turn queue, as the host last said. The composer's title counts
    /// them; the words themselves are in the input box only after a stop hands them back.
    queued: Vec<cwoffee_engine::steer::View>,
    /// Which palette this terminal is painting in, and how it got chosen. A terminal has no media
    /// query, so `system` is resolved from `COLORFGBG` where a terminal reports one — and when it
    /// does not, the dark palette is said to be a default rather than claimed as a followed
    /// preference.
    palette: Palette,
    /// Which kind the next mid-turn press is, as the host says it. The input box's title prints it,
    /// so a reader knows what Enter is promising before pressing it.
    mid_turn: cwoffee_engine::steer::MidTurn,
    /// A `/undo` or `/forget` listing the terminal is waiting for an answer on. It owns the keyboard
    /// the way an approval card does: nothing else is sent until it is answered or refused.
    pending_cmd: Option<commands::Pending>,
    /// The nodes `/tree` last listed, so `/leaf N` addresses one of them by its own words rather
    /// than by a position the terminal would have to re-derive.
    tree_nodes: Vec<cwoffee_engine::tree::TreeNode>,
    /// The person's own messages, newest last. `/branch` repeats one of them back to the engine,
    /// which is the proof that the row meant is the row shown.
    user_rows: Vec<String>,
    /// Set when Ctrl-Q walks away from unsent words, so the shell prints it after the screen is
    /// restored — the last thing the terminal can say.
    quit_note: Option<String>,
    /// The hub this start spawned, when the config asked for one. `/mcp` reads it; nothing here
    /// starts a server somebody else wrote.
    mcp: Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
    mcp_route: String,
}

impl App {
    fn new(model: String) -> Self {
        Self {
            items: Vec::new(),
            input: String::new(),
            scroll_up: None,
            tel: TurnTelemetry::default(),
            avg: Rolling::new(8),
            ctx_tokens: 0,
            compactions: 0,
            mode: ApprovalMode::Ask,
            reasoning: Reasoning::XHigh,
            busy: false,
            session_id: 0,
            pending: None,
            pending_q: None,
            q_tail: VecDeque::new(),
            q_sel: Vec::new(),
            quit: false,
            model,
            queued: Vec::new(),
            palette: Palette::dark(),
            mid_turn: cwoffee_engine::steer::MidTurn::default(),
            pending_cmd: None,
            tree_nodes: Vec::new(),
            user_rows: Vec::new(),
            quit_note: None,
            mcp: None,
            mcp_route: "none - no server is running in this start".to_string(),
        }
    }

    fn apply(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::TurnFiles { files, .. } => {
                for f in &files {
                    self.items.push(Item::Note(format!(
                        "file · {} · {} bytes",
                        f.path, f.bytes
                    )));
                }
            }
            // The name arriving, one paint per token. The TUI has no conversation row to
            // type into, and the settled name arrives right behind this as
            // `SessionChanged`, which is already narrated. So: deliberately silent here,
            // and listed rather than swallowed by a wildcard so the protocol stays auditable.
            EngineEvent::TitleDelta { .. } => {}
            EngineEvent::SessionChanged { session_id, title } => {
                self.items.push(Item::Note(format!(
                    "── session {session_id}{} ──",
                    if title.is_empty() { String::new() } else { format!(" · {title}") }
                )));
            }
            // Live mid-turn estimates. The TUI prints its telemetry line when the
            // turn ends; redrawing a status line a few times a second would fight
            // the transcript for no information the final line does not carry.
            EngineEvent::TurnStats(_) => {}
            EngineEvent::TurnStart { turn_id, model, .. } => {
                self.busy = true;
                self.tel = TurnTelemetry::default();
                self.items.push(Item::Note(format!("── turn {turn_id} · {model} ──")));
            }
            EngineEvent::Reasoning(r) => {
                // Some models emit empty or whitespace-only reasoning deltas
                // between text deltas. Left in, they split one answer into
                // several blocks, each re-prefixed, which reads like a bug.
                if r.trim().is_empty() {
                    // nothing to render
                } else if let Some(Item::Reasoning(buf)) = self.items.last_mut() {
                    buf.push_str(&r);
                } else {
                    self.items.push(Item::Reasoning(r));
                }
            }
            EngineEvent::Text(t) => {
                // A blank reasoning block sitting between two text deltas would
                // otherwise start a second assistant block mid-sentence.
                if matches!(self.items.last(), Some(Item::Reasoning(b)) if b.trim().is_empty()) {
                    self.items.pop();
                }
                if let Some(Item::Assistant(buf)) = self.items.last_mut() {
                    buf.push_str(&t);
                } else {
                    self.items.push(Item::Assistant(t));
                }
            }
            EngineEvent::ToolCallBegin { id: _, name } => {
                self.items.push(Item::Tool {
                    name,
                    args: String::new(),
                    output: String::new(),
                    running: true,
                    ok: false,
                    exit: None,
                });
            }
            EngineEvent::ToolCallArgs(_) => {}
            EngineEvent::ToolCallReady { args, .. } => {
                if let Some(Item::Tool { args: slot, .. }) = self.items.last_mut() {
                    *slot = args.to_string();
                }
            }
            EngineEvent::ToolCallParseError { raw, message, .. } => {
                self.items.push(Item::Fail(format!("tool args unparseable: {message}\n  raw: {raw}")));
            }
            EngineEvent::ToolExecStart { .. } => {}
            EngineEvent::ToolExecOutput { stream, chunk, .. } => {
                if let Some(Item::Tool { output, .. }) = self.items.last_mut() {
                    if stream == ExecStream::Stderr && !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&chunk);
                }
            }
            EngineEvent::ToolExecEnd { ok, exit_code, .. } => {
                if let Some(Item::Tool { ok: slot, exit, running, .. }) = self.items.last_mut() {
                    *slot = ok;
                    *exit = exit_code;
                    *running = false;
                }
            }
            // The TUI answers approvals from `ap_rx` directly; this variant
            // exists for front ends that consume a single merged stream.
            EngineEvent::ApprovalRequested { .. } => {}
            // Same for questions: they arrive on their own channel, where the
            // reply sender is attached. The event copy carries no way to answer.
            EngineEvent::QuestionRequested { .. } | EngineEvent::QuestionAnswered { .. } => {}
            // The tool stopped waiting: its timeout expired, or the turn was
            // cancelled with the question parked. The prompt on screen cannot be
            // answered any more, so drop it. Holding the sender would leave a
            // key handler that sends into a closed channel.
            EngineEvent::QuestionExpired { id, .. } => {
                if self.pending_q.as_ref().is_some_and(|(q, _)| q.id == id) {
                    self.pending_q = None;
                    self.items
                        .push(Item::Note("the question closed without an answer".into()));
                    promote(self);
                } else if let Some(at) = self
                    .q_tail
                    .iter()
                    .position(|(q, _)| q.id == id)
                {
                    // The engine stopped waiting for a question that has not been
                    // shown yet. Dropping it here is what keeps the terminal from
                    // prompting about a call that has already returned.
                    self.q_tail.remove(at);
                }
            }
            // Mid-turn steering. The pending list is kept as state (the input box names it); a
            // landing becomes its own line in the transcript, because the model really was told it,
            // and a turn that stopped hands the words back to the input.
            EngineEvent::QueueChanged { items, .. } => {
                self.queued = items;
            }
            EngineEvent::SteerLanded { kind, text, step, typed_at, .. } => {
                self.items.push(Item::User(text));
                // The row is stamped at the press — `typed_at` is when Enter was hit, not when the
                // model got round to reading it — and the receipt says which step took it. Both
                // halves are the engine's wording, so a terminal and a window describe the same
                // gesture in the same words.
                self.items.push(Item::Note(format!(
                    "landed at your press ({}): {}",
                    crate::commands::clock(typed_at),
                    cwoffee_engine::steer::landing_note(kind, step)
                )));
            }
            EngineEvent::QueueReturned { items, .. } => {
                for i in &items {
                    if !self.input.is_empty() {
                        self.input.push('\n');
                    }
                    self.input.push_str(i.text.as_str());
                }
                self.items.push(Item::Note(cwoffee_engine::steer::returned_note(items.len())));
            }
            EngineEvent::Usage(u) => {
                self.avg.push(u.clone());
                self.tel = u;
            }
            EngineEvent::Note { message, .. } => self.items.push(Item::Note(message)),
            EngineEvent::Error { message, fatal } => {
                if fatal {
                    self.items.push(Item::Fail(message));
                } else {
                    self.items.push(Item::Warn(message));
                }
            }
            EngineEvent::TurnEnd { finish, .. } => {
                self.busy = false;
                if finish != FinishReason::Stop {
                    self.items.push(Item::Note(format!("turn ended: {finish:?}")));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

struct Args {
    /// Everything here is an *override*. Unset means "whatever the config file
    /// and environment produced", so precedence stays CLI > env > file > default
    /// in one readable place.
    config: Option<PathBuf>,
    url: Option<String>,
    model: Option<String>,
    workspace: Option<PathBuf>,
    db: PathBuf,
    mode: Option<String>,
    kind: Option<String>,
    context: Option<usize>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        config: None,
        url: None,
        model: None,
        workspace: None,
        db: dirs_data_dir().join("sessions.db"),
        mode: None,
        kind: None,
        context: None,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            argv.get(*i).cloned().ok_or_else(|| anyhow::anyhow!("{} needs a value", argv[*i - 1]))
        };
        match argv[i].as_str() {
            "--config" => a.config = Some(PathBuf::from(next(&mut i)?)),
            "--url" => a.url = Some(next(&mut i)?),
            "--model" => a.model = Some(next(&mut i)?),
            "--workspace" | "-C" => a.workspace = Some(PathBuf::from(next(&mut i)?)),
            "--db" => a.db = PathBuf::from(next(&mut i)?),
            "--mode" => {
                let v = next(&mut i)?;
                if ApprovalMode::parse(&v).is_none() {
                    anyhow::bail!("mode must be ask | session | auto");
                }
                a.mode = Some(v);
            }
            "--openai" => a.kind = Some("openai".into()),
            "--anthropic" => a.kind = Some("anthropic".into()),
            "--context" => a.context = Some(next(&mut i)?.parse()?),
            "--help" | "-h" => {
                println!(
                    "cwoffee [options]\n  \
                     --config PATH      config file (default ~/.config/cwoffee/config.toml)\n  \
                     --url URL          API root (default http://127.0.0.1:8090/v1)\n  \
                     --model NAME       model id sent to the API\n  \
                     -C, --workspace D  workspace directory (default cwd)\n  \
                     --db PATH          sqlite path\n  \
                     --mode M           ask | session | auto\n  \
                     --openai | --anthropic\n  \
                     --context N        prompt token budget\n\n  \
                     precedence: flags > environment > config file > defaults\n\n  \
                     keys: enter sends (and while a turn runs, queues a mid-turn message: \
                     steer or follow-up, whichever `mid_turn` says) · x takes the oldest waiting \
                     message back · ctrl-c cancel or quit · ctrl-q quit (it names any words left \
                     unsent) · esc quit · pgup/pgdn scroll · ctrl-a cycle approval mode · \
                     ctrl-r cycle thinking · ctrl-u empty the composer · ctrl-l clear the \
                     transcript\n\n  \
                     doors: /branch [N] · /tree · /leaf N · /files · /undo · /forget · /mcp · /help\n\n  \
                     an approval card answers y/a/n only while the composer is empty: typing goes \
                     to the composer, and a press there queues a message beside the card without \
                     answering it"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?} (try --help)"),
        }
        i += 1;
    }
    Ok(a)
}

fn dirs_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
        });
    base.join("cwoffee")
}

#[tokio::main]
async fn main() -> Result<()> {
    let flags = parse_args()?;
    // What the start knows before the screen exists. These used to go to stderr, and a pty that
    // was already painting interleaved them into the header line: tmux's grid showed
    // `Cwoffee  ccwoffee-mockt ·20sessionn0`, which is the same corruption a person would call a
    // broken window. Boot facts are transcript lines now, so they are readable inside the app that
    // is reporting them — including by the gate, which drives a pty and has no other way to see
    // that a server it configured came back dead.
    let mut boot: Vec<String> = Vec::new();

    for note in cwoffee_engine::legacy::migrate_legacy_dirs() {
        boot.push(note);
    }

    // precedence: CLI flags > environment > config file > defaults
    let (mut appcfg, loaded) = Config::load(flags.config.as_deref())
        .map_err(|e| anyhow::anyhow!(e))?;
    appcfg.apply_env();
    if let Some(v) = flags.url {
        appcfg.provider.base_url = v;
    }
    if let Some(v) = flags.model {
        appcfg.provider.model = v;
    }
    if let Some(v) = flags.mode {
        appcfg.mode = Some(v);
    }
    if let Some(v) = flags.kind {
        appcfg.provider.kind = v;
    }
    if let Some(v) = flags.context {
        appcfg.context.target_tokens = Some(v);
    }
    if let Some(p) = &loaded {
        boot.push(format!("config: {}", p.display()));
    }

    // A CLI user expects the workspace to be the directory they launched from;
    // the desktop app deliberately does *not* do this, because a WM-launched GUI
    // inherits an arbitrary CWD.
    let workspace = flags
        .workspace
        .clone()
        .or_else(|| appcfg.workspace.as_ref().map(PathBuf::from))
        .unwrap_or(std::env::current_dir()?);

    if let Some(parent) = flags.db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Arc::new(Store::open(&flags.db)?);

    let provider: std::sync::Arc<dyn cwoffee_engine::provider::Provider> = appcfg.build_provider()
        .map_err(|e| anyhow::anyhow!(e))?;

    let model = if appcfg.provider.model.is_empty() {
        "local".to_string()
    } else {
        appcfg.provider.model.clone()
    };

    let mut cfg = HostConfig::new(model, workspace.clone());
    cfg.cfg_path = Some(Config::default_path());
    cfg.mode = appcfg.mode();
    // The thinking level is written back by the host when it is changed, so it is
    // read back here as well. A word that is not a level stops the TUI at boot
    // rather than running the model at a level nobody chose.
    if let Some(r) = appcfg.reasoning_checked().map_err(anyhow::Error::msg)? {
        cfg.reasoning = r;
    }
    cfg.context_budget = appcfg.context.target_tokens;
    // The prompt states the egress mode; take it from the same ExecCfg the tool
    // registry was built with rather than restating it here.
    cfg.egress = appcfg.exec_cfg().net.mode;
    // None means the server's own output cap applies; see Config.
    cfg.max_tokens = appcfg.max_output_tokens;
    // 0 = no cap. The hardcoded 14 was cutting real tasks short; see Config.
    cfg.max_tool_iters = appcfg.max_tool_iters();
    // Validated at boot, not per request: see Config::sampling_checked.
    cfg.sampling = appcfg.sampling_checked().map_err(anyhow::Error::msg)?;
    // The exec sandbox's egress block, validated here so a mode word that does not
    // parse stops the app rather than silently becoming the default.
    appcfg.net_checked().map_err(anyhow::Error::msg)?;
    cfg.question_timeout = appcfg.question_timeout();
    appcfg.question_timeout_checked().map_err(anyhow::Error::msg)?;
    // Derived per turn from the live workspace; see SessionHost.
    cfg.env_override = None;
    cfg.approval_timeout =
        std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    // What a message typed during a turn promises. The same file, the same word, the same host:
    // a terminal and a desktop window pressing Enter in one conversation are given one answer.
    cfg.mid_turn = appcfg.mid_turn();
    // Third-party tools, started from the config exactly as the desktop and the gateway start them:
    // the hub lives in this process, the catalog is the sentence the env block carries, and the
    // tools stay registered-but-hidden until `explore` names them. A TUI that read `[mcp]` and did
    // nothing with it — which is what this used to be — answers "No MCP server is running" about
    // servers the person did configure.
    let mcp_cfg = appcfg.mcp_cfg();
    if let Err(e) = Config::mcp_problem(&mcp_cfg) {
        boot.push(format!(
            "[mcp] config is bad: {e} - fix it in {}",
            Config::default_path().display()
        ));
    }
    let mcp_wanted = mcp_cfg.has_servers();
    if !mcp_wanted && !mcp_cfg.servers.is_empty() {
        boot.push(format!(
            "[mcp] lists {} server(s) and none is enabled; nothing spawned",
            mcp_cfg.servers.len()
        ));
    }

    let mode = cfg.mode;
    let reasoning = cfg.reasoning;
    let model = cfg.model.clone();

    let mut reg = Registry::with_v1_tools_exec(appcfg.search_backend(), appcfg.exec_cfg());
    let mut mcp_hub: Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>> = None;
    let mut mcp_route = "none - no server is running in this start".to_string();
    if mcp_wanted {
        let hub = cwoffee_engine::mcp::Hub::start(&mcp_cfg).await;
        for (name, line) in hub.servers_named() {
            // A dead server is a sentence naming it, and the sentence is the hub's own state
            // rather than a re-read of the config file. `/mcp` repeats it; this is where it is
            // first read, because a person who configured `jira` should learn that at boot.
            boot.push(format!("mcp {name}: {line}"));
        }
        // The catalog sentence rides the env block, rebuilt per turn — never the system prompt,
        // which is the cache prefix. Same budget, same path, same ceiling as the desktop.
        cfg.mcp_catalog = hub.catalog_block();
        let hub = Arc::new(tokio::sync::Mutex::new(hub));
        let catalog = hub.lock().await.catalog();
        cwoffee_engine::mcp::install_into(&mut reg, &catalog, &hub);
        boot.push(format!(
            "mcp: {} third-party tool(s) registered, hidden behind explore (the catalog block is \
             {}B against a {}B ceiling)",
            catalog.len(),
            cfg.mcp_catalog.len(),
            cwoffee_engine::CATALOG_BYTE_BUDGET
        ));
        mcp_hub = Some(hub);
        mcp_route = "this process".to_string();
    }
    let mut host = SessionHost::spawn(provider, reg, Arc::clone(&store), cfg);

    // No config file means no model id; ask the endpoint rather than sending an
    // empty one and taking a 400.
    if let Some(m) = host.adopt_first_model().await {
        boot.push(format!("no model configured, using the endpoint's first: {m}"));
    }

    // Terminal setup is wrapped so a panic still leaves the shell usable.
    // The engine talks on stderr — the sampling line, a saved setting, a titling failure, a
    // search config it applied. A terminal that owns the screen cannot let those land on it: they
    // are written at whatever cell the cursor happens to be in, and the frame is corrupted until
    // the next repaint. Measured, not theorised: a pty run showed `Cwoffee  ccwoffee-mockt
    // ·20sessionn0` in the header because a boot note was painted over it. So fd 2 is pointed at a
    // log file for the length of the run and handed back on exit, where the lines are printed.
    let parked = park_stderr(&flags.db);
    if let Some(p) = &parked {
        boot.push(format!(
            "engine notes go to {} while this screen is up; they print when the terminal closes",
            p.path.display()
        ));
    }
    let terminal = ratatui::init();
    // A 0x0 terminal is not a rendering detail, it is an input we refuse. A pty
    // with no winsize set reports this, and a degenerate geometry is what let an
    // earlier wrap() loop allocate gigabytes and push the box into swap. Failing
    // here is honest; limping along is not.
    if let Ok(sz) = terminal.size() {
        if sz.width == 0 || sz.height == 0 {
            ratatui::restore();
            anyhow::bail!(
                "terminal reports {}x{} - Cwoffee needs a real terminal size. \
                 If you are driving it from a script or pty, set winsize first.",
                sz.width,
                sz.height
            );
        }
    }
    let result: Result<Option<String>> = run(
        terminal,
        &mut host,
        Arc::clone(&store),
        model,
        mode,
        reasoning,
        appcfg.mid_turn(),
        mcp_hub.clone(),
        appcfg.theme(),
        mcp_route.clone(),
        boot,
    )
    .await;
    ratatui::restore();
    // The screen is gone, so the engine's lines can be said out loud now.
    if let Some(p) = parked {
        p.release();
    }
    match result? {
        Some(note) => {
            eprintln!("cwoffee: {note}");
            Ok(())
        }
        None => Ok(()),
    }
}

/// Runs the terminal until the person quits. Ctrl-Q's note about words left unsent comes back to
/// `main`, which prints it once the screen is restored. Returns Ctrl-Q's note about words left unsent, so
/// the caller prints it after the screen is restored — the last thing the terminal can say.
async fn run(
    mut terminal: DefaultTerminal,
    host: &mut SessionHost,
    store: Arc<Store>,
    model: String,
    mode: ApprovalMode,
    reasoning: Reasoning,
    mid_turn: cwoffee_engine::steer::MidTurn,
    mcp: Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
    theme: cwoffee_engine::theme::Theme,
    mcp_route: String,
    mut boot: Vec<String>,
) -> Result<Option<String>> {
    let mut app = App::new(model);
    // The palette is the host's stored choice, and a terminal resolves `system` from the one signal
    // it has — `COLORFGBG`, where a terminal bothers to report it. Silence is reported as silence: a
    // default that reads as "followed the system" is the claim `crate::theme` refuses to make.
    let resolved = cwoffee_engine::theme::Theme::resolve_colorfgbg(
        theme,
        std::env::var("COLORFGBG").ok().as_deref(),
    );
    app.palette = Palette::of(resolved.theme);
    boot.push(resolved.says);
    // The start's own facts, in the transcript where they can be read after the screen is up.
    for note in boot {
        app.items.push(Item::Note(note));
    }
    app.mode = mode;
    app.mid_turn = mid_turn;
    app.mcp = mcp;
    app.mcp_route = mcp_route;
    // The level the file asked for is the level the chip shows. Showing the
    // constructor's default instead would put a number on screen that the model is
    // not running at - and the TUI persists changes, so the two would then fight.
    app.reasoning = reasoning;

    // The receivers move out of the host once: recv() needs &mut, and two
    // consumers would silently split one event stream.
    let (mut ev_rx, mut ap_rx) = host.take_receivers().expect("event receivers");
    let mut q_rx = host.take_questions().expect("question receiver");

    // Keyboard on a blocking thread: crossterm's read() is blocking, and running
    // it on the async runtime would stall the stream.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if key_tx.send(k).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    while !app.quit {
        dbg_tick(&app, &mut terminal);
        terminal.draw(|f| render(f, &app))?;

        // Block for one event, then drain everything already queued and draw
        // once. Drawing per token would re-wrap the transcript ~50x/s.
        tokio::select! {
            Some(k) = key_rx.recv() => handle_key(k, &mut app, host, &store).await,
            Some(ev) = ev_rx.recv() => app.apply(ev),
            Some((req, reply)) = ap_rx.recv() => {
                app.pending = Some((req, reply));
                app.busy = true;
            }
            Some((q, reply)) = q_rx.recv() => {
                if app.pending_q.is_some() {
                    app.q_tail.push_back((q, reply));
                } else {
                    preselect(&mut app.q_sel, &q);
                    app.pending_q = Some((q, reply));
                }
                app.busy = true;
            }
            else => break,
        }

        while let Ok(ev) = ev_rx.try_recv() {
            app.apply(ev);
        }
        let st = host.state();
        app.session_id = st.session_id;
        app.ctx_tokens = st.context_tokens;
        app.compactions = st.compactions;
        app.mode = st.mode;
        app.model = st.model.clone();
        // The queue and the mid-turn kind are the host's answer, read from the host — a terminal
        // that kept its own copy of "what is waiting" would be a second ledger, and the composer's
        // title would drift from what the engine is actually holding.
        app.queued = st.queued.clone();
        app.mid_turn = st.mid_turn;
    }
    Ok(app.quit_note)
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

async fn handle_key(k: KeyEvent, app: &mut App, host: &SessionHost, store: &Arc<Store>) {
    // A `/undo` or `/forget` listing owns the keyboard while it is up, exactly as an approval
    // does: the terminal must not let a stray Enter start a turn next to a confirmation, and it
    // must not treat `y` typed elsewhere as agreement.
    // A single-key answer is only a single-key answer while the composer is empty. Otherwise the
    // keys belong to typing, and a message that happens to start with `y` would answer a card or
    // agree to a revert. This is the same rule the desktop's card runs by, where typing goes to the
    // composer and only the button resolves the decision.
    // A single-key answer is only a single-key answer while the composer is empty. Otherwise the
    // keys belong to typing, and a message that happens to start with `y` would answer a card or
    // agree to a revert. This is the desktop card's rule, where typing goes to the composer and
    // only the button resolves the decision. Keys that are not the panel's own fall through to
    // typing — swallowing them would leave the composer empty forever and make every card key live.
    let free = app.input.is_empty();
    if app.pending_cmd.is_some() && free {
        let answer = match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            // Anything else that dismisses fails closed: a denial on a stray key is the rule the
            // approval prompt already follows.
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(yes) = answer {
            let p = app.pending_cmd.take().expect("checked above");
            match p {
                commands::Pending::Undo { preview } => {
                    push_lines(commands::undo_confirm(store.as_ref(), host, &preview, yes), app, host);
                }
                commands::Pending::Forget { shown } => {
                    if yes {
                        push_lines(
                            commands::forget_confirm(store.as_ref(), host, app.session_id, shown),
                            app,
                            host,
                        );
                    } else {
                        push_lines(
                            commands::cancel_pending(&commands::Pending::Forget { shown }),
                            app,
                            host,
                        );
                    }
                }
            }
            return;
        }
        // Not the panel's key, so it belongs to the composer. Swallowing every keystroke here means
        // the box never fills, which keeps the panel's own keys armed and lets the first letter of a
        // message answer a revert.
    }
    // A question owns the keyboard while it is up: 1-9 choose (or toggle, when
    // the question allows several), Enter sends, Esc declines. Declining sends an
    // empty answer, which the tool reports to the model as "dismissed without
    // answering" rather than as a choice it can invent.
    if app.pending_q.is_some() {
        // Nothing is sent until Enter. Submit-on-keypress sounded quick and was a
        // misclick machine: the answer to a question about deleting files should
        // not be one slip of the finger away.
        let submit = |app: &mut App| {
            let (q, reply) = app.pending_q.take().expect("checked above");
            let sel = app.q_sel.clone();
            app.q_sel.clear();
            let labels: Vec<String> = sel
                .iter()
                .filter_map(|i| q.options.get(*i).map(|o| o.label.clone()))
                .collect();
            let _ = reply.send(Answer {
                text: labels.join(", "),
                options: sel,
            });
            promote(app);
        };
        match k.code {
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c.to_digit(10).unwrap() as usize - 1;
                let q = app.pending_q.as_ref().expect("checked above").0.clone();
                if idx >= q.options.len() {
                    return; // a digit past the last option does nothing
                }
                if q.multi {
                    if let Some(at) = app.q_sel.iter().position(|x| *x == idx) {
                        app.q_sel.remove(at);
                    } else {
                        app.q_sel.push(idx);
                        app.q_sel.sort();
                    }
                } else {
                    app.q_sel = vec![idx];
                }
            }
            KeyCode::Enter => {
                if app.q_sel.is_empty() {
                    // Enter with nothing picked takes the recommendation, which is
                    // the whole point of marking one.
                    let recommended = app.pending_q.as_ref().and_then(|(q, _)| q.recommended);
                    if let Some(r) = recommended {
                        app.q_sel = vec![r];
                    } else {
                        return;
                    }
                }
                submit(app);
            }
            // Declining is explicit and sends an empty answer, which the tool
            // reports as "dismissed without answering" rather than as a choice the
            // model is free to invent.
            KeyCode::Esc | KeyCode::Char('c') => {
                let (_, reply) = app.pending_q.take().expect("checked above");
                app.q_sel.clear();
                let _ = reply.send(Answer::default());
                // Declining one question of a set does not decline the others: they
                // were asked separately and each gets its own answer or its own pass.
                promote(app);
            }
            _ => {}
        }
        return;
    }

    // An approval prompt answers with a key — but only while the composer is empty. A card that
    // swallows typing is a card that makes the mid-turn queue unreachable, and the desktop's card
    // does not do that: typing there goes to the composer and Enter queues a message beside the
    // card, answering nothing. Keys fall through to typing whenever there are words in the box.
    if app.pending.is_some() && free {
        let answer = match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalAnswer::Allow),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalAnswer::AllowSession),
            // Anything else that dismisses (n, Esc, Ctrl-C) denies: an approval
            // prompt must fail closed, including on a stray keypress.
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('c') => {
                Some(ApprovalAnswer::Deny)
            }
            _ => None,
        };
        if let Some(ans) = answer {
            let (req, reply) = app.pending.take().expect("checked above");
            let _ = reply.send(ans);
            app.items.push(Item::Note(format!(
                "{} ▸ {}",
                if ans == ApprovalAnswer::Deny { "denied" } else { "allowed" },
                req.summary
            )));
            return;
        }
        // Not a card key: it belongs to the composer. A card that swallows typing makes the mid-turn
        // queue unreachable while it is up, which is the divergence from the desktop this replaced —
        // there, typing goes to the box and only the button resolves the decision.
    }

    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            // Ctrl-Q quits even with words waiting — that is the person's decision, not the
            // terminal's — and says what it cost, naming the waiting items, after the screen is
            // restored. Vanishing typing without a trace is the failure this key exists to admit.
            KeyCode::Char('q') => {
                let mut waiting: Vec<String> = app
                    .queued
                    .iter()
                    .map(|v| format!("{} ({})", clip(&v.text, 40), v.kind.label()))
                    .collect();
                if !app.input.trim().is_empty() {
                    waiting.push(format!("unsent words in the input ({})", clip(&app.input, 40)));
                }
                if !waiting.is_empty() {
                    app.quit_note = Some(format!(
                        "quit with {} waiting, unsent: {}",
                        waiting.len(),
                        waiting.join("; ")
                    ));
                }
                app.quit = true;
            }
            KeyCode::Char('c') => {
                if app.busy {
                    host.cancel();
                    app.items.push(Item::Warn("cancelling…".into()));
                } else {
                    app.quit = true;
                }
            }
            KeyCode::Char('a') => {
                app.mode = match app.mode {
                    ApprovalMode::Ask => ApprovalMode::Session,
                    ApprovalMode::Session => ApprovalMode::Auto,
                    ApprovalMode::Auto => ApprovalMode::Ask,
                };
                host.send(HostCmd::SetMode { mode: app.mode });
            }
            KeyCode::Char('r') => {
                app.reasoning = match app.reasoning {
                    Reasoning::Off => Reasoning::Low,
                    Reasoning::Low => Reasoning::Medium,
                    Reasoning::Medium => Reasoning::High,
                    Reasoning::High => Reasoning::XHigh,
                    Reasoning::XHigh => Reasoning::Off,
                };
                host.send(HostCmd::SetReasoning { reasoning: app.reasoning, persist: false });
            }
            KeyCode::Char('u') => app.input.clear(),
            KeyCode::Char('l') => app.items.clear(),
            _ => {}
        }
        return;
    }

    match k.code {
        KeyCode::Esc => app.quit = true,
        KeyCode::Enter => {
            let text = std::mem::take(&mut app.input);
            if text.trim().is_empty() {
                return;
            }
            if let Some((cmd, rest)) = text.trim().split_once(char::is_whitespace) {
                if cmd.starts_with('/') {
                    run_slash(app, host, store, &cmd[1..], rest.trim()).await;
                    return;
                }
            } else if text.trim().starts_with('/') {
                run_slash(app, host, store, &text.trim()[1..], "").await;
                return;
            }
            if app.busy {
                // A press while the model is answering is a **mid-turn press**, not a second turn:
                // the host queues it as a steer or a follow-up, whichever `mid_turn` says, and the
                // announcement in the input box's title named that kind before Enter was pressed.
                // What still refuses mid-turn is every other door — branch, leaf, revert, forget —
                // and the engine's own sentence says so.
                match host.queue_mid_turn(text.clone(), Vec::new(), Some(app.session_id)) {
                    Ok(view) => {
                        app.items.push(Item::Note(format!(
                            "queued {}: {} — {}",
                            view.kind.label(),
                            view.text.chars().take(60).collect::<String>(),
                            view.note
                        )));
                    }
                    Err(why) => {
                        // The words go back where they came from. A refused press that eats the
                        // typing is the failure the feature was built to remove.
                        app.input = text;
                        app.items.push(Item::Warn(why));
                    }
                }
                return;
            }
            // The engine does not echo the user turn over the event stream,
            // so the transcript would swallow your own message without this.
            app.user_rows.push(text.clone());
            app.items.push(Item::User(text.clone()));
            app.scroll_up = None;
            host.submit(text);
        }
        // Take the oldest waiting mid-turn message back, before its boundary comes round.
        KeyCode::Char('x') if !app.queued.is_empty() => {
            let id = app.queued[0].id;
            match host.withdraw_queued(id) {
                Ok(view) => {
                    if !app.input.is_empty() {
                        app.input.push('\n');
                    }
                    app.input.push_str(&view.text);
                    app.items.push(Item::Note(format!(
                        "taken back: {} — those words were never sent",
                        view.text.chars().take(60).collect::<String>()
                    )));
                }
                Err(why) => app.items.push(Item::Warn(why)),
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) if k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT => {
            app.input.push(c);
        }
        KeyCode::PageUp => {
            let n = app.scroll_up.unwrap_or(0) + 10;
            app.scroll_up = Some(n);
        }
        KeyCode::PageDown => {
            app.scroll_up = match app.scroll_up {
                Some(n) if n > 10 => Some(n - 10),
                _ => None,
            };
        }
        KeyCode::End | KeyCode::Down => app.scroll_up = None,
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// slash commands — the doors, through the engine's own functions
// ---------------------------------------------------------------------------

use std::os::fd::{AsRawFd, FromRawFd};

/// fd 2 moved to a file for the length of a painting run, with the original held for the way back.
struct ParkedStderr {
    /// The fd stderr was pointing at before the swap — the terminal, normally.
    saved: std::os::fd::OwnedFd,
    path: PathBuf,
}

/// Returns `None` when the log cannot be opened, in which case stderr is left alone: an app that
/// cannot write a log has no business swallowing what the engine says.
fn park_stderr(db: &std::path::Path) -> Option<ParkedStderr> {
    let dir = db.parent().map(|p| p.to_path_buf()).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("cwoffee-tui-notes-{}.log", std::process::id()));
    let file = std::fs::File::create(&path).ok()?;
    let saved = unsafe {
        // dup(2) first: the original has to survive the swap so `release` can put it back.
        let fd = libc::dup(2);
        if fd < 0 {
            return None;
        }
        std::os::fd::OwnedFd::from_raw_fd(fd)
    };
    let ok = unsafe { libc::dup2(file.as_raw_fd(), 2) };
    if ok < 0 {
        return None;
    }
    // The File is dropped on purpose: fd 2 holds its descriptor, and the buffer is flushed by the
    // writer on each line because these are `eprintln!` calls.
    drop(file);
    Some(ParkedStderr { saved, path })
}

impl ParkedStderr {
    /// Put fd 2 back and print what the engine said while the screen was up.
    fn release(self) {
        unsafe {
            libc::dup2(self.saved.as_raw_fd(), 2);
        }
        match std::fs::read_to_string(&self.path) {
            Ok(text) if !text.trim().is_empty() => {
                eprintln!("cwoffee: engine notes from this run ({})", self.path.display());
                for line in text.lines() {
                    eprintln!("  {line}");
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("cwoffee: could not read the engine notes at {}: {e}", self.path.display()),
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The terminal's two palettes, in the same words the engine uses for the rest of the app.
///
/// A terminal paints on somebody else's background, so this chooses *inks* rather than filling the
/// screen: a TUI that set its own background colour would fight the user's terminal settings, which
/// is the opposite of a theme being a theme. The header badge and the box lines do carry a colour,
/// and that is what a reader — and the gate — can see change between the two.
pub struct Palette {
    /// `light` | `dark`, the engine's word for the palette in force.
    pub name: &'static str,
    pub ink: Color,
    pub dim: Color,
    pub faint: Color,
    pub badge_bg: Color,
    pub badge_fg: Color,
    pub answer: Color,
    pub tool: Color,
    pub box_line: Color,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            name: "dark",
            ink: Color::White,
            dim: Color::DarkGray,
            faint: Color::Gray,
            badge_bg: Color::Cyan,
            badge_fg: Color::Black,
            answer: Color::Green,
            tool: Color::Cyan,
            box_line: Color::Gray,
        }
    }

    pub fn light() -> Self {
        Self {
            name: "light",
            ink: Color::Black,
            dim: Color::DarkGray,
            faint: Color::DarkGray,
            badge_bg: Color::Blue,
            badge_fg: Color::White,
            answer: Color::Blue,
            tool: Color::Magenta,
            box_line: Color::DarkGray,
        }
    }

    pub fn of(t: cwoffee_engine::theme::Theme) -> Self {
        match t {
            cwoffee_engine::theme::Theme::Light => Self::light(),
            cwoffee_engine::theme::Theme::Dark => Self::dark(),
            // `system` never reaches here; the caller resolves it first.
            cwoffee_engine::theme::Theme::System => Self::dark(),
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        format!("{}…", c[..n].iter().collect::<String>())
    }
}

/// Print what a command said, and take over the keyboard if it asked for a confirmation.
///
/// `moved` is not inferred from the text: the host's own state is the answer to "which
/// conversation am I on", so a branch or a leaf switch re-reads it rather than trusting the line
/// the terminal just printed.
fn push_lines(out: commands::Outcome, app: &mut App, host: &SessionHost) {
    for line in out.lines {
        app.items.push(Item::Note(line));
    }
    app.tree_nodes = out.nodes;
    // Words a command handed back. Assigned, never sent: the branch's whole question is what to do
    // with that message, and the composer is where the person answers it.
    if let Some(text) = out.prefill {
        app.input = text;
    }
    if out.moved {
        app.session_id = host.state().session_id;
        // The transcript on screen belongs to the conversation the command just left. Clearing it
        // and saying so is honest; leaving it under a new session id is how a person ends up
        // reading another conversation's files as this one's.
        app.items.clear();
        app.user_rows.clear();
        app.items.push(Item::Note(format!(
            "this terminal is on conversation {} now. What was above belonged to the previous \
             conversation; /files, /tree and /branch read this one.",
            app.session_id
        )));
    }
    app.pending_cmd = out.pending;
}

async fn run_slash(app: &mut App, host: &SessionHost, store: &Arc<Store>, cmd: &str, rest: &str) {
    let session = app.session_id;
    match cmd {
        "help" | "?" => {
            app.items.push(Item::Note(HELP.lines().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")));
        }
        "branch" => {
            let n: usize = if rest.is_empty() { 1 } else { rest.parse().unwrap_or(1) };
            let shown = app.user_rows.iter().rev().nth(n - 1).cloned();
            push_lines(commands::branch(store.as_ref(), host, session, n, shown).await, app, host);
        }
        "tree" => {
            // One read. The nodes it printed are kept for `/leaf N`: the proof a leaf switch asks
            // for is the text this terminal showed, not a fresh copy of the row.
            let out = commands::tree(store.as_ref(), host, session).await;
            push_lines(out, app, host);
        }
        "leaf" => {
            let n: usize = match rest.parse() {
                Ok(v) => v,
                Err(_) => {
                    app.items.push(Item::Warn("usage: /leaf N — N is the node `/tree` listed".to_string()));
                    return;
                }
            };
            let nodes = app.tree_nodes.clone();
            push_lines(commands::leaf(store.as_ref(), host, session, &nodes, n).await, app, host);
            app.session_id = host.state().session_id;
        }
        "files" => push_lines(commands::files(store.as_ref(), session), app, host),
        "undo" => push_lines(commands::undo_preview(store.as_ref(), session), app, host),
        "forget" => {
            if app.pending_cmd.is_some() {
                // Two presses: the first lists, the second deletes. Handled by the y/n prompt.
                app.items.push(Item::Note("press y to delete the copies listed above, n to keep them".to_string()));
                return;
            }
            push_lines(commands::forget_arm(store.as_ref(), session), app, host);
        }
        "mcp" => {
            app.items.push(Item::Note(mcp_report(app, host).await));
        }
        other => {
            app.items.push(Item::Warn(format!(
                "/{other} is not a command this terminal has. Try /help — and nothing was sent."
            )));
        }
    }
}

/// What this start actually spawned, read from the hub rather than recited from the config file.
/// A person who wrote `[mcp]` is asking "is what I configured running?", and a status page has to
/// answer with the process state, including the servers that came back dead.
async fn mcp_report(app: &App, host: &SessionHost) -> String {
    let budget = cwoffee_engine::CATALOG_BYTE_BUDGET;
    match &app.mcp {
        None => format!(
            "No MCP server is running in this start. The config's [mcp] block is read at boot; if it \
             names servers and none is listed here, the block was disabled or bad.\n  route: {} · the \
             model is told about servers inside <env>, within a {budget}B tool-list budget",
            app.mcp_route
        ),
        Some(hub) => {
            let hub = hub.lock().await;
            let mut lines = Vec::new();
            for (name, says) in hub.servers_named() {
                lines.push(format!("  {name}: {says}"));
            }
            let cat = hub.catalog();
            let block = hub.catalog_block();
            lines.push(format!(
                "  catalog: {} tool(s) registered and withheld behind explore; block {}B against a {budget}B ceiling",
                cat.len(),
                block.len()
            ));
            lines.push(format!(
                "  route: {} · every call into one of these asks, per call — an \"allow for this \
                 conversation\" you gave exec does not cover it",
                app.mcp_route
            ));
            if host.state().persona == cwoffee_engine::host::Persona::Chat {
                lines.push("  this host is in Chat mode: the coworker's tools are not mounted".to_string());
            }
            lines.join("\n")
        }
    }
}

const HELP: &str = "\
doors, as slash-commands (nothing here is a second implementation — each one calls the same \
function the desktop and the phone call):
  /branch [N]   cut a second conversation out of the N-th newest message of yours (default 1);
                the words come back into the input, and this terminal moves onto the branch
  /tree         the nodes this conversation holds, which one the next answer reads through, and
                what is off that path
  /leaf N       stand the leaf at node N of the last /tree. Nothing is copied, nothing is deleted
  /files        every path this conversation wrote, per turn, and what its copies cost the store
  /undo         list what putting that turn's files back would do, then y to do it (n to leave it).
                Edited-since files ask for a separate yes
  /forget       list the stored copies, then y to delete them (two presses). Refused mid-turn
  /mcp          the servers this start spawned, their state, and the catalog's byte cost
  /x            while a turn runs, Enter queues a mid-turn message; x takes the oldest one back
There is no watch-only terminal: every command here drives the host. Mid-turn, branch, leaf, undo
and forget refuse — a turn is given its tools when it starts and reads one path at a time.";

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Dev-only loop trace, gated on CWOFFEE_DEBUG. Used to find the spin that
/// preceded the memory blowup: it records what the loop sees each iteration.
fn dbg_tick(app: &App, terminal: &DefaultTerminal) {
    if std::env::var_os("CWOFFEE_DEBUG").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/cwoffee_debug.log")
    {
        let _ = writeln!(
            f,
            "iter items={} busy={} size={:?}",
            app.items.len(),
            app.busy,
            terminal.size()
        );
    }
}

fn render(f: &mut Frame, app: &App) {
    let has_approval = app.pending.is_some();
    let has_question = app.pending_q.is_some();
    let question_height = app
        .pending_q
        .as_ref()
        .map(|(q, _)| (q.options.len() + 4).min(14) as u16)
        .unwrap_or(0);
    let has_confirm = app.pending_cmd.is_some();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),             // header
            Constraint::Min(1),                // transcript
            Constraint::Length(if has_question { question_height } else { 0 }),
            Constraint::Length(if has_approval { 5 } else { 0 }),
            // A command confirmation is a panel, not a transcript line: it is the one thing on
            // screen waiting for a key, and it has to look like that.
            Constraint::Length(if has_confirm { 4 } else { 0 }),
            Constraint::Length(3),             // input
            Constraint::Length(1),             // telemetry
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_transcript(f, app, chunks[1]);
    if has_question {
        draw_question(f, app, chunks[2]);
    }
    if has_approval {
        draw_approval(f, app, chunks[3]);
    }
    if has_confirm {
        draw_confirm(f, app, chunks[4]);
    }
    draw_input(f, app, chunks[5]);
    draw_telemetry(f, app, chunks[6]);
}

/// Move the next queued question onto the screen, if one is waiting.
fn promote(app: &mut App) {
    match app.q_tail.pop_front() {
        Some((q, reply)) => {
            preselect(&mut app.q_sel, &q);
            app.pending_q = Some((q, reply));
        }
        // Nothing queued. `busy` is left alone on purpose: the turn is still running
        // and its own events own that flag. Clearing it here would report an idle app
        // while the model is still working.
        None => {}
    }
}

/// Pre-selection is a courtesy, not a decision: Enter still has to be pressed, and
/// a multi-select question that arrives with the recommendation already ticked is
/// one keystroke from an answer instead of three.
fn preselect(sel: &mut Vec<usize>, q: &Question) {
    *sel = match q.recommended {
        Some(r) if q.multi => vec![r],
        _ => Vec::new(),
    };
}

/// The `ask_user` prompt. Options are numbered so the choice is one keystroke,
/// and the recommended one is marked - the model's opinion is shown, not
/// imposed, and Enter takes it.
fn draw_question(f: &mut Frame, app: &App, area: Rect) {
    let Some((q, _)) = &app.pending_q else { return };
    let mut lines = vec![Line::from(Span::styled(
        format!(" {} ", q.question),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for (i, o) in q.options.iter().enumerate() {
        let star = q.recommended == Some(i);
        let picked = app.q_sel.contains(&i);
        let mut spans = vec![
            Span::styled(
                format!("   {}) {}. ", if q.multi { if picked {"[x]"} else {"[ ]"} } else if picked {"(*)"} else {"( )"}, i + 1),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                o.label.clone(),
                if picked {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else if star {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC)
                } else {
                    Style::default()
                },
            ),
        ];
        if star {
            spans.push(Span::styled(
                "  <- recommended",
                Style::default().fg(Color::Green),
            ));
        }
        lines.push(Line::from(spans));
        if let Some(d) = &o.detail {
            lines.push(Line::from(Span::styled(
                format!("       {d}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    let mut keys = if q.multi {
        "1-9 toggle (several allowed)".to_string()
    } else {
        "1-9 choose".to_string()
    };
    keys.push_str(" · Enter sends");
    if q.recommended.is_some() {
        keys.push_str(" (no choice = the recommendation)");
    }
    // No "type instead" here on purpose: the TUI has no text entry while a
    // question is up, and advertising an affordance that is not wired would be a
    // lie in the one place a user is reading carefully.
    keys.push_str(" · Esc decline");
    lines.push(Line::from(Span::styled(
        format!("   {keys}"),
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines))
                        .block(
                Block::default()
                    .borders(Borders::ALL)
                    // The count is part of the prompt: a set of three questions that
                    // looks like one question gets one answer, and the other two time
                    // out in a turn the user is watching stall.
                    // The count is part of the prompt: a set of three questions that
                    // looks like one question gets one answer, and the other two time
                    // out in a turn the user is watching stall. Only the number waiting
                    // is shown, because the terminal is not the thing that numbered them.
                    .title(if app.q_tail.is_empty() {
                        " the model is asking ".to_string()
                    } else {
                        format!(" the model is asking ({} more waiting) ", app.q_tail.len())
                    }),
            ),
        area,
    );
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mode = match app.mode {
        ApprovalMode::Ask => "ask",
        ApprovalMode::Session => "session",
        ApprovalMode::Auto => "auto",
    };
    let thinking = match app.reasoning {
        Reasoning::Off => "off",
        Reasoning::Low => "low",
        Reasoning::Medium => "med",
        Reasoning::High => "high",
        Reasoning::XHigh => "xhigh",
    };
    let line = Line::from(vec![
        // The badge carries the palette's own word, because "which theme is this terminal in" is a
        // question a reader asks the screen, and the gate can read the answer out of the escape codes.
        Span::styled(
            format!(" Cwoffee {} ", app.palette.name),
            Style::default().fg(app.palette.badge_fg).bg(app.palette.badge_bg),
        ),
        Span::raw(format!("  {}  ·  session {}  ·  thinking {}  ·  ", app.model, app.session_id, thinking)),
        Span::styled(format!("approvals: {mode}"), Style::default().fg(Color::Yellow)),
        Span::raw("  (ctrl-a mode · ctrl-r thinking)"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Flatten items to styled logical lines, then wrap by hand.
///
/// Wrapping is done here rather than with `Paragraph::wrap` because scrolling to
/// the bottom requires knowing the wrapped line count, and the widget does not
/// expose it before rendering.
fn flatten(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for item in &app.items {
        match item {
            Item::User(t) => {
                push_wrapped(&mut out, t, width, "you ▸ ", Style::default().fg(app.palette.ink));
            }
            Item::Reasoning(t) => {
                push_wrapped(&mut out, t, width, "  ", Style::default().fg(app.palette.dim).add_modifier(Modifier::ITALIC));
            }
            Item::Assistant(t) => {
                push_wrapped(&mut out, t, width, "cwoffee ▸ ", Style::default().fg(app.palette.answer));
            }
            Item::Tool { name, args, output, running, ok, exit } => {
                let status = if *running { "running…" } else if *ok { "ok" } else { "failed" };
                let head = format!(
                    "tool ▸ {name} [{status}{}]{}",
                    exit.map(|e| format!(" exit {e}")).unwrap_or_default(),
                    if *running { "" } else { "" }
                );
                push_wrapped(
                    &mut out,
                    &head,
                    width,
                    "",
                    Style::default().fg(if *ok { app.palette.tool } else { Color::Red }),
                );
                if !args.is_empty() {
                    push_wrapped(&mut out, args, width, "  args  ", Style::default().fg(app.palette.dim));
                }
                if !output.is_empty() {
                    push_wrapped(&mut out, output, width, "  ", Style::default().fg(app.palette.faint));
                }
            }
            Item::Note(t) => push_wrapped(&mut out, t, width, "", Style::default().fg(app.palette.dim)),
            Item::Warn(t) => push_wrapped(&mut out, t, width, "⚠ ", Style::default().fg(Color::Yellow)),
            Item::Fail(t) => push_wrapped(&mut out, t, width, "✗ ", Style::default().fg(Color::Red)),
        }
    }
    out
}

fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, prefix: &str, style: Style) {
    let width = width.max(8);
    // The prefix is part of the line. Subtracting it can reach 0 (a long prefix
    // in a narrow pane), and 0 must never reach wrap().
    let body = width.saturating_sub(prefix.chars().count()).max(1);
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(Line::from(Span::styled(prefix.to_string(), style)));
            continue;
        }
        let mut first = true;
        for chunk in wrap(raw, body) {
            let p = if first { prefix } else { "" };
            out.push(Line::from(Span::styled(format!("{p}{chunk}"), style)));
            first = false;
        }
    }
}

/// Greedy word wrap on char boundaries.
///
/// Every branch must make progress. An earlier version looped forever when
/// `width` was 0 (a pty with no winsize set reports 0x0): `cut` came out as 0,
/// an empty line was pushed, `cur` never shrank, and the Vec grew until the
/// process was killed. A UI that eats 8 GB on a resize is not a cosmetic bug.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    // Hard ceiling so a future logic error fails a test with wrong output
    // instead of hanging the process or eating the machine.
    const MAX_LINES: usize = 200_000;
    for word in s.split(' ') {
        let sep = if cur.is_empty() { "" } else { " " };
        let candidate_len = cur.chars().count() + sep.len() + word.chars().count();
        if candidate_len > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        } else {
            cur.push_str(sep);
            cur.push_str(word);
        }
        // A single word longer than the line still has to break somewhere.
        while cur.chars().count() > width {
            let cut = cur
                .char_indices()
                .map(|(i, _)| i)
                .nth(width)
                .unwrap_or(cur.len());
            // Progress is mandatory: if the cut did not move, force one char.
            let cut = if cut == 0 { cur.chars().next().map(|c| c.len_utf8()).unwrap_or(1) } else { cut };
            lines.push(cur[..cut].to_string());
            cur = cur[cut..].to_string();
            if lines.len() >= MAX_LINES {
                return lines;
            }
        }
        if lines.len() >= MAX_LINES {
            return lines;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    // A 0x0 area is real: a pty with no winsize set, or a terminal mid-resize.
    // Rendering into it produces no output and must not be allowed to reach the
    // wrap code with width 0.
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let lines = flatten(app, width);
    let visible = area.height as usize;
    let total = lines.len();

    let scroll = match app.scroll_up {
        Some(up) => total.saturating_sub(1 + up),
        None => total.saturating_sub(visible),
    };

    let text: Text<'static> = Text::from(lines);
    let p = Paragraph::new(text).scroll((scroll as u16, 0));
    f.render_widget(p, area);
}

fn draw_approval(f: &mut Frame, app: &App, area: Rect) {
    let Some((req, _)) = &app.pending else { return };
    let body = vec![
        Line::from(Span::styled(
            format!(" APPROVAL REQUIRED — {} ", req.tool),
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("   {}", req.summary),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            format!(
                "   {}",
                if app.input.is_empty() {
                    "y allow · a allow for conversation · n/Esc deny · typing goes to the composer"
                } else {
                    "the composer holds words: Enter queues them beside this card (it answers nothing); \
                     Ctrl-U empties the box to answer with y/a/n"
                }
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" approve ")),
        area,
    );
}

/// The `/undo` and `/forget` confirmation. An escalation names itself as one: a card that reads
/// "Revert" when a file has been edited since the turn under-states what the key does, and that is
/// the mistake the desktop's separate button exists to avoid.
fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.pending_cmd else { return };
    let escalating = matches!(p, commands::Pending::Undo { preview } if preview.edited_since > 0);
    let body = vec![
        Line::from(Span::styled(
            p.prompt(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            if app.input.is_empty() {
                format!(
                    "   y {} · n/Esc leave it{}",
                    if escalating { "overwrite them" } else { "do it" },
                    if escalating { " — an escalation, and a separate yes" } else { "" }
                )
            } else {
                "   the composer holds words, so this listing is not answered; Ctrl-U empties it to answer"
                    .to_string()
            },
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .title(if escalating { " revert anyway? " } else { " confirm " }),
        ),
        area,
    );
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let shown = if app.busy && app.input.is_empty() {
        "…".to_string()
    } else {
        app.input.clone()
    };
    // The composer announces what Enter promises. While a turn runs Enter is not "send": it is
    // this kind of mid-turn press, in the engine's own words, with the host's own waiting count
    // next to it. One title for both states is the lie this feature exists to stop telling.
    let title = if app.busy {
        // The words are the engine's: `announce` is the same sentence the desktop composer prints,
        // so a terminal and a window cannot promise different things to the same Enter.
        format!(
            " {} · {} · x takes one back · {} waiting ",
            app.mid_turn.label(),
            app.mid_turn.announce(),
            app.queued.len()
        )
    } else {
        " ask - Enter sends · /branch /tree /files /undo /forget /mcp ".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::raw(shown)))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// The telemetry bar. These five numbers are the reason the bar exists: on a
/// local model, cache hit rate and prefill speed explain almost all perceived
/// latency, and without them a slow turn is indistinguishable from a hang.
fn draw_telemetry(f: &mut Frame, app: &App, area: Rect) {
    let t = &app.tel;
    let cache = t.cache_hit_pct();
    let cache_style = if cache > 60.0 {
        Color::Green
    } else if cache > 20.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let spec = t.spec_accept_pct();
    let line = Line::from(vec![
        Span::styled(
            format!(" cache {:.0}% ", cache),
            Style::default().fg(cache_style).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " decode {:.0}/s (avg {:.0}) ",
            t.server_decode_tps, app.avg.avg_decode_tps()
        )),
        Span::raw(format!(
            " prefill {:.0}/s eff {:.0}/s (avg {:.0}) ",
            t.server_prefill_tps,
            t.effective_prefill_tps(),
            app.avg.avg_effective_prefill_tps()
        )),
        Span::raw(format!(" spec {:.0}% ", spec)),
        Span::raw(format!(" ctx {} ", app.ctx_tokens)),
        // Compaction is the one thing that throws the prefix cache away, so it
        // shows up as a sudden slowdown. If it is not labelled, it looks like a bug.
        if app.compactions > 0 {
            Span::styled(
                format!(" compacted {} ", app.compactions),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
        Span::styled(
            format!(" ttft {}ms ", t.ttft_ms),
            Style::default().fg(if t.ttft_ms > 3000 { Color::Yellow } else { Color::Gray }),
        ),
        Span::styled(
            format!(" tokens {}/{} ", t.cached_tokens, t.prompt_tokens),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression: a pty with no winsize reports 0x0, which fed width 0 into
    /// wrap(). `cut` came out as 0, an empty line was pushed, `cur` never
    /// shrank, and the Vec grew until the kernel killed the process at 8 GB.
    /// This test must terminate; if it hangs, the progress guard is broken.
    #[test]
    fn wrap_terminates_and_stays_bounded_at_degenerate_widths() {
        let text = "the quick brown fox jumps over the lazy dog ".repeat(50);
        for width in [0usize, 1, 2, 3, 7] {
            let lines = wrap(&text, width);
            assert!(
                lines.len() <= text.chars().count() + 2,
                "width {width}: produced {} lines for a {} char string",
                lines.len(),
                text.chars().count()
            );
            assert!(
                lines.iter().all(|l| l.chars().count() <= width.max(1)),
                "width {width}: overlong line {:?}",
                lines.iter().max_by_key(|l| l.chars().count())
            );
        }
    }

    #[test]
    fn wrap_keeps_every_word() {
        let s = "alpha beta gamma delta epsilon zeta";
        let joined = wrap(s, 10).join(" ");
        for w in s.split(' ') {
            assert!(joined.contains(w), "{w} lost during wrap");
        }
    }

    #[test]
    fn wrap_breaks_a_single_word_longer_than_the_line() {
        let lines = wrap("antidisestablishmentarianism", 5);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.chars().count() <= 5), "{lines:?}");
        assert_eq!(lines.concat(), "antidisestablishmentarianism");
    }

    /// A prefix as wide as the pane used to compute `width - prefix.len()` as a
    /// huge usize (saturating only in the other direction) and reach wrap(0).
    #[test]
    fn prefix_nearly_as_wide_as_the_pane_is_safe() {
        let mut out: Vec<Line<'static>> = Vec::new();
        for width in [0usize, 1, 4, 8, 9, 20] {
            push_wrapped(&mut out, "some text that needs wrapping here", width, "cwoffee > ", Style::default());
            assert!(out.len() < 500, "width {width} produced {} lines", out.len());
            out.clear();
        }
    }

    #[test]
    fn blank_reasoning_deltas_do_not_split_an_answer() {
        let mut app = App::new("m".into());
        app.apply(EngineEvent::Text("half one".into()));
        app.apply(EngineEvent::Reasoning("   ".into()));
        app.apply(EngineEvent::Text(" half two".into()));
        let assistants = app.items.iter().filter(|i| matches!(i, Item::Assistant(_))).count();
        assert_eq!(assistants, 1, "answer was split into {assistants} blocks: {:?}", app.items);
        assert_eq!(app.input.len(), 0);
    }

    #[test]
    fn user_turn_appears_in_the_transcript() {
        let mut app = App::new("m".into());
        app.items.push(Item::User("hello".into()));
        let lines = flatten(&app, 80);
        assert!(lines.iter().any(|l| l.to_string().contains("hello")), "user text missing");
    }
}

    /// Reproduce the 0x0-terminal blowup in-process: a TestBackend with no cells,
    /// drawn many times. Bounded by construction - if rendering leaks, this
    /// test grows and fails rather than eating the machine.
    #[test]
    fn render_into_a_zero_sized_terminal_is_bounded() {
        use ratatui::backend::TestBackend;
        let mut app = App::new("m".into());
        app.items.push(Item::Assistant("some words to render".into()));
        app.items.push(Item::User("hi".into()));
        for w in [(0u16, 0u16), (1, 0), (0, 1), (1, 1), (3, 1)] {
            let mut term = ratatui::Terminal::new(TestBackend::new(w.0, w.1)).unwrap();
            for _ in 0..500 {
                term.draw(|f| render(f, &app)).unwrap();
            }
        }
    }
