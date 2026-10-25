/**
 * Is the model going round in circles?
 *
 * This exists because the thinking blocks used to have a preview, and that preview was the thing a
 * reader watched to tell "working" from "stuck". The preview is gone - it was the most expensive
 * thing a long turn did on this hardware - so the question needs answering without the text.
 * `thinking · 4.1k words` says a model is producing. This says whether what it is producing is the
 * same thing over and over.
 *
 * Three rules, all bounded, because the transcript paints on a fanless screen and this runs inside
 * the render loop's thread:
 *
 *   1. **a character run** - `!!!!`, a wall of blank lines, a 1-6 character unit repeated. Looked
 *      for in a tail bounded to `charRun * 8` characters, so the cost per delta is constant.
 *   2. **a word phrase that recurs at a constant gap**, confirmed by a tandem compare (the last p
 *      words are the p before them, word for word). One hash and one map lookup per word, over a
 *      ring of the last `ring` words: the turn can be a million words long and this stays flat.
 *   3. **the same sentence over and over with small edits** - the paraphrase loop, which the tandem
 *      compare cannot see because the words are not identical. Each finished sentence is hashed
 *      with any all-digit token folded to `#`; six identical sentences in a row is a loop.
 *
 * `scripts/loop-probe.mjs` holds a copy of this algorithm and the measurements behind the numbers
 * in the comments: loops of every shape fire within 2 deltas to 114 words, at about **12 µs per
 * 18-character delta** (~0.7 µs per character, ~0.2 ms of a 16.7 ms frame at a typical 200
 * deltas/s), and four controls that must stay quiet - a list quoted once, a finding restated three
 * times, a 12-row table, 13,269 words of prose - all stay quiet. The selftest re-runs the same
 * cases against *this* copy, so if the two copies drift, the app's own test says so.
 *
 * Tuning that was measured, not guessed:
 *   - `reps` is 5, not 3. With 3, an answer that repeated one line four times fired the detector on
 *     a 1,000-word control.
 *   - the tandem confirm exists because without it a 13,269-word essay was called a loop at word
 *     1,230 - ordinary writing reuses phrases; it does not repeat a phrase five times at the same
 *     distance.
 *   - `maxPeriod` is 40 words: a phrase that comes back 300 words later is a theme, not a loop.
 */

export type LoopKind = 'chars' | 'phrase' | 'sentence'

export interface LoopFinding {
  kind: LoopKind
  /** how many words had been seen when it fired */
  words: number
  /** how many deltas had been seen when it fired */
  deltas: number
  /** the repeated thing, in a form safe to show a human */
  phrase: string
  times: number
  /** the loop's period, in words, for the phrase rule */
  period?: number
}

export class LoopWatch {
  private ring: number
  private n: number
  private reps: number
  private charRun: number
  private maxPeriod: number
  private sentRepsWant = 6

  private buf: string[]
  private pos = 0
  private deltas = 0
  private map = new Map<number, { at: number; period: number; reps: number }>()
  private carry = ''
  private tail = ''
  private sentWords = 0
  private sentHash = 0
  private lastSentHash = 0
  private sentReps = 0
  private fired: LoopFinding | null = null

  constructor({ ring = 400, n = 5, reps = 5, charRun = 24, maxPeriod = 40 } = {}) {
    this.ring = ring
    this.n = n
    this.reps = reps
    this.charRun = charRun
    this.maxPeriod = maxPeriod
    this.buf = new Array(ring).fill('')
  }

  /** What this detector has seen so far, for the header and for the selftest. */
  stats() {
    return { words: this.pos, deltas: this.deltas, fired: this.fired }
  }

  /** Feed one streamed chunk (a coalesced group of deltas, not necessarily one token). */
  push(delta: string): LoopFinding | null {
    if (!delta) return this.fired
    this.deltas++
    // 1. the character run, on a tail bounded to a few lines of text.
    this.tail = (this.tail + delta).slice(-(this.charRun * 8))
    const run = this.#charLoop()
    if (run) return this.#note(run)

    // Tokenise, carrying the last partial word across the chunk boundary so a word split between
    // two chunks is still one word.
    const src = this.carry + delta
    this.carry = ''
    const endsWithWord = /[\w'’-]/.test(src[src.length - 1] ?? '')
    const parts = src.split(/([^\w'’-]+)/).filter(Boolean)
    for (let i = 0; i < parts.length; i++) {
      const w = parts[i]
      if (!/[\w'’-]/.test(w)) {
        // A separator that ends a sentence is worth noticing: rule 3 works one level up from
        // words, and it needs to know where a sentence finished.
        if (/[.!?\n]/.test(w)) this.#endSentence()
        continue
      }
      if (i === parts.length - 1 && endsWithWord) {
        this.carry = w
        break
      }
      this.#word(w.toLowerCase())
    }
    return this.fired
  }

  #charLoop(): Omit<LoopFinding, 'words' | 'deltas'> | null {
    // One character repeated (newlines included: a wall of blank lines is the same failure), or a
    // 1-6 character unit repeated at the tail. Both have to reach `charRun` characters of it, or
    // it is a two-character unit typed twice and not a loop.
    const re = new RegExp(`(?:([\\s\\S])\\1{${this.charRun - 1},}|([\\s\\S]{1,6})\\2{7,})`)
    const hit = re.exec(this.tail)
    if (!hit || hit[0].length < this.charRun) return null
    const unit = hit[2] ?? hit[1]
    return {
      kind: 'chars',
      phrase: unit.replace(/\s/g, (c) => (c === '\n' ? '\\n' : c)),
      times: Math.round(hit[0].length / unit.length),
    }
  }

  #word(w: string) {
    const at = this.pos
    this.buf[at % this.ring] = w
    this.pos++
    // The sentence-level hash for rule 3, with any all-digit token folded to `#` so "line 7" and
    // "line 8" are the same sentence. One mix per word, next to free.
    this.sentWords++
    this.sentHash = Math.imul(this.sentHash ^ hashWord(/^\d+$/.test(w) ? '#' : w), 16777619) >>> 0
    if (at + 1 < this.n) return

    // Hash the n-gram ending here, from its n words. n is 5, so this is five multiplies per word;
    // a textbook rolling hash would need a power table for the outgoing word, which costs about the
    // same and reads worse.
    let h = 0
    for (let k = at - this.n + 1; k <= at; k++) h = (Math.imul(h, 16777619) ^ hashWord(this.#wordAt(k))) >>> 0

    const e = this.map.get(h)
    if (!e) {
      this.map.set(h, { at, period: 0, reps: 1 })
      // Bounded memory. Throwing away history costs detections in a turn that has already been
      // looping for 40,000 words, which is a turn that has already been reported.
      if (this.map.size > this.ring * 2) this.map.clear()
    } else {
      const gap = at - e.at
      if (gap > this.maxPeriod) {
        // Outside the window this is a theme, not a loop. Without this ceiling the detector fires
        // on ordinary writing.
        e.period = 0
        e.reps = 1
        e.at = at
      } else if (e.period && gap === e.period) e.reps++
      else {
        e.period = gap
        e.reps = 2
      }
      e.at = at
      if (e.reps >= this.reps && this.#tandem(e.period)) {
        this.#note({
          kind: 'phrase',
          phrase: this.#slice(at - this.n + 1, at + 1).join(' '),
          times: e.reps,
          period: e.period,
        })
      }
    }
  }

  #wordAt(k: number) {
    return this.buf[((k % this.ring) + this.ring) % this.ring]
  }

  #slice(from: number, to: number) {
    const out: string[] = []
    for (let k = from; k < to; k++) out.push(this.#wordAt(k))
    return out
  }

  /**
   * Confirm a candidate the way a loop actually looks: the last `p` words are the `p` words before
   * them, word for word. A recurring 5-gram at a constant gap is only a *candidate* - ordinary
   * writing does that. A tandem repeat of a five-word phrase, five times over, is not.
   *
   * Cost: `p` comparisons, and only for a candidate that already recurred at a constant gap.
   */
  #tandem(p: number) {
    if (!p || p > this.ring / 2) return false
    const at = this.pos
    for (let k = 1; k <= p; k++) if (this.#wordAt(at - k) !== this.#wordAt(at - p - k)) return false
    return true
  }

  /**
   * Rule 3, the paraphrase loop: the same sentence over and over with one number changed each time.
   * Six identical (number-folded) sentences in a row is a loop; four is a numbered list. Both were
   * measured in the probe.
   */
  #endSentence() {
    const n = this.sentWords
    if (n >= 8 && this.sentHash) {
      this.sentReps = this.lastSentHash === this.sentHash ? this.sentReps + 1 : 1
      this.lastSentHash = this.sentHash
      if (this.sentReps >= this.sentRepsWant)
        this.#note({ kind: 'sentence', phrase: `${n} words`, times: this.sentReps })
    }
    this.sentWords = 0
    this.sentHash = 0
  }

  #note(f: Omit<LoopFinding, 'words' | 'deltas'>): LoopFinding {
    if (!this.fired) this.fired = { ...f, words: this.pos, deltas: this.deltas }
    return this.fired
  }
}

function hashWord(w: string) {
  let h = 2166136261
  for (let i = 0; i < w.length; i++) h = Math.imul(h ^ w.charCodeAt(i), 16777619) >>> 0
  return h >>> 0
}

/** One line, in the reader's language, for the accordion header and the toast. */
export function describeLoop(f: LoopFinding): string {
  if (f.kind === 'chars') return `repeating "${f.phrase}" ${f.times}x`
  if (f.kind === 'sentence') return `saying the same ${f.words}-word sentence ${f.times} times in a row`
  return `repeating "${f.phrase}" every ${f.period} words, ${f.times}x`
}
