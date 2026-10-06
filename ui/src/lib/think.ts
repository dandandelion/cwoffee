/**
 * "thought for 47s" / "thought for 2m34s" / "thought for 1h05m".
 *
 * Its own module with no imports so the format is testable without a DOM, and
 * so the live path and the reload path cannot drift apart.
 */
export function fmtThink(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000))
  if (s < 60) return `Thought for ${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `Thought for ${m}m${String(s % 60).padStart(2, '0')}s`
  const h = Math.floor(m / 60)
  return `Thought for ${h}h${String(m % 60).padStart(2, '0')}m`
}
