/**
 * Run with: node --experimental-strip-types scripts/md_test.ts
 *
 * The security cases are the point. Model output quotes fetched web pages, so
 * anything that reaches this function is untrusted input from the internet.
 */
import { renderMarkdown, escapeHtml } from '../src/lib/markdown.ts'

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

if (failed) {
  console.log(`\n${failed} markdown test(s) failed`)
  process.exit(1)
}
console.log('markdown: all checks passed')
