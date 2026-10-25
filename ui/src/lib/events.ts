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
}

// Mirrors engine `Risk`. Harmless never prompts; Workspace changes a file that
// already exists inside the workspace; Sensitive runs code or reaches outside it.
export type Risk = 'harmless' | 'workspace' | 'sensitive'
export type ApprovalAnswer = 'allow' | 'allowSession' | 'deny'

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
}

export interface WorkspaceRow {
  path: string
  label: string
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
