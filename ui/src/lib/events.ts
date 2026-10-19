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
  // Mirrors engine FileKind. `artifact` is a deliverable to open from the chat;
  // `edit` is a file the model changed in a tree; `unknown` is a row stored
  // before the distinction existed, and is rendered the way those always were.
  kind?: 'artifact' | 'edit' | 'unknown'
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
  // The question closed with no answer: the tool timed out, or the turn was
  // cancelled while it was parked. The card must stop offering a choice.
  | { kind: 'questionExpired'; d: { id: number } }
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
