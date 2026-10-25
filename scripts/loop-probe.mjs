#!/usr/bin/env node
/**
 * Is the model going round in circles? A streaming detector, measured before it is shipped.
 *
 * Why this exists now: the thinking blocks used to have a preview, and the preview was the thing
 * a reader watched to know whether the model was working or stuck. The preview is gone (it was the
 * most expensive thing a long turn did on this hardware), so the loop question needs answering
 * without the text: `thinking · 4.1k words` says a model is producing, and this says whether what
 * it is producing is the same thing over and over.
 *
 * What it does, all on a bounded window so the cost cannot grow with the turn:
 *   1. a character run  - `!!!!!!`, `~~~~`, a period of 1-6 chars repeated at the tail;
 *   2. a word n-gram that recurs at a constant gap - the shape of "the same sentence again", and
 *      of "three words over and over", found by a rolling hash over a ring of the last `ring`
 *      words, and then *confirmed* by a tandem compare (the last p words equal the p before them).
 *      Five recurrences, not three: with three, an answer that repeats one line four times - a
 *      table, a quoted list, a heading twice - was measured firing the detector on a 1,000-word
 *      control. One hash and one map lookup per *word*, whatever the turn's length;
 *   3. novelty - distinct words in the last 200 falling under 6%. This is the loose net for the
 *      case where a model re-says the same paragraph with small edits, so no gram repeats at a
 *      constant gap.
 *
 * Run it: `node scripts/loop-probe.mjs`. It reports, for each pathological stream, how many words
 * in the loop it took to fire, and what it cost per word, and it runs the detector over real prose
 * to show the false-positive rate is not zero-cost either. Nothing here is wired into the app yet;
 * that is the decision this file exists to inform.
 */

export class LoopWatch {
  /**
   * @param ring   how many words to remember (memory and time are bounded by this, not by the turn)
   * @param n      the n-gram size; 5 words is long enough to be a phrase and short enough to be
   *               reached by a model repeating three words
   * @param reps   how many recurrences at a constant gap before it is called a loop
   */
  constructor({ ring = 400, n = 5, reps = 5, charRun = 24, novelWindow = 200, novelFloor = 0.06, maxPeriod = 40 } = {}) {
    this.ring = ring
    this.n = n
    this.reps = reps
    this.charRun = charRun
    this.maxPeriod = maxPeriod
    this.novelWindow = novelWindow
    this.novelFloor = novelFloor
    this.sentRepsWant = 6
    this.buf = new Array(ring) // a ring of lower-cased words, so no shifting and no copying
    this.pos = 0 // absolute word count this detector has seen
    this.map = new Map() // gram key -> { at, period, reps }
    this.carry = '' // a word still being written when the delta ended
    this.tail = '' // bounded character tail for the run detector
    this.deltas = 0
    this.sentWords = 0
    this.sentHash = 0
    this.lastSentHash = 0
    this.sentReps = 0
    this.fired = null
  }

  /** Feed one streamed delta. Returns a finding, or null. */
  push(delta) {
    if (!delta) return null
    this.deltas++
    // 1. the character run, on a tail bounded to a few lines of text.
    this.tail = (this.tail + delta).slice(-(this.charRun * 8))
    const run = this.#charLoop()
    if (run) return this.#note(run)

    // Tokenise, carrying the last partial word across the delta boundary so a word split between
    // two deltas is still one word.
    const src = this.carry + delta
    this.carry = ''
    const endsWithWord = /[\w'’-]/.test(src[src.length - 1] ?? '')
    const parts = src.split(/([^\w'’-]+)/).filter(Boolean)
    for (let i = 0; i < parts.length; i++) {
      const w = parts[i]
      if (!/[\w'’-]/.test(w)) {
        // A separator that ends a sentence is worth noticing: the sentence rule works one level up
        // from words, and it needs to know where a sentence finished.
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

  #charLoop() {
    const t = this.tail
    // One character repeated (newlines included: a wall of blank lines is the same failure), or a
    // 1-6 character unit repeated eight times or more at the tail of what just arrived.
    const re = new RegExp(`(?:([\\s\\S])\\1{${this.charRun - 1},}|([\\s\\S]{1,6})\\2{7,})`)
    const hit = re.exec(t)
    if (!hit) return null
    // One rule for how much repetition counts: `charRun` characters of it. Without that, a unit of
    // two characters repeated eight times is 16 characters, which is shorter than a line and is
    // sometimes just typing.
    if (hit[0].length < this.charRun) return null
    const unit = hit[2] ?? hit[1]
    return { kind: 'chars', phrase: unit.replace(/\s/g, (c) => (c === '\n' ? '\\n' : c)), times: Math.round(hit[0].length / unit.length) }
  }

  #word(w) {
    const at = this.pos
    this.buf[at % this.ring] = w
    this.pos++
    // The sentence-level hash, with any all-digit token folded to `#` so "line 7" and "line 8" are
    // the same sentence. One mix per word, next to free.
    this.sentWords++
    this.sentHash = Math.imul(this.sentHash ^ hashWord(/^\d+$/.test(w) ? '#' : w), 16777619) >>> 0
    if (at + 1 < this.n) return
    // Hash the n-gram ending here, from its n words. n is 5, so this is five multiplies per word;
    // a true rolling hash would need a power table for the outgoing word, which costs about the
    // same and reads worse. Measured below either way.
    let h = 0
    for (let k = at - this.n + 1; k <= at; k++) {
      const word = this.buf[((k % this.ring) + this.ring) % this.ring]
      h = (Math.imul(h, 16777619) ^ hashWord(word)) >>> 0
    }
    const e = this.map.get(h)
    if (!e) {
      this.map.set(h, { at, period: 0, reps: 1 })
      if (this.map.size > this.ring * 2) this.map.clear() // bounded memory, at the cost of history
    } else {
      const gap = at - e.at
      if (gap > this.maxPeriod) {
        // A phrase that comes back 300 words later is a theme. A phrase that comes back inside
        // forty words, three times, at the same distance, is a loop. Without this ceiling the
        // detector fires on ordinary writing, which was measured on the first run: it called a
        // 13,000-word essay a loop at word 1,230 because "on a fanless screen" came back twice.
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
        const phrase = this.buf
          .slice(0, Math.min(this.pos, this.ring))
          .slice(-(this.n))
          .join(' ')
        this.#note({ kind: 'phrase', phrase, period: e.period, times: e.reps, at })
      }
    }
    // 3. novelty, checked every 32 words rather than every word: it is the loose net, and a
    //    Set over 200 words every word would be the most expensive part of this detector.
    if (this.pos % 32 === 0 && this.pos > this.novelWindow) {
      const win = []
      for (let k = this.pos - this.novelWindow; k < this.pos; k++)
        win.push(this.buf[((k % this.ring) + this.ring) % this.ring])
      const distinct = new Set(win).size
      if (distinct / win.length < this.novelFloor)
        this.#note({ kind: 'novelty', distinct, of: win.length, ratio: distinct / win.length })
    }
  }

  /**
   * Confirm a candidate the way a loop actually looks: the last `p` words are the `p` words before
   * them, word for word. A recurring 5-gram at a constant gap is a *candidate* - ordinary writing
   * does that, and the first measured run of this probe called a 13,000-word essay a loop at word
   * 3,134 for exactly that reason. A tandem repeat of a phrase of five words or more, three times
   * over, is not ordinary writing.
   *
   * Cost: `p` comparisons, and only for a candidate that already repeated at a constant gap.
   */
  #tandem(p) {
    if (!p || p > this.ring / 2) return false
    const at = this.pos
    for (let k = 1; k <= p; k++) {
      const a = this.buf[(((at - k) % this.ring) + this.ring) % this.ring]
      const b = this.buf[(((at - p - k) % this.ring) + this.ring) % this.ring]
      if (a !== b) return false
    }
    return true
  }

  /**
   * The paraphrase loop: the same sentence over and over with one number changed each time. Word
   * n-grams and the tandem compare both miss it, because the words are not identical.
   *
   * So the same mechanism runs one level up: hash each finished sentence with any all-digit token
   * folded to `#`, and call six identical sentences in a row a loop. Six is the number that keeps
   * an ordinary numbered list out of the way while still catching a model that lost the plot -
   * measured both ways below.
   */
  #endSentence() {
    const n = this.sentWords
    if (n >= 8 && this.sentHash) {
      this.sentReps = this.lastSentHash === this.sentHash ? this.sentReps + 1 : 1
      this.lastSentHash = this.sentHash
      if (this.sentReps >= this.sentRepsWant) this.#note({ kind: 'sentence', times: this.sentReps, words: n })
    }
    this.sentWords = 0
    this.sentHash = 0
  }

  #note(f) {
    if (!this.fired) this.fired = { ...f, words: this.pos, deltas: this.deltas }
    return this.fired
  }

  stats() {
    return { words: this.pos, deltas: this.deltas, fired: this.fired }
  }
}

function hashWord(w) {
  let h = 2166136261
  for (let i = 0; i < w.length; i++) h = (Math.imul(h ^ w.charCodeAt(i), 16777619)) >>> 0
  return h >>> 0
}

// --------------------------------------------------------------------------- measurements

function stream(w, chunks, size) {
  const t0 = performance.now()
  for (let i = 0; i < chunks.length; i++) w.push(chunks[i])
  return performance.now() - t0
}

function* chunked(s, size) {
  for (let i = 0; i < s.length; i += size) yield s.slice(i, i + size)
}

const SENTENCE = 'The quarterly figures were restated after the auditor raised a concern about inventory. '
const PHRASE = 'let me check that for you '
const PROSE = (() => {
  // Real-ish prose, and long: the false-positive test. Written to be varied on purpose, because a
  // detector that fires on ordinary writing is a detector that gets turned off within a week. The
  // order is pseudo-random so an ordinary phrase recurs at an ordinary (uneven) gap rather than at
  // the constant gap the phrase rule looks for.
  const bits = [
    ['the transcript', 'the accordion', 'the composer', 'the gateway', 'a pinned reader', 'the follow rule'],
    ['stopped growing', 'started counting', 'held its ground', 'moved under the reader', 'refused to move', 'settled again'],
    ['after every tool card', 'when the answer landed', 'while the model thought', 'on a fanless screen', 'during a long build', 'in the stored session'],
    ['because nobody had asked to see it', 'which is what the report described', 'so the rule changed once', 'and the check says so', 'without touching the record', 'for the same reason twice'],
  ]
  let seed = 20261024
  const rnd = () => ((seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff) / 0x7fffffff)
  let s = ''
  for (let i = 0; i < 900; i++) {
    const pick = (k) => bits[k][Math.floor(rnd() * bits[k].length)]
    s += `${pick(0)} ${pick(1)} ${pick(2)} ${pick(3)} ${i % 7 === 0 ? `(${i} of ${900 - (i % 13)})` : ''}. `
  }
  return s
})()

const NEAR = (() => {
  // The hard one: a model re-stating the same paragraph with one number changed each time. A
  // tandem repeat of exact words does not exist here, so this is what the novelty net is for -
  // and what it is not good at. Reported honestly either way.
  let s = ''
  for (let i = 1; i <= 900; i++)
    s += `I should read the file notes/step.md and check line ${i} for the value, then report the value. `
  return s
})()

const cases = [
  ['infinite punctuation', '!' + '!'.repeat(40000) + SENTENCE],
  ['a period of 4 chars', 'ab_~'.repeat(9000)],
  ['the same sentence x2000', SENTENCE.repeat(2000)],
  ['three words over and over', PHRASE.repeat(4000)],
  ['a stall mid-thought', 'so I should read the file and then ' + 'the file and then '.repeat(3000)],
  ['the same paragraph, one number changed', NEAR],
  ['a list quoted once (want: no fire)', '1. Open the notes file and read the first line. 2. Compare it with the totals from last quarter. 3. Write the summary in plain language. 4. Close the file and report the difference. ' + PROSE.slice(0, 4000)],
  ['a finding restated three times (want: no fire)', 'The margin came from inventory. ' + PROSE.slice(0, 900) + 'The margin came from inventory. ' + PROSE.slice(900, 1800) + 'The margin came from inventory. ' + PROSE.slice(1800, 2600)],
  ['a table with the same shape (want: no fire)', Array.from({ length: 12 }, (_, i) => `| row ${i + 1} | grew | 1.${i + 1}% |`).join('\n') + PROSE.slice(0, 2000)],
  ['ordinary long prose (want: no fire)', PROSE],
]

let pad = 0
const rows = []
for (const [name, text] of cases) {
  const chunks = [...chunked(text, 18)] // deltas of ~18 chars, like a token stream
  const w = new LoopWatch()
  const ms = stream(w, chunks, 18)
  const s = w.stats()
  rows.push({
    case: name,
    words: s.words,
    deltas: chunks.length,
    fired: s.fired
      ? `${s.fired.kind} after ${s.fired.words ? `${s.fired.words} words` : `${s.fired.deltas} deltas`}: ${describe(s.fired)}`
      : 'no',
    'µs/delta': ((ms / Math.max(1, chunks.length)) * 1000).toFixed(2),
    total: `${ms.toFixed(0)} ms`,
  })
  pad = Math.max(pad, name.length)
}

function describe(f) {
  if (f.kind === 'chars') return `${f.times}x "${f.phrase}"`
  if (f.kind === 'sentence') return `the same ${f.words}-word sentence ${f.times} times in a row`
  if (f.kind === 'phrase') return `gap ${f.period} words, ${f.times}x: "${f.phrase}"`
  return `${f.distinct} distinct words in ${f.of} (${(f.ratio * 100).toFixed(1)}%)`
}

console.table
  ? console.table(rows)
  : rows.forEach((r) => console.log(JSON.stringify(r)))

const worst = rows.reduce((a, b) => (parseFloat(b['µs/delta']) > parseFloat(a['µs/delta']) ? b : a))
console.log(`\nworst cost per delta of 18 chars: ${worst['µs/delta']} µs (${worst.case})`)
console.log(`memory: a ring of ${new LoopWatch().ring} words plus a map capped at ${new LoopWatch().ring * 2} entries`)
