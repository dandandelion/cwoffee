//! Cowry TUI — a thin skin over `cowry_engine::host::SessionHost`.
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

use cowry_engine::approval::{ApprovalAnswer, ApprovalMode, ApprovalRequest};
use cowry_engine::event::{EngineEvent, ExecStream, FinishReason};
use cowry_engine::host::{default_env_block, HostCmd, HostConfig, SessionHost};
use cowry_engine::provider::Reasoning;
use cowry_engine::store::Store;
use cowry_engine::telemetry::{Rolling, TurnTelemetry};
use cowry_engine::tools::Registry;
use cowry_engine::Config;

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
    quit: bool,
    model: String,
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
            reasoning: Reasoning::Low,
            busy: false,
            session_id: 0,
            pending: None,
            quit: false,
            model,
        }
    }

    fn apply(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::TurnStart { turn_id, model } => {
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
            EngineEvent::Usage(u) => {
                self.avg.push(u.clone());
                self.tel = u;
            }
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
                    "cowry [options]\n  \
                     --config PATH      config file (default ~/.config/cowry/config.toml)\n  \
                     --url URL          API root (default http://127.0.0.1:8090/v1)\n  \
                     --model NAME       model id sent to the API\n  \
                     -C, --workspace D  workspace directory (default cwd)\n  \
                     --db PATH          sqlite path\n  \
                     --mode M           ask | session | auto\n  \
                     --openai | --anthropic\n  \
                     --context N        prompt token budget\n\n  \
                     precedence: flags > environment > config file > defaults\n\n  \
                     keys: enter send · ctrl-c cancel/quit · esc quit · \
                     pgup/pgdn scroll · ctrl-a cycle approval mode · ctrl-r cycle thinking"
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
    base.join("cowry")
}

#[tokio::main]
async fn main() -> Result<()> {
    let flags = parse_args()?;

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
        appcfg.context.target_tokens = v;
    }
    if let Some(p) = &loaded {
        eprintln!("config: {}", p.display());
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

    let provider: Box<dyn cowry_engine::provider::Provider> = appcfg.build_provider()
        .map_err(|e| anyhow::anyhow!(e))?;

    let model = if appcfg.provider.model.is_empty() {
        "local".to_string()
    } else {
        appcfg.provider.model.clone()
    };

    let mut cfg = HostConfig::new(model, workspace.clone());
    cfg.mode = appcfg.mode();
    cfg.context_budget = appcfg.context.target_tokens;
    cfg.env_block = default_env_block(&workspace, "linux");
    cfg.approval_timeout =
        std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    let mode = cfg.mode;
    let model = cfg.model.clone();

    let mut host = SessionHost::spawn(
        provider,
        Registry::with_v1_tools_search(appcfg.search_backend()),
        store,
        cfg,
    );

    // Terminal setup is wrapped so a panic still leaves the shell usable.
    let terminal = ratatui::init();
    // A 0x0 terminal is not a rendering detail, it is an input we refuse. A pty
    // with no winsize set reports this, and a degenerate geometry is what let an
    // earlier wrap() loop allocate gigabytes and push the box into swap. Failing
    // here is honest; limping along is not.
    if let Ok(sz) = terminal.size() {
        if sz.width == 0 || sz.height == 0 {
            ratatui::restore();
            anyhow::bail!(
                "terminal reports {}x{} - Cowry needs a real terminal size. \
                 If you are driving it from a script or pty, set winsize first.",
                sz.width,
                sz.height
            );
        }
    }
    let result = run(terminal, &mut host, model, mode).await;
    ratatui::restore();
    result
}

async fn run(
    mut terminal: DefaultTerminal,
    host: &mut SessionHost,
    model: String,
    mode: ApprovalMode,
) -> Result<()> {
    let mut app = App::new(model);
    app.mode = mode;

    // The receivers move out of the host once: recv() needs &mut, and two
    // consumers would silently split one event stream.
    let (mut ev_rx, mut ap_rx) = host.take_receivers().expect("event receivers");

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
            Some(k) = key_rx.recv() => handle_key(k, &mut app, host),
            Some(ev) = ev_rx.recv() => app.apply(ev),
            Some((req, reply)) = ap_rx.recv() => {
                app.pending = Some((req, reply));
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
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

fn handle_key(k: KeyEvent, app: &mut App, host: &SessionHost) {
    // An approval prompt owns the keyboard while it is up.
    if app.pending.is_some() {
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
        }
        return;
    }

    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
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
                    Reasoning::High => Reasoning::Off,
                };
                host.send(HostCmd::SetReasoning { reasoning: app.reasoning });
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
            if !text.trim().is_empty() {
                // The engine does not echo the user turn over the event stream,
                // so the transcript would swallow your own message without this.
                app.items.push(Item::User(text.clone()));
                app.scroll_up = None;
                host.submit(text);
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
// render
// ---------------------------------------------------------------------------

/// Dev-only loop trace, gated on COWRY_DEBUG. Used to find the spin that
/// preceded the memory blowup: it records what the loop sees each iteration.
fn dbg_tick(app: &App, terminal: &DefaultTerminal) {
    if std::env::var_os("COWRY_DEBUG").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/cowry_debug.log")
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),             // header
            Constraint::Min(1),                // transcript
            Constraint::Length(if has_approval { 5 } else { 0 }),
            Constraint::Length(3),             // input
            Constraint::Length(1),             // telemetry
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_transcript(f, app, chunks[1]);
    if has_approval {
        draw_approval(f, app, chunks[2]);
    }
    draw_input(f, app, chunks[3]);
    draw_telemetry(f, app, chunks[4]);
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
    };
    let line = Line::from(vec![
        Span::styled(" Cowry ", Style::default().fg(Color::Black).bg(Color::Cyan)),
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
                push_wrapped(&mut out, t, width, "you ▸ ", Style::default().fg(Color::White));
            }
            Item::Reasoning(t) => {
                push_wrapped(&mut out, t, width, "  ", Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC));
            }
            Item::Assistant(t) => {
                push_wrapped(&mut out, t, width, "cowry ▸ ", Style::default().fg(Color::Green));
            }
            Item::Tool { name, args, output, running, ok, exit } => {
                let status = if *running { "running…" } else if *ok { "ok" } else { "failed" };
                let head = format!(
                    "tool ▸ {name} [{status}{}]{}",
                    exit.map(|e| format!(" exit {e}")).unwrap_or_default(),
                    if *running { "" } else { "" }
                );
                push_wrapped(&mut out, &head, width, "", Style::default().fg(if *ok { Color::Cyan } else { Color::Red }));
                if !args.is_empty() {
                    push_wrapped(&mut out, args, width, "  args  ", Style::default().fg(Color::DarkGray));
                }
                if !output.is_empty() {
                    push_wrapped(&mut out, output, width, "  ", Style::default().fg(Color::Gray));
                }
            }
            Item::Note(t) => push_wrapped(&mut out, t, width, "", Style::default().fg(Color::DarkGray)),
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
        Line::from(Span::raw("   y allow · a allow for session · n/Esc deny")),
    ];
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" approve ")),
        area,
    );
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let shown = if app.busy && app.input.is_empty() {
        "…".to_string()
    } else {
        app.input.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::raw(shown)))
            .block(Block::default().borders(Borders::ALL).title(" ask ")),
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
            push_wrapped(&mut out, "some text that needs wrapping here", width, "cowry > ", Style::default());
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
