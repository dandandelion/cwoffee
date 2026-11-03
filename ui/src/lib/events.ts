/**
 * Mirror of `crates/engine/src/event.rs`.
 *
 * Adjacently tagged: `{ kind, d }`. Variant names are camelCase; struct-variant
 * fields stay snake_case (serde's container `rename_all` does not touch them).
 * If you change the Rust enum, change this file — the compiler will not.
 */

export type FinishReason = 'stop' | 'toolCalls' | 'length' | 'cancelled' | 'error' | 'other'
export type ExecStream = 'stdout' | 'stderr'
export type QuestionOption = {
  label: string
  detail?: string
}

/** The model asked the user to choose, and the turn is parked until someone
   answers. `recommended` indexes into `options`; it is badged, not forced. */
export type QuestionRequested = {
  id: number
  question: string
  options: QuestionOption[]
  recommended?: number
  allow_custom: boolean
  /** The model allows more than one answer, so the card ticks boxes. */
  multi: boolean
  /** Present when the model asked several questions in one call. They belong on
     one card: three interruptions for one decision is three chances to get it
     wrong. `index`/`total` let the card say "question 2 of 3" without waiting to
     find out whether more are coming. */
  set?: { id: number; index: number; total: number }
}

export type FileChange = {
  path: string
  added: number
  removed: number
  diff?: string
  created: boolean
}

export type OutputFile = {
  path: string
  name: string
  mime: string
  bytes: number
  // Mirrors engine FileKind. `attachment` is the only kind the host will hand over:
  // the model wrote it into `.cws/assistant_attachments/` on purpose, so a control to
  // save it where you like appears. `artifact` is an older generated-file label still
  // present in rows written before the attachments folder existed; `scratch` is
  // working material under `.cws/`, `edit` a file changed in your own tree, and
  // `unknown` a row stored before the distinction existed (rendered as it always was).
  kind?: 'artifact' | 'edit' | 'scratch' | 'attachment' | 'unknown'
  /**
   * Did this turn make the file, or change one that was already there? From the tool that
   * looked at the disk before writing (`OutputFile::created`), because the distinction stops
   * being recoverable afterwards. A made file is shown as a card; a changed one is a chip.
   * Absent on rows written before the fact existed, which are rendered the way they always
   * were — an unlabelled row must not claim a turn made something.
   */
  created?: boolean
}

// Mirrors engine `Risk`. Harmless never prompts; Workspace changes a file that
// already exists inside the workspace; Sensitive runs code or reaches outside it.
export type Risk = 'harmless' | 'workspace' | 'sensitive'
export type ApprovalAnswer = 'allow' | 'allowSession' | 'deny'

/**
 * What a message typed during a turn promises. The words are pi's own RPC verbs —
 * `crates/engine/src/steer.rs` owns them, and the config key `mid_turn` takes the same two —
 * so the settings screen, the composer's announcement and the frame on the wire cannot drift.
 */
export type MidTurn = 'steer' | 'follow_up'

/** The stored palette. `system` is a subscription, not a colour: each surface follows its own
 * system's preference, live, which is why a phone tab and a desktop can disagree and both be right. */
export type HostTheme = 'light' | 'dark' | 'system'

/**
 * One mid-turn message that is waiting: the words, the promise, and the id this host assigned.
 * The id is cwoffee's, not pi's — pi's queue is a list of plain strings — so it is the only handle
 * a withdrawal has on either core.
 *
 * `attachments` is a count, never the bytes: this rides every status read and every queue change,
 * and a picture inside a status line is a megabyte of IPC per repaint.
 */
export type QueuedItem = {
  id: number
  text: string
  kind: MidTurn
  /** Seconds: when Enter was pressed. The stored row gets this stamp, not the landing time. */
  typed_at: number
  session: number
  attachments: number
  /**
   * "into this turn" / "after this turn", from the engine (`crate::steer::MidTurn::label`). The
   * card prints this rather than writing its own, because two clients paraphrasing one promise is
   * how they drift.
   */
  label: string
  /**
   * The engine's pending sentence (`MidTurn::pending_note`), which says the message has not been
   * sent yet — the half a waiting line must never imply on its own.
   */
  note: string
}

export interface TurnTelemetry {
  prompt_tokens: number
  cached_tokens: number
  completion_tokens: number
  ttft_ms: number
  server_prefill_tps: number
  server_decode_tps: number
  draft_n: number
  draft_accepted: number
}

/** Live mid-turn estimates. Mirrors `TurnStats` in the engine.
 *
 * These are estimates: the server reports token counts only at the end of the
 * turn, so anything shown while a turn is running is derived from characters.
 */
export interface TurnStats {
  ttft_ms: number
  streaming_ms: number
  chars: number
  est_tokens: number
  est_decode_tps: number
  thinking: boolean
}

/** One row of the model picker, as the endpoint describes it. */
export interface ModelInfo {
  id: string
  context_tokens: number | null
  input_modalities: string[]
  tools: boolean | null
  tools_inferred: boolean
  source: string | null
  server: string | null
  slots: number | null
  advertised: boolean
}

export type EngineEvent =
  | { kind: 'sessionChanged'; d: { session_id: number; title: string } }
  /**
   * A conversation naming itself, live. `text` is the *cumulative* title rather than
   * an increment, so a client can drop a duplicate or receive one out of order and
   * still settle on the same string. `done` marks the final paint; the row drops its
   * caret there. The stored rename arrives right behind this as `sessionChanged`.
   */
  | { kind: 'titleDelta'; d: { session_id: number; text: string; done: boolean } }
  | { kind: 'turnStart'; d: { turn_id: number; model: string; session_id: number } }
  | { kind: 'text'; d: string }
  | { kind: 'reasoning'; d: string }
  | { kind: 'toolCallBegin'; d: { id: string; name: string } }
  | { kind: 'toolCallArgs'; d: string }
  | { kind: 'toolCallReady'; d: { id: string; args: unknown } }
  | { kind: 'toolCallParseError'; d: { id: string; raw: string; message: string } }
  | { kind: 'toolExecStart'; d: { id: string } }
  | { kind: 'toolExecOutput'; d: { id: string; stream: ExecStream; chunk: string } }
  | { kind: 'turnFiles'; d: { turn_id: number; files: OutputFile[] } }
  | { kind: 'toolExecEnd'; d: { id: string; ok: boolean; exit_code: number | null; changes: FileChange[] } }
  | {
      kind: 'approvalRequested'
      d: { id: number; tool: string; risk: Risk; summary: string; args: unknown }
    }
  | { kind: 'questionRequested'; d: QuestionRequested }
  | { kind: 'questionAnswered'; d: { id: number } }
  // The question closed with no answer: the person stopped the turn, a clock somebody
  // wrote down ran out, or the window holding the card went away. The card must stop
  // offering a choice, and it has to say which of those three happened — "the model was
  // told it got none" about a turn the reader stopped themselves is a sentence about
  // something that did not happen. `why` is optional: an older stream, or a card settled
  // by a failed reply, has no reason attached and gets the neutral wording.
  | { kind: 'questionExpired'; d: { id: number; why?: 'turnStopped' | 'timedOut' | 'windowGone' } }
  | { kind: 'turnStats'; d: TurnStats }
  /**
   * A message typed during a turn went into the conversation. Emitted when the words are put into
   * the prompt the *next* request is built from — never when they were typed — because "the model
   * has it" is only a claim once a request carries it. `step` is that round-trip; `typed_at` is
   * when Enter was pressed, which is also the stamp on the row.
   */
  | {
      kind: 'steerLanded'
      d: {
        id: number
        session: number
        kind: MidTurn
        text: string
        typed_at: number
        turn_id: number
        step: number
      }
    }
  /**
   * The whole mid-turn queue, as it stands. Whole rather than incremental for the same reason the
   * host's `queue_update` parity needs it: a tab that missed one event must not keep painting a
   * line for a message that is no longer waiting.
   */
  | { kind: 'queueChanged'; d: { session: number; items: QueuedItem[] } }
  /**
   * The turn stopped with messages still waiting. Their words go back to the composer and nothing
   * was sent — a queued line that quietly disappears is a lost keystroke with a card on top of it.
   */
  | { kind: 'queueReturned'; d: { session: number; items: QueuedItem[] } }
  | { kind: 'usage'; d: TurnTelemetry }
  | { kind: 'turnEnd'; d: { turn_id: number; finish: FinishReason } }
  | { kind: 'note'; d: { message: string; session?: number } }
  | { kind: 'error'; d: { message: string; fatal: boolean } }

export interface GpuReport {
  egl_vendor: string
  glx_vendor: string
  render_nodes: string[]
  leaked_nvidia: boolean
}

export type ReasoningLevel = 'off' | 'low' | 'medium' | 'high' | 'xhigh'

export interface HostState {
  session_id: number
  model: string
  mode: 'ask' | 'session' | 'auto'
  /**
   * Cowork or chat: whether the host has tools and a folder at all, as opposed to
   * `mode`, which decides whether a tool needs asking. Optional because a window may
   * talk to a gateway built before the verb existed; a missing field means "the host
   * never told us", and the UI then shows what it last heard rather than inventing.
   */
  persona?: 'cowork' | 'chat'
  /**
   * Whether the conversation the host is holding may reach the web: whether `web_search` and
   * `web_fetch` are mounted for it, and whether its system prompt tells the model they are there.
   *
   * Optional because a window may talk to a host built before the field existed; absence means
   * "this host never told us", and the UI then behaves as off rather than painting a control that
   * does nothing. It lives in the host's state rather than only in the composer because it is a
   * claim about what the *model* was told — the chip, the prompt and the mounted tools have to
   * agree, and a second window looking at the same conversation has to see the same answer.
   */
  web?: boolean
  /**
   * What a message typed during a turn promises: into this turn at its next step (`steer`), or
   * after it (`follow_up`). Read from the host rather than kept per window, because two tabs
   * pressing Enter in one conversation must be given the same promise — and the composer's line
   * says which kind the next press is, so it has to be the host's answer.
   *
   * Optional: a window talking to a host built before the field exists shows no announcement rather
   * than inventing one.
   */
  mid_turn?: MidTurn
  /**
   * The palette the **host** stores: `light` | `dark` | `system`. One value in `config.toml`, read at
   * start by the window, a device tab and the terminal. A window may show an override, and only that
   * window's chrome says so — see `theme_note` and `theme_about`.
   */
  theme?: HostTheme
  /** Where the choice lives, in the engine's words (`crate::theme::Theme::setting_note`). */
  theme_note?: string
  /** About's line: which value is the host's and which is this client's (`crate::theme::Theme::about_says`). */
  theme_about?: string
  /** How the palette this surface shows got chosen — a media query, a stored value, or a terminal's `COLORFGBG`. */
  theme_resolved?: string
  /**
   * The composer's mid-turn line, in the engine's words (`crate::steer::composer_hint`). Printed
   * rather than paraphrased: it is a claim about what the next request will carry, and the process
   * that carries it is the one that gets to say so. Absent means a host that never published one,
   * and the composer then falls back to the two sentences it knows, and says nothing about a step it
   * has not been told about.
   */
  mid_turn_hint?: string
  /**
   * What the stored choice means, in the engine's sentence (`crate::steer::setting_note`), quoted by
   * Settings. It names where the choice lives, because a phone tab and the desktop press the same
   * Enter and must be promised the same thing.
   */
  mid_turn_note?: string
  /**
   * What a mid-turn press cannot do — answer a card, change the approval mode, change the tools this
   * turn started with (`crate::steer::card_refusal`). Printed beside the dial so a reader is not told
   * one limit by a refusal and a different one by a description.
   */
  mid_turn_refusal?: string
  /**
   * What is waiting in the mid-turn queue for the conversation this host holds. A tab that reloaded
   * mid-turn paints these lines rather than showing a transcript that lost what the person typed.
   */
  queued?: QueuedItem[]
  /**
   * What the running model process was actually started with, in words: "no tools at all",
   * "web_search, web_fetch". The Web chip is a promise about what the model can reach; this is the
   * receipt for it, so the two can be held against each other instead of one being trusted to imply
   * the other. Absent means the core does not state one — which must never be rendered as "none".
   */
  web_tools?: string
  reasoning: ReasoningLevel
  /** The effort new conversations start at - what the settings screen edits. Optional because a
    page may talk to a gateway built before the field existed. */
  reasoning_default?: ReasoningLevel
  /** Whether `reasoning` is this conversation's own override rather than the default. The composer
    chip says "this chat" when it is true, because one control now serves two different asks. */
  reasoning_overridden?: boolean
  /** The levels this model says it can answer for, in the words the dial uses, as the host learned
    them: probed from the endpoint on the Rust core, asked of the model process on the pi core.
    Empty means nobody asked, which is not a claim that the model cannot think. A dial step that the
    model folds or rejects is how a reader ends up believing the knob is dead, so the offered list
    belongs next to the dial rather than in a log. */
  reasoning_levels?: string[]
  busy: boolean
  turns: number
  context_tokens: number
  compactions: number
  /**
   * The engine's own prompt budget: the line history is trimmed past. It is not
   * the model's window, and the difference is the whole complaint - a session at
   * 53k of a 262k window was being trimmed because the budget was a constant.
   */
  context_budget: number
  /**
   * How the ceiling got its value. A number means a person pinned it (config or the
   * settings screen); `null`/absent means the app derives it as half of the window the
   * endpoint states. The difference is visible because a derived ceiling follows a model
   * change and a pinned one does not — a panel that cannot tell them apart has to watch
   * its own number move for reasons it cannot explain.
   */
  budget_pin?: number | null
  /** What the endpoint stated as the window, or null when it never said. */
  model_window: number | null
  /** How long an `ask_user` card stays open. Names itself in the timeout text. */
  question_timeout_secs: number
  approval_timeout_secs: number
  /**
   * What the running turn has said but not yet closed. A page that reloads in the middle of
   * an answer is handed stored rows that stop at the last closed message, so without this the
   * reply on screen starts wherever the reload landed. The pi core fills this as it streams;
   * an empty string means the host has nothing to give, which is how it looks to a client
   * talking to the Rust core.
   */
  open_text?: string
  open_reasoning?: string
  workspace: string
  /**
   * Why the last workspace change was refused, and a counter so a caller can tell a
   * fresh refusal from an old one. `set_workspace` returns this text; a UI that only
   * watches `workspace` sees a change that did not happen and blames itself.
   */
  workspace_refusal?: string | null
  refusal_seq: number
}

export interface Session {
  id: number
  title: string
  workspace: string
  model: string
  created_at: number
  updated_at: number
  message_count: number
  /** Which mode made this conversation: "cowork" or "chat". Absent means the store is
      older than the column, which means cowork. */
  persona?: string
  /** Whether this conversation may reach the web (`web_search`, `web_fetch`). Mirrors the host's
    `web`; a chat opened from the sidebar has to show its own chip, not the last one typed. */
  web_on?: boolean
}

export interface WorkspaceRow {
  path: string
  label: string
  /** The folder's colour as `#rrggbb`, or empty for "no colour". Optional because a page may talk
    to a gateway built before the column existed; absence reads as uncoloured. The store refuses
    anything that is not a colour, and `safeColor` on the client refuses it again before it reaches
    a `style` attribute — a record written by an older build is not a reason to interpolate text
    into markup. */
  color?: string
  sessions: number
}

export interface SearchHit {
  session_id: number
  message_id: number
  role: string
  created_at: number
  snippet: string
  rank: number
}

export interface AttachmentMeta {
  mime: string
  name: string | null
  bytes: number
  /**
   * Set only when *this window* already holds the payload — a message you just sent, or an image
   * the reader has opened once. A transcript page deliberately carries name and size alone, because
   * dragging every picture across IPC to render a conversation costs megabytes for pictures nobody
   * is looking at. The field is how the transcript knows it can draw the picture now instead of
   * offering to fetch it.
   */
  url?: string
}

export interface StoredMessage {
  id: number
  session_id: number
  role: 'system' | 'user' | 'assistant' | 'tool'
  content: string
  reasoning: string
  tool_call_id: string | null
  tool_calls_json: string
  reasoning_signature: string
  /** Milliseconds the model spent thinking; 0 when it did not think. */
  reasoning_ms: number
  files: OutputFile[]
  created_at: number
  attachments: AttachmentMeta[]
  /** Line counts / diff for a file call; UI metadata, never sent to the model. */
  changes?: FileChange[]
}

/**
 * cwoffee's **kept** browser — the one behind the window's browser panel, which the `browse`
 * tool can drive with `"session": true`.
 *
 * The payload is built by the engine (`browser_hub::status_payload`) on both transports; the
 * fields are typed here rather than read ad hoc because a panel that guesses at a field name
 * fails quietly, and this one fails as a blank picture.
 */
export interface BrowserPanel {
  mounted: boolean
  driver?: string | null
  node?: string
  /** False when the person turned kept browsers off (`[browser] keep = false`). */
  keep_off?: boolean
  off_reason?: string | null
  idle_secs: number
  /** Seconds of idle left; null when nothing is alive to time out. */
  idle_left: number | null
  alive: boolean
  opened: boolean
  url: string
  title: string
  opened_by: 'person' | 'model' | 'nobody' | string
  last_actor: 'person' | 'model' | 'nobody' | string
  last_act: string
  /** Why it is not alive — including "closed itself after 5 minutes idle". */
  why: string
  console: { kind: string; text: string }[]
  page_errors: string[]
  requests_failed: string[]
  downloads_blocked: string[]
  frames_taken: number
  /** Whether the browser is sending frames of its own (its screencast is on), which is what makes
      the panel a live view rather than a picture somebody took. Reported by the driver, not
      assumed by the asking: a Chromium with no screencast answers `false` and the panel says so. */
  live?: boolean
  /** Why the live view is not available, when it was asked for and is not. */
  live_why?: string
  /** The id of the newest picture this session has. Separate from `seq`, which counts page state:
      a live view draws several pictures of one state, so a poll that compared `seq` would report a
      moving page as nothing new. */
  frame_pic?: number
  /** True when the page is a page **in this window** — painted by this app into the panel's box.
      It changes what the panel draws: there is no picture, no live mark and no stream to ask for,
      because the thing in the box is the document itself. Reported by the app, never inferred. */
  native?: boolean
  /** How many documents this session has had. A record says which of them it happened on, which is
      the difference between "the page was already broken" and "your click broke it". */
  nav?: number
  /** The id of the newest record the page has made. A reader holding this id has seen everything. */
  diag_seq?: number
  /** Records per kind — the numbers behind the header badge. The records themselves are one command
      away (`browser_log`), so a shut drawer does not carry them once a second. */
  diag_counts?: Record<string, number>
  /** How many of the held records are things that went wrong, rather than things the page said. */
  diag_failed?: number
  viewport?: { width: number; height: number }
  /** For a page painted in this window: where the widget tree put the page widget, in window
   *  pixels. `shown: false` or a zero box means the page was never given the area the panel left
   *  for it, which the panel says rather than leaving as an empty frame. */
  placed?: { x: number; y: number; width: number; height: number; shown: boolean } | null
  /** Bumps whenever the page or the session changes. Not what the frame poll compares against —
      that compares `frame_pic` — but the panel shows the page's own state with it. */
  seq: number
  /** Whether *this* client may drive the browser, or only watch it. */
  writable: boolean
  /** Which browser this is, said out loud: this window's own, or the machine running the tools. */
  route: string
  says: string
}

/** One thing a page said about itself. The shape is the engine's (`browser_hub::Diag`), not one
    invented here: the drawer, the model's `page_log` tool and the health report are three readings
    of the same record, and a fourth vocabulary would be three answers to one question. */
export interface DiagRecord {
  seq: number
  /** What the record is of: what the page printed, what threw, what it asked for that failed, what
      it loaded, where it went, what it handed over. */
  kind: 'console' | 'error' | 'network' | 'resource' | 'navigation' | 'download' | string
  level: 'info' | 'warning' | 'error' | string
  text: string
  url?: string | null
  /** An HTTP status, when the record is about a request. Zero when the request never answered. */
  status?: number | null
  method?: string | null
  /** Where in the page it happened, as the page gave it: file:line:col. */
  at?: string | null
  /** Which document it belongs to. */
  nav: number
  on_this_page?: boolean
  ago_secs?: number
}

/** One answer from `browser_log` / `page_log`: the records, and the numbers that say whether there
    are more than the reader has seen. */
export interface BrowserLog {
  ok: boolean
  alive: boolean
  url: string
  nav: number
  newest: number
  held: number
  returned: number
  health?: { counts: Record<string, number>; failed_total: number; nav: number; url: string }
  records: DiagRecord[]
}
