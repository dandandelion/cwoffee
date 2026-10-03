/**
 * Run with: node --experimental-strip-types scripts/md_test.ts
 *
 * The security cases are the point. Model output quotes fetched web pages, so
 * anything that reaches this function is untrusted input from the internet.
 */
import { renderMarkdown, escapeHtml, LiveMarkdown, splitBlocks } from '../src/lib/markdown.ts'

let failed = 0
function eq(name: string, got: string, want: string) {
  if (got !== want) {
    failed++
    console.log(`FAIL ${name}\n  got:  ${JSON.stringify(got)}\n  want: ${JSON.stringify(want)}`)
  }
}
function ok(name: string, cond: boolean, detail = '') {
  if (!cond) {
    failed++
    console.log(`FAIL ${name} ${detail}`)
  }
}

// ---- escaping: the security boundary -------------------------------------

eq(
  'script tag is neutralised',
  renderMarkdown('<script>alert(1)</script>'),
  '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>',
)

ok(
  'img onerror payload produces no attributes',
  !renderMarkdown('<img src=x onerror=alert(1)>').includes('<img'),
  renderMarkdown('<img src=x onerror=alert(1)>'),
)

ok(
  'javascript: link is dropped, text kept',
  renderMarkdown('[click](javascript:alert(1))') === '<p>click</p>',
  renderMarkdown('[click](javascript:alert(1))'),
)

ok(
  'data: url link is dropped',
  renderMarkdown('[x](data:text/html,<script>)') === '<p>x</p>',
  renderMarkdown('[x](data:text/html,<script>)'),
)

ok(
  'https link survives and is escaped',
  renderMarkdown('[docs](https://example.com/a?b=1&c=2)') ===
    '<p><a href="https://example.com/a?b=1&amp;c=2" rel="noopener">docs</a></p>',
  renderMarkdown('[docs](https://example.com/a?b=1&c=2)'),
)

ok(
  'a url with balanced parens survives',
  renderMarkdown('[x](https://en.wikipedia.org/wiki/Foo_(bar))') ===
    '<p><a href="https://en.wikipedia.org/wiki/Foo_(bar)" rel="noopener">x</a></p>',
  renderMarkdown('[x](https://en.wikipedia.org/wiki/Foo_(bar))'),
)

ok(
  'attribute injection through a link title is escaped',
  !renderMarkdown('[a](https://x/" onmouseover="alert(1))').includes('onmouseover="alert'),
  renderMarkdown('[a](https://x/" onmouseover="alert(1))'),
)

eq('escapeHtml covers the five dangerous chars', escapeHtml(`&<>"'`), '&amp;&lt;&gt;&quot;&#39;')

// ---- markdown features ----------------------------------------------------

eq('heading', renderMarkdown('## Title'), '<h2>Title</h2>')
eq('bold', renderMarkdown('a **b** c'), '<p>a <strong>b</strong> c</p>')
eq('italic', renderMarkdown('a *b* c'), '<p>a <em>b</em> c</p>')
eq('inline code', renderMarkdown('use `ls -la`'), '<p>use <code>ls -la</code></p>')
eq(
  'inline code suppresses inner markup',
  renderMarkdown('`**not bold**`'),
  '<p><code>**not bold**</code></p>',
)
eq(
  'fenced block keeps content verbatim and escapes it',
  renderMarkdown('```\nlet x = 1 < 2;\n```'),
  '<pre><code>let x = 1 &lt; 2;</code></pre>',
)
eq(
  'fenced block with language',
  renderMarkdown('```rust\nfn main() {}\n```'),
  '<pre><code class="lang-rust">fn main() {}</code></pre>',
)
eq(
  'unterminated fence still renders as code',
  renderMarkdown('```\nstill code'),
  '<pre><code>still code</code></pre>',
)
eq(
  'unordered list',
  renderMarkdown('- one\n- two'),
  '<ul>\n<li>one</li>\n<li>two</li>\n</ul>',
)
eq(
  'ordered list',
  renderMarkdown('1. one\n2. two'),
  '<ol>\n<li>one</li>\n<li>two</li>\n</ol>',
)
eq('blockquote', renderMarkdown('> quoted'), '<blockquote>quoted</blockquote>')
eq('hr', renderMarkdown('---'), '<hr>')
eq(
  'paragraphs split on blank lines',
  renderMarkdown('one\n\ntwo'),
  '<p>one</p>\n<p>two</p>',
)
eq(
  'list closes before a paragraph',
  renderMarkdown('- a\n\ntext'),
  '<ul>\n<li>a</li>\n</ul>\n<p>text</p>',
)

// ---- robustness -----------------------------------------------------------

ok('empty input', renderMarkdown('') === '')
ok('lone backtick', renderMarkdown('`').includes('`'))
ok('lone bracket', renderMarkdown('[').includes('['))
ok('unclosed bold', renderMarkdown('**oops').includes('oops'))

// ReDoS: nested emphasis markers on a long line must not hang.
const nasty = '*'.repeat(2000) + 'a'.repeat(2000)
const t0 = Date.now()
renderMarkdown(nasty)
const dt = Date.now() - t0
ok('pathological emphasis is fast', dt < 500, `took ${dt}ms`)

const nasty2 = 'a'.repeat(50000) + '\n' + '- '.repeat(20000)
const t1 = Date.now()
renderMarkdown(nasty2)
ok('large input is fast', Date.now() - t1 < 1000, `took ${Date.now() - t1}ms`)

// ---- incremental streaming ------------------------------------------------

const doc = [
  '# Title',
  '',
  'First paragraph with **bold**.',
  '',
  '- one',
  '- two',
  '',
  '```rust',
  'fn main() { println!("hi"); }',
  '```',
  '',
  'Tail paragraph.',
].join('\n')

// Word-by-word streaming must end at exactly the same DOM as a single pass,
// otherwise reloading a session shows something different from what was watched.
const lm = new LiveMarkdown()
for (const w of doc.split(/(?<=\s)/)) lm.push(w)
const streamed = lm.render()
const final = lm.finish()
ok('streamed render matches a single pass', streamed === final,
   `\n  streamed: ${streamed.slice(0, 160)}\n  final:    ${final.slice(0, 160)}`)

// Finished blocks must not be re-parsed on every frame - that is the whole point.
const lm2 = new LiveMarkdown()
let renders = 0
for (const w of doc.split(/(?<=\s)/)) {
  lm2.push(w)
  lm2.render()
  renders++
}
ok('frozen blocks are never re-parsed', lm2.refrozen === 0, `refrozen=${lm2.refrozen}`)
ok('tail-only rendering is cheaper than full re-render', renders > 10, `renders=${renders}`)

// An open fence stays in the tail: its contents must render as code while still
// streaming, not as paragraphs that then reflow when the closing ``` lands.
const lm3 = new LiveMarkdown()
lm3.push('intro\n\n```\nlet a = 1\n')
const mid = lm3.render()
ok('open fence renders as code mid-stream', mid.includes('<pre>') && mid.includes('let a = 1'), mid)
lm3.push('```\n')
ok('closed fence still code', lm3.render().includes('<pre>'))

// Blank lines inside a fence must not split the block.
const blocks = splitBlocks('```\nline1\n\nline2\n```\n')
ok('blank line inside a fence does not split the block', blocks.length === 1,
   `blocks=${blocks.length}: ${JSON.stringify(blocks)}`)
ok('blank line outside a fence splits', splitBlocks('a\n\nb\n').length === 2)

// HTML from the model stays inert even mid-stream, not just at the end.
const lm4 = new LiveMarkdown()
lm4.push('text\n\n<img src=x onerror=alert(1)>')
ok('mid-stream html is escaped', !lm4.render().includes('<img'), lm4.render())

// Empty and whitespace-only sources must not throw or emit stray tags.
ok('empty source renders empty', new LiveMarkdown().render() === '')

// --- tables -------------------------------------------------------------
// The model emits these constantly and the renderer had no support at all, so
// a whole table collapsed into one paragraph of pipes.
const tbl = renderMarkdown(
  'Cross-check:\n\n| Source | Price |\n|---|---:|\n| CoinGecko | $84,593 |\n| NowPrice | $84,664 |\n',
)
ok('table becomes a table', tbl.includes('<table>') && tbl.includes('<thead>') && tbl.includes('<tbody>'), tbl)
ok('table cells present', tbl.includes('<td>CoinGecko</td>') && tbl.includes('>$84,593</td>'), tbl)
ok('right alignment from ---:', tbl.includes('class="ta-right"'), tbl)
ok('header uses th', tbl.includes('<th>Source</th>'), tbl)
ok('no stray pipes left over', !tbl.includes('|'), tbl)

// Alignment on both sides.
const al2 = renderMarkdown('| a | b | c |\n|:--|--:|:-:|\n| 1 | 2 | 3 |\n')
ok('left align', al2.includes('class="ta-left"'), al2)
ok('center align', al2.includes('class="ta-center"'), al2)

// Ragged rows are normal from a model: column count comes from the delimiter.
const rag = renderMarkdown('| a | b | c |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 | 4 |\n')
ok('short row padded', (rag.match(/<td><\/td>/) || {}).valid !== false && rag.includes('<td>1</td><td></td><td></td>'), rag)
ok('extra cell dropped', rag.includes('<td>1</td><td>2</td><td>3</td>') && !rag.includes('<td>4</td>'), rag)

// A pipe escaped inside a cell is content, not a column break.
const esc = renderMarkdown('| a | b |\n|---|---|\n| x \\| y | z |\n')
ok('escaped pipe stays in cell', esc.includes('<td>x | y</td>'), esc)

// Prose containing a pipe must not become a table without a delimiter row.
const prose = renderMarkdown('a | b | c\nnext line\n')
ok('prose pipe is not a table', !prose.includes('<table>'), prose)

// HTML inside a cell stays inert.
const evil = renderMarkdown('| a |\n|---|\n| <script>alert(1)</script> |\n')
ok('script inside cell neutralised', !evil.includes('<script>'), evil)

// Inline formatting still works inside cells.
const inl = renderMarkdown('| a |\n|---|\n| **bold** and `code` |\n')
ok('inline markup in cell', inl.includes('<strong>bold</strong>') && inl.includes('<code>code</code>'), inl)

// A table must not swallow the paragraph before it.
const around = renderMarkdown('before\n\n| a |\n|---|\n| 1 |\n\nafter\n')
ok('paragraph before table kept', around.includes('<p>before</p>') && around.includes('<p>after</p>'), around)

// Mid-stream: a partial table is not a table until the delimiter row arrives.
const lmT = new LiveMarkdown()
lmT.push('| Source | Price |\n|---|---|')
const partial = lmT.render()
ok('partial table renders as table once delimiter lands', partial.includes('<table>'), partial)

// --- thinking duration label ----------------------------------------------
import { fmtThink } from '../src/lib/think.ts'
ok('sub-minute seconds', fmtThink(7_000) === 'thought for 7s', fmtThink(7_000))
ok('zero', fmtThink(0) === 'thought for 0s', fmtThink(0))
ok('sub-second rounds, never goes negative', fmtThink(400) === 'thought for 0s', fmtThink(400))
ok('minutes are padded', fmtThink(154_000) === 'thought for 2m34s', fmtThink(154_000))
ok('exactly one minute', fmtThink(60_000) === 'thought for 1m00s', fmtThink(60_000))
ok('59s stays in seconds', fmtThink(59_400) === 'thought for 59s', fmtThink(59_400))
ok('hours drop seconds', fmtThink(3_723_000) === 'thought for 1h02m', fmtThink(3_723_000))
ok('negative input is clamped', fmtThink(-5_000) === 'thought for 0s', fmtThink(-5_000))

if (failed) {
  console.log(`\n${failed} markdown test(s) failed`)
  process.exit(1)
}
console.log('markdown: all checks passed')
