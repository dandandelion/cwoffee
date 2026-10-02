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

function renderInline(text: string): string {
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

  for (const raw of lines) {
    const line = raw

    // fenced code
    if (line.startsWith('```')) {
      if (!inCode) {
        flushPara()
        closeList()
        inCode = true
        codeLang = line.slice(3).trim()
        code = []
      } else {
        const lang = codeLang && /^[a-z0-9+-]*$/i.test(codeLang) ? ` class="lang-${codeLang}"` : ''
        html.push(`<pre><code${lang}>${code.join('\n')}</code></pre>`)
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

    // ordinary paragraph text
    closeList()
    para.push(line)
  }

  if (inCode) {
    // unterminated fence: still render as code rather than swallowing the text
    html.push(`<pre><code>${code.join('\n')}</code></pre>`)
  }
  flushPara()
  closeList()
  return html.join('\n')
}
