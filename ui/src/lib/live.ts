/**
 * Client-side telemetry for the status bar (PLAN.md §7).
 *
 * Why this lives in the UI and not only in the engine: "current decode" is about
 * what the *user perceives right now*, which is delta arrival at the webview.
 * The engine's `usage` event carries the server's per-turn aggregates (raw
 * prefill, server decode, spec-decode acceptance) and those feed the *averages*.
 *
 * Two prefill rates, never merged:
 *   effective = prompt_tokens / ttft   (what you experienced; huge on cache hits)
 *   raw       = server_prefill_tps     (true compute rate)
 */

export class LiveWindow {
  private marks: number[] = []
  constructor(private windowMs = 1000) {}

  reset() {
    this.marks = []
  }

  mark(at: number = performance.now()) {
    this.marks.push(at)
    const cutoff = at - this.windowMs
    let drop = 0
    while (drop < this.marks.length && this.marks[drop] < cutoff) drop++
    if (drop) this.marks.splice(0, drop)
  }

  /** tokens/sec over the window; 0 until we have at least one gap */
  decodeTps(): number {
    if (this.marks.length < 2) return 0
    const span = (this.marks[this.marks.length - 1] - this.marks[0]) / 1000
    if (span <= 0) return 0
    return (this.marks.length - 1) / span
  }
}

export interface TurnStats {
  rawPrefillTps: number
  effectivePrefillTps: number
  decodeTps: number
  cacheHitPct: number
  specAcceptPct: number
}

export function statsFromTurn(t: {
  prompt_tokens: number
  cached_tokens: number
  ttft_ms: number
  server_prefill_tps: number
  server_decode_tps: number
  draft_n: number
  draft_accepted: number
}): TurnStats {
  return {
    rawPrefillTps: t.server_prefill_tps,
    effectivePrefillTps:
      t.ttft_ms > 0 && t.prompt_tokens > 0 ? t.prompt_tokens / (t.ttft_ms / 1000) : 0,
    decodeTps: t.server_decode_tps,
    cacheHitPct: t.prompt_tokens > 0 ? (100 * t.cached_tokens) / t.prompt_tokens : 0,
    specAcceptPct: t.draft_n > 0 ? (100 * t.draft_accepted) / t.draft_n : 0,
  }
}

/** Mean over the last N turns, not lifetime. Turn 1 is cold-cache and skews it. */
export class Rolling {
  private turns: TurnStats[] = []
  constructor(private max = 10) {}

  push(s: TurnStats) {
    this.turns.push(s)
    if (this.turns.length > this.max) this.turns.shift()
  }

  get len() {
    return this.turns.length
  }

  private mean(f: (s: TurnStats) => number): number {
    if (!this.turns.length) return 0
    return this.turns.reduce((a, s) => a + f(s), 0) / this.turns.length
  }

  avgDecodeTps() {
    return this.mean((s) => s.decodeTps)
  }
  avgRawPrefillTps() {
    return this.mean((s) => s.rawPrefillTps)
  }
  avgCacheHitPct() {
    return this.mean((s) => s.cacheHitPct)
  }
  avgSpecAcceptPct() {
    return this.mean((s) => s.specAcceptPct)
  }
}

export const fmt = {
  rate(n: number) {
    return n > 0 ? n.toFixed(0) : '—'
  },
  pct(n: number) {
    return n > 0 ? `${n.toFixed(0)}%` : '—'
  },
  k(n: number) {
    return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`
  },
}
