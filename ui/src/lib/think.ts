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

/**
 * A word count that fits in a header line: 940, 1.4k, 12k.
 *
 * The thought line shows this while it streams, which is the only sign of life a hidden
 * reasoning block has. Rounding above 1k is deliberate: nobody reads a precise word count of a
 * thought they cannot open, and a number that ticks in the hundreds is a number that flickers.
 */
export function fmtWords(n: number): string {
  if (n < 1000) return String(n)
  const k = n / 1000
  return k < 10 ? `${k.toFixed(1).replace(/\.0$/, '')}k` : `${Math.round(k)}k`
}

/**
 * How many words a stored block of reasoning holds, without building an array of them.
 *
 * The reload path needs this to put the same number on a thought line that the live path put
 * there while it streamed. `split(/\s+/)` would allocate one string per word - several thousand
 * per long thought, for a number displayed as `1.4k` - so this walks the matches.
 */
export function countWords(s: string): number {
  let n = 0
  const re = /\S+/g
  while (re.exec(s) !== null) n++
  return n
}
