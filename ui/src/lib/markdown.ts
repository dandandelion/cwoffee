/**
 * Markdown for finished messages only.
 *
 * Two rules that are not style preferences:
 *
 * 1. **Never run this per token.** It is called once, at stream end
 *    (PLAN.md §9 rule 3). Re-parsing a growing string 50 times a second is
 *    quadratic and reflows the page mid-sentence.
 *
 * 2. **Escape first, then transform.** Model output routinely contains text
 *    lifted out of fetched web pages, and `web_fetch` will happily hand back
 *    `<script>` from a page. If we let markdown pass through raw HTML, a fetched
 *    page becomes script running in the app. So every character is escaped
 *    before a single tag is introduced, and the only tags in the output are ones
 *    this file wrote. Link targets are scheme-checked for the same reason:
 *    `[click](javascript:...)` is a real payload in scraped content.
 *
 * Deliberately a subset. A full CommonMark implementation is a dependency and a
 * surface; this covers what models actually emit.
 */

const ESCAPES: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ESCAPES[c])
}

/** Only these schemes may appear in a rendered link. */
function safeUrl(u: string): string {
  const t = u.trim()
  if (/^(https?:|mailto:)/i.test(t)) return t
  // protocol-relative and fragment/anchor links are harmless and useful
  if (/^(#|\/(?!\/))/.test(t)) return t
  return ''
}

export function renderInline(text: string): string {
  // text is already escaped
  let out = ''
  let i = 0
  while (i < text.length) {
    const c = text[i]

    // inline code: the only span where nothing else is interpreted
    if (c === '`') {
      const end = text.indexOf('`', i + 1)
      if (end > i) {
        out += `<code>${text.slice(i + 1, end)}</code>`
        i = end + 1
        continue
      }
    }

    // link [text](url)
    if (c === '[') {
      const close = text.indexOf(']', i)
      if (close > i && text[close + 1] === '(') {
        // Scan for the matching close paren, not the first ')': a payload like
        // [click](javascript:alert(1)) has parens inside the target, and stopping
        // early leaks a stray ')' into the output.
        let paren = -1
        let depth = 0
        for (let j = close + 1; j < text.length; j++) {
          if (text[j] === '(') depth++
          else if (text[j] === ')') {
            depth--
            if (depth === 0) {
              paren = j
              break
            }
          }
        }
        if (paren > close) {
          const label = text.slice(i + 1, close)
          const href = safeUrl(text.slice(close + 2, paren))
          if (href) {
            out += `<a href="${href}" rel="noopener">${label}</a>`
          } else {
            // Unsafe or unparseable: keep the text, drop the link. Silently
            // rendering it would be worse than losing the hyperlink.
            out += label
          }
          i = paren + 1
          continue
        }
      }
    }

    // bold **x**
    if (c === '*' && text[i + 1] === '*') {
      const end = text.indexOf('**', i + 2)
      if (end > i + 1) {
        out += `<strong>${text.slice(i + 2, end)}</strong>`
        i = end + 2
        continue
      }
    }

    // italic *x*  (single, not part of **)
    if (c === '*' && text[i + 1] !== '*' && text[i + 1] !== ' ' && text[i + 1] !== undefined) {
      const end = text.indexOf('*', i + 1)
      if (end > i + 1 && text[end - 1] !== ' ') {
        out += `<em>${text.slice(i + 1, end)}</em>`
        i = end + 1
        continue
      }
    }

    out += c
    i += 1
  }
  return out
}

/**
 * Convert markdown to sanitized HTML.
 *
 * Input is untrusted (model output, possibly quoting a fetched page). Output
 * contains only tags this function emits.
 */
/**
 * Split a table row into cells.
 *
 * A `\|` inside a cell is a literal pipe, not a column separator. Leading and
 * trailing pipes are structural and dropped, but a trailing *escaped* pipe is
 * content, so the closing-pipe strip has to check for it.
 */
function splitRow(line: string): string[] {
  let s = line.trim()
  if (s.startsWith('|')) s = s.slice(1)
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1)
  const cells: string[] = []
  let cur = ''
  for (let k = 0; k < s.length; k++) {
    const ch = s[k]
    if (ch === '\\' && s[k + 1] === '|') {
      cur += '|'
      k++
      continue
    }
    if (ch === '|') {
      cells.push(cur.trim())
      cur = ''
      continue
    }
    cur += ch
  }
  cells.push(cur.trim())
  return cells
}

/** `|---|` `|:---|` `|---:|` `|:---:|` in any column count. */
// Single dashes are legal (`|:-:|`), so this is `-+`, not `-{2,}`.
const TABLE_DELIM = /^\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?$/

function aligns(delim: string): string[] {
  return splitRow(delim).map((c) => {
    const l = c.startsWith(':')
    const r = c.endsWith(':')
    if (l && r) return 'center'
    if (r) return 'right'
    if (l) return 'left'
    return ''
  })
}

/**
 * Render a pipe table. `header` is the line before the delimiter row.
 *
 * Ragged rows are normal from a model, so the column count comes from the
 * delimiter and short rows are padded rather than dropped.
 */
function renderTable(header: string, delim: string, rows: string[]): string {
  const cols = splitRow(delim)
  const al = aligns(delim)
  const n = cols.length
  const pad = (cells: string[]) =>
    Array.from({ length: n }, (_, i) => cells[i] ?? '')

  const cell = (tag: string, text: string, i: number) => {
    const a = al[i] ? ` class="ta-${al[i]}"` : ''
    return `<${tag}${a}>${renderInline(escapeHtml(text))}</${tag}>`
  }

  const head = pad(splitRow(header))
    .map((c, i) => cell('th', c, i))
    .join('')
  const body = rows
    .map((r) => `<tr>${pad(splitRow(r)).map((c, i) => cell('td', c, i)).join('')}</tr>`)
    .join('')
  return `<table><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table>`
}

/**
 * Fenced code becomes a titled block with actions, not a bare <pre>.
 *
 * The source lives in the <code> textContent and is read back at click time.
 * Duplicating it into a data attribute would double the DOM for every block and
 * mean two copies that can disagree.
 *
 * Only html and svg get preview actions. Every other language would need its
 * own sandbox story, and "we can preview it" is a promise the renderer cannot
 * keep for, say, a C++ file.
 */
function codeBlock(langRaw: string, escapedCode: string): string {
  const lang = (langRaw || '').toLowerCase()
  const safe = /^[a-z0-9+-]*$/.test(lang) ? lang : ''
  const acts: string[] = [
    '<button class="code-act" data-act="copy" type="button" title="Copy code">copy</button>',
  ]
  if (safe === 'html') {
    acts.push(
      '<button class="code-act" data-act="preview" type="button" title="Preview in a panel">preview</button>',
    )
  } else if (safe === 'svg') {
    acts.push(
      '<button class="code-act" data-act="inline" type="button" title="Toggle inline preview">inline</button>',
      '<button class="code-act" data-act="preview" type="button" title="Preview in a panel">preview</button>',
    )
  }
  return (
    `<div class="codeblk" data-lang="${safe}">\n` +
    `<div class="code-head"><span class="code-lang">${escapeHtml(safe || 'text')}</span>` +
    `<span class="code-acts">${acts.join('')}</span></div>\n` +
    `<pre><code${safe ? ` class="lang-${safe}"` : ''}>${escapedCode}</code></pre>\n` +
    `</div>`
  )
}

/**
 * Split a growing answer into finished blocks and a tail that is not.
 *
 * Unlike `splitBlocks`, which returns pieces, this returns the byte offset up to
 * which the source is finished, because the caller has to retire exactly the
 * word spans that the finished blocks replace.
 *
 * This is what lets an answer format *while it streams* instead of at the end:
 * a paragraph becomes a paragraph the moment its blank line arrives, and only
 * the last, still-growing block stays plain text.
 *
 * `cut` is the index into `src` up to which everything is finished. A block is
 * closed by a blank line, and an unclosed code fence means everything from that
 * fence on is unfinished - a half-written table or list would otherwise render
 * as a mangled one and then get rewritten when the rest arrives.
 */
export function splitCommitted(src: string): { blocks: string[]; cut: number } {
  const lines = src.split('\n')
  const blocks: string[] = []
  let blockStart = 0
  let cut = 0
  let inFence = false
  let pos = 0
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]
    const lineEnd = pos + line.length
    if (/^\s*(?:```|~~~)/.test(line)) inFence = !inFence
    // A trailing empty piece is just the newline the source ended with, not a
    // blank line: a table row or list item may be the next thing to arrive.
    const trailing = i === lines.length - 1 && line === ''
    if (!inFence && !trailing && line.trim() === '') {
      const body = src.slice(blockStart, lineEnd)
      if (body.trim()) blocks.push(body)
      // The blank line itself belongs to what it terminated, so the committed
      // region is a clean prefix of the source and the word spans retired
      // against it end on a word boundary.
      cut = Math.min(lineEnd + 1, src.length)
      blockStart = lineEnd + 1
    }
    pos = lineEnd + 1
  }
  return { blocks, cut: Math.min(cut, src.length) }
}

export function renderMarkdown(src: string): string {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const html: string[] = []
  let para: string[] = []
  let listType: 'ul' | 'ol' | null = null
  let inCode = false
  let codeLang = ''
  let code: string[] = []

  const flushPara = () => {
    if (!para.length) return
    html.push(`<p>${renderInline(escapeHtml(para.join('\n')))}</p>`)
    para = []
  }
  const closeList = () => {
    if (listType) {
      html.push(`</${listType}>`)
      listType = null
    }
  }

  for (let idx = 0; idx < lines.length; idx++) {
    const line = lines[idx]

    // fenced code
    if (line.startsWith('```')) {
      if (!inCode) {
        flushPara()
        closeList()
        inCode = true
        codeLang = line.slice(3).trim()
        code = []
      } else {
        html.push(codeBlock(codeLang, code.join('\n')))
        inCode = false
        codeLang = ''
      }
      continue
    }
    if (inCode) {
      code.push(escapeHtml(line))
      continue
    }

    const t = line.trim()

    if (t === '') {
      flushPara()
      closeList()
      continue
    }

    // horizontal rule
    if (/^(-{3,}|\*{3,}|_{3,})$/.test(t)) {
      flushPara()
      closeList()
      html.push('<hr>')
      continue
    }

    // heading
    const h = /^(#{1,6})\s+(.*)$/.exec(t)
    if (h) {
      flushPara()
      closeList()
      const lvl = h[1].length
      html.push(`<h${lvl}>${renderInline(escapeHtml(h[2]))}</h${lvl}>`)
      continue
    }

    // blockquote
    if (t.startsWith('>')) {
      flushPara()
      closeList()
      html.push(`<blockquote>${renderInline(escapeHtml(t.replace(/^>\s?/, '')))}</blockquote>`)
      continue
    }

    // unordered list
    const ul = /^[-*+]\s+(.*)$/.exec(t)
    if (ul) {
      flushPara()
      if (listType !== 'ul') {
        closeList()
        html.push('<ul>')
        listType = 'ul'
      }
      html.push(`<li>${renderInline(escapeHtml(ul[1]))}</li>`)
      continue
    }

    // ordered list
    const ol = /^\d+[.)]\s+(.*)$/.exec(t)
    if (ol) {
      flushPara()
      if (listType !== 'ol') {
        closeList()
        html.push('<ol>')
        listType = 'ol'
      }
      html.push(`<li>${renderInline(escapeHtml(ol[1]))}</li>`)
      continue
    }

    // Table: a header line containing a pipe, immediately followed by a
    // delimiter row. Both must be present, which is what keeps "a | b" in
    // prose from becoming a table.
    if (t.includes('|') && idx + 1 < lines.length && TABLE_DELIM.test(lines[idx + 1].trim())) {
      flushPara()
      closeList()
      const header = t
      const delim = lines[idx + 1].trim()
      let j = idx + 2
      const rows: string[] = []
      while (j < lines.length && lines[j].trim() !== '' && lines[j].includes('|')) {
        rows.push(lines[j].trim())
        j++
      }
      html.push(renderTable(header, delim, rows))
      idx = j - 1
      continue
    }

    // ordinary paragraph text
    closeList()
    para.push(line)
  }

  if (inCode) {
    // unterminated fence: still render as code rather than swallowing the text.
    // The model stops mid-block more often than markdown documents suggest, and
    // swallowing the remainder loses content the user can still read.
    html.push(codeBlock(codeLang, code.join('\n')))
  }
  flushPara()
  closeList()
  return html.join('\n')
}

/**
 * Split markdown into blocks at blank lines, keeping fenced code intact.
 *
 * Only blocks that cannot change again may be frozen: a block ends at a blank
 * line outside a fence, and a fence is only complete once its closing ``` has
 * arrived. An unterminated fence therefore stays in the tail and keeps being
 * re-rendered as its contents stream in.
 */
export function splitBlocks(src: string): string[] {
  const out: string[] = []
  let buf = ''
  let inFence = false
  for (const line of src.split('\n')) {
    if (line.trimStart().startsWith('```')) inFence = !inFence
    buf += line + '\n'
    if (!inFence && line.trim() === '') {
      out.push(buf)
      buf = ''
    }
  }
  if (buf) out.push(buf)
  return out
}

/**
 * Incremental markdown for streaming text.
 *
 * Re-parsing the whole answer on every frame is what makes a streaming UI stutter
 * on a local model: the parser costs more than token generation does. This keeps
 * finished blocks rendered exactly once and re-parses only the tail block that is
 * still growing.
 *
 * `finish()` deliberately re-renders the whole source. During streaming the
 * output is allowed to be a good approximation; the final DOM must match what a
 * single pass over the stored text produces, or a reload would show something
 * different from what the user just watched.
 */
/**
 * Split an arrival into the units that animate together: a word plus whatever
 * whitespace follows it. Whitespace travels with the word in front of it so the
 * gap never becomes an animating unit of its own, which is what makes the text
 * read as words arriving rather than the whole line fading in.
 */
export function revealWords(delta: string): string[] {
  return delta.match(/\S+\s*|\s+/g) ?? (delta ? [delta] : [])
}

/** Default reveal duration, mirrored in app.css. The streaming path overrides it
 *  per paint from the measured word rate, so this is the fallback for the first
 *  paint and for anything that animates outside a turn. */
export const REVEAL_ANIM_MS = 320
/** Floor and ceiling for the adaptive duration. Below the floor the effect is a
 *  flicker; above the ceiling a fast model outruns its own animation, which is
 *  the "playing catch up" the user reported. */
export const REVEAL_MIN_MS = 110
export const REVEAL_MAX_MS = 170

/**
 * How long one word should take to land, given how fast words are arriving.
 *
 * A reveal that ignores the stream rate either outruns it (words still fading in
 * after the model has moved on, which reads as the animation chasing the text) or
 * lags behind it (a fast model looks like plain text with a hitch). Taking the
 * inter-word interval and clamping keeps the animation the same length as the
 * gap it is filling.
 */
export function revealDuration(wordsPerSec: number): number {
  if (!isFinite(wordsPerSec) || wordsPerSec <= 0) return REVEAL_ANIM_MS
  const ms = 600 / wordsPerSec
  return Math.min(REVEAL_MAX_MS, Math.max(REVEAL_MIN_MS, Math.round(ms)))
}

/**
 * Evaluated per paint rather than at import: it has to be false when the user
 * asked for reduced motion, and a page (or a test runner) with no matchMedia at
 * all should get the animation rather than lose it to a missing API.
 */
export function revealOn(): boolean {
  return !(
    typeof matchMedia === 'function' &&
    matchMedia('(prefers-reduced-motion: reduce)').matches
  )
}

export class LiveMarkdown {
  private src = ''
  private frozenHtml = ''
  private frozenCount = 0
  /** Count of render() calls that re-parsed a frozen block; used by tests. */
  refrozen = 0

  push(chunk: string) {
    this.src += chunk
  }

  get text(): string {
    return this.src
  }

  /** Bookkeeping shared by render() and renderAnimated(). */
  private parts(): { frozen: string; tail: string } {
    const blocks = splitBlocks(this.src)
    const complete = Math.max(0, blocks.length - 1)
    if (complete > this.frozenCount) {
      for (let i = this.frozenCount; i < complete; i++) {
        // Blocks are joined with a newline so the incremental output is byte for
        // byte what a single pass produces. Without it the DOM shifts at stream
        // end, which reads as a flicker.
        this.frozenHtml += (this.frozenHtml ? '\n' : '') + renderMarkdown(blocks[i])
      }
      this.frozenCount = complete
    } else if (complete < this.frozenCount) {
      // The source shrank or re-blocked; the cache is no longer valid.
      this.frozenHtml = ''
      this.frozenCount = 0
      this.refrozen++
    }
    return { frozen: this.frozenHtml, tail: blocks.slice(this.frozenCount).join('') }
  }

  render(): string {
    const { frozen, tail } = this.parts()
    if (!tail) return frozen
    return frozen + (frozen ? '\n' : '') + renderMarkdown(tail)
  }

  finish(): string {
    return renderMarkdown(this.src)
  }
}
