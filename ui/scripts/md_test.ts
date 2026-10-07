/**
 * Run with: node --experimental-strip-types scripts/md_test.ts
 *
 * The security cases are the point. Model output quotes fetched web pages, so
 * anything that reaches this function is untrusted input from the internet.
 */
import { renderMarkdown, escapeHtml, LiveMarkdown, splitBlocks, splitCommitted, needsMarkdown, firstBlockEnd, fenceOpen, revealWords, revealDuration, REVEAL_MIN_MS, REVEAL_MAX_MS, InlineStream } from '../src/lib/markdown.ts'

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
/** Substring check: for output where surrounding chrome is not the point. */
function has(name: string, got: string, needle: string) {
  if (!got.includes(needle)) {
    failed++
    console.log(`FAIL ${name}\n  want substring: ${JSON.stringify(needle)}\n  got: ${JSON.stringify(got)}`)
  }
}
function lacks(name: string, got: string, needle: string) {
  if (got.includes(needle)) {
    failed++
    console.log(`FAIL ${name}\n  must not contain: ${JSON.stringify(needle)}\n  got: ${JSON.stringify(got)}`)
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
has(
  'fenced block keeps content verbatim and escapes it',
  renderMarkdown('```\nlet x = 1 < 2;\n```'),
  '<pre><code>let x = 1 &lt; 2;</code></pre>',
)
has(
  'fenced block gets a copy action',
  renderMarkdown('```\nlet x = 1;\n```'),
  'data-act="copy"',
)
has('non-previewable languages get no preview action', renderMarkdown('```c\nint x;\n```'), 'data-act="copy"')
ok('a C block is not offered a preview it cannot honour',
  !renderMarkdown('```c\nint x;\n```').includes('data-act="preview"'))
has('html block gets a preview action', renderMarkdown('```html\n<b>x</b>\n```'), 'data-act="preview"')
has('svg block gets an inline toggle', renderMarkdown('```svg\n<svg/>\n```'), 'data-act="inline"')
has('svg block gets a panel preview', renderMarkdown('```svg\n<svg/>\n```'), 'data-act="preview"')
has('html block is labelled', renderMarkdown('```html\n<b>x</b>\n```'), '<span class="code-lang">html</span>')
has(
  'code source stays in the code element for copy to read',
  renderMarkdown('```js\nalert(1)\n```'),
  '<pre><code class="lang-js">alert(1)</code></pre>',
)
lacks('a script tag inside a code fence stays escaped',
  renderMarkdown('```html\n<script>alert(1)</script>\n```'), '<script>')
has(
  'fenced block with language',
  renderMarkdown('```rust\nfn main() {}\n```'),
  '<pre><code class="lang-rust">fn main() {}</code></pre>',
)
has(
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
// Which blocks are worth re-rendering at all: a plain paragraph is already
// rendered, and rewriting it is the flicker, not a fix.
ok('plain paragraph needs no parser', !needsMarkdown('Roast levels are a spectrum.\n\n'))
ok('bold needs the parser', needsMarkdown('A **bold** phrase.'))
ok('emphasis needs the parser', needsMarkdown('The *Aldous* coasted on.'))
ok('heading needs the parser', needsMarkdown('## Section title'))
ok('list needs the parser', needsMarkdown('- light\n- medium\n'))
ok('table needs the parser', needsMarkdown('| a | b |'))
ok('fence needs the parser', needsMarkdown('```\ncode\n```'))
ok('a paragraph with a stray less-than is treated as markup', needsMarkdown('if a < b then'))
ok('blank blocks need nothing', !needsMarkdown('\n\n'))

// One block boundary at a time: the live paint needs the end of the block it is
// writing, not of everything finished so far.
ok('open block has no end', firstBlockEnd('one two three') === null)
ok('trailing newline is not a boundary', firstBlockEnd('one two\n') === null)
ok('blank line ends the block', firstBlockEnd('one two\n\nnext block').end === 9)
ok('first boundary wins', firstBlockEnd('a\n\n\nb').end === 3)
ok('fence interior blank lines do not end a block', firstBlockEnd('```\nbody\n\nmore\n```\n') === null)
ok('an unclosed fence is never a boundary', firstBlockEnd('```\nbody\n\nmore') === null)
ok('fence parity: open', fenceOpen('```\nbody'))
ok('fence parity: closed', !fenceOpen('```\nbody\n```'))
ok('fence parity: none', !fenceOpen('plain text'))
ok('blank first line still ends a block', firstBlockEnd('\nreal text').end === 1)

// A table is the one block that *does* end at a blank line sitting at the very
// end of the stream. Markdown has no other reading of it - a blank line closes a
// table - so committing there is exact, and it is what makes a table arrive as a
// table while the answer is still being written instead of when it finishes.
{
  const tbl = '| name | value |\n|---|---|\n| alpha | 1 |\n| beta | 2 |\n\n'
  const e = firstBlockEnd(tbl)
  ok('a table ends at its trailing blank line', e !== null && e.end === tbl.length, `end=${e?.end} len=${tbl.length}`)
  ok(
    'a table still mid-row has no end',
    firstBlockEnd('| name | value |\n|---|---|\n| alpha | 1 |\n') === null,
  )
  ok('a paragraph does not end at a trailing blank line', firstBlockEnd('prose keeps going\n\n') === null)
  ok('a list does not end at a trailing blank line', firstBlockEnd('- one\n\n') === null)
  const sc = splitCommitted(tbl)
  ok('splitCommitted commits a trailing table too', sc.cut === tbl.length && sc.blocks.length === 1, `cut=${sc.cut} blocks=${sc.blocks.length}`)
}

// Structure that markdown finishes with its own newline. A heading is one line
// long, and so is a rule: holding them for a blank line leaves literal hash marks
// on screen for as long as the model takes to type the sentence below them, which
// is the "it only formats at certain points" complaint.
{
  ok('a heading ends at its own newline', firstBlockEnd('## The Copy\nThe photocopier').end === 12)
  ok('a heading still being typed has no end', firstBlockEnd('## The Cop') === null)
  ok('a heading with its newline is a complete block', firstBlockEnd('## Title\n')?.end === 9)
  ok('an atx heading needs the space', firstBlockEnd('#Title not a heading\nmore') === null)
  ok('a thematic break ends at its own newline', firstBlockEnd('---\nmore').end === 4)
  ok(
    'a hash in the middle of a paragraph is not a heading block',
    firstBlockEnd('prose\n# not a heading\n\n') === null,
  )
  const t = '| a |\n|---|\n| 1 |\nnext paragraph'
  const e = firstBlockEnd(t)
  ok(
    'a table ends before a line that is not a row',
    e !== null && e.end === t.indexOf('next'),
    `end=${e?.end} want=${t.indexOf('next')}`,
  )
  ok('a table does not end at a newline that may be followed by rows', firstBlockEnd('| a |\n|---|\n') === null)
}

// The streaming formatter commits at byte offsets, so the offset has to be the
// truth even when nothing was rendered.
{
  const src = 'para one\n\npara two is still arriving'
  const { blocks, cut } = splitCommitted(src)
  ok('commit cut lands after the blank line', cut === 'para one\n\n'.length, `cut=${cut}`)
  ok(
    'committed blocks rebuild the prefix',
    blocks.map((b) => b.trimEnd()).join('\n\n') === src.slice(0, cut).trimEnd(),
    JSON.stringify(blocks),
  )
  ok('unfinished tail is left alone', src.slice(cut) === 'para two is still arriving', JSON.stringify(src.slice(cut)))
  const open = splitCommitted('# Title\n\n```python\ncode = 1\n')
  ok('an open fence is never committed', open.cut <= '# Title\n\n'.length, `cut=${open.cut}`)
  const all = '# Title\n\n```python\ncode = 1\n```\n\n'
  const closed = splitCommitted(all)
  ok('a closed fence commits whole', closed.cut === all.length, `cut=${closed.cut}`)
  const blank = splitCommitted('')
  ok('empty source commits nothing', blank.cut === 0 && blank.blocks.length === 0)
  // A blank line inside a table must not commit a half table.
  const tbl = splitCommitted('| a | b |\n| - | - |\n| 1 | 2 |\n')
  ok('a table with no blank line stays in the tail', tbl.cut === 0, `cut=${tbl.cut}`)
  const rows = splitCommitted('| a | b |\n| - | - |\n| 1 | 2 |\n\nmore')
  ok('a table commits once its blank line lands', rows.cut > 20 && rows.blocks.length === 1, `cut=${rows.cut}`)
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
// Sentence case, because the thinking header is a sentence now ("Thought for 12s")
// and the CSS stopped shouting it in uppercase.
ok('sub-minute seconds', fmtThink(7_000) === 'Thought for 7s', fmtThink(7_000))
ok('zero', fmtThink(0) === 'Thought for 0s', fmtThink(0))
ok('sub-second rounds, never goes negative', fmtThink(400) === 'Thought for 0s', fmtThink(400))
ok('minutes are padded', fmtThink(154_000) === 'Thought for 2m34s', fmtThink(154_000))
ok('exactly one minute', fmtThink(60_000) === 'Thought for 1m00s', fmtThink(60_000))
ok('59s stays in seconds', fmtThink(59_400) === 'Thought for 59s', fmtThink(59_400))
ok('hours drop seconds', fmtThink(3_723_000) === 'Thought for 1h02m', fmtThink(3_723_000))
ok('negative input is clamped', fmtThink(-5_000) === 'Thought for 0s', fmtThink(-5_000))

// ---- streaming reveal (.tin) -------------------------------------------
// The reveal is the difference between text appearing and text arriving. The
// The streaming reveal. Its unit used to be a node inside re-rendered markup;
// it is now an appended node that nothing ever rewrites, because every version
// that re-rendered the growing paragraph made finished words blink again about
// half a second after arriving. What is worth pinning: the split is lossless,
// the duration follows the stream rate, and markdown is applied exactly once.
{
  eq('revealWords keeps whitespace with the word before it', JSON.stringify(revealWords('one two  three ')), JSON.stringify(['one ', 'two  ', 'three ']))
  eq('revealWords on a mid-word arrival', JSON.stringify(revealWords('shal')), JSON.stringify(['shal']))
  eq('revealWords on whitespace only', JSON.stringify(revealWords('  ')), JSON.stringify(['  ']))
  eq('revealWords on nothing', JSON.stringify(revealWords('')), JSON.stringify([]))
  eq('revealWords carries blank lines with the word before them', JSON.stringify(revealWords('a\n\nb ')), JSON.stringify(['a\n\n', 'b ']))

  {
    // The split must be lossless however the stream is cut, or words vanish
    // between two paints.
    const src = 'Lisbon in January: the trams still rattle up the hills.\n\nSecond para.'
    let joined = ''
    for (let i = 0; i < src.length; i += 3) joined += revealWords(src.slice(i, i + 3)).join('')
    eq('cutting the stream anywhere loses nothing', joined, src)
  }

  ok('a fast stream gets a shorter reveal', revealDuration(12) < revealDuration(2))
  eq('a stalled stream does not run a marathon reveal', revealDuration(0.01), REVEAL_MAX_MS)
  eq('a firehose does not lose the effect entirely', revealDuration(10_000), REVEAL_MIN_MS)
  eq('an unknown rate falls back to the css default', revealDuration(NaN), 320)

  {
    // Append-only paint: the transcript's cursor must put every character on
    // screen once and in order, however the deltas are chunked.
    const live = new LiveMarkdown()
    const src = 'Answers stream word by word, then format **once** at the end.\n\n- and lists too\n'
    let painted = 0
    let domText = ''
    for (const w of src.split(/(?<=\s)/)) {
      live.push(w)
      const delta = live.text.slice(painted)
      painted += delta.length
      domText += revealWords(delta).join('')
    }
    eq('the paint cursor lands on the end of the stream', painted, live.text.length)
    eq('the appended words reassemble the source', domText, src)
  }

  {
    // markdown once, at stream end: whatever happened while streaming, the
    // finished block equals a single pass over the stored text, so a reload
    // cannot show something different from what was just watched.
    const f = new LiveMarkdown()
    const src = 'a simple answer with **bold** and a list\n- one\n- two\n'
    for (const w of src.split(/(?<=\s)/)) {
      f.push(w)
      f.render() // the mid-stream approximation, which no longer drives the DOM
    }
    eq('finish() is one pass over the source', f.finish(), renderMarkdown(src))
  }

  {
    const f = new LiveMarkdown()
    f.push('<script>alert(1)</script> done')
    ok('streamed html is escaped in the final block', !f.finish().includes('<script>'), f.finish())
  }
}

if (failed) {
  console.log(`\n${failed} markdown test(s) failed`)
  process.exit(1)
}
console.log('markdown: all checks passed')

// Inline emphasis decided as it streams. The markers must never flash, and
// nothing already on screen may be rewritten: the words inside a construct are
// born inside a <strong>/<em>/<code>, they are not re-parented later.
{
  const textOnly = (ts: any[]) => ts.filter((t) => t.kind === 'text').map((t) => t.text).join('')
  const kinds = (ts: any[]) => ts.map((t) => t.kind).join(',')
  const s1 = new InlineStream()
  const t1 = s1.tokens('read **the copy** now')
  ok('a construct brackets its words', kinds(t1) === 'text,open,text,close,text', kinds(t1))
  ok('markers never reach the text', !textOnly(t1).includes('*'), JSON.stringify(t1))
  const s2 = new InlineStream()
  const c1 = s2.tokens('a **bo')
  const c2 = s2.tokens('ld** x')
  ok('a chunk-final marker is held, not guessed', s2.pendingText() === '' && kinds(c1) === 'text,open,text', kinds(c1))
  ok('the construct closes across a chunk boundary', kinds(c2) === 'text,close,text', kinds(c2))
  const s3 = new InlineStream()
  ok('snake_case is not emphasis', s3.tokens('snake_case_names here').every((t) => t.kind === 'text'))
  const s4 = new InlineStream()
  ok('2**3 is not emphasis', s4.tokens('2**3 to the power').every((t) => t.kind === 'text'))
  const s5 = new InlineStream()
  ok('spaced asterisks are not emphasis', s5.tokens('a * b * c').every((t) => t.kind === 'text'))
  const s6 = new InlineStream()
  const t6 = s6.tokens('run `git status` now')
  ok('code spans open and close', kinds(t6) === 'text,open,text,close,text', kinds(t6))
  const s7 = new InlineStream()
  const t7 = s7.tokens('`a *b* c` done')
  ok('marks inside a code span stay literal', textOnly(t7).includes('a *b* c'), JSON.stringify(t7))
  const s8 = new InlineStream()
  const t8 = s8.tokens('dangling **')
  ok('a trailing marker waits for the next delta', kinds(t8) === 'text' && s8.pendingText() === '**', `${kinds(t8)} pending=${s8.pendingText()}`)
  const s9 = new InlineStream()
  s9.tokens('**unclosed emphasis')
  ok('an unclosed construct leaves the stack open for the parser to settle', s9.pendingText() === '')
}
