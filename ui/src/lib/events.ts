/**
 * Mirror of `crates/engine/src/event.rs`.
 *
 * Adjacently tagged: `{ kind, d }`. Variant names are camelCase; struct-variant
 * fields stay snake_case (serde's container `rename_all` does not touch them).
 * If you change the Rust enum, change this file — the compiler will not.
 */

export type FinishReason = 'stop' | 'toolCalls' | 'length' | 'cancelled' | 'error' | 'other'
export type ExecStream = 'stdout' | 'stderr'
export type Risk = 'readOnly' | 'mutating' | 'exec'
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

export type EngineEvent =
  | { kind: 'sessionChanged'; d: { session_id: number; title: string } }
  | { kind: 'turnStart'; d: { turn_id: number; model: string } }
  | { kind: 'text'; d: string }
  | { kind: 'reasoning'; d: string }
  | { kind: 'toolCallBegin'; d: { id: string; name: string } }
  | { kind: 'toolCallArgs'; d: string }
  | { kind: 'toolCallReady'; d: { id: string; args: unknown } }
  | { kind: 'toolCallParseError'; d: { id: string; raw: string; message: string } }
  | { kind: 'toolExecStart'; d: { id: string } }
  | { kind: 'toolExecOutput'; d: { id: string; stream: ExecStream; chunk: string } }
  | { kind: 'toolExecEnd'; d: { id: string; ok: boolean; exit_code: number | null } }
  | {
      kind: 'approvalRequested'
      d: { id: number; tool: string; risk: Risk; summary: string; args: unknown }
    }
  | { kind: 'usage'; d: TurnTelemetry }
  | { kind: 'turnEnd'; d: { turn_id: number; finish: FinishReason } }
  | { kind: 'error'; d: { message: string; fatal: boolean } }

export interface GpuReport {
  egl_vendor: string
  glx_vendor: string
  render_nodes: string[]
  leaked_nvidia: boolean
}

export interface HostState {
  session_id: number
  model: string
  mode: 'ask' | 'session' | 'auto'
  reasoning: 'off' | 'low' | 'medium' | 'high'
  busy: boolean
  turns: number
  context_tokens: number
  compactions: number
  approval_timeout_secs: number
}

export interface Session {
  id: number
  title: string
  workspace: string
  model: string
  created_at: number
  updated_at: number
  message_count: number
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
  created_at: number
  attachments: AttachmentMeta[]
}
